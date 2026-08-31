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
    validate_storage_state, PaykitIdentitySecretKey, PaykitSdkError, PubkyPublicKey,
    PubkySessionAccess, PubkySessionProvider, Result, PAYKIT_SESSION_CAPABILITIES,
};

const SHARED_STATE_ENVELOPE_VERSION: u32 = 1;
const MAX_SHARED_STATE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Serialize)]
struct EncryptedStateEnvelopeRef<'a> {
    version: u32,
    key_generation: u64,
    nonce: [u8; 24],
    ciphertext: &'a [u8],
}

#[derive(Deserialize)]
struct EncryptedStateEnvelope<'a> {
    version: u32,
    key_generation: u64,
    nonce: [u8; 24],
    #[serde(borrow)]
    ciphertext: &'a [u8],
}

struct RemoteStateSnapshot {
    state: StorageState,
    revision: Option<String>,
}

struct EncryptedStateBlob {
    bytes: Vec<u8>,
    etag: String,
}

/// Encrypted identity-wide SDK state stored in Pubky.
///
/// Each transaction reads the latest complete state, applies one SDK storage
/// transaction, and replaces the encrypted resource when state changed. One
/// instance serializes its own operations, while homeserver-enforced ETag
/// preconditions prevent independent instances from overwriting each other.
/// Encryption protects state contents and integrity, but not resource
/// existence, size, update timing, or replay by the homeserver. After a write
/// transport error, the adapter only reports success if it can read back the
/// exact encrypted revision it attempted to store.
#[derive(Clone)]
pub struct PubkySharedStateStorage {
    session_provider: Arc<dyn PubkySessionProvider>,
    transaction_lock: Arc<Mutex<()>>,
    last_revision: Arc<std::sync::Mutex<Option<String>>>,
}

impl PubkySharedStateStorage {
    /// Create encrypted Pubky-backed storage using the current live session.
    ///
    /// The provider must supply identity-wide Paykit key material, either
    /// directly or derived from the matching local Pubky identity secret, plus
    /// a session with [`crate::PAYKIT_SESSION_CAPABILITIES`]. Session creation,
    /// persistence, capability renewal, and key distribution remain the
    /// caller's responsibility. Request timeouts come from the Pubky client.
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

