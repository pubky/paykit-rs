//! The operating system's random generator, reached through the code paths production code
//! uses. A browser has no OS entropy source: on wasm32 these tests show that both getrandom
//! major versions in the graph reach `crypto.getRandomValues`.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use paykit_lib::identifiers::AttemptId;
use pubky_switch_client::store::{
    AcquireOutcome, CommitOutcome, KeyValue, MemoryStore, StateStore, StoreHandle, StoreOwner,
    WriteBatch,
};
use pubky_switch_client::StoreError;
use pubky_switch_core::keys::AppKeys;

conformance_test! {
    /// Attempt IDs and app keys come from pubky-switch-core's generator, which reads getrandom
    /// 0.2 (through `chacha20poly1305`'s `OsRng`): two fresh values differ (Paykit spec 7.2).
    fn test_randomness_switch_core_values_differ() {
        let (first, second) = (AttemptId::generate().unwrap(), AttemptId::generate().unwrap());
        assert_ne!(first, second);
        assert_ne!(first.to_bytes(), [0; 16]);

        let (first, second) = (AppKeys::generate().unwrap(), AppKeys::generate().unwrap());
        assert_ne!(first.app_id(), second.app_id());
        assert_ne!(first.bundle_public(), second.bundle_public());
    }
}

/// A host store that records the owner of each acquisition, the identity pubky-switch-client
/// draws from getrandom 0.3 when a namespace is acquired.
#[derive(Default)]
struct OwnerLog {
    store: MemoryStore,
    owners: Mutex<Vec<[u8; 32]>>,
}

impl StateStore for OwnerLog {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.store.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.store.scan(ns, prefix, after, limit)
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        self.store.commit(batch)
    }

    fn acquire(&self, ns: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        self.owners.lock().unwrap().push(*owner.as_bytes());
        self.store.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: StoreOwner,
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        self.store.commit_if(ns, owner, batch)
    }
}

/// The output of a future that completes on its first poll, as a call through an inline
/// [`StoreHandle`] does: it never waits on anything.
fn first_poll<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("an inline store call waited"),
    }
}

conformance_test! {
    /// Each namespace acquisition draws a fresh owner from pubky-switch-client's getrandom 0.3,
    /// and a held namespace refuses a second owner.
    fn test_randomness_store_owners_differ() {
        let log = Arc::new(OwnerLog::default());
        let handle = StoreHandle::inline(log.clone());
        let first = first_poll(handle.acquire("paykit-browser/first")).unwrap();
        let second = first_poll(handle.acquire("paykit-browser/second")).unwrap();
        assert!(matches!(
            first_poll(handle.acquire("paykit-browser/first")),
            Err(StoreError::InUse)
        ));
        drop((first, second));

        let owners = log.owners.lock().unwrap();
        assert_eq!(owners.len(), 3);
        assert_ne!(owners[0], owners[1]);
        assert_ne!(owners[1], owners[2]);
        assert_ne!(owners[0], owners[2]);
        assert!(owners.iter().all(|owner| owner != &[0; 32]));
    }
}
