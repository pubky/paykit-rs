use std::{
    any::Any,
    sync::{Arc, Mutex},
};

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
///
/// Each store instance is bound to one complete logical SDK state record.
/// Callbacks receive no identity or Paykit Receiver Path key, so apps must use
/// separately namespaced store instances when identities or configured Paykit
/// Receiver Paths need to retain distinct state. A store may be reused after
/// clearing the previous identity's data when intentionally switching the
/// logical state it represents. Share a store between handles only when they
/// intentionally operate on the same logical state.
///
/// Callbacks execute synchronously on the thread executing or polling the SDK
/// call. Each SDK handle owns a transaction mutex shared by that handle's
/// storage clones. The mutex serializes callbacks made inside SDK storage
/// transactions on that handle; it does not coordinate the revision-read API,
/// other SDK handles, or other processes.
///
/// Callbacks must return promptly. Use bounded local durable persistence and
/// bounded local coordination needed for atomic compare-and-swap. Do not use
/// network or iCloud round trips, unbounded cross-process waits, or re-enter or
/// depend on work from the same SDK handle.
///
/// A slow transactional callback blocks that foreign thread and queues other
/// storage transactions on the same SDK handle. The revision-read API invokes
/// the load callback outside the transaction mutex and can overlap transaction
/// callbacks. Other SDK handles can also invoke callbacks concurrently.
/// Implementations must be thread-safe. Store-level coordination may block
/// across handles or processes and must remain bounded.
///
/// Paykit does not encrypt state blobs; they contain sensitive private runtime
/// state and must not be logged. Store them in protected local storage with
/// backup exclusion unless the app encrypts them, and encrypt them before cloud
/// or cross-device transport. If the blob is lost, private runtime state cannot
/// be safely reconstructed from homeserver data alone.
#[uniffi::export(with_foreign)]
pub trait FfiSdkStateBlobStore: Send + Sync {
    /// Load the current SDK state blob, when one exists.
    ///
    /// The blob and revision must be a coherent snapshot of the same committed
    /// state.
    fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError>;

    /// Atomically replace the complete SDK state blob.
    ///
    /// `expected_revision` is `None` when no previous blob was loaded and
    /// matches only an absent stored blob. Otherwise it must match the current
    /// stored revision. Return a new opaque revision after a successful write.
    /// On conflict or failure, leave the stored data intact and return the
    /// binding's storage error variant; Paykit does not automatically retry.
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

/// Encode an SDK state blob snapshot for apps that store blob and revision together.
#[uniffi::export]
pub fn encode_sdk_state_blob_snapshot(
    snapshot: FfiSdkStateBlobSnapshot,
) -> Result<Vec<u8>, PaykitFfiError> {
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
    Ok(FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(envelope.blob)),
        revision: envelope.revision,
    })
}