    async fn load_session_access(&self) -> Result<PubkySessionAccess> {
        let access = self
            .session_provider
            .load_session_access()
            .await?
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "Pubky shared state requires an active session".into(),
                source: None,
            })?;
        access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        Ok(access)
    }

    async fn load_access(&self) -> Result<PubkySessionAccess> {
        let access = self.load_session_access().await?;
        if access.paykit_identity_secret_key().is_none() {
            return Err(PaykitSdkError::Identity {
                context: "Pubky shared state requires the Paykit identity secret".into(),
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
        let revision = Some(encrypted.etag);
        let state = decrypt_state(access, &encrypted.bytes)?;
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

    async fn commit_encrypted_state(
        &self,
        access: &PubkySessionAccess,
        expected_revision: Option<String>,
        encrypted: Vec<u8>,
    ) -> Result<()> {
        let attempted_revision = state_revision(&encrypted);
        let storage = access.session.storage();
        let write_result = match expected_revision.as_deref() {
            Some(etag) => {
                storage
                    .put_if_match(paykit_lib::PAYKIT_SHARED_STATE_PATH, encrypted, etag)
                    .await
            }
            None => {
                storage
                    .put_if_absent(paykit_lib::PAYKIT_SHARED_STATE_PATH, encrypted)
                    .await
            }
        };

        match write_result {
            Ok(response) => self.record_revision(Some(response_etag(&response)?)),
            Err(write_error) if is_precondition_failed(&write_error) => {
                Err(PaykitSdkError::Storage {
                    context: "Pubky shared state changed during transaction".into(),
                    source: Some(write_error.into()),
                })
            }
            Err(write_error) => {
                let committed_revision = load_encrypted_blob(access)
                    .await
                    .ok()
                    .flatten()
                    .filter(|stored| state_revision(&stored.bytes) == attempted_revision)
                    .map(|stored| stored.etag);
                match committed_revision {
                    Some(revision) => self.record_revision(Some(revision)),
                    None => Err(PaykitSdkError::Transport {
                        context: "write encrypted Pubky shared state could not be confirmed".into(),
                        source: Some(write_error.into()),
                    }),
                }
            }
        }
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
        self.commit_encrypted_state(&access, snapshot.revision, encrypted)
            .await?;
        Ok(result)
    }

    async fn rotate_paykit_identity_key_erased<'a>(
        &self,
        current_key: PaykitIdentitySecretKey,
        replacement_key: PaykitIdentitySecretKey,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        current_key.validate_successor(&replacement_key)?;
        let _guard = self.transaction_lock.lock().await;
        let access = self.load_session_access().await?;
        let encrypted = load_encrypted_blob(&access).await?;
        let revision = encrypted.as_ref().map(|blob| blob.etag.clone());
        if self.last_revision()?.is_some() && revision.is_none() {
            return Err(PaykitSdkError::Storage {
                context: "previously observed Pubky shared state is missing".into(),
                source: None,
            });
        }
        self.record_revision(revision.clone())?;

        let initial_state = match encrypted.as_ref().map(|blob| blob.bytes.as_slice()) {
            None => StorageState::default(),
            Some(encrypted) => match encrypted_state_key_generation(encrypted)? {
                generation if generation == current_key.key_generation() => {
                    decrypt_state_with_key(&current_key, &access.public_key()?, encrypted)?
                }
                generation if generation == replacement_key.key_generation() => {
                    decrypt_state_with_key(&replacement_key, &access.public_key()?, encrypted)?
                }
                generation => {
                    return Err(PaykitSdkError::Identity {
                        context: format!(
                            "shared-state key generation {generation} cannot rotate from {} to {}",
                            current_key.key_generation(),
                            replacement_key.key_generation()
                        ),
                        source: None,
                    });
                }
            },
        };
        let (updated_state, result) = run_storage_state_transaction(initial_state, f)?;
        validate_storage_state(&updated_state).map_err(|_| PaykitSdkError::Storage {
            context: "SDK state failed validation before Paykit key rotation".into(),
            source: None,
        })?;
        let encrypted =
            encrypt_state_with_key(&replacement_key, &access.public_key()?, &updated_state)?;
        self.commit_encrypted_state(&access, revision, encrypted)
            .await?;
        Ok(result)
    }
}

async fn load_encrypted_blob(access: &PubkySessionAccess) -> Result<Option<EncryptedStateBlob>> {
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
    let etag = response_etag(&response)?;
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
    Ok(Some(EncryptedStateBlob { bytes, etag }))
}

fn encrypt_state(access: &PubkySessionAccess, state: &StorageState) -> Result<Vec<u8>> {
    let secret = paykit_identity_secret_key(access)?;
    encrypt_state_with_key(&secret, &access.public_key()?, state)
}

fn decrypt_state(access: &PubkySessionAccess, encrypted: &[u8]) -> Result<StorageState> {
    let secret = paykit_identity_secret_key(access)?;
    decrypt_state_with_key(&secret, &access.public_key()?, encrypted)
}

fn paykit_identity_secret_key(access: &PubkySessionAccess) -> Result<PaykitIdentitySecretKey> {
    access
        .paykit_identity_secret_key()
        .ok_or_else(|| PaykitSdkError::Identity {
            context: "Pubky shared state requires the Paykit identity secret".into(),
            source: None,
        })
}

fn encrypt_state_with_key(
    secret: &PaykitIdentitySecretKey,
    public_key: &PubkyPublicKey,
    state: &StorageState,
) -> Result<Vec<u8>> {
    validate_state_identity(public_key, state)?;
    let plaintext = Zeroizing::new(encode_storage_state_blob(state)?);
    let key = Zeroizing::new(secret.shared_state_key());
    let cipher = XChaCha20Poly1305::new(key.as_ref().into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let aad = shared_state_aad(public_key, secret.key_generation());
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_ref(),
                aad: &aad,
            },
        )
        .map_err(|_| PaykitSdkError::Storage {
            context: "encrypt Pubky shared state".into(),
            source: None,
        })?;
    let envelope = EncryptedStateEnvelopeRef {
        version: SHARED_STATE_ENVELOPE_VERSION,
        key_generation: secret.key_generation(),
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
    secret: &PaykitIdentitySecretKey,
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
    if envelope.key_generation != secret.key_generation() {
        return Err(PaykitSdkError::Identity {
            context: format!(
                "Paykit key generation {} does not match shared-state generation {}",
                secret.key_generation(),
                envelope.key_generation
            ),
            source: None,
        });
    }
    let key = Zeroizing::new(secret.shared_state_key());
    let cipher = XChaCha20Poly1305::new(key.as_ref().into());
    let aad = shared_state_aad(public_key, envelope.key_generation);
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                XNonce::from_slice(&envelope.nonce),
                Payload {
                    msg: envelope.ciphertext,
                    aad: &aad,
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

fn encrypted_state_key_generation(encrypted: &[u8]) -> Result<u64> {
    let envelope: EncryptedStateEnvelope<'_> =
        postcard::from_bytes(encrypted).map_err(|err| PaykitSdkError::Storage {
            context: "decode encrypted Pubky shared state".into(),
            source: Some(err.into()),
        })?;
    if envelope.version != SHARED_STATE_ENVELOPE_VERSION || envelope.key_generation == 0 {
        return Err(PaykitSdkError::Storage {
            context: "encrypted Pubky shared state has an unsupported version or key generation"
                .into(),
            source: None,
        });
    }
    Ok(envelope.key_generation)
}

fn shared_state_aad(public_key: &PubkyPublicKey, key_generation: u64) -> Vec<u8> {
    format!("{}:{key_generation}", public_key.as_str()).into_bytes()
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

fn is_precondition_failed(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::PRECONDITION_FAILED
    )
}

fn response_etag(response: &reqwest::Response) -> Result<String> {
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| PaykitSdkError::Storage {
            context: "Pubky shared-state response is missing an ETag".into(),
            source: None,
        })?;
    if etag.starts_with("W/") {
        return Err(PaykitSdkError::Storage {
            context: "Pubky shared-state response returned a weak ETag".into(),
            source: None,
        });
    }
    Ok(etag
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(etag)
        .to_owned())
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

    fn secret(byte: u8, key_generation: u64) -> PaykitIdentitySecretKey {
        PaykitIdentitySecretKey::new([byte; 32], key_generation).unwrap()
    }

    #[test]
    fn test_encrypted_state_round_trips() {
        let state = StorageState::default();
        let key = secret(7, 1);
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
        let encrypted = encrypt_state_with_key(&secret(7, 1), &identity, &state).unwrap();
        assert!(decrypt_state_with_key(&secret(8, 1), &identity, &encrypted).is_err());
        assert!(decrypt_state_with_key(&secret(7, 1), &self::identity(), &encrypted).is_err());
        assert!(decrypt_state_with_key(&secret(7, 2), &identity, &encrypted).is_err());
    }

    #[test]
    fn test_encrypted_state_rejects_tampering() {
        let state = StorageState::default();
        let identity = identity();
        let mut encrypted = encrypt_state_with_key(&secret(7, 1), &identity, &state).unwrap();
        let last = encrypted.last_mut().unwrap();
        *last ^= 1;
        assert!(decrypt_state_with_key(&secret(7, 1), &identity, &encrypted).is_err());
    }

    #[test]
    fn test_encrypted_state_uses_fresh_nonce() {
        let state = StorageState::default();
        let identity = identity();
        let secret = secret(7, 1);
        let first = encrypt_state_with_key(&secret, &identity, &state).unwrap();
        let second = encrypt_state_with_key(&secret, &identity, &state).unwrap();
        assert_ne!(first, second);
        assert_ne!(state_revision(&first), state_revision(&second));
    }

    #[test]
    fn test_encrypted_state_rekeys_without_changing_logical_state() {
        let identity = identity();
        let state = StorageState {
            identity_state: Some(crate::IdentityState {
                public_key: Some(identity.clone()),
                initialized_at: chrono::Utc::now(),
            }),
            ..StorageState::default()
        };
        let current_key = secret(7, 1);
        let replacement_key = secret(8, 2);
        let current = encrypt_state_with_key(&current_key, &identity, &state).unwrap();
        let decoded = decrypt_state_with_key(&current_key, &identity, &current).unwrap();
        let replacement = encrypt_state_with_key(&replacement_key, &identity, &decoded).unwrap();

        assert_eq!(encrypted_state_key_generation(&replacement).unwrap(), 2);
        assert!(decrypt_state_with_key(&current_key, &identity, &replacement).is_err());
        assert_eq!(
            decrypt_state_with_key(&replacement_key, &identity, &replacement).unwrap(),
            state
        );
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

        let error = encrypt_state_with_key(&secret(7, 1), &active_identity, &state).unwrap_err();

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
