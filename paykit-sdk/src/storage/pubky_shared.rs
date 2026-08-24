use std::{any::Any, sync::Arc};

use async_trait::async_trait;
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    XChaCha20Poly1305, XNonce,
};
use pubky::{errors::RequestError, Error as PubkyError, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

use super::{
    decode_storage_state_blob, encode_storage_state_blob, run_storage_state_transaction,
    StorageAdapter, StorageState, StorageTransactionCallback,
};
use crate::{
    validate_storage_state, PaykitSdkError, PubkyPublicKey, PubkySessionAccess,
    PubkySessionProvider, Result, PAYKIT_SESSION_CAPABILITIES,
};

const SHARED_STATE_ENVELOPE_VERSION: u32 = 1;
const MAX_SHARED_STATE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Serialize)]
struct EncryptedStateEnvelopeRef<'a> {
    version: u32,
    nonce: [u8; 24],
    ciphertext: &'a [u8],
}

#[derive(Deserialize)]
struct EncryptedStateEnvelope<'a> {
    version: u32,
    nonce: [u8; 24],
    #[serde(borrow)]
    ciphertext: &'a [u8],
}

struct RemoteStateSnapshot {
    state: StorageState,
    revision: Option<String>,
}

/// Encrypted identity-wide SDK state stored in Pubky.
///
/// Each transaction reads the latest complete state, applies one SDK storage
/// transaction, and replaces the encrypted resource when state changed. One
/// instance serializes its own operations, but independent instances must not
/// write concurrently until the homeserver can enforce conditional writes or
/// a durable lock. Encryption protects state contents and integrity, but not
/// resource existence, size, update timing, or replay by the homeserver. After
/// a write transport error, the adapter only reports success if it can read
/// back the exact encrypted revision it attempted to store.
#[derive(Clone)]
pub struct PubkySharedStateStorage {
    session_provider: Arc<dyn PubkySessionProvider>,
    transaction_lock: Arc<Mutex<()>>,
    last_revision: Arc<std::sync::Mutex<Option<String>>>,
}

impl PubkySharedStateStorage {
    /// Create encrypted Pubky-backed storage using the current live session.
    ///
    /// The provider must supply the matching local identity secret and a
    /// session with [`crate::PAYKIT_SESSION_CAPABILITIES`]. Session creation,
    /// persistence, capability renewal, and key rotation remain the caller's
    /// responsibility. Request timeouts come from the supplied Pubky client.
    pub fn new<K>(session_provider: K) -> Self
    where
        K: PubkySessionProvider + 'static,
    {
        Self {
            session_provider: Arc::new(session_provider),
            transaction_lock: Arc::new(Mutex::new(())),
            last_revision: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Return the revision observed by the latest completed storage operation.
    pub fn last_revision(&self) -> Result<Option<String>> {
        self.last_revision
            .lock()
            .map(|revision| revision.clone())
            .map_err(|err| PaykitSdkError::Storage {
                context: "Pubky shared-state revision lock poisoned".into(),
                source: Some(anyhow::anyhow!(err.to_string())),
            })
    }

    async fn load_access(&self) -> Result<PubkySessionAccess> {
        let access = self
            .session_provider
            .load_session_access()
            .await?
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "Pubky shared state requires an active session".into(),
                source: None,
            })?;
        access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        if access.local_secret_key.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "Pubky shared state requires the local identity secret".into(),
                source: None,
            });
        }
        Ok(access)
    }

    async fn load_remote_state(&self, access: &PubkySessionAccess) -> Result<RemoteStateSnapshot> {
        let Some(encrypted) = load_encrypted_blob(access).await? else {
            return Ok(RemoteStateSnapshot {
                state: StorageState::default(),
                revision: None,
            });
        };
        let revision = Some(state_revision(&encrypted));
        let state = decrypt_state(access, &encrypted)?;
        Ok(RemoteStateSnapshot { state, revision })
    }

    fn record_revision(&self, revision: Option<String>) -> Result<()> {
        *self
            .last_revision
            .lock()
            .map_err(|err| PaykitSdkError::Storage {
                context: "Pubky shared-state revision lock poisoned".into(),
                source: Some(anyhow::anyhow!(err.to_string())),
            })? = revision;
        Ok(())
    }
}

