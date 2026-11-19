//! The Switch client's `StateStore` contract, as cases over any store.
//!
//! Each case takes a fresh, empty store, checks what the contract and the client's
//! `MemoryStore` show for one part of it, and returns the namespaces it wrote, so a caller with
//! a durable store can open it again and compare their contents ([`contents`]). The cases port
//! the client's own tests of `MemoryStore` and of its owned store handle, and call only the
//! trait, so they run unchanged on any store.

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use pubky_switch_client::store::{
    check_owned_batch, AcquireOutcome, BatchError, CommitOutcome, KeyValue, StateStore, StoreError,
    StoreHandle, StoreLease, StoreOwner, WriteBatch, MAX_BATCH_BYTES, MAX_VALUE_BYTES,
};

/// The namespaces a case wrote.
pub type Namespaces = &'static [&'static str];

/// The contents of `namespaces`: every entry of each, as `scan` returns them.
pub fn contents(store: &dyn StateStore, namespaces: &[&str]) -> Vec<(String, Vec<KeyValue>)> {
    namespaces
        .iter()
        .map(|ns| {
            (
                (*ns).to_owned(),
                store.scan(ns, b"", None, u32::MAX).unwrap(),
            )
        })
        .collect()
}

/// Check that the store holds nothing in `namespaces`: each case starts on a fresh store.
fn assert_fresh(store: &dyn StateStore, namespaces: &[&str]) {
    for (ns, entries) in contents(store, namespaces) {
        assert!(
            entries.is_empty(),
            "the store is not fresh: {ns:?} has entries"
        );
    }
}

/// A batch of one put.
pub fn put(ns: &str, key: &[u8], value: &[u8]) -> WriteBatch {
    let mut batch = WriteBatch::new();
    batch.put(ns, key, value).unwrap();
    batch
}

fn get(store: &dyn StateStore, ns: &str, key: &[u8]) -> Option<Vec<u8>> {
    store.get(ns, key).unwrap()
}

fn keys(entries: Vec<KeyValue>) -> Vec<Vec<u8>> {
    entries.into_iter().map(|(key, _)| key).collect()
}

/// Bytes that differ from one position to the next, so a value read back shifted, cut or
/// zeroed does not compare equal.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|at| (at % 251) as u8).collect()
}

/// The largest value, as a `usize`.
fn max_value_len() -> usize {
    usize::try_from(MAX_VALUE_BYTES).unwrap()
}

/// Poll a future that is ready at once, as an inline store handle's are.
fn ready<T>(future: impl Future<Output = T>) -> T {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the future was not ready"),
    }
}

/// Fresh owners. Only the Switch client's `StoreHandle::acquire` makes them, and it hands each
/// to the store it acquires from, so a store that keeps them, and grants nothing, lends them.
pub fn owners<const N: usize>() -> [StoreOwner; N] {
    #[derive(Default)]
    struct Keeper(Mutex<Vec<StoreOwner>>);
    impl StateStore for Keeper {
        fn get(&self, _: &str, _: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
            Err(StoreError::Unavailable)
        }
        fn scan(
            &self,
            _: &str,
            _: &[u8],
            _: Option<&[u8]>,
            _: u32,
        ) -> Result<Vec<KeyValue>, StoreError> {
            Err(StoreError::Unavailable)
        }
        fn commit(&self, _: WriteBatch) -> Result<(), StoreError> {
            Err(StoreError::Unavailable)
        }
        fn acquire(&self, _: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
            self.0.lock().unwrap().push(owner);
            Ok(AcquireOutcome::Busy)
        }
        fn commit_if(
            &self,
            _: &str,
            _: StoreOwner,
            _: WriteBatch,
        ) -> Result<CommitOutcome, StoreError> {
            Err(StoreError::Unavailable)
        }
    }
    let keeper = Arc::new(Keeper::default());
    let handle = StoreHandle::inline(keeper.clone());
    for _ in 0..N {
        assert!(matches!(
            ready(handle.acquire("ns")),
            Err(StoreError::InUse)
        ));
    }
    let owners = std::mem::take(&mut *keeper.0.lock().unwrap());
    owners.try_into().unwrap()
}

