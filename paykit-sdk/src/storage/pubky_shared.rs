use std::{any::Any, future::Future, sync::Arc, time::Duration};

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
    StorageAdapter, StorageKeyRotationCallback, StorageOperation, StorageState,
    StorageTransactionCallback,
};
use crate::{
    validate_storage_state, PaykitIdentitySecretKey, PaykitSdkError, PubkyPublicKey,
    PubkySessionAccess, PubkySessionProvider, Result, PAYKIT_SESSION_CAPABILITIES,
};

const SHARED_STATE_ENVELOPE_VERSION: u32 = 1;
const MAX_SHARED_STATE_BYTES: usize = 64 * 1024 * 1024;
const UNCERTAIN_WRITE_COOLDOWN: Duration = Duration::from_secs(5 * 60);

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
    revision: String,
}

struct SharedStateOperation {
    owner: Arc<Mutex<()>>,
    access: PubkySessionAccess,
    lock: pubky::StorageLock,
    state: Mutex<std::result::Result<StorageState, InvalidatedSharedState>>,
}

enum InvalidatedSharedState {
    LockLost,
    UnconfirmedWrite,
}

impl InvalidatedSharedState {
    fn error(&self) -> PaykitSdkError {
        match self {
            Self::LockLost => PaykitSdkError::ConcurrentUpdate {
                context: "Pubky shared-state lock lost; restart the storage operation".into(),
                source: None,
            },
            Self::UnconfirmedWrite => PaykitSdkError::Storage {
                context: "shared-state operation cannot continue after an unconfirmed write".into(),
                source: None,
            },
        }
    }
}

tokio::task_local! {
    static SHARED_STATE_OPERATION: Arc<SharedStateOperation>;
}

/// Paired metadata from one successfully completed shared-state operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedBackupStateRevision {
    /// Storage revision of the observed state, including transient leases.
    pub state_revision: String,
    /// Content fingerprint of that state's SDK backup projection.
    pub backup_revision: String,
}

pub(crate) fn shared_state_operation_active() -> bool {
    SHARED_STATE_OPERATION.try_with(|_| ()).is_ok()
}

/// Encrypted identity-wide SDK state stored in Pubky.
///
/// Each operation reads the latest complete state under a renewed homeserver
/// write lock. Its transactions reuse that state and replace the encrypted
/// resource whenever it changes, before returning. One instance serializes its
/// own operations. State is never reused after releasing the homeserver lock.
/// Encryption protects state contents and integrity, but not resource
/// existence, size, update timing, or replay by the homeserver. Unconfirmed
/// writes leave a remote marker; the next transaction waits five minutes under
/// its renewed lock before reading state. This is a best-effort delay, not a
/// substitute for homeserver enforcement of lock ownership at publication.
/// Cancelling the SDK operation interrupts that wait; a later call restarts it.
/// Whole-operation timeouts must allow the cooldown, unlike HTTP request timeouts.
#[derive(Clone)]
pub struct PubkySharedStateStorage {
    session_provider: Arc<dyn PubkySessionProvider>,
    transaction_lock: Arc<Mutex<()>>,
    last_revision: Arc<std::sync::Mutex<Option<String>>>,
    observed_backup_revision: Arc<std::sync::Mutex<Option<ObservedBackupStateRevision>>>,
}

impl PubkySharedStateStorage {
    fn active_operation(&self) -> Option<Arc<SharedStateOperation>> {
        SHARED_STATE_OPERATION
            .try_with(|operation| {
                Arc::ptr_eq(&operation.owner, &self.transaction_lock).then(|| Arc::clone(operation))
            })
            .ok()
            .flatten()
    }

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
            observed_backup_revision: Arc::new(std::sync::Mutex::new(None)),
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

