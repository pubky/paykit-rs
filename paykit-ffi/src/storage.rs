use std::{
    any::Any,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use paykit_sdk::storage::{
    run_storage_state_transaction, StorageAdapter, StorageState, StorageTransactionCallback,
};
use paykit_sdk::{validate_storage_state, PaykitSdkError, SdkBackupState};
use serde::{Deserialize, Serialize};

use crate::errors::{ffi_error_to_sdk, storage_error, PaykitFfiError};
use crate::secrets::FfiSdkStateBlob;
use crate::{SDK_BACKUP_BLOB_VERSION, SDK_STATE_BLOB_VERSION};

const INVALID_STATE_BLOB_CODE: &str = "invalid_state_blob";
const INVALID_STATE_BLOB_CONTEXT: &str = "SDK state blob failed validation";

/// Current SDK state blob with its platform storage revision.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiSdkStateBlobSnapshot {
    /// Encoded SDK state.
    pub blob: Arc<FfiSdkStateBlob>,
    /// Opaque platform storage revision for optimistic writes.
    pub revision: String,
}

/// Platform-owned durable blob store for SDK state.
///
/// The SDK invokes these callbacks while holding its per-handle storage lock.
/// Implementations must not call back into the same SDK handle from either
/// callback because doing so would deadlock.
#[uniffi::export(with_foreign)]
pub trait FfiSdkStateBlobStore: Send + Sync {
    /// Load the current SDK state blob, when one exists.
    fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError>;

    /// Atomically save a new SDK state blob.
    ///
    /// `expected_revision` is `None` when no previous blob was loaded. The
    /// platform store should reject the write if the stored revision changed.
    /// A successful changed write must return a non-empty, globally unique
    /// revision that has never represented an earlier state blob. Reusing a
    /// revision permits an ABA stale write to overwrite newer state.
    fn save_state_blob_atomically(
        &self,
        blob: Arc<FfiSdkStateBlob>,
        expected_revision: Option<String>,
    ) -> Result<String, PaykitFfiError>;
}

#[derive(Clone)]
pub(crate) struct FfiSdkStorage {
    pub(crate) store: Arc<dyn FfiSdkStateBlobStore>,
    pub(crate) transaction_lock: Arc<Mutex<()>>,
}

impl FfiSdkStorage {
    fn load_validated_state(&self) -> paykit_sdk::Result<Option<(String, StorageState)>> {
        let snapshot = self
            .store
            .load_state_blob()
            .map_err(|err| ffi_error_to_sdk(err, "load SDK state blob"))?;
        snapshot
            .map(|snapshot| {
                let state =
                    decode_state_blob_snapshot(&snapshot.revision, &snapshot.blob.export_bytes())
                        .map_err(|err| ffi_error_to_sdk(err, "load SDK state blob"))?;
                Ok((snapshot.revision, state))
            })
            .transpose()
    }

    pub(crate) fn state_revision(&self) -> Result<Option<String>, PaykitFfiError> {
        let _guard = self.transaction_lock.lock().map_err(|_| {
            storage_error(
                "state_transaction_lock_poisoned",
                "SDK state transaction lock poisoned",
            )
        })?;
        self.load_validated_state()
            .map(|snapshot| snapshot.map(|(revision, _)| revision))
            .map_err(Into::into)
    }
}

#[async_trait]
impl StorageAdapter for FfiSdkStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        let _guard = self
            .transaction_lock
            .lock()
            .map_err(|_| PaykitSdkError::Storage {
                context: "SDK state transaction lock poisoned".into(),
                source: None,
            })?;
        let snapshot = self.load_validated_state()?;
        let (expected_revision, initial_state) = match snapshot {
            Some((revision, state)) => (Some(revision), state),
            None => (None, StorageState::default()),
        };

        let (updated_state, result) = run_storage_state_transaction(initial_state.clone(), f)?;

        if updated_state != initial_state {
            validate_storage_state(&updated_state).map_err(|_| PaykitSdkError::Storage {
                context: INVALID_STATE_BLOB_CONTEXT.into(),
                source: None,
            })?;
            let blob = Arc::new(FfiSdkStateBlob::new(
                encode_storage_state(&updated_state)
                    .map_err(|err| ffi_error_to_sdk(err, "encode SDK state blob"))?,
            ));
            let previous_revision = expected_revision.clone();
            let new_revision = self
                .store
                .save_state_blob_atomically(blob, expected_revision)
                .map_err(|err| ffi_error_to_sdk(err, "save SDK state blob"))?;
            if new_revision.is_empty()
                || previous_revision.as_deref() == Some(new_revision.as_str())
            {
                return Err(PaykitSdkError::Storage {
                    context:
                        "state blob store returned an unchanged or empty revision after a write"
                            .into(),
                    source: None,
                });
            }
        }

        Ok(result)
    }
}