/// Acquire `ns` for `owner`, which must be granted.
pub fn acquire(store: &dyn StateStore, ns: &str, owner: StoreOwner) -> Arc<dyn StoreLease> {
    match store.acquire(ns, owner).unwrap() {
        AcquireOutcome::Acquired(lease) => lease,
        AcquireOutcome::Busy => panic!("{ns:?} was expected to be free"),
    }
}

/// Whether `owner` is refused `ns` because another owner holds it.
pub fn is_busy(store: &dyn StateStore, ns: &str, owner: StoreOwner) -> bool {
    matches!(store.acquire(ns, owner).unwrap(), AcquireOutcome::Busy)
}

/// A batch applies its writes in order: a put then a delete of the same key leaves it absent,
/// a delete then a put leaves it present, a second put overwrites the first, and a later
/// commit applies after an earlier one.
pub fn batch_order(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns", "other"];
    assert_fresh(store, NAMESPACES);
    let mut batch = WriteBatch::new();
    batch.put("ns", b"overwritten", b"1").unwrap();
    batch.put("ns", b"overwritten", b"2").unwrap();
    batch.put("ns", b"put-then-delete", b"3").unwrap();
    batch.delete("ns", b"put-then-delete").unwrap();
    batch.delete("ns", b"delete-then-put").unwrap();
    batch.put("ns", b"delete-then-put", b"4").unwrap();
    batch.put("ns", b"p/put-then-prefix", b"5").unwrap();
    batch.delete_prefix("ns", b"p/").unwrap();
    batch.put("ns", b"p/prefix-then-put", b"6").unwrap();
    store.commit(batch).unwrap();
    assert_eq!(get(store, "ns", b"overwritten"), Some(b"2".to_vec()));
    assert_eq!(get(store, "ns", b"put-then-delete"), None);
    assert_eq!(get(store, "ns", b"delete-then-put"), Some(b"4".to_vec()));
    assert_eq!(get(store, "ns", b"p/put-then-prefix"), None);
    assert_eq!(get(store, "ns", b"p/prefix-then-put"), Some(b"6".to_vec()));
    // Namespaces are apart: the same key in another namespace is absent.
    assert_eq!(get(store, "other", b"overwritten"), None);

    let mut batch = WriteBatch::new();
    batch.put("ns", b"overwritten", b"7").unwrap();
    batch.delete("ns", b"delete-then-put").unwrap();
    batch.put("other", b"overwritten", b"8").unwrap();
    store.commit(batch).unwrap();
    assert_eq!(get(store, "ns", b"overwritten"), Some(b"7".to_vec()));
    assert_eq!(get(store, "ns", b"delete-then-put"), None);
    assert_eq!(get(store, "other", b"overwritten"), Some(b"8".to_vec()));
    assert_eq!(
        keys(store.scan("ns", b"", None, u32::MAX).unwrap()),
        vec![b"overwritten".to_vec(), b"p/prefix-then-put".to_vec()]
    );
    NAMESPACES
}