#[async_trait]
impl StorageAdapter for PubkySharedStateStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let _guard = self.transaction_lock.lock().await;
        let access = self.load_access().await?;
        let snapshot = self.load_remote_state(&access).await?;
        if self.last_revision()?.is_some() && snapshot.revision.is_none() {
            return Err(PaykitSdkError::Storage {
                context: "previously observed Pubky shared state is missing".into(),
                source: None,
            });
        }
        self.record_revision(snapshot.revision.clone())?;
        let initial_state = snapshot.state;
        let (updated_state, result) = run_storage_state_transaction(initial_state.clone(), f)?;

        if updated_state == initial_state {
            return Ok(result);
        }
        drop(initial_state);

        validate_storage_state(&updated_state).map_err(|_| PaykitSdkError::Storage {
            context: "SDK state failed validation before Pubky storage write".into(),
            source: None,
        })?;
        let encrypted = encrypt_state(&access, &updated_state)?;
        let current_revision = load_encrypted_blob(&access)
            .await?
            .as_deref()
            .map(state_revision);
        if current_revision != snapshot.revision {
            return Err(PaykitSdkError::Storage {
                context: "Pubky shared state changed during transaction".into(),
                source: None,
            });
        }

        let revision = state_revision(&encrypted);
        let write_result = access
            .session
            .storage()
            .put(paykit_lib::PAYKIT_SHARED_STATE_PATH, encrypted)
            .await;
        if let Err(write_error) = write_result {
            let committed = load_encrypted_blob(&access)
                .await
                .map(|stored| {
                    stored
                        .as_deref()
                        .is_some_and(|bytes| state_revision(bytes) == revision)
                })
                .unwrap_or(false);
            if !committed {
                return Err(PaykitSdkError::Transport {
                    context: "write encrypted Pubky shared state could not be confirmed".into(),
                    source: Some(write_error.into()),
                });
            }
        }
        self.record_revision(Some(revision))?;
        Ok(result)
    }
}

async fn load_encrypted_blob(access: &PubkySessionAccess) -> Result<Option<Vec<u8>>> {
    let mut response = match access
        .session
        .storage()
        .get(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
    {
        Ok(response) => response,
        Err(err) if is_not_found(&err) => return Ok(None),
        Err(err) => {
            return Err(PaykitSdkError::Transport {
                context: "read encrypted Pubky shared state".into(),
                source: Some(err.into()),
            });
        }
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SHARED_STATE_BYTES as u64)
    {
        return Err(shared_state_size_error());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| PaykitSdkError::Transport {
            context: "read encrypted Pubky shared-state bytes".into(),
            source: Some(err.into()),
        })?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_SHARED_STATE_BYTES {
            return Err(shared_state_size_error());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

fn encrypt_state(access: &PubkySessionAccess, state: &StorageState) -> Result<Vec<u8>> {
    let key = shared_state_key(access)?;
    encrypt_state_with_key(&key, &access.public_key()?, state)
}

fn decrypt_state(access: &PubkySessionAccess, encrypted: &[u8]) -> Result<StorageState> {
    let key = shared_state_key(access)?;
    decrypt_state_with_key(&key, &access.public_key()?, encrypted)
}

fn shared_state_key(access: &PubkySessionAccess) -> Result<Zeroizing<[u8; 32]>> {
    access
        .local_secret_key
        .as_ref()
        .map(|secret| Zeroizing::new(secret.paykit_shared_state_key()))
        .ok_or_else(|| PaykitSdkError::Identity {
            context: "Pubky shared state requires the local identity secret".into(),
            source: None,
        })
}

fn encrypt_state_with_key(
    key: &[u8; 32],
    public_key: &PubkyPublicKey,
    state: &StorageState,
) -> Result<Vec<u8>> {
    validate_state_identity(public_key, state)?;
    let plaintext = Zeroizing::new(encode_storage_state_blob(state)?);
    let cipher = XChaCha20Poly1305::new(key.into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_ref(),
                aad: public_key.as_str().as_bytes(),
            },
        )
        .map_err(|_| PaykitSdkError::Storage {
            context: "encrypt Pubky shared state".into(),
            source: None,
        })?;
    let envelope = EncryptedStateEnvelopeRef {
        version: SHARED_STATE_ENVELOPE_VERSION,
        nonce: nonce.into(),
        ciphertext: &ciphertext,
    };
    let encrypted = postcard::to_allocvec(&envelope).map_err(|err| PaykitSdkError::Storage {
        context: "encode encrypted Pubky shared state".into(),
        source: Some(err.into()),
    })?;
    if encrypted.len() > MAX_SHARED_STATE_BYTES {
        return Err(shared_state_size_error());
    }
    Ok(encrypted)
}

fn decrypt_state_with_key(
    key: &[u8; 32],
    public_key: &PubkyPublicKey,
    encrypted: &[u8],
) -> Result<StorageState> {
    if encrypted.len() > MAX_SHARED_STATE_BYTES {
        return Err(shared_state_size_error());
    }
    let envelope: EncryptedStateEnvelope<'_> =
        postcard::from_bytes(encrypted).map_err(|err| PaykitSdkError::Storage {
            context: "decode encrypted Pubky shared state".into(),
            source: Some(err.into()),
        })?;
    if envelope.version != SHARED_STATE_ENVELOPE_VERSION {
        return Err(PaykitSdkError::Storage {
            context: format!(
                "unsupported encrypted Pubky shared-state version {}, expected {}",
                envelope.version, SHARED_STATE_ENVELOPE_VERSION
            ),
            source: None,
        });
    }
    let cipher = XChaCha20Poly1305::new(key.into());
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(&envelope.nonce),
                Payload {
                    msg: envelope.ciphertext,
                    aad: public_key.as_str().as_bytes(),
                },
            )
            .map_err(|_| PaykitSdkError::Storage {
                context: "decrypt Pubky shared state".into(),
                source: None,
            })?,
    );
    let state = decode_storage_state_blob(&plaintext)?;
    validate_state_identity(public_key, &state)?;
    Ok(state)
}

