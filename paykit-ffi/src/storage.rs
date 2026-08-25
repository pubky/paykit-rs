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
use crate::{
    SDK_BACKUP_BLOB_MIN_READ_VERSION, SDK_BACKUP_BLOB_VERSION, SDK_STATE_BLOB_MIN_READ_VERSION,
    SDK_STATE_BLOB_VERSION,
};

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

        // Lazy generation upgrade: skipping the save when the state is
        // unchanged leaves an old-generation blob byte-for-byte intact, so
        // rollback to an older binary stays possible until the first real
        // state change re-stamps the blob at the current generation.
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

/// Leading version field shared by every FFI blob envelope.
///
/// Postcard is positional and non-self-describing, so a full-envelope decode
/// of an unknown future layout could succeed with garbage. Every envelope
/// leads with a `u32` version, which is decoded on its own first; the full
/// envelope body is decoded only for versions this build supports. Bodies are
/// identical across supported versions, so one body decode covers the range.
#[derive(Deserialize)]
struct EnvelopeVersionPrefix {
    version: u32,
}

fn envelope_version(bytes: &[u8]) -> Result<u32, postcard::Error> {
    postcard::take_from_bytes::<EnvelopeVersionPrefix>(bytes).map(|(prefix, _)| prefix.version)
}

pub(crate) fn encode_storage_state(state: &StorageState) -> Result<Vec<u8>, PaykitFfiError> {
    postcard::to_allocvec(&StorageStateEnvelope {
        version: SDK_STATE_BLOB_VERSION,
        state: state.clone(),
    })
    .map_err(|err| storage_error("encode_state_blob", format!("encode SDK state blob: {err}")))
}

// SECURITY: decode errors cross the FFI boundary as platform exception text,
// so they must name only version numbers and postcard error variants, never
// blob bytes or decoded content.
pub(crate) fn decode_storage_state(bytes: &[u8]) -> paykit_sdk::Result<StorageState> {
    let version = envelope_version(bytes).map_err(|err| PaykitSdkError::Storage {
        context: format!("decode SDK state blob: {err}"),
        source: None,
    })?;
    match version {
        SDK_STATE_BLOB_MIN_READ_VERSION..=SDK_STATE_BLOB_VERSION => {
            let envelope: StorageStateEnvelope =
                postcard::from_bytes(bytes).map_err(|err| PaykitSdkError::Storage {
                    context: format!("decode SDK state blob: {err}"),
                    source: None,
                })?;
            Ok(envelope.state)
        }
        other => Err(PaykitSdkError::Storage {
            context: format!(
                "unsupported SDK state blob version {other}, expected \
                 {SDK_STATE_BLOB_MIN_READ_VERSION} through {SDK_STATE_BLOB_VERSION}"
            ),
            source: None,
        }),
    }
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
    let version = envelope_version(bytes).map_err(|err| {
        storage_error(
            "decode_backup_blob",
            format!("decode SDK backup blob: {err}"),
        )
    })?;
    match version {
        SDK_BACKUP_BLOB_MIN_READ_VERSION..=SDK_BACKUP_BLOB_VERSION => {
            let envelope: BackupStateEnvelope = postcard::from_bytes(bytes).map_err(|err| {
                storage_error(
                    "decode_backup_blob",
                    format!("decode SDK backup blob: {err}"),
                )
            })?;
            Ok(envelope.backup)
        }
        other => Err(storage_error(
            "unsupported_backup_blob_version",
            format!(
                "unsupported SDK backup blob version {other}, expected \
                 {SDK_BACKUP_BLOB_MIN_READ_VERSION} through {SDK_BACKUP_BLOB_VERSION}"
            ),
        )),
    }
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
    let version = envelope_version(&bytes).map_err(|err| {
        storage_error(
            "decode_state_snapshot_blob",
            format!("decode SDK state blob snapshot: {err}"),
        )
    })?;
    match version {
        SDK_STATE_BLOB_MIN_READ_VERSION..=SDK_STATE_BLOB_VERSION => {
            let envelope: StateBlobSnapshotEnvelope =
                postcard::from_bytes(&bytes).map_err(|err| {
                    storage_error(
                        "decode_state_snapshot_blob",
                        format!("decode SDK state blob snapshot: {err}"),
                    )
                })?;
            Ok(FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(envelope.blob)),
                revision: envelope.revision,
            })
        }
        other => Err(storage_error(
            "unsupported_state_snapshot_blob_version",
            format!(
                "unsupported SDK state snapshot blob version {other}, expected \
                 {SDK_STATE_BLOB_MIN_READ_VERSION} through {SDK_STATE_BLOB_VERSION}"
            ),
        )),
    }
}