/// `scan` returns the keys that start with the prefix, in ascending byte order, strictly after
/// `after`, at most `limit` of them, each with its value.
pub fn scan_prefix_after_limit(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns"];
    assert_fresh(store, NAMESPACES);
    // Written out of order, with bytes below and above every ASCII letter.
    let written: [&[u8]; 9] = [
        b"p/2", b"p/10", b"q/1", b"p", b"p/\xff", b"p/1", b"p/\x00", b"o", b"p0",
    ];
    let mut batch = WriteBatch::new();
    for key in written {
        batch.put("ns", key, [key, b"=v"].concat()).unwrap();
    }
    store.commit(batch).unwrap();

    let p_slash: Vec<Vec<u8>> = [&b"p/\x00"[..], b"p/1", b"p/10", b"p/2", b"p/\xff"]
        .map(<[u8]>::to_vec)
        .to_vec();
    let found = store.scan("ns", b"p/", None, 10).unwrap();
    assert_eq!(
        found,
        p_slash
            .iter()
            .map(|key| (key.clone(), [key.as_slice(), b"=v"].concat()))
            .collect::<Vec<_>>()
    );
    // `after` is exclusive, and need not be a stored key.
    assert_eq!(
        keys(store.scan("ns", b"p/", Some(b"p/1"), 2).unwrap()),
        p_slash[2..4].to_vec()
    );
    assert_eq!(
        keys(store.scan("ns", b"p/", Some(b"p/15"), 10).unwrap()),
        p_slash[3..].to_vec()
    );
    assert_eq!(
        keys(store.scan("ns", b"p/", Some(b"a"), 10).unwrap()),
        p_slash
    );
    assert!(store
        .scan("ns", b"p/", Some(b"p/\xff"), 10)
        .unwrap()
        .is_empty());
    // The limit.
    assert!(store.scan("ns", b"", None, 0).unwrap().is_empty());
    assert_eq!(
        keys(store.scan("ns", b"p/", None, 1).unwrap()),
        p_slash[..1].to_vec()
    );
    // A prefix that is itself a key, and a prefix shared by keys of different lengths.
    assert_eq!(
        keys(store.scan("ns", b"p/1", None, 10).unwrap()),
        vec![b"p/1".to_vec(), b"p/10".to_vec()]
    );
    assert_eq!(
        keys(store.scan("ns", b"p", None, 10).unwrap()),
        [vec![b"p".to_vec()], p_slash.clone(), vec![b"p0".to_vec()]].concat()
    );
    assert!(store.scan("ns", b"zz", None, 10).unwrap().is_empty());
    assert!(store.scan("none", b"", None, 10).unwrap().is_empty());

    // The empty prefix lists the namespace in ascending byte order, and paging with the last
    // key as `after` walks it once.
    let mut all: Vec<Vec<u8>> = written.map(<[u8]>::to_vec).to_vec();
    all.sort();
    assert_eq!(keys(store.scan("ns", b"", None, u32::MAX).unwrap()), all);
    let mut paged = Vec::new();
    loop {
        let page = keys(
            store
                .scan("ns", b"", paged.last().map(Vec::as_slice), 2)
                .unwrap(),
        );
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2);
        paged.extend(page);
    }
    assert_eq!(paged, all);
    NAMESPACES
}

/// `Delete` removes one key and `DeletePrefix` every key with the prefix, the empty prefix the
/// whole namespace, each in its own namespace only; deleting what is absent changes nothing.
pub fn delete_and_delete_prefix(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns", "other"];
    assert_fresh(store, NAMESPACES);
    let mut batch = WriteBatch::new();
    for key in [&b"p/1"[..], b"p/2", b"q", b"r"] {
        batch.put("ns", key, b"x").unwrap();
    }
    batch.put("other", b"p/1", b"x").unwrap();
    batch.put("other", b"q", b"x").unwrap();
    store.commit(batch).unwrap();

    let mut batch = WriteBatch::new();
    batch.delete("ns", b"q").unwrap();
    batch.delete("ns", b"missing").unwrap();
    batch.delete("absent", b"q").unwrap();
    store.commit(batch).unwrap();
    assert_eq!(
        keys(store.scan("ns", b"", None, 10).unwrap()),
        vec![b"p/1".to_vec(), b"p/2".to_vec(), b"r".to_vec()]
    );
    assert_eq!(get(store, "other", b"q"), Some(b"x".to_vec()));

    let mut batch = WriteBatch::new();
    batch.delete_prefix("ns", b"p/").unwrap();
    batch.delete_prefix("absent", b"").unwrap();
    store.commit(batch).unwrap();
    assert_eq!(
        keys(store.scan("ns", b"", None, 10).unwrap()),
        vec![b"r".to_vec()]
    );
    assert_eq!(
        keys(store.scan("other", b"", None, 10).unwrap()),
        vec![b"p/1".to_vec(), b"q".to_vec()]
    );

    let mut batch = WriteBatch::new();
    batch.delete_prefix("ns", b"").unwrap();
    store.commit(batch).unwrap();
    assert!(store.scan("ns", b"", None, 10).unwrap().is_empty());
    assert_eq!(get(store, "ns", b"r"), None);
    assert_eq!(store.scan("other", b"", None, 10).unwrap().len(), 2);
    NAMESPACES
}