fn validate_state_identity(public_key: &PubkyPublicKey, state: &StorageState) -> Result<()> {
    let Some(stored_public_key) = state
        .identity_state
        .as_ref()
        .and_then(|identity| identity.public_key.as_ref())
    else {
        return Ok(());
    };
    if stored_public_key != public_key {
        return Err(PaykitSdkError::Storage {
            context: "Pubky shared state identity does not match the active session".into(),
            source: None,
        });
    }
    Ok(())
}

fn state_revision(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn shared_state_size_error() -> PaykitSdkError {
    PaykitSdkError::Storage {
        context: format!("encrypted Pubky shared state exceeds {MAX_SHARED_STATE_BYTES} bytes"),
        source: None,
    }
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct MissingSessionProvider;

    #[async_trait]
    impl PubkySessionProvider for MissingSessionProvider {
        async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
            Ok(None)
        }

        async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
            Ok(None)
        }

        async fn clear_session_access(&self) -> Result<()> {
            Ok(())
        }
    }

    fn identity() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    #[test]
    fn test_encrypted_state_round_trips() {
        let state = StorageState::default();
        let key = [7; 32];
        let identity = identity();
        let encrypted = encrypt_state_with_key(&key, &identity, &state).unwrap();
        assert_eq!(
            decrypt_state_with_key(&key, &identity, &encrypted).unwrap(),
            state
        );
    }

    #[test]
    fn test_encrypted_state_rejects_wrong_key_and_identity() {
        let state = StorageState::default();
        let identity = identity();
        let encrypted = encrypt_state_with_key(&[7; 32], &identity, &state).unwrap();
        assert!(decrypt_state_with_key(&[8; 32], &identity, &encrypted).is_err());
        assert!(decrypt_state_with_key(&[7; 32], &self::identity(), &encrypted).is_err());
    }

    #[test]
    fn test_encrypted_state_rejects_tampering() {
        let state = StorageState::default();
        let identity = identity();
        let mut encrypted = encrypt_state_with_key(&[7; 32], &identity, &state).unwrap();
        let last = encrypted.last_mut().unwrap();
        *last ^= 1;
        assert!(decrypt_state_with_key(&[7; 32], &identity, &encrypted).is_err());
    }

    #[test]
    fn test_encrypted_state_uses_fresh_nonce() {
        let state = StorageState::default();
        let identity = identity();
        let first = encrypt_state_with_key(&[7; 32], &identity, &state).unwrap();
        let second = encrypt_state_with_key(&[7; 32], &identity, &state).unwrap();
        assert_ne!(first, second);
        assert_ne!(state_revision(&first), state_revision(&second));
    }

    #[test]
    fn test_encrypted_state_rejects_mismatched_bound_identity() {
        let active_identity = identity();
        let state = StorageState {
            identity_state: Some(crate::IdentityState {
                public_key: Some(identity()),
                initialized_at: chrono::Utc::now(),
            }),
            ..StorageState::default()
        };

        let error = encrypt_state_with_key(&[7; 32], &active_identity, &state).unwrap_err();

        assert!(matches!(
            error,
            PaykitSdkError::Storage { context, .. }
                if context.contains("does not match the active session")
        ));
    }

    #[tokio::test]
    async fn test_pubky_shared_state_requires_active_session() {
        let storage = PubkySharedStateStorage::new(MissingSessionProvider);
        let error = storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PaykitSdkError::Identity { context, .. }
                if context.contains("requires an active session")
        ));
    }
}
