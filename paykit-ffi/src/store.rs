//! Device-bound, durable storage supplied by the host.
use pubky_switch_client::store::{
    check_owned_batch, AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError, StoreLease,
    StoreOwner, Write, WriteBatch,
};
use std::sync::{Arc, Mutex};

/// One key and value from an ordered protected-store scan.
#[derive(uniffi::Record)]
pub struct StoreEntry {
    /// The original binary key in the requested namespace.
    pub key: Vec<u8>,
    /// The protected bytes stored at this key.
    pub value: Vec<u8>,
}

/// One write of a `WriteBatch`.
#[derive(uniffi::Enum)]
pub enum StoreWrite {
    /// Set `key` in `ns` to `value`.
    Put {
        /// The protected store namespace that owns this operation.
        namespace: String,
        /// The key.
        key: Vec<u8>,
        /// The value.
        value: Vec<u8>,
    },
    /// Remove `key` from `ns`, if present.
    Delete {
        /// The protected store namespace that owns this operation.
        namespace: String,
        /// The key.
        key: Vec<u8>,
    },
    /// Remove every key of `ns` that starts with `prefix`.
    DeletePrefix {
        /// The protected store namespace that owns this operation.
        namespace: String,
        /// The prefix; empty for the whole namespace.
        prefix: Vec<u8>,
    },
}

fn writes(batch: WriteBatch) -> Vec<StoreWrite> {
    batch
        .into_writes()
        .into_iter()
        .map(|write| match write {
            Write::Put { ns, key, value } => StoreWrite::Put {
                namespace: ns,
                key,
                value,
            },
            Write::Delete { ns, key } => StoreWrite::Delete { namespace: ns, key },
            Write::DeletePrefix { ns, prefix } => StoreWrite::DeletePrefix {
                namespace: ns,
                prefix,
            },
        })
        .collect()
}

/// Unknown means that a write may have committed. Every other error reports no write.
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
    /// The writes may or may not have been applied: the caller reads its state again before it
    /// goes on.
    #[error("the commit outcome is unknown")]
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
/// Calls can overlap and run on a blocking worker. Ordered scans exclude the previous key.
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
    /// Return false if a live writer already owns this namespace.
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

pub(crate) struct HostStore(pub Arc<dyn WalletStore>);

impl HostStore {
    fn commit_as(
        &self,
        namespace: &str,
        owner: &[u8; 32],
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        check_owned_batch(namespace, &batch)?;
        Ok(
            if self
                .0
                .commit_if(namespace.into(), owner.to_vec(), writes(batch))?
            {
                CommitOutcome::Applied
            } else {
                CommitOutcome::NotOwner
            },
        )
    }
}

impl StateStore for HostStore {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.0.get(ns.into(), key.into())?)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        Ok(self
            .0
            .scan(ns.into(), prefix.into(), after.map(Vec::from), limit)?
            .into_iter()
            .map(|entry| (entry.key, entry.value))
            .collect())
    }

    fn commit(&self, _: WriteBatch) -> Result<(), StoreError> {
        Err(StoreError::NotOwner)
    }

    fn acquire(&self, ns: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        Ok(
            match WalletStoreSession::acquire_for(self.0.clone(), ns.into(), *owner.as_bytes())? {
                Some(lease) => AcquireOutcome::Acquired(lease),
                None => AcquireOutcome::Busy,
            },
        )
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: StoreOwner,
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        self.commit_as(ns, owner.as_bytes(), batch)
    }
}

/// Keep the app's writer lease alive in every off-thread store call, including a
/// commit that still runs after its awaiting mobile operation was cancelled.
pub(crate) struct WalletStoreSession {
    pub(crate) ownership: Arc<LeaseOwner>,
}

// Only releases with a failed or unknown outcome stay here. The original store
// and token, including for an earlier host handle, are needed for an exact retry.
static FAILED_RELEASES: Mutex<Vec<Arc<LeaseOwner>>> = Mutex::new(Vec::new());

pub(crate) struct LeaseOwner {
    host: HostStore,
    namespace: String,
    token: [u8; 32],
    releasing: Mutex<()>,
}
impl LeaseOwner {
    fn release(self: &Arc<Self>) -> Result<(), StorageError> {
        let _busy = self.releasing.lock().unwrap_or_else(|e| e.into_inner());
        let result = self
            .host
            .0
            .release(self.namespace.clone(), self.token.to_vec());
        {
            let mut failed = FAILED_RELEASES.lock().unwrap_or_else(|e| e.into_inner());
            failed.retain(|previous| !Arc::ptr_eq(previous, self));
            if result.is_err() {
                failed.push(self.clone());
            }
        }
        result
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
}
impl StoreLease for WalletStoreSession {}

impl WalletStoreSession {
    fn acquire_for(
        store: Arc<dyn WalletStore>,
        namespace: String,
        owner: [u8; 32],
    ) -> Result<Option<Arc<Self>>, StoreError> {
        LeaseOwner::retry_failed(&namespace);
        if !store.acquire(namespace.clone(), owner.to_vec())? {
            return Ok(None);
        }
        Ok(Some(Arc::new(Self {
            ownership: Arc::new(LeaseOwner {
                host: HostStore(store),
                namespace,
                token: owner,
                releasing: Mutex::new(()),
            }),
        })))
    }
}
impl Drop for WalletStoreSession {
    fn drop(&mut self) {
        let _ = self.ownership.release();
    }
}
impl From<uniffi::UnexpectedUniFFICallbackError> for StorageError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Unknown
    }
}