/// What `MemoryStore` shows about empty things: a fresh store and an empty batch hold
/// nothing; an empty value and an empty key are stored like any other; the empty namespace
/// name is a namespace like any other; and a namespace whose last key is deleted reads as one
/// never written, and can be written again.
pub fn empty_namespaces_keys_and_values(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["", "ns"];
    assert_fresh(store, NAMESPACES);
    assert_eq!(get(store, "ns", b"k"), None);
    store.commit(WriteBatch::new()).unwrap();
    assert_fresh(store, NAMESPACES);

    let mut batch = WriteBatch::new();
    batch.put("ns", b"empty-value", b"").unwrap();
    batch.put("ns", b"", b"empty-key").unwrap();
    batch.put("", b"k", b"empty-namespace").unwrap();
    store.commit(batch).unwrap();
    // An empty value is present, unlike an absent key.
    assert_eq!(get(store, "ns", b"empty-value"), Some(Vec::new()));
    assert_eq!(get(store, "ns", b""), Some(b"empty-key".to_vec()));
    assert_eq!(
        keys(store.scan("ns", b"", None, 10).unwrap()),
        vec![Vec::new(), b"empty-value".to_vec()]
    );
    assert_eq!(get(store, "", b"k"), Some(b"empty-namespace".to_vec()));
    assert_eq!(get(store, "ns", b"k"), None);

    let mut batch = WriteBatch::new();
    batch.delete("ns", b"empty-value").unwrap();
    batch.delete("ns", b"").unwrap();
    store.commit(batch).unwrap();
    assert!(store.scan("ns", b"", None, 10).unwrap().is_empty());
    assert_eq!(get(store, "ns", b""), None);
    store.commit(put("ns", b"again", b"1")).unwrap();
    assert_eq!(get(store, "ns", b"again"), Some(b"1".to_vec()));
    NAMESPACES
}

/// A plain `commit` that touches an owned namespace is refused whole with `InUse`: none of
/// its writes is applied, in that namespace or any other. Namespaces nobody owns stay writable,
/// a second owner is `Busy`, and once the lease is dropped plain commits work again.
pub fn plain_commit_refused_on_owned_namespace(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["free", "owned"];
    assert_fresh(store, NAMESPACES);
    let [owner, second] = owners();
    let mut batch = WriteBatch::new();
    batch.put("free", b"k", b"before").unwrap();
    batch.put("owned", b"k", b"before").unwrap();
    store.commit(batch).unwrap();
    let lease = acquire(store, "owned", owner);

    // The free namespace's write comes first, and is refused with the rest.
    let mut batch = WriteBatch::new();
    batch.put("free", b"k", b"after").unwrap();
    batch.put("free", b"new", b"after").unwrap();
    batch.put("owned", b"k", b"after").unwrap();
    assert_eq!(store.commit(batch), Err(StoreError::InUse));
    let mut batch = WriteBatch::new();
    batch.delete_prefix("owned", b"").unwrap();
    assert_eq!(store.commit(batch), Err(StoreError::InUse));
    let mut batch = WriteBatch::new();
    batch.delete("owned", b"k").unwrap();
    assert_eq!(store.commit(batch), Err(StoreError::InUse));
    assert_eq!(get(store, "free", b"k"), Some(b"before".to_vec()));
    assert_eq!(get(store, "free", b"new"), None);
    assert_eq!(get(store, "owned", b"k"), Some(b"before".to_vec()));

    store.commit(put("free", b"k", b"after")).unwrap();
    assert_eq!(get(store, "free", b"k"), Some(b"after".to_vec()));
    assert_eq!(
        store.commit_if("owned", owner, put("owned", b"k", b"owner")),
        Ok(CommitOutcome::Applied)
    );
    assert!(is_busy(store, "owned", second));

    drop(lease);
    store.commit(put("owned", b"k", b"plain")).unwrap();
    assert_eq!(get(store, "owned", b"k"), Some(b"plain".to_vec()));
    NAMESPACES
}

