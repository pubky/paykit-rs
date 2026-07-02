use std::{any::Any, sync::Arc};

use async_trait::async_trait;
use paykit_sdk::storage::{
    run_storage_state_transaction, StorageAdapter, StorageState, StorageTransactionCallback,
};
use paykit_sdk::{PaykitSdkError, SdkBackupState};
use serde::{Deserialize, Serialize};

use crate::errors::{ffi_error_to_sdk, storage_error, PaykitFfiError};
use crate::secrets::FfiSdkStateBlob;
use crate::{SDK_BACKUP_BLOB_VERSION, SDK_STATE_BLOB_VERSION};

/// Current SDK state blob with its platform storage revision.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiSdkStateBlobSnapshot {
    /// Encoded SDK state.
    pub blob: Arc<FfiSdkStateBlob>,
    /// Opaque platform storage revision for optimistic writes.
    pub revision: String,
}

/// Platform-owned durable blob store for SDK state.
#[uniffi::export(with_foreign)]
pub trait FfiSdkStateBlobStore: Send + Sync {
    /// Load the current SDK state blob, when one exists.
    fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError>;

    /// Atomically save a new SDK state blob.
    ///
    /// `expected_revision` is `None` when no previous blob was loaded. The
    /// platform store should reject the write if the stored revision changed.
    fn save_state_blob_atomically(
        &self,
        blob: Arc<FfiSdkStateBlob>,
        expected_revision: Option<String>,
    ) -> Result<String, PaykitFfiError>;
}

#[derive(Clone)]
pub(crate) struct FfiSdkStorage {
    pub(crate) store: Arc<dyn FfiSdkStateBlobStore>,
}

#[async_trait]
impl StorageAdapter for FfiSdkStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        let snapshot = self
            .store
            .load_state_blob()
            .map_err(|err| ffi_error_to_sdk(err, "load SDK state blob"))?;
        let expected_revision = snapshot.as_ref().map(|snapshot| snapshot.revision.clone());
        let initial_state = snapshot
            .map(|snapshot| decode_storage_state(&snapshot.blob.export_bytes()))
            .transpose()?
            .unwrap_or_default();

        let (updated_state, result) = run_storage_state_transaction(initial_state.clone(), f)?;

        if updated_state != initial_state {
            let blob = Arc::new(FfiSdkStateBlob::new(
                encode_storage_state(&updated_state)
                    .map_err(|err| ffi_error_to_sdk(err, "encode SDK state blob"))?,
            ));
            self.store
                .save_state_blob_atomically(blob, expected_revision)
                .map_err(|err| ffi_error_to_sdk(err, "save SDK state blob"))?;
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

pub(crate) fn encode_storage_state(state: &StorageState) -> Result<Vec<u8>, PaykitFfiError> {
    postcard::to_allocvec(&StorageStateEnvelope {
        version: SDK_STATE_BLOB_VERSION,
        state: state.clone(),
    })
    .map_err(|err| storage_error("encode_state_blob", format!("encode SDK state blob: {err}")))
}

pub(crate) fn decode_storage_state(bytes: &[u8]) -> paykit_sdk::Result<StorageState> {
    let envelope: StorageStateEnvelope =
        postcard::from_bytes(bytes).map_err(|err| PaykitSdkError::Storage {
            context: format!("decode SDK state blob: {err}"),
            source: None,
        })?;
    if envelope.version != SDK_STATE_BLOB_VERSION {
        return Err(PaykitSdkError::Storage {
            context: format!(
                "unsupported SDK state blob version {}, expected {}",
                envelope.version, SDK_STATE_BLOB_VERSION
            ),
            source: None,
        });
    }
    Ok(envelope.state)
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