    /// Return paired backup metadata without reading storage or checking live access.
    ///
    /// This is historical observation, never authority or proof of current remote
    /// state. Unavailable during storage operations, after failed or cancelled
    /// operations, and after key rotation or recovery until a normal operation
    /// completes successfully. Discard caller caches when changing identity or keys.
    pub fn observed_backup_state_revision(&self) -> Result<Option<ObservedBackupStateRevision>> {
        let Ok(_guard) = self.transaction_lock.try_lock() else {
            return Ok(None);
        };
        self.observed_backup_revision
            .lock()
            .map(|revision| revision.clone())
            .map_err(|_| PaykitSdkError::Storage {
                context: "Pubky backup revision lock poisoned".into(),
                source: None,
            })
    }

    fn record_backup_revision(&self, revision: Option<ObservedBackupStateRevision>) {
        if let Ok(mut observed) = self.observed_backup_revision.lock() {
            *observed = revision;
        }
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
            let owner = access.public_key()?.to_public_key()?;
            if paykit_lib::get_paykit_app_registry(&access.outbox_client.public_storage(), &owner)
                .await?
                .is_some_and(|registry| registry.noise_public_key().is_some())
            {
                return Err(PaykitSdkError::Storage {
                    context: "Pubky shared state is missing for an initialized Paykit identity"
                        .into(),
                    source: None,
                });
            }
            return Ok(RemoteStateSnapshot {
                state: StorageState::default(),
                revision: None,
            });
        };
        let revision = Some(encrypted.revision);
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
        lock: &pubky::StorageLock,
        encrypted: Vec<u8>,
    ) -> Result<()> {
        let attempted_revision = state_revision(&encrypted);
        let storage = access.session.storage();
        // Each encrypted attempt has a fresh nonce. A distinct marker prevents
        // delayed cleanup of one write from clearing another write's marker.
        let pending_path = format!(
            "{}{}",
            paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX,
            blake3::hash(&encrypted).to_hex(),
        );
        if let Err(error) = storage.put(&pending_path, Vec::new()).await {
            // No state PUT was started, even if the marker PUT reached the server.
            let _ = remove_pending_write(&storage, &pending_path).await;
            return Err(shared_write_error(
                "mark pending Pubky shared-state write",
                error,
            ));
        }
        let write_result = storage.put_locked(lock, encrypted).await;

        match write_result {
            Ok(_) => {
                // Cleanup failure must not turn a confirmed commit into a
                // failed transaction. The leftover marker causes a cooldown.
                let _ = remove_pending_write(&storage, &pending_path).await;
                self.record_revision(Some(attempted_revision))
            }
            Err(write_error) if is_precondition_failed(&write_error) => {
                let _ = remove_pending_write(&storage, &pending_path).await;
                Err(PaykitSdkError::ConcurrentUpdate {
                    context: "Pubky shared-state lock expired during transaction".into(),
                    source: Some(write_error.into()),
                })
            }
            Err(write_error) if is_quota_rejection(&write_error) => {
                // Pubky homeserver quota checks reject before publishing staged bytes.
                let _ = remove_pending_write(&storage, &pending_path).await;
                Err(shared_write_error(
                    "Pubky shared-state write rejected by storage quota",
                    write_error,
                ))
            }
            Err(write_error) if is_rate_limit_rejection(&write_error) => {
                // A throttled PUT is rejected before publication, not left in flight.
                let _ = remove_pending_write(&storage, &pending_path).await;
                Err(shared_write_error(
                    "Pubky shared-state write rejected by rate limit",
                    write_error,
                ))
            }
            Err(write_error) => {
                tracing::warn!(
                    "shared-state commit could not be confirmed; retaining write marker"
                );
                Err(shared_write_error(
                    "write encrypted Pubky shared state could not be confirmed",
                    write_error,
                ))
            }
        }
    }
}