/// `commit_if` applies nothing for an owner that does not hold the namespace: a wrong owner
/// while another holds it, and a stale owner after the namespace changed hands.
pub fn wrong_and_stale_owner_apply_nothing(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns"];
    assert_fresh(store, NAMESPACES);
    let [owner, wrong, next] = owners();
    let lease = acquire(store, "ns", owner);
    let mut batch = put("ns", b"key", b"bad");
    batch.put("ns", b"other", b"bad").unwrap();
    assert_eq!(
        store.commit_if("ns", wrong, batch.clone()),
        Ok(CommitOutcome::NotOwner)
    );
    assert_fresh(store, NAMESPACES);
    assert!(is_busy(store, "ns", wrong));
    assert_eq!(store.commit(batch.clone()), Err(StoreError::InUse));
    assert_eq!(
        store.commit_if("ns", owner, put("ns", b"key", b"first")),
        Ok(CommitOutcome::Applied)
    );

    drop(lease);
    let lease = acquire(store, "ns", next);
    assert_eq!(
        store.commit_if("ns", owner, batch),
        Ok(CommitOutcome::NotOwner)
    );
    assert_eq!(get(store, "ns", b"key"), Some(b"first".to_vec()));
    assert_eq!(get(store, "ns", b"other"), None);
    assert_eq!(
        store.commit_if("ns", next, put("ns", b"key", b"next")),
        Ok(CommitOutcome::Applied)
    );
    assert_eq!(get(store, "ns", b"key"), Some(b"next".to_vec()));
    drop(lease);
    // A namespace nobody owns has no owner to match.
    assert_eq!(
        store.commit_if("ns", next, put("ns", b"key", b"late")),
        Ok(CommitOutcome::NotOwner)
    );
    assert_eq!(get(store, "ns", b"key"), Some(b"next".to_vec()));
    NAMESPACES
}

/// Ownership lasts until the last holder of the lease drops it, and only for its namespace.
///
/// A stale lease, dropped while a newer owner holds its namespace, cannot be reached through
/// one store's calls: a newer owner acquires only after the last holder of the older lease
/// dropped it. In the browser, a lease also keeps its store's files open, so the store cannot
/// be opened again, and get a newer owner, while the lease lives (`opening.rs`).
pub fn lease_drop_releases(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["first", "second"];
    assert_fresh(store, NAMESPACES);
    let [owner, other] = owners();
    let lease = acquire(store, "first", owner);
    let holder = Arc::clone(&lease);
    drop(lease);
    // One holder is left.
    assert!(is_busy(store, "first", other));
    assert_eq!(
        store.commit_if("first", owner, put("first", b"k", b"1")),
        Ok(CommitOutcome::Applied)
    );
    drop(holder);
    let lease = acquire(store, "first", other);
    assert_eq!(
        store.commit_if("first", owner, put("first", b"k", b"stale")),
        Ok(CommitOutcome::NotOwner)
    );
    drop(lease);

    // One owner on two namespaces: dropping one lease leaves the other namespace owned.
    let first = acquire(store, "first", owner);
    let second = acquire(store, "second", owner);
    drop(first);
    assert!(is_busy(store, "second", other));
    assert_eq!(
        store.commit(put("second", b"k", b"raw")),
        Err(StoreError::InUse)
    );
    assert_eq!(
        store.commit_if("second", owner, put("second", b"k", b"2")),
        Ok(CommitOutcome::Applied)
    );
    store.commit(put("first", b"k", b"raw")).unwrap();
    drop(second);
    store.commit(put("second", b"k", b"raw")).unwrap();
    assert_eq!(get(store, "first", b"k"), Some(b"raw".to_vec()));
    assert_eq!(get(store, "second", b"k"), Some(b"raw".to_vec()));
    NAMESPACES
}

