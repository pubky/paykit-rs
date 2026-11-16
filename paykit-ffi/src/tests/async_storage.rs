//! THROWAWAY (#411): tests for the browser-only async state blob store.
//!
//! They run natively with `--features web` (the trait and the storage exist
//! only under that feature); the browser e2e in paykit-web exercises the same
//! code through IndexedDB.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use paykit_sdk::storage::StorageAdapter;
use paykit_sdk::{IdentityState, PaykitSdkError, PubkyPublicKey};

use crate::storage::{FfiSdkAsyncStateBlobStore, FfiSdkAsyncStorage, FfiSdkStorageAdapter};
use crate::*;

#[derive(Default)]
struct MemoryAsyncStore {
    current: Mutex<Option<(String, Vec<u8>)>>,
    writes: Mutex<u32>,
    /// Revision to return from the next save instead of a fresh one.
    next_revision_override: Mutex<Option<String>>,
}

#[async_trait]
impl FfiSdkAsyncStateBlobStore for MemoryAsyncStore {
    async fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
        Ok(self
            .current
            .lock()
            .unwrap()
            .clone()
            .map(|(revision, bytes)| FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(bytes)),
                revision,
            }))
    }

    async fn save_state_blob_atomically(
        &self,
        blob: Arc<FfiSdkStateBlob>,
        expected_revision: Option<String>,
    ) -> Result<String, PaykitFfiError> {
        let mut current = self.current.lock().unwrap();
        if current.as_ref().map(|(revision, _)| revision.clone()) != expected_revision {
            return Err(PaykitFfiError::ConcurrentUpdate {
                code: "revision_changed".into(),
                context: "test store revision changed".into(),
            });
        }
        let mut writes = self.writes.lock().unwrap();
        *writes += 1;
        let revision = self
            .next_revision_override
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| format!("rev-{writes}"));
        *current = Some((revision.clone(), blob.export_bytes()));
        Ok(revision)
    }
}

fn identity() -> IdentityState {
    let key = pubky::Keypair::random().public_key();
    IdentityState {
        public_key: Some(PubkyPublicKey::from_public_key(&key)),
        initialized_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn test_async_storage_round_trips_and_reports_last_revision() {
    let store = Arc::new(MemoryAsyncStore::default());
    let storage = FfiSdkStorageAdapter::AsyncCallback(FfiSdkAsyncStorage::new(store.clone()));
    assert_eq!(storage.state_revision().unwrap(), None);

    let state = identity();
    storage.save_identity_state(state.clone()).await.unwrap();
    assert_eq!(storage.state_revision().unwrap().as_deref(), Some("rev-1"));

    // A second adapter over the same store sees the committed state.
    let reopened = FfiSdkStorageAdapter::AsyncCallback(FfiSdkAsyncStorage::new(store.clone()));
    assert_eq!(reopened.load_identity_state().await.unwrap(), Some(state));
    assert_eq!(reopened.state_revision().unwrap().as_deref(), Some("rev-1"));
    assert_eq!(*store.writes.lock().unwrap(), 1);
}

#[tokio::test]
async fn test_async_storage_surfaces_concurrent_update_from_stale_writer() {
    let store = Arc::new(MemoryAsyncStore::default());
    let first = FfiSdkAsyncStorage::new(store.clone());
    first.save_identity_state(identity()).await.unwrap();

    // Another writer commits a newer revision between `first`'s load and save.
    let result = first
        .transaction(|tx| {
            *store.current.lock().unwrap() = Some(("rev-other".into(), Vec::new()));
            tx.save_identity_state(identity());
            Ok(())
        })
        .await;
    match result {
        Err(PaykitSdkError::ConcurrentUpdate { .. }) => {}
        other => panic!("expected ConcurrentUpdate, got {other:?}"),
    }
}

#[tokio::test]
async fn test_async_storage_rejects_unchanged_revision_after_write() {
    let store = Arc::new(MemoryAsyncStore::default());
    let storage = FfiSdkAsyncStorage::new(store.clone());
    storage.save_identity_state(identity()).await.unwrap();

    // A store that reuses the previous revision would allow ABA overwrites.
    *store.next_revision_override.lock().unwrap() = Some("rev-1".into());
    let error = storage.save_identity_state(identity()).await.unwrap_err();
    assert!(
        matches!(error, PaykitSdkError::Storage { ref context, .. } if context.contains("unchanged or empty revision")),
        "{error:?}"
    );
}