#[async_trait]
impl StorageAdapter for PubkySharedStateStorage {
    async fn run_operation_erased<'a>(
        &self,
        operation: StorageOperation<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        if self.active_operation().is_some() {
            return operation.await;
        }
        self.record_backup_revision(None);
        if shared_state_operation_active() {
            return Err(PaykitSdkError::Policy {
                context: "cannot nest operations for different shared-state adapters".into(),
                source: None,
            });
        }
        let _guard = self.transaction_lock.lock().await;
        self.record_backup_revision(None);
        let access = self.load_access().await?;
        let session = access.session.clone();
        let (result, state) = with_shared_state_lock(&session, |lock| async move {
            // A late write can commit and remove its marker during a state read.
            // Checking markers first prevents accepting that read's stale contents.
            wait_for_pending_writes(&access.session).await?;
            let snapshot = self.load_remote_state(&access).await?;
            if self.last_revision()?.is_some() && snapshot.revision.is_none() {
                return Err(PaykitSdkError::Storage {
                    context: "previously observed Pubky shared state is missing".into(),
                    source: None,
                });
            }
            self.record_revision(snapshot.revision)?;
            let context = Arc::new(SharedStateOperation {
                owner: Arc::clone(&self.transaction_lock),
                access,
                lock,
                state: Mutex::new(Ok(snapshot.state)),
            });
            let result = SHARED_STATE_OPERATION
                .scope(Arc::clone(&context), operation)
                .await?;
            let state = Arc::try_unwrap(context)
                .ok()
                .and_then(|context| context.state.into_inner().ok());
            Ok((result, state))
        })
        .await?;
        // Hash only after the remote lock scope ends. The local lock still pairs
        // this completed state with its revision; neither is reused for authority.
        let observed = state.and_then(|state| {
            Some(ObservedBackupStateRevision {
                state_revision: self.last_revision().ok()??,
                backup_revision: crate::SdkBackupState::from_storage_state(state)
                    .content_fingerprint()
                    .ok()?,
            })
        });
        self.record_backup_revision(observed);
        Ok(result)
    }

    async fn recover_shared_state_from_backup(
        &self,
        current_key: PaykitIdentitySecretKey,
        replacement_key: PaykitIdentitySecretKey,
        state: super::ValidatedStorageState,
    ) -> Result<()> {
        self.record_backup_revision(None);
        if shared_state_operation_active() {
            return Err(PaykitSdkError::Policy {
                context: "shared-state recovery requires a separate storage operation".into(),
                source: None,
            });
        }
        current_key.validate_successor(&replacement_key)?;
        let state = state.into_storage_state();
        let _guard = self.transaction_lock.lock().await;
        self.record_backup_revision(None);
        let access = self.load_access().await?;
        let owner = access.public_key()?;
        replacement_key.validate_pubky_derivation(access.local_secret_key.as_ref())?;
        if state
            .identity_state
            .as_ref()
            .and_then(|identity| identity.public_key.as_ref())
            != Some(&owner)
            || state.paykit_noise_public_key.as_ref()
                != Some(&super::paykit_noise_public_key(&replacement_key))
            || access.paykit_identity_secret_key().as_ref() != Some(&current_key)
        {
            return Err(PaykitSdkError::Identity {
                context: "shared-state recovery identity or keys do not match the live session"
                    .into(),
                source: None,
            });
        }
        validate_storage_state(&state)?;
        let session = access.session.clone();
        with_shared_state_lock(
            &session,
            |lock| async move {
                wait_for_pending_writes(&access.session).await?;
                let registry = paykit_lib::get_paykit_app_registry(
                    &access.outbox_client.public_storage(),
                    &owner.to_public_key()?,
                )
                .await?
                .ok_or_else(|| PaykitSdkError::NotFound {
                    context: "shared-state recovery requires the Paykit App Registry".into(),
                    source: None,
                })?;
                let registry_replaced = registry.key_generation() == replacement_key.key_generation();
                let registry_key = if registry_replaced { &replacement_key } else { &current_key };
                if registry.key_generation() != registry_key.key_generation()
                    || registry.noise_public_key()
                        != Some(&super::paykit_noise_public_key(registry_key).to_public_key()?)
                {
                    return Err(PaykitSdkError::Identity {
                        context: "shared-state recovery keys do not match the App Registry".into(),
                        source: None,
                    });
                }
                let encrypted = load_encrypted_blob(&access).await?;
                if let Some(blob) = &encrypted {
                    if recovery_already_committed(&current_key, &replacement_key, &owner, &blob.bytes)? {
                        self.record_revision(Some(blob.revision.clone()))?;
                        return Ok(());
                    }
                }
                if registry_replaced {
                    return Err(PaykitSdkError::Identity {
                        context: "replacement shared state is missing or corrupt; use its current key and a new successor".into(),
                        source: None,
                    });
                }
                let encrypted = encrypt_state_with_key(&replacement_key, &owner, &state)?;
                self.commit_encrypted_state(&access, &lock, encrypted).await
            },
        )
        .await
    }

    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let Some(operation) = self.active_operation() else {
            return self
                .run_operation_erased(Box::pin(self.transaction_erased(f)))
                .await;
        };
        let mut state = operation.state.lock().await;
        let access = self.load_access().await?;
        if access.public_key()? != operation.access.public_key()?
            || access.paykit_identity_secret_key() != operation.access.paykit_identity_secret_key()
        {
            return Err(PaykitSdkError::Identity {
                context: "Pubky identity or Paykit key changed during storage operation".into(),
                source: None,
            });
        }
        let initial_state = state.as_ref().map_err(InvalidatedSharedState::error)?;
        let (mut updated_state, result) = run_storage_state_transaction(initial_state.clone(), f)?;
        if &updated_state == initial_state {
            return Ok(result);
        }

        // Validate before compaction so malformed evidence cannot disappear
        // merely because a newer list supersedes it.
        validate_storage_state(&updated_state).map_err(|_| PaykitSdkError::Storage {
            context: "SDK state failed validation before Pubky storage write".into(),
            source: None,
        })?;
        super::compaction::compact_private_payment_lists(&mut updated_state);
        let encrypted = encrypt_state(&access, &updated_state)?;
        // A failed or cancelled commit must not leave reusable stale state.
        *state = Err(InvalidatedSharedState::UnconfirmedWrite);
        if let Err(error) = self
            .commit_encrypted_state(&access, &operation.lock, encrypted)
            .await
        {
            if error.is_concurrent_update() {
                *state = Err(InvalidatedSharedState::LockLost);
            }
            return Err(error);
        }
        *state = Ok(updated_state);
        Ok(result)
    }

    async fn rotate_paykit_identity_key_erased<'a>(
        &self,
        current_key: PaykitIdentitySecretKey,
        replacement_key: PaykitIdentitySecretKey,
        f: StorageKeyRotationCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        self.record_backup_revision(None);
        if shared_state_operation_active() {
            return Err(PaykitSdkError::Policy {
                context: "Paykit key rotation requires a separate storage operation".into(),
                source: None,
            });
        }
        current_key.validate_successor(&replacement_key)?;
        let _guard = self.transaction_lock.lock().await;
        self.record_backup_revision(None);
        let access = self.load_session_access().await?;
        current_key.validate_pubky_derivation(access.local_secret_key.as_ref())?;
        replacement_key.validate_pubky_derivation(access.local_secret_key.as_ref())?;
        let session = access.session.clone();
        with_shared_state_lock(
            &session,
            |lock| async move {
                wait_for_pending_writes(&access.session).await?;
                let encrypted = load_encrypted_blob(&access).await?;
                let revision = encrypted.as_ref().map(|blob| blob.revision.clone());
                if self.last_revision()?.is_some() && revision.is_none() {
                    return Err(PaykitSdkError::Storage {
                        context: "previously observed Pubky shared state is missing".into(),
                        source: None,
                    });
                }
                self.record_revision(revision.clone())?;

                let (initial_state, already_rotated) =
                    match encrypted.as_ref().map(|blob| blob.bytes.as_slice()) {
                        None => (StorageState::default(), false),
                        Some(encrypted) => match encrypted_state_key_generation(encrypted)? {
                            generation if generation == current_key.key_generation() => (
                                decrypt_state_with_key(&current_key, &access.public_key()?, encrypted)?,
                                false,
                            ),
                            generation if generation == replacement_key.key_generation() => (
                                decrypt_state_with_key(&replacement_key, &access.public_key()?, encrypted)?,
                                true,
                            ),
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
                let (mut updated_state, result) = run_storage_state_transaction(
                    initial_state,
                    Box::new(|tx| {
                        let result = f(tx, already_rotated)?;
                        tx.save_paykit_noise_public_key(super::paykit_noise_public_key(&replacement_key));
                        Ok(result)
                    }),
                )?;
                validate_storage_state(&updated_state).map_err(|_| PaykitSdkError::Storage {
                    context: "SDK state failed validation before Paykit key rotation".into(),
                    source: None,
                })?;
                super::compaction::compact_private_payment_lists(&mut updated_state);
                let encrypted =
                    encrypt_state_with_key(&replacement_key, &access.public_key()?, &updated_state)?;
                self.commit_encrypted_state(&access, &lock, encrypted)
                    .await?;
                Ok(result)
            },
        )
        .await
    }
}