/// A value of `MAX_VALUE_BYTES` (16 MiB) and a batch of `MAX_BATCH_BYTES` (32 MiB) are
/// accepted and read back; `WriteBatch` refuses one byte more of either before any store sees
/// it, as `TooLarge` once it is a store error.
pub fn value_and_batch_size_caps(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns", "n"];
    assert_fresh(store, NAMESPACES);
    let len = max_value_len();
    let value = pattern(len);
    store.commit(put("ns", b"largest", &value)).unwrap();
    let found = get(store, "ns", b"largest").unwrap();
    assert!(
        found == value,
        "the 16 MiB value read back differs ({} bytes)",
        found.len()
    );
    let mut batch = WriteBatch::new();
    batch.delete("ns", b"largest").unwrap();
    store.commit(batch).unwrap();

    let mut batch = WriteBatch::new();
    let error = batch.put("ns", b"over", vec![0; len + 1]).unwrap_err();
    let over = MAX_VALUE_BYTES + 1;
    assert_eq!(
        error,
        BatchError::ValueTooLarge {
            size: over,
            cap: MAX_VALUE_BYTES
        }
    );
    assert_eq!(
        StoreError::from(error),
        StoreError::TooLarge {
            size: over,
            cap: MAX_VALUE_BYTES
        }
    );
    assert!(batch.is_empty());

    // Two values whose writes, with namespace and key, come to the batch cap exactly.
    let mut full = WriteBatch::new();
    full.put("n", b"1", &value[..len - 2]).unwrap();
    full.put("n", b"2", vec![7; len - 2]).unwrap();
    assert_eq!(full.size(), MAX_BATCH_BYTES);
    let error = full.delete("n", b"3").unwrap_err();
    assert_eq!(
        StoreError::from(error),
        StoreError::TooLarge {
            size: MAX_BATCH_BYTES + 2,
            cap: MAX_BATCH_BYTES
        }
    );
    assert_eq!(full.writes().len(), 2);
    store.commit(full).unwrap();
    let first = get(store, "n", b"1").unwrap();
    assert!(first == value[..len - 2], "value 1 read back differs");
    let second = get(store, "n", b"2").unwrap();
    assert!(second == vec![7; len - 2], "value 2 read back differs");
    assert_eq!(get(store, "ns", b"over"), None);
    NAMESPACES
}

/// `commit_if` checks `check_owned_batch` first: its condition counts the namespace and the
/// owner's 32 bytes, so a batch that fits on its own can be refused with `TooLarge`, and a
/// write in another namespace is refused; both apply nothing.
pub fn owned_batch_size_cap(store: &dyn StateStore) -> Namespaces {
    const NAMESPACES: Namespaces = &["n", "other"];
    assert_fresh(store, NAMESPACES);
    let [owner] = owners();
    let lease = acquire(store, "n", owner);
    let half = max_value_len();
    let value = pattern(half);
    // The condition adds 33 bytes: one of namespace and the owner's 32.
    let mut exact = WriteBatch::new();
    exact.put("n", b"a", &value[..]).unwrap();
    exact.put("n", b"b", vec![0; half - 4 - 33]).unwrap();
    assert_eq!(exact.size() + 33, MAX_BATCH_BYTES);
    assert_eq!(check_owned_batch("n", &exact), Ok(()));
    assert_eq!(
        store.commit_if("n", owner, exact),
        Ok(CommitOutcome::Applied)
    );

    // The same size with other contents, and one more write: the batch alone still fits,
    // only its condition takes it over the cap.
    let mut over = WriteBatch::new();
    over.put("n", b"a", vec![1; half]).unwrap();
    over.put("n", b"c", vec![0; half - 4 - 33]).unwrap();
    over.delete("n", b"").unwrap();
    assert!(over.size() <= MAX_BATCH_BYTES);
    let too_large = StoreError::TooLarge {
        size: MAX_BATCH_BYTES + 1,
        cap: MAX_BATCH_BYTES,
    };
    assert_eq!(check_owned_batch("n", &over), Err(too_large.clone()));
    assert_eq!(store.commit_if("n", owner, over), Err(too_large));
    let a = get(store, "n", b"a").unwrap();
    assert!(a == value, "the refused batch changed key a");
    assert_eq!(get(store, "n", b"c"), None);

    // A write outside the owned namespace refuses the whole batch.
    let mut mixed = put("n", b"d", b"bad");
    mixed.put("other", b"key", b"bad").unwrap();
    assert!(matches!(
        store.commit_if("n", owner, mixed),
        Err(StoreError::Other(_))
    ));
    assert!(matches!(
        store.commit_if("n", owner, put("other", b"key", b"bad")),
        Err(StoreError::Other(_))
    ));
    assert_eq!(get(store, "n", b"d"), None);
    assert_eq!(get(store, "other", b"key"), None);

    assert_eq!(
        store.commit_if("n", owner, WriteBatch::new()),
        Ok(CommitOutcome::Applied)
    );
    // Replace the large values, so the state left behind is small.
    let mut batch = WriteBatch::new();
    batch.delete_prefix("n", b"").unwrap();
    batch.put("n", b"a", b"small").unwrap();
    assert_eq!(
        store.commit_if("n", owner, batch),
        Ok(CommitOutcome::Applied)
    );
    drop(lease);
    NAMESPACES
}

