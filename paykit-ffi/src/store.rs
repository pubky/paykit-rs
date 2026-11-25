//! Device-bound, durable storage supplied by the host.
use pubky_dispatch_client::store::{
    check_owned_batch, AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError, StoreLease,
    StoreOwner, Write, WriteBatch,
};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

/// One key and value from an ordered protected-store scan.
#[derive(uniffi::Record)]
pub struct StoreEntry {
    /// The original binary key in the requested namespace.
    pub key: Vec<u8>,
    /// The protected bytes stored at this key.
    pub value: Vec<u8>,
}

/// A write to the physical namespace covered by a conditional host transaction.
#[derive(uniffi::Enum)]
pub enum StoreWrite {
    /// Set this key to its protected bytes.
    Put {
        /// The binary key in this namespace.
        key: Vec<u8>,
        /// The protected bytes.
        value: Vec<u8>,
    },
    /// Remove this exact key when present.
    Delete {
        /// The binary key in the owned namespace.
        key: Vec<u8>,
    },
    /// Remove all keys with this prefix in the owned namespace.
    DeletePrefix {
        /// The prefix, empty for the whole namespace.
        prefix: Vec<u8>,
    },
}

/// Unknown means a write or acquisition may have applied. Other errors report no change.
#[derive(Debug, Clone, thiserror::Error, uniffi::Error)]
pub enum StorageError {
    /// Another owner holds a namespace; no writes were applied.
    #[error("the namespace is busy")]
    InUse,
    /// The conditional writer no longer owns its namespace; no writes were applied.
    #[error("the writer no longer owns the namespace")]
    NotOwner,
    /// Nothing was applied: the call may be retried.
    #[error("storage is unavailable")]
    Unavailable,
    /// The acquisition, release or commit may have applied. Reopen using the original
    /// protected store to determine the durable result.
    #[error("the storage outcome is unknown")]
    Unknown,
    /// The store is out of space; nothing was applied.
    #[error("storage is full")]
    Full,
    /// A stored value failed its own checks.
    #[error("stored data is corrupt")]
    Corrupt,
    /// A write batch, or one value in it, over the working sizes (`store::MAX_VALUE_BYTES`,
    /// `store::MAX_BATCH_BYTES`), refused before the host sees it.
    #[error("the batch exceeds the size limit")]
    TooLarge {
        /// The size in bytes of the value, or of the batch with the refused write.
        size: u64,
        /// The cap.
        cap: u64,
    },
    /// Any other failure, described by the host.
    #[error("other storage failure")]
    Other,
}

impl From<StorageError> for StoreError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Other => Self::Other("host storage failure".into()),
            StorageError::InUse => Self::InUse,
            StorageError::NotOwner => Self::NotOwner,
            StorageError::Unavailable => Self::Unavailable,
            StorageError::Unknown => Self::Unknown,
            StorageError::Full => Self::Full,
            StorageError::Corrupt => Self::Corrupt,
            StorageError::TooLarge { size, cap } => Self::TooLarge { size, cap },
        }
    }
}

/// Implement against the same encrypted, backup-excluded physical store on every handle.
/// Calls can overlap and run on a blocking worker.
/// A batch commits all writes atomically and durably. Every write requires its live owner.
/// Acquisition excludes other processes until release or process death. A conditional
/// commit compares the owner and applies all writes in the same transaction.
#[uniffi::export(with_foreign)]
pub trait WalletStore: Send + Sync {
    /// Read the exact key, returning null only when it is absent.
    fn get(&self, namespace: String, key: Vec<u8>) -> Result<Option<Vec<u8>>, StorageError>;
    /// Read at most the requested number of matching keys in ascending byte order.
    fn scan(
        &self,
        namespace: String,
        prefix: Vec<u8>,
        after: Option<Vec<u8>>,
        limit: u32,
    ) -> Result<Vec<StoreEntry>, StorageError>;
    /// Return false if a live writer already owns this namespace. If the acquisition
    /// might have applied before its answer was lost, return Unknown.
    fn acquire(&self, namespace: String, owner: Vec<u8>) -> Result<bool, StorageError>;
    /// Release this exact owner idempotently. A repeat after an unknown result must
    /// leave a newer owner intact. Process exit releases the physical writer lock.
    fn release(&self, namespace: String, owner: Vec<u8>) -> Result<(), StorageError>;
    /// Return false if this exact owner no longer holds the namespace.
    fn commit_if(
        &self,
        namespace: String,
        owner: Vec<u8>,
        writes: Vec<StoreWrite>,
    ) -> Result<bool, StorageError>;
}