async fn with_shared_state_lock<T, F, Fut>(session: &pubky::PubkySession, operation: F) -> Result<T>
where
    F: FnOnce(pubky::StorageLock) -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut acquired = false;
    let result =
        paykit_lib::with_write_lock(session, paykit_lib::PAYKIT_SHARED_STATE_PATH, |lock| {
            acquired = true;
            operation(lock)
        })
        .await;
    match result {
        Err(error) if !acquired && error.is_concurrent_update() => {
            if !pending_write_paths(session)
                .await
                .is_ok_and(|paths| paths.is_empty())
            {
                Err(PaykitSdkError::SharedStateBusy {
                    context: "Pubky shared state is locked and pending writes could not be ruled out; retry later".into(),
                    source: Some(error.into()),
                })
            } else {
                Err(error)
            }
        }
        result => result,
    }
}

async fn pending_write_paths(session: &pubky::PubkySession) -> Result<Vec<String>> {
    let pending = match session
        .storage()
        .list(paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX)
        .map_err(|error| shared_write_error("list pending Pubky shared-state writes", error))?
        .shallow(true)
        .limit(100)
        .send()
        .await
    {
        Ok(pending) => pending,
        Err(error) if is_not_found(&error) => return Ok(Vec::new()),
        Err(error) => {
            return Err(shared_write_error(
                "list pending Pubky shared-state writes",
                error,
            ));
        }
    };
    for entry in &pending {
        let suffix = entry
            .path
            .as_str()
            .strip_prefix(paykit_lib::PAYKIT_SHARED_STATE_WRITE_PATH_PREFIX);
        if entry.owner != session.public_key()
            || !suffix
                .is_some_and(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            // An unknown marker may describe an unfinished write. Do not let
            // another writer proceed by treating it as an empty directory.
            return Err(PaykitSdkError::Storage {
                context: "invalid pending Pubky shared-state write path".into(),
                source: None,
            });
        }
    }
    Ok(pending
        .into_iter()
        .map(|entry| entry.path.to_string())
        .collect())
}