/// The Switch client's owned store handle on this store: a handle that acquired a namespace
/// keeps it while it or a capability from it lives, even after a raw owner was refused; its
/// commits and reads go through the store.
///
/// It takes the store shared, as the client's `StoreHandle` holds it.
pub fn owned_handle_keeps_exclusion(store: Arc<dyn StateStore>) -> Namespaces {
    const NAMESPACES: Namespaces = &["ns"];
    assert_fresh(store.as_ref(), NAMESPACES);
    let [raw] = owners();
    let handle = StoreHandle::inline(Arc::clone(&store));
    let mut owner = ready(handle.acquire("ns")).unwrap();
    assert!(is_busy(store.as_ref(), "ns", raw));
    assert!(matches!(
        ready(handle.acquire("ns")),
        Err(StoreError::InUse)
    ));
    assert_eq!(
        store.commit_if("ns", raw, put("ns", b"key", b"bad")),
        Ok(CommitOutcome::NotOwner)
    );
    ready(owner.commit(put("ns", b"key", b"owned"))).unwrap();
    assert_eq!(ready(owner.get(b"key")).unwrap(), Some(b"owned".to_vec()));
    assert_eq!(get(store.as_ref(), "ns", b"key"), Some(b"owned".to_vec()));
    // The handle refuses a batch for another namespace before the store sees it.
    assert!(ready(owner.commit(put("other", b"key", b"bad"))).is_err());

    // A capability keeps the namespace after the handle is gone.
    let capability = owner.ownership().unwrap();
    drop(owner);
    assert!(matches!(
        ready(handle.acquire("ns")),
        Err(StoreError::InUse)
    ));
    assert_eq!(
        store.commit(put("ns", b"key", b"raw")),
        Err(StoreError::InUse)
    );
    drop(capability);
    let mut next = ready(handle.acquire("ns")).unwrap();
    ready(next.commit(put("ns", b"key", b"next"))).unwrap();
    assert_eq!(
        ready(next.scan(b"", None, 10)).unwrap(),
        vec![(b"key".to_vec(), b"next".to_vec())]
    );
    NAMESPACES
}

/// Of several threads that acquire one namespace at once, exactly one is granted it.
///
/// Native only: a browser runs this WebAssembly on one thread, and an `OpfsStore` cannot be
/// shared with another worker. There the exclusion that matters is between workers, which the
/// browser's file locks give; the browser tests check it from a nested worker (`opening.rs`).
#[cfg(not(target_arch = "wasm32"))]
pub fn concurrent_acquisition_has_one_owner(store: &dyn StateStore) {
    const THREADS: usize = 8;
    let owners: [StoreOwner; THREADS] = owners();
    let start = std::sync::Barrier::new(THREADS);
    let acquired = std::sync::Barrier::new(THREADS);
    let winners: usize = std::thread::scope(|scope| {
        let threads: Vec<_> = owners
            .into_iter()
            .map(|owner| {
                let (start, acquired) = (&start, &acquired);
                scope.spawn(move || {
                    start.wait();
                    let outcome = store.acquire("ns", owner).unwrap();
                    // Every attempt finishes while the granted lease is still held.
                    acquired.wait();
                    usize::from(matches!(outcome, AcquireOutcome::Acquired(_)))
                })
            })
            .collect();
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .sum()
    });
    assert_eq!(winners, 1);
}