#[derive(Serialize, Deserialize)]
struct StorageStateEnvelope {
    version: u32,
    state: StorageState,
}

#[derive(Serialize, Deserialize)]
struct BackupStateEnvelope {
    version: u32,
    backup: SdkBackupState,
}

#[derive(Serialize, Deserialize)]
struct StateBlobSnapshotEnvelope {
    version: u32,
    revision: String,
    blob: Vec<u8>,
}

pub(crate) fn encode_storage_state(state: &StorageState) -> Result<Vec<u8>, PaykitFfiError> {
    postcard::to_allocvec(&StorageStateEnvelope {
        version: SDK_STATE_BLOB_VERSION,
        state: state.clone(),
    })
    .map_err(|err| storage_error("encode_state_blob", format!("encode SDK state blob: {err}")))
}

pub(crate) fn decode_storage_state(bytes: &[u8]) -> Result<StorageState, PaykitFfiError> {
    let envelope: StorageStateEnvelope = postcard::from_bytes(bytes)
        .map_err(|_| storage_error("decode_state_blob", "could not decode SDK state blob"))?;
    if envelope.version != SDK_STATE_BLOB_VERSION {
        return Err(storage_error(
            "unsupported_state_blob_version",
            format!(
                "unsupported SDK state blob version {}, expected {}",
                envelope.version, SDK_STATE_BLOB_VERSION
            ),
        ));
    }
    validate_storage_state(&envelope.state).map_err(|_| invalid_state_blob_error())?;
    Ok(envelope.state)
}

fn decode_state_blob_snapshot(revision: &str, blob: &[u8]) -> Result<StorageState, PaykitFfiError> {
    if revision.is_empty() {
        return Err(invalid_state_blob_error());
    }
    decode_storage_state(blob)
}

fn invalid_state_blob_error() -> PaykitFfiError {
    storage_error(INVALID_STATE_BLOB_CODE, INVALID_STATE_BLOB_CONTEXT)
}

pub(crate) fn encode_backup_state(backup: &SdkBackupState) -> Result<Vec<u8>, PaykitFfiError> {
    postcard::to_allocvec(&BackupStateEnvelope {
        version: SDK_BACKUP_BLOB_VERSION,
        backup: backup.clone(),
    })
    .map_err(|err| {
        storage_error(
            "encode_backup_blob",
            format!("encode SDK backup blob: {err}"),
        )
    })
}

pub(crate) fn decode_backup_state(bytes: &[u8]) -> Result<SdkBackupState, PaykitFfiError> {
    let envelope: BackupStateEnvelope = postcard::from_bytes(bytes).map_err(|err| {
        storage_error(
            "decode_backup_blob",
            format!("decode SDK backup blob: {err}"),
        )
    })?;
    if envelope.version != SDK_BACKUP_BLOB_VERSION {
        return Err(storage_error(
            "unsupported_backup_blob_version",
            format!(
                "unsupported SDK backup blob version {}, expected {}",
                envelope.version, SDK_BACKUP_BLOB_VERSION
            ),
        ));
    }
    Ok(envelope.backup)
}

/// Encode an SDK state blob snapshot for apps that store blob and revision together.
#[uniffi::export]
pub fn encode_sdk_state_blob_snapshot(
    snapshot: FfiSdkStateBlobSnapshot,
) -> Result<Vec<u8>, PaykitFfiError> {
    if snapshot.revision.is_empty() {
        return Err(invalid_state_blob_error());
    }
    postcard::to_allocvec(&StateBlobSnapshotEnvelope {
        version: SDK_STATE_BLOB_VERSION,
        revision: snapshot.revision,
        blob: snapshot.blob.export_bytes(),
    })
    .map_err(|err| {
        storage_error(
            "encode_state_snapshot_blob",
            format!("encode SDK state blob snapshot: {err}"),
        )
    })
}

/// Decode an SDK state blob snapshot previously encoded by Paykit FFI.
#[uniffi::export]
pub fn decode_sdk_state_blob_snapshot(
    bytes: Vec<u8>,
) -> Result<FfiSdkStateBlobSnapshot, PaykitFfiError> {
    let envelope: StateBlobSnapshotEnvelope = postcard::from_bytes(&bytes).map_err(|err| {
        storage_error(
            "decode_state_snapshot_blob",
            format!("decode SDK state blob snapshot: {err}"),
        )
    })?;
    if envelope.version != SDK_STATE_BLOB_VERSION {
        return Err(storage_error(
            "unsupported_state_snapshot_blob_version",
            format!(
                "unsupported SDK state snapshot blob version {}, expected {}",
                envelope.version, SDK_STATE_BLOB_VERSION
            ),
        ));
    }
    decode_state_blob_snapshot(&envelope.revision, &envelope.blob)?;
    Ok(FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(envelope.blob)),
        revision: envelope.revision,
    })
}