async fn wait_for_pending_writes(session: &pubky::PubkySession) -> Result<()> {
    let storage = session.storage();
    let mut waited = false;
    loop {
        let pending = pending_write_paths(session).await?;
        if pending.is_empty() {
            return Ok(());
        }
        if !waited {
            // Keep the state lock throughout the wait. Cancellation leaves the
            // markers intact so the next app waits afresh, without clock skew.
            tracing::warn!(
                pending_writes = pending.len(),
                cooldown_seconds = UNCERTAIN_WRITE_COOLDOWN.as_secs(),
                "waiting under shared-state lock for uncertain writes"
            );
            tokio::time::sleep(UNCERTAIN_WRITE_COOLDOWN).await;
            tracing::info!("shared-state uncertain-write cooldown completed");
            waited = true;
        }
        for path in pending {
            remove_pending_write(&storage, &path).await?;
        }
    }
}

async fn remove_pending_write(storage: &pubky::SessionStorage, path: &str) -> Result<()> {
    for attempt in 0..3 {
        match storage.delete(path).await {
            Ok(_) => return Ok(()),
            Err(error) if is_not_found(&error) => return Ok(()),
            Err(error) if attempt == 2 => {
                tracing::warn!(
                    "pending shared-state write marker could not be removed; next operation will wait"
                );
                return Err(shared_write_error(
                    "remove pending Pubky shared-state write",
                    error,
                ));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await,
        }
    }
    unreachable!("bounded marker removal attempts always return")
}

fn shared_write_error(context: &str, error: PubkyError) -> PaykitSdkError {
    PaykitSdkError::Transport {
        context: context.into(),
        source: Some(error.into()),
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
    let revision = state_revision(&bytes);
    Ok(Some(EncryptedStateBlob { bytes, revision }))
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
    let plaintext = decrypt_state_blob(secret, public_key, encrypted)?;
    let state = decode_storage_state_blob(&plaintext)?;
    validate_state_identity(public_key, &state)?;
    Ok(state)
}

fn decrypt_state_blob(
    secret: &PaykitIdentitySecretKey,
    public_key: &PubkyPublicKey,
    encrypted: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    if encrypted.len() > MAX_SHARED_STATE_BYTES {
        return Err(shared_state_size_error());
    }
    let envelope = decode_encrypted_state_envelope(encrypted)?;
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
    Ok(plaintext)
}

fn recovery_already_committed(
    current_key: &PaykitIdentitySecretKey,
    replacement_key: &PaykitIdentitySecretKey,
    owner: &PubkyPublicKey,
    encrypted: &[u8],
) -> Result<bool> {
    // Read the header independently: truncated ciphertext is recoverable, but
    // unrecognizable headers must not be treated as evidence of the old generation.
    let ((version, generation), _) =
        postcard::take_from_bytes::<(u32, u64)>(encrypted).map_err(|error| {
            PaykitSdkError::Storage {
                context: "cannot identify the corrupt shared-state generation".into(),
                source: Some(error.into()),
            }
        })?;
    if version != SHARED_STATE_ENVELOPE_VERSION {
        return Err(PaykitSdkError::Storage {
            context: "cannot recover an unsupported shared-state envelope version".into(),
            source: None,
        });
    }
    if generation == replacement_key.key_generation() {
        let state = decrypt_state_with_key(replacement_key, owner, encrypted)?;
        if state
            .identity_state
            .as_ref()
            .and_then(|identity| identity.public_key.as_ref())
            != Some(owner)
            || state.paykit_noise_public_key.as_ref()
                != Some(&super::paykit_noise_public_key(replacement_key))
        {
            return Err(PaykitSdkError::Identity {
                context: "replacement shared state does not match recovery identity and key".into(),
                source: None,
            });
        }
        return Ok(true);
    }
    if generation != current_key.key_generation() {
        return Err(PaykitSdkError::Identity {
            context: "shared-state recovery found an unexpected key generation".into(),
            source: None,
        });
    }
    if let Ok(plaintext) = decrypt_state_blob(current_key, owner, encrypted) {
        if postcard::take_from_bytes::<u32>(&plaintext)
            .is_ok_and(|(version, _)| version != super::SDK_STATE_BLOB_VERSION)
        {
            return Err(PaykitSdkError::Storage {
                context: "cannot recover an unsupported SDK state blob version".into(),
                source: None,
            });
        }
        if let Ok(state) = decode_storage_state_blob(&plaintext) {
            validate_state_identity(owner, &state)?;
            return Err(PaykitSdkError::Policy {
                context: "cannot recover a backup over healthy shared state".into(),
                source: None,
            });
        }
    }
    Ok(false)
}

fn encrypted_state_key_generation(encrypted: &[u8]) -> Result<u64> {
    let envelope = decode_encrypted_state_envelope(encrypted)?;
    if envelope.version != SHARED_STATE_ENVELOPE_VERSION || envelope.key_generation == 0 {
        return Err(PaykitSdkError::Storage {
            context: "encrypted Pubky shared state has an unsupported version or key generation"
                .into(),
            source: None,
        });
    }
    Ok(envelope.key_generation)
}

fn decode_encrypted_state_envelope(encrypted: &[u8]) -> Result<EncryptedStateEnvelope<'_>> {
    let (envelope, remainder) =
        postcard::take_from_bytes(encrypted).map_err(|err| PaykitSdkError::Storage {
            context: "decode encrypted Pubky shared state".into(),
            source: Some(err.into()),
        })?;
    if !remainder.is_empty() {
        return Err(PaykitSdkError::Storage {
            context: "encrypted Pubky shared state contains trailing bytes".into(),
            source: None,
        });
    }
    Ok(envelope)
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
    paykit_lib::content_revision(bytes)
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

fn is_quota_rejection(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::INSUFFICIENT_STORAGE
    )
}

fn is_rate_limit_rejection(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::TOO_MANY_REQUESTS
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
    fn test_invalidated_state_distinguishes_lock_loss_from_uncertain_writes() {
        assert!(InvalidatedSharedState::LockLost
            .error()
            .is_concurrent_update());
        let error = InvalidatedSharedState::UnconfirmedWrite.error();
        assert!(matches!(error, PaykitSdkError::Storage { .. }));
        assert!(!error.is_concurrent_update());
    }

    #[test]
    fn test_recovery_rejects_healthy_state_and_unknown_generations() {
        let owner = identity();
        let current = secret(7, 1);
        let replacement = secret(8, 2);
        let healthy = encrypt_state_with_key(&current, &owner, &StorageState::default()).unwrap();
        assert!(matches!(
            recovery_already_committed(&current, &replacement, &owner, &healthy),
            Err(PaykitSdkError::Policy { .. })
        ));
        for (version, generation) in [(1_u32, 0_u64), (1, 3), (2, 1)] {
            let header = postcard::to_allocvec(&(version, generation)).unwrap();
            assert!(recovery_already_committed(&current, &replacement, &owner, &header).is_err());
        }
        assert!(recovery_already_committed(&current, &replacement, &owner, &[]).is_err());
        let truncated = postcard::to_allocvec(&(1_u32, 1_u64)).unwrap();
        assert!(!recovery_already_committed(&current, &replacement, &owner, &truncated).unwrap());
        let mut damaged = healthy;
        *damaged.last_mut().unwrap() ^= 1;
        assert!(!recovery_already_committed(&current, &replacement, &owner, &damaged).unwrap());
    }

    #[test]
    fn test_recovery_retry_requires_valid_replacement_state() {
        let owner = identity();
        let current = secret(7, 1);
        let replacement = secret(8, 2);
        let state = StorageState {
            identity_state: Some(crate::IdentityState {
                public_key: Some(owner.clone()),
                initialized_at: chrono::Utc::now(),
            }),
            paykit_noise_public_key: Some(super::super::paykit_noise_public_key(&replacement)),
            ..StorageState::default()
        };
        let encrypted = encrypt_state_with_key(&replacement, &owner, &state).unwrap();
        assert!(recovery_already_committed(&current, &replacement, &owner, &encrypted).unwrap());
        assert!(recovery_already_committed(&current, &secret(9, 2), &owner, &encrypted).is_err());
        assert!(
            recovery_already_committed(&current, &replacement, &identity(), &encrypted).is_err()
        );
        let mut damaged = encrypted;
        *damaged.last_mut().unwrap() ^= 1;
        assert!(recovery_already_committed(&current, &replacement, &owner, &damaged).is_err());
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
    fn test_encrypted_state_rejects_trailing_bytes() {
        let key = secret(7, 1);
        let identity = identity();
        let mut encrypted =
            encrypt_state_with_key(&key, &identity, &StorageState::default()).unwrap();
        encrypted.push(0);
        assert!(decrypt_state_with_key(&key, &identity, &encrypted).is_err());
        assert!(encrypted_state_key_generation(&encrypted).is_err());
    }

    #[test]
    fn test_write_rejections_do_not_include_ambiguous_failures() {
        for status in [
            StatusCode::INSUFFICIENT_STORAGE,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            let error = PubkyError::Request(RequestError::Server {
                status,
                message: String::new(),
            });
            assert_eq!(
                is_quota_rejection(&error),
                status == StatusCode::INSUFFICIENT_STORAGE
            );
            assert_eq!(
                is_rate_limit_rejection(&error),
                status == StatusCode::TOO_MANY_REQUESTS
            );
        }
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