/// A blocking enrollment call holds this session until its physical operation returns.
pub(crate) struct WalletStoreSession {
    runtime: tokio::runtime::Handle,
    pub(crate) ownership: Arc<LeaseOwner>,
}

// Only releases with a failed or unknown outcome stay here. The original store
// and token, including for an earlier host handle, are needed for an exact retry.
static FAILED_RELEASES: Mutex<Vec<Arc<LeaseOwner>>> = Mutex::new(Vec::new());

pub(crate) struct LeaseOwner {
    host: Arc<dyn WalletStore>,
    namespace: String,
    token: [u8; 32],
    result: watch::Sender<Option<Result<(), StorageError>>>,
    releasing: Mutex<()>,
}
impl LeaseOwner {
    fn release(self: &Arc<Self>) -> Result<(), StorageError> {
        let _busy = self.releasing.lock().unwrap_or_else(|e| e.into_inner());
        if self.result.borrow().as_ref().is_some_and(Result::is_ok) {
            return Ok(());
        }
        let result = crate::runtime::in_callback(|| {
            self.host
                .release(self.namespace.clone(), self.token.to_vec())
        });
        self.record(result.clone());
        result
    }

    fn record(self: &Arc<Self>, result: Result<(), StorageError>) {
        let mut failed = FAILED_RELEASES.lock().unwrap_or_else(|e| e.into_inner());
        failed.retain(|previous| !Arc::ptr_eq(previous, self));
        if result.is_err() {
            failed.push(self.clone());
        }
        self.result.send_replace(Some(result));
    }

    fn retry_failed(namespace: &str) {
        let failed: Vec<_> = FAILED_RELEASES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|owner| owner.namespace == namespace)
            .cloned()
            .collect();
        for owner in failed {
            let _ = owner.release();
        }
    }
    pub(crate) async fn first_release(&self) -> Result<(), StorageError> {
        let mut released = self.result.subscribe();
        let result = released
            .wait_for(Option::is_some)
            .await
            .map_err(|_| StorageError::Unknown)?
            .as_ref()
            .cloned()
            .ok_or(StorageError::Unknown)?;
        result
    }

    pub(crate) async fn finish(self: &Arc<Self>) -> Result<(), crate::WalletError> {
        if self.first_release().await.is_err() {
            let owner = self.clone();
            tokio::task::spawn_blocking(move || owner.release())
                .await
                .map_err(|_| StoreError::Unknown)?
                .map_err(StoreError::from)?;
        }
        Ok(())
    }
}
impl WalletStoreSession {
    pub(crate) async fn open(
        host: Arc<dyn WalletStore>,
        namespace: String,
    ) -> Result<Arc<Self>, StoreError> {
        let runtime = tokio::runtime::Handle::current();
        let token =
            pubky_dispatch_core::enc::random_bytes().map_err(|_| StoreError::Unavailable)?;
        tokio::task::spawn_blocking(move || {
            LeaseOwner::retry_failed(&namespace);
            let owner = Arc::new(LeaseOwner {
                host,
                namespace,
                token,
                result: watch::channel(None).0,
                releasing: Mutex::new(()),
            });
            match crate::runtime::in_callback(|| {
                owner
                    .host
                    .acquire(owner.namespace.clone(), owner.token.to_vec())
            }) {
                Ok(true) => Ok(Arc::new(Self {
                    runtime,
                    ownership: owner,
                })),
                Ok(false) => Err(StoreError::InUse),
                Err(StorageError::Unknown) => {
                    // The physical writer may already belong to this token. An idempotent
                    // release leaves any other writer intact and retains this owner for retry
                    // if release itself cannot be confirmed.
                    let _ = owner.release();
                    Err(StoreError::Unknown)
                }
                Err(failure) => Err(failure.into()),
            }
        })
        .await
        .map_err(|_| StoreError::Unknown)?
    }

    pub(crate) async fn get(self: &Arc<Self>, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let held = self.clone();
        let key = key.to_vec();
        tokio::task::spawn_blocking(move || {
            crate::runtime::in_callback(|| {
                held.ownership
                    .host
                    .get(held.ownership.namespace.clone(), key)
            })
        })
        .await
        .map_err(|_| StoreError::Unknown)?
        .map_err(Into::into)
    }

    pub(crate) async fn put(
        self: &Arc<Self>,
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<(), StoreError> {
        let owner = &self.ownership;
        let mut batch = WriteBatch::new();
        batch
            .put(&owner.namespace, key.to_vec(), value)
            .map_err(StoreError::from)?;
        let held = self.clone();
        tokio::task::spawn_blocking(move || held.commit(batch))
            .await
            .map_err(|_| StoreError::Unknown)?
    }
}
impl StoreLease for WalletStoreSession {}
impl WalletStoreSession {
    fn owns(&self, namespace: &str) -> Result<(), StoreError> {
        if self.ownership.namespace == namespace {
            Ok(())
        } else {
            Err(StoreError::NotOwner)
        }
    }
}
impl StateStore for WalletStoreSession {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.owns(ns)?;
        Ok(crate::runtime::in_callback(|| {
            self.ownership.host.get(ns.into(), key.into())
        })?)
    }
    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.owns(ns)?;
        Ok(crate::runtime::in_callback(|| {
            self.ownership
                .host
                .scan(ns.into(), prefix.into(), after.map(Vec::from), limit)
        })?
        .into_iter()
        .map(|e| (e.key, e.value))
        .collect())
    }
    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        let owner = &self.ownership;
        check_owned_batch(&owner.namespace, &batch)?;
        let writes = batch
            .into_writes()
            .into_iter()
            .map(|w| match w {
                Write::Put { key, value, .. } => StoreWrite::Put { key, value },
                Write::Delete { key, .. } => StoreWrite::Delete { key },
                Write::DeletePrefix { prefix, .. } => StoreWrite::DeletePrefix { prefix },
            })
            .collect();
        if crate::runtime::in_callback(|| {
            owner
                .host
                .commit_if(owner.namespace.clone(), owner.token.to_vec(), writes)
        })? {
            Ok(())
        } else {
            Err(StoreError::NotOwner)
        }
    }
    fn acquire(&self, _: &str, _: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        Ok(AcquireOutcome::Busy)
    }
    fn commit_if(
        &self,
        _: &str,
        _: StoreOwner,
        _: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        Ok(CommitOutcome::NotOwner)
    }
}
// A foreign release can wait for device I/O. Keep the exact physical fence until
// the last in-flight store operation is gone, then release on the blocking pool.
struct FinalRelease(Option<Arc<LeaseOwner>>);

impl FinalRelease {
    fn run(mut self) {
        if let Some(owner) = &self.0 {
            let _ = owner.release();
        }
        self.0 = None;
    }
}

impl Drop for FinalRelease {
    fn drop(&mut self) {
        // A runtime that shuts down before starting its job cannot confirm release.
        // The next open or shutdown can retry the same token on its own live runtime.
        if let Some(owner) = &self.0 {
            owner.record(Err(StorageError::Unknown));
        }
    }
}

impl Drop for WalletStoreSession {
    fn drop(&mut self) {
        let release = FinalRelease(Some(self.ownership.clone()));
        self.runtime.spawn_blocking(move || release.run());
    }
}
impl From<uniffi::UnexpectedUniFFICallbackError> for StorageError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Unknown
    }
}
