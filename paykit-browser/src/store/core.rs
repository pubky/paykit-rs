//! The store's logic over two slot files, generic over [`SlotFile`] so the unit tests can run
//! it on in-memory files: inspecting the files without a key, opening, the five `StateStore`
//! calls, ownership and leases.
//!
//! Every slot is sealed by the store's [`SnapshotSealer`] (`seal.rs`, `format.rs`). The sealer,
//! and with it the data key, is in memory for the store's life; how the key is kept between
//! sessions is the browser side's.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Arc, Mutex, MutexGuard};

use pubky_switch_client::store::{
    check_owned_batch, AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError, StoreLease,
    StoreOwner, Write, WriteBatch,
};

use super::file::{FileError, SlotFile};
use super::format::{self, Entries, Inspection, Slot, SlotHeader, SlotId, HEADER_LEN};
use super::seal::{self, SnapshotSealer};
use super::OpenError;

/// A store on two slot files, shared with its leases.
pub(crate) struct SlotStore<F> {
    core: Arc<Mutex<Core<F>>>,
}

struct Core<F> {
    files: [F; 2],
    /// Seals every slot this store writes and opens every slot it reads.
    sealer: SnapshotSealer,
    /// The slot that holds `entries`, or `None` before the first commit.
    current: Option<SlotId>,
    /// The generation of `entries`; 0 before the first commit.
    generation: u64,
    entries: Entries,
    /// The namespaces' owners. They live in memory only: the browser's exclusive file handles
    /// already keep every other worker and tab out of the whole store, so ownership only has
    /// to exclude other callers of this one instance, as in the Switch client's `MemoryStore`.
    owners: BTreeMap<String, StoreOwner>,
    /// Set once memory and disk may disagree: after a commit whose outcome is unknown, or a
    /// panic while the lock was held. The next call first flushes both files and reads the
    /// state again from disk, as opening does, and clears it once both succeed.
    broken: bool,
}

/// Lock the core. A panic while it was held may have come between a slot's write and its
/// flush, so a poisoned lock marks the store broken. The poison is cleared, since the mark now
/// carries it: the next call reads the state from disk once, not on every later call.
fn lock<F>(core: &Mutex<Core<F>>) -> MutexGuard<'_, Core<F>> {
    core.lock().unwrap_or_else(|poisoned| {
        let mut guard = poisoned.into_inner();
        guard.broken = true;
        core.clear_poison();
        guard
    })
}

impl<F: SlotFile> SlotStore<F> {
    /// What the two files' headers show, read without a key: both empty, a slot of the earlier
    /// unsealed format, or the headers of the slots present, with their format versions and
    /// store identifiers. It reads at most a header of each file and writes nothing, so the
    /// browser side can decide how to find the key before it opens the store.
    pub(crate) fn inspect(files: &mut [F; 2]) -> Result<Inspection, OpenError> {
        let [a, b] = read_prefixes(files)?;
        Ok(Inspection::of([a.header(), b.header()]))
    }

    /// Read both slots, open them with `sealer`, and open the store at the current one.
    /// Opening writes nothing.
    ///
    /// # Errors
    ///
    /// [`OpenError::UnsealedFormat`] when a file starts as a slot of the earlier unsealed
    /// format, decided on its header before anything is decrypted;
    /// [`OpenError::NewerFormat`] for a slot of a newer format; [`OpenError::KeyLost`] when a
    /// complete header of this format names another store than `sealer`'s;
    /// [`OpenError::Corrupt`] when the slots hold no state to trust (a slot that does not open
    /// under the key counts as damaged); [`OpenError::Unavailable`] when a file call fails.
    pub(crate) fn open(mut files: [F; 2], sealer: SnapshotSealer) -> Result<Self, OpenError> {
        let current = read_current(&mut files, &sealer)?;
        Ok(Self {
            core: Arc::new(Mutex::new(Core {
                files,
                sealer,
                current: current.slot,
                generation: current.generation,
                entries: current.entries,
                owners: BTreeMap::new(),
                broken: false,
            })),
        })
    }

    /// Lock the core of a usable store. A broken store first reads its state again from disk;
    /// while that fails, every call fails with it.
    fn lock(&self) -> Result<MutexGuard<'_, Core<F>>, StoreError> {
        let mut core = lock(&self.core);
        if core.broken {
            core.reload()?;
        }
        Ok(core)
    }
}

fn unavailable(error: FileError) -> OpenError {
    OpenError::Unavailable {
        name: error.name().to_owned(),
    }
}

/// Read both slots and pick the current state, as opening does. Nothing is written.
///
/// The order of the checks: a slot of the unsealed format is refused on its header, before
/// anything is decrypted. Then both slots are opened with `sealer`; a slot of a newer format
/// refuses the store, as a newer build may keep its key differently; then a complete header of
/// this format that names another store is [`OpenError::KeyLost`], whatever the other slot
/// holds: such a slot is never this store's, and the key in hand may be the wrong one. Only
/// then do the two-slot rules of [`format::pick_current`] apply.
fn read_current<F: SlotFile>(
    files: &mut [F; 2],
    sealer: &SnapshotSealer,
) -> Result<format::Current, OpenError> {
    let [a_prefix, b_prefix] = read_prefixes(files)?;
    let [a, b] = files;
    let headers = [a_prefix.header(), b_prefix.header()];
    if Inspection::of(headers) == Inspection::UnsealedFormat {
        return Err(OpenError::UnsealedFormat);
    }
    let a = read_rest(a, sealer, a_prefix).map_err(unavailable)?;
    let b = read_rest(b, sealer, b_prefix).map_err(unavailable)?;
    let newer = matches!(a, Slot::NewerFormat(_)) || matches!(b, Slot::NewerFormat(_));
    let other_store = headers
        .into_iter()
        .filter_map(SlotHeader::store_id)
        .any(|store_id| store_id != sealer.store_id());
    if other_store && !newer {
        return Err(OpenError::KeyLost);
    }
    format::pick_current(a, b)
}

/// A slot file's size and its first bytes, up to a header.
struct Prefix {
    size: u64,
    bytes: Vec<u8>,
}

impl Prefix {
    /// What the header shows without a key.
    fn header(&self) -> SlotHeader {
        format::inspect_header(&self.bytes, self.size)
    }
}

/// Read both slot files' sizes and first bytes, at most a header of each.
fn read_prefixes<F: SlotFile>(files: &mut [F; 2]) -> Result<[Prefix; 2], OpenError> {
    let [a, b] = files;
    Ok([
        read_prefix(a).map_err(unavailable)?,
        read_prefix(b).map_err(unavailable)?,
    ])
}

/// Read a slot file's size and its first bytes, at most a header.
fn read_prefix<F: SlotFile>(file: &mut F) -> Result<Prefix, FileError> {
    let size = file.size()?;
    if size == 0 {
        return Ok(Prefix {
            size,
            bytes: Vec::new(),
        });
    }
    // At most `HEADER_LEN`, so the conversion cannot fail.
    let prefix_len = usize::try_from(size.min(HEADER_LEN as u64)).unwrap_or(HEADER_LEN);
    let bytes = file.read_all(prefix_len)?;
    Ok(Prefix { size, bytes })
}

/// Read the rest of a slot whose `prefix` was read: the header decides first, so a length on
/// disk is checked against the file size and the caps before the rest of the slot is read or
/// allocated.
fn read_rest<F: SlotFile>(
    file: &mut F,
    sealer: &SnapshotSealer,
    prefix: Prefix,
) -> Result<Slot, FileError> {
    if let Some(slot) = format::read_header(&prefix.bytes, prefix.size) {
        return Ok(slot);
    }
    // `read_header` bounded the size by the caps, which fit in wasm32's `usize`.
    let len = usize::try_from(prefix.size)
        .map_err(|_| FileError::Failed("slot larger than memory".into()))?;
    Ok(format::decode_slot(sealer, file.read_all(len)?))
}

/// Read one slot whole, as opening does.
fn read_slot<F: SlotFile>(file: &mut F, sealer: &SnapshotSealer) -> Result<Slot, FileError> {
    let prefix = read_prefix(file)?;
    read_rest(file, sealer, prefix)
}

impl<F: SlotFile> Core<F> {
    /// Read the state of a broken store again from disk, through the files it still holds
    /// open, with the same reads and choice of slot as opening; then clear the mark. Both files
    /// are flushed first: a read through the handles also returns bytes that were written but
    /// never flushed, which a power loss could still take away. Only once the flushes succeed
    /// does disk win, as it would for a store opened again: after a failed flush, the new
    /// state if its slot is valid once flushed, the old one if not. Owners stay: their leases
    /// are still held.
    ///
    /// On failure the store stays broken, and the call fails: `Unavailable` when a file call
    /// fails, the flushes included, which a later call may get past; `Corrupt` when the slots
    /// hold no state an open would accept. A slot of a newer or the unsealed format, or of
    /// another store, counts as corrupt here: this store holds both files under the browser's
    /// exclusive lock and writes only its own sealed slots, so such a slot can only be damage.
    fn reload(&mut self) -> Result<(), StoreError> {
        for file in &mut self.files {
            file.flush().map_err(|_| StoreError::Unavailable)?;
        }
        let current = read_current(&mut self.files, &self.sealer).map_err(|error| match error {
            OpenError::Unavailable { .. } => StoreError::Unavailable,
            OpenError::Corrupt
            | OpenError::NewerFormat { .. }
            | OpenError::UnsealedFormat
            | OpenError::KeyLost => StoreError::Corrupt,
            // Not returned by reading the slots.
            OpenError::InvalidName | OpenError::NotDedicatedWorker | OpenError::InUse => {
                StoreError::Unavailable
            }
        })?;
        self.current = current.slot;
        self.generation = current.generation;
        self.entries = current.entries;
        self.broken = false;
        Ok(())
    }

    /// Apply `writes` to a copy of the state, write the whole new state to the slot that is
    /// not current, and only once it is flushed make it the state in memory.
    ///
    /// The current slot is never written, so until the new slot is complete and the file is
    /// cut to its length, the current slot stays the one an open picks. A write or a cut that
    /// fails may still have written every byte, so the new slot is then read back: if it is
    /// not a complete, valid slot of the new generation, the commit did not happen
    /// (`Unavailable`, or `Full` for the quota); if it is, or it cannot be read back, the
    /// outcome is unknown, as after a failed flush. Once the new slot is complete, only a
    /// failed flush is left, and then nobody can tell whether the slot will survive. An
    /// unknown outcome returns `Unknown` and marks the store broken, so the next call flushes
    /// again and reads the state from disk.
    ///
    /// When the random number generator fails, the commit writes nothing and returns
    /// `StoreError::Other`, as the Switch client does when its own randomness fails.
    fn commit(&mut self, writes: Vec<Write>) -> Result<(), StoreError> {
        // Nothing is written when the counter is spent; it takes 2^64 - 1 commits.
        let generation = self.generation.checked_add(1).ok_or_else(|| {
            StoreError::Other("the store's generation counter is exhausted".into())
        })?;
        // A fresh random nonce for every write, never one derived from the generation or any
        // other counter: after a torn or failed write, the same generation is written again
        // with different content, and a counter nonce would then be used twice under one key,
        // which breaks both the secrecy and the authenticity of XChaCha20-Poly1305. 24 random
        // bytes make a repeat negligible.
        let nonce = seal::random_nonce()
            .map_err(|_| StoreError::Other("the store's randomness is unavailable".into()))?;
        let mut next = self.entries.clone();
        apply(&mut next, writes);
        let slot =
            format::encode_slot(&self.sealer, generation, &nonce, &next).ok_or(StoreError::Full)?;
        let target = self.current.map_or(SlotId::A, SlotId::other);
        let file = &mut self.files[target.index()];
        let new_len = u64::try_from(slot.len()).unwrap_or(u64::MAX);

        let old_len = file.size().map_err(not_applied)?;
        // Bytes left from a longer old slot keep the new one invalid until they are cut off.
        // A file that is not longer has nothing to cut, and is not cut.
        let written = file.write_all(&slot).and_then(|()| {
            if old_len > new_len {
                file.truncate(new_len)
            } else {
                Ok(())
            }
        });
        if let Err(error) = written {
            return Err(self.failed_write(target, generation, error));
        }
        let file = &mut self.files[target.index()];
        if file.flush().is_err() {
            self.broken = true;
            return Err(StoreError::Unknown);
        }

        self.entries = next;
        self.generation = generation;
        self.current = Some(target);
        Ok(())
    }
}

impl<F: SlotFile> Core<F> {
    /// The error for a commit whose write or cut of the slot `target` at `generation` failed
    /// with `error`. A browser call that throws, or reports fewer bytes, may still have written
    /// them all, so the slot is read back. Unless it is a complete, valid slot of `generation`,
    /// the current slot is still the one an open picks, and nothing was applied. If it is, or
    /// it cannot be read back, the outcome is unknown: `Unknown`, and the store is marked
    /// broken.
    fn failed_write(&mut self, target: SlotId, generation: u64, error: FileError) -> StoreError {
        match read_slot(&mut self.files[target.index()], &self.sealer) {
            Ok(Slot::Valid {
                generation: found, ..
            }) if found == generation => {}
            Ok(_) => return not_applied(error),
            Err(_) => {}
        }
        self.broken = true;
        StoreError::Unknown
    }
}

/// The error for a commit whose new slot is not complete on disk: nothing was applied.
fn not_applied(error: FileError) -> StoreError {
    match error {
        FileError::Quota => StoreError::Full,
        FileError::Failed(_) => StoreError::Unavailable,
    }
}

/// Apply `writes` in order, as the Switch client's `MemoryStore` does: a namespace left with no
/// key is removed.
fn apply(entries: &mut Entries, writes: Vec<Write>) {
    for write in writes {
        match write {
            Write::Put { ns, key, value } => {
                entries.entry(ns).or_default().insert(key, value);
            }
            Write::Delete { ns, key } => {
                if let Some(keys) = entries.get_mut(&ns) {
                    keys.remove(&key);
                }
            }
            Write::DeletePrefix { ns, prefix } => {
                if let Some(keys) = entries.get_mut(&ns) {
                    keys.retain(|key, _| !key.starts_with(&prefix));
                }
            }
        }
    }
    entries.retain(|_, keys| !keys.is_empty());
}

/// The ownership of one namespace, released when the last holder drops it.
///
/// It keeps the store's core, and so its open files and their locks, alive, as the Switch
/// client's `MemoryStore` lease keeps its store: the contract's exclusion lasts until the last
/// lease holder drops, and the client's `StoreOwnership` holds a lease without the store.
struct Lease<F> {
    core: Arc<Mutex<Core<F>>>,
    namespace: String,
    owner: StoreOwner,
}

impl<F: Send> StoreLease for Lease<F> {}

impl<F> Drop for Lease<F> {
    fn drop(&mut self) {
        let mut core = lock(&self.core);
        // Release only an ownership that is still this lease's.
        if core.owners.get(&self.namespace) == Some(&self.owner) {
            core.owners.remove(&self.namespace);
        }
    }
}

// `scan`, `apply`, the ownership checks of `commit`, `acquire` and `commit_if`, and `Lease`
// follow the Switch client's `MemoryStore` (`crates/pubky-switch-client/src/store/memory.rs` at
// the workspace's pinned rev 309f01d). When that file changes upstream, check these too.
impl<F: SlotFile + Send + 'static> StateStore for SlotStore<F> {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let core = self.lock()?;
        Ok(core.entries.get(ns).and_then(|keys| keys.get(key)).cloned())
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        let core = self.lock()?;
        let Some(keys) = core.entries.get(ns) else {
            return Ok(Vec::new());
        };
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        // Every key with the prefix is at or past it. A cursor at or past the prefix starts the
        // range just past the cursor, so each page of a paginated scan skips the earlier pages
        // without walking them; a cursor before the prefix excludes nothing.
        let start = match after {
            Some(after) if after >= prefix => Bound::Excluded(after),
            _ => Bound::Included(prefix),
        };
        Ok(keys
            .range::<[u8], _>((start, Bound::Unbounded))
            .take_while(|(key, _)| key.starts_with(prefix))
            .take(limit)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        let mut core = self.lock()?;
        if batch
            .writes()
            .iter()
            .any(|write| core.owners.contains_key(write.ns()))
        {
            return Err(StoreError::InUse);
        }
        core.commit(batch.into_writes())
    }

    fn acquire(&self, ns: &str, owner: StoreOwner) -> Result<AcquireOutcome, StoreError> {
        let mut core = self.lock()?;
        if core.owners.contains_key(ns) {
            return Ok(AcquireOutcome::Busy);
        }
        core.owners.insert(ns.to_owned(), owner);
        Ok(AcquireOutcome::Acquired(Arc::new(Lease {
            core: Arc::clone(&self.core),
            namespace: ns.to_owned(),
            owner,
        })))
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: StoreOwner,
        batch: WriteBatch,
    ) -> Result<CommitOutcome, StoreError> {
        check_owned_batch(ns, &batch)?;
        // The owner is compared under the same lock as the commit, so no other call can take
        // the namespace in between.
        let mut core = self.lock()?;
        if core.owners.get(ns) != Some(&owner) {
            return Ok(CommitOutcome::NotOwner);
        }
        core.commit(batch.into_writes())?;
        Ok(CommitOutcome::Applied)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    use pubky_switch_client::store::StoreHandle;

    use super::super::file::{MemoryDisk, MemoryFile, Op, OpKind};
    use super::super::format::tests::{
        decode, encode, sealer, test_key, unsealed_slot, TEST_STORE,
    };
    use super::super::format::{
        seal_raw, FORMAT_VERSION, GENERATION_AT, NONCE_AT, SEALED_LEN_AT, STORE_ID_AT,
    };
    use super::super::seal::{fault, DataKey, StoreId, NONCE_LEN, STORE_ID_LEN, TAG_LEN};
    use super::*;

    type Store = SlotStore<MemoryFile>;

    fn open(disk: &MemoryDisk) -> Store {
        SlotStore::open(disk.files(), sealer()).unwrap()
    }

    /// The error opening the tests' store on `disk` fails with.
    fn open_error(disk: &MemoryDisk) -> Option<OpenError> {
        SlotStore::open(disk.files(), sealer()).err()
    }

    fn put(ns: &str, key: &[u8], value: &[u8]) -> WriteBatch {
        let mut batch = WriteBatch::new();
        batch.put(ns, key.to_vec(), value.to_vec()).unwrap();
        batch
    }

    fn get(store: &Store, key: &[u8]) -> Option<Vec<u8>> {
        store.get("ns", key).unwrap()
    }

    fn state(value: &[u8]) -> Entries {
        let mut entries = Entries::new();
        entries
            .entry("ns".to_owned())
            .or_default()
            .insert(b"k".to_vec(), value.to_vec());
        entries
    }

    /// A slot of the tests' store holding `k = value` in `ns` at `generation`.
    fn slot(generation: u64, value: &[u8]) -> Vec<u8> {
        encode(generation, &state(value)).unwrap()
    }

    /// The nonce of the slot in `bytes`.
    fn nonce_of(bytes: &[u8]) -> [u8; NONCE_LEN] {
        bytes[NONCE_AT..SEALED_LEN_AT].try_into().unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// The generation and slot the store holds in memory.
    fn position(store: &Store) -> (u64, Option<SlotId>) {
        let core = lock(&store.core);
        (core.generation, core.current)
    }

    fn writes_anything(ops: &[Op]) -> bool {
        ops.iter()
            .any(|op| matches!(op, Op::Write(..) | Op::Truncate(..) | Op::Flush(_)))
    }

    /// Poll a future that is ready at once, as the inline store handle's are.
    fn ready<T>(future: impl Future<Output = T>) -> T {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("the future was not ready"),
        }
    }

    /// Fresh owners. Only the Switch client's `StoreHandle::acquire` makes them, and it hands
    /// each to the store it acquires from, so a store that keeps them lends them here.
    fn owners<const N: usize>() -> [StoreOwner; N] {
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

    store_test! {
        fn test_store_commits_survive_reopening() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            assert_eq!(position(&store), (0, None));
            store.commit(put("ns", b"a", b"1")).unwrap();
            let mut batch = put("ns", b"b", b"2");
            batch.delete("ns", b"a".to_vec()).unwrap();
            batch.put("other", b"c".to_vec(), b"3".to_vec()).unwrap();
            store.commit(batch).unwrap();
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            drop(store);

            // The commits went to alternate slots, each holding the whole state.
            assert!(matches!(decode(&disk.bytes(0)), Slot::Valid { generation: 1, .. }));
            assert!(matches!(decode(&disk.bytes(1)), Slot::Valid { generation: 2, .. }));
            let store = open(&disk);
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            assert_eq!(get(&store, b"a"), None);
            assert_eq!(get(&store, b"b"), Some(b"2".to_vec()));
            assert_eq!(
                store.scan("other", b"", None, 10).unwrap(),
                vec![(b"c".to_vec(), b"3".to_vec())]
            );
        }
    }

    store_test! {
        fn test_store_torn_write_keeps_the_previous_state() {
            // Slot A holds generation 1 and slot B generation 2; generation 3 goes over A, the
            // same length as the slot it replaces, so a torn write leaves old bytes behind it.
            let len = slot(3, b"value-3").len();
            for cut in [0, 10, HEADER_LEN - 1, HEADER_LEN + 5, len - TAG_LEN, len - 5, len - 1] {
                let disk = MemoryDisk::default();
                let store = open(&disk);
                store.commit(put("ns", b"k", b"value-1")).unwrap();
                store.commit(put("ns", b"k", b"value-2")).unwrap();
                disk.tear_next_write_at(cut);
                assert_eq!(store.commit(put("ns", b"k", b"value-3")), Err(StoreError::Unavailable));
                assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()), "cut {cut}");
                drop(store);

                let store = open(&disk);
                assert_eq!(position(&store), (2, Some(SlotId::B)), "cut {cut}");
                assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()), "cut {cut}");
                store.commit(put("ns", b"k", b"value-4")).unwrap();
                drop(store);
                let store = open(&disk);
                assert_eq!(position(&store), (3, Some(SlotId::A)), "cut {cut}");
                assert_eq!(get(&store, b"k"), Some(b"value-4".to_vec()), "cut {cut}");
            }

            // A torn first write to an empty slot leaves only a prefix.
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.tear_next_write_at(HEADER_LEN + 3);
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unavailable));
            drop(store);
            let store = open(&disk);
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            store.commit(put("ns", b"k", b"value-3")).unwrap();
            assert_eq!(position(&store), (2, Some(SlotId::B)));
        }
    }

    store_test! {
        fn test_store_open_ignores_a_damaged_slot() {
            let current = slot(2, b"value-2");
            let len = current.len();
            // The magic, the version, the generation, the nonce, the sealed length, the
            // ciphertext at its start, middle and end, and the tag. (A changed store identifier
            // is another store's slot: `KeyLost`, below.)
            for at in [0, 8, 14, NONCE_AT, NONCE_AT + 23, SEALED_LEN_AT + 1, HEADER_LEN, (HEADER_LEN + len - TAG_LEN) / 2, len - TAG_LEN - 1, len - TAG_LEN, len - 1] {
                let disk = MemoryDisk::default();
                disk.set_bytes(0, slot(1, b"value-1"));
                let mut damaged = current.clone();
                damaged[at] ^= 0x01;
                disk.set_bytes(1, damaged);
                let store = open(&disk);
                assert_eq!(position(&store), (1, Some(SlotId::A)), "byte {at}");
                assert_eq!(get(&store, b"k"), Some(b"value-1".to_vec()), "byte {at}");
                // The next commit overwrites the damaged slot.
                store.commit(put("ns", b"k", b"value-3")).unwrap();
                assert_eq!(position(&store), (2, Some(SlotId::B)), "byte {at}");
            }
        }
    }

    store_test! {
        fn test_store_open_takes_the_higher_generation_in_either_slot() {
            for (a, b, current) in [(5, 7, SlotId::B), (7, 5, SlotId::A)] {
                let disk = MemoryDisk::default();
                disk.set_bytes(0, slot(a, format!("value-{a}").as_bytes()));
                disk.set_bytes(1, slot(b, format!("value-{b}").as_bytes()));
                let store = open(&disk);
                assert_eq!(position(&store), (7, Some(current)));
                assert_eq!(get(&store, b"k"), Some(b"value-7".to_vec()));
                disk.take_ops();
                store.commit(put("ns", b"k", b"value-8")).unwrap();
                // The commit goes to the other slot, at the next generation.
                assert!(disk.take_ops().contains(&Op::Write(current.other().index(), slot(8, b"value-8").len())));
                assert_eq!(position(&store), (8, Some(current.other())));
            }
        }
    }

    store_test! {
        fn test_store_open_with_one_valid_slot() {
            for other in [Vec::new(), b"not a slot".to_vec()] {
                for valid in [0, 1] {
                    let disk = MemoryDisk::default();
                    disk.set_bytes(valid, slot(4, b"value-4"));
                    disk.set_bytes(1 - valid, other.clone());
                    let store = open(&disk);
                    assert_eq!(position(&store).0, 4);
                    assert_eq!(get(&store, b"k"), Some(b"value-4".to_vec()));
                }
            }
        }
    }

    store_test! {
        fn test_store_open_refuses_two_invalid_slots() {
            let disk = MemoryDisk::default();
            disk.set_bytes(0, b"torn".to_vec());
            let mut torn = slot(2, b"value-2");
            torn.pop();
            disk.set_bytes(1, torn);
            assert_eq!(open_error(&disk), Some(OpenError::Corrupt));
            // A slot that opens under the key but whose payload does not decode is corrupt as
            // well, even next to a valid slot.
            disk.set_bytes(0, slot(1, b"value-1"));
            disk.set_bytes(1, seal_raw(&sealer(), FORMAT_VERSION, 2, &[1, 2, 3]));
            assert_eq!(open_error(&disk), Some(OpenError::Corrupt));
            assert!(!writes_anything(&disk.take_ops()));
        }
    }

    store_test! {
        fn test_store_open_two_empty_slots_is_a_new_store() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            assert_eq!(position(&store), (0, None));
            assert_eq!(store.scan("ns", b"", None, 10).unwrap(), Vec::new());
            assert_eq!(disk.take_ops(), vec![Op::Size(0), Op::Size(1)]);
            // A torn first commit next to an empty slot is a new store too.
            disk.set_bytes(1, slot(1, b"value-1")[..10].to_vec());
            assert_eq!(position(&open(&disk)), (0, None));
            // A torn slot that does not start as a first commit is not.
            disk.set_bytes(1, b"torn".to_vec());
            assert_eq!(open_error(&disk), Some(OpenError::Corrupt));
        }
    }

    store_test! {
        fn test_store_open_refuses_a_newer_format_and_writes_nothing() {
            let newer_version = FORMAT_VERSION + 1;
            // A newer build's slot has its version under its tag.
            let newer = seal_raw(&sealer(), newer_version, 9, b"anything");
            // Only the magic and the version, of a format whose header may be shorter.
            let short = newer[..12].to_vec();
            // A newer slot next to a slot of another store is still a newer format: a newer
            // build may keep its key differently.
            let mut other_store = slot(1, b"value-1");
            other_store[STORE_ID_AT] ^= 0x01;
            for (bytes, version) in [(newer.clone(), newer_version), (short, newer_version)] {
                for (at, other) in [(0, slot(1, b"value-1")), (1, slot(1, b"value-1")), (0, other_store.clone())] {
                    let disk = MemoryDisk::default();
                    disk.set_bytes(1 - at, other);
                    disk.set_bytes(at, bytes.clone());
                    assert_eq!(
                        open_error(&disk),
                        Some(OpenError::NewerFormat { version })
                    );
                    assert!(!writes_anything(&disk.take_ops()));
                    assert_eq!(disk.bytes(at), bytes);
                }
            }
        }
    }

    store_test! {
        fn test_store_open_ignores_a_changed_version_field_in_the_stale_slot() {
            // The version changes to a newer one (0x82 or 3) or an older one (0).
            for flip in [0x80, 0x01, 0x02] {
                let disk = MemoryDisk::default();
                let mut stale = slot(1, b"value-1");
                stale[8] ^= flip;
                disk.set_bytes(0, stale);
                disk.set_bytes(1, slot(2, b"value-2"));
                let store = open(&disk);
                assert_eq!(position(&store), (2, Some(SlotId::B)), "flip {flip}");
                assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()), "flip {flip}");
                // The next commit overwrites the damaged slot.
                store.commit(put("ns", b"k", b"value-3")).unwrap();
                assert_eq!(position(&store), (3, Some(SlotId::A)), "flip {flip}");
            }
        }
    }

    store_test! {
        fn test_store_open_damaged_only_slot_is_corrupt_and_torn_first_commit_is_new() {
            // A store with one commit: slot A holds generation 1 and slot B is empty. A changed
            // byte in A's magic, version, generation, nonce, sealed length, ciphertext or tag
            // leaves a slot that fails its checks, with nothing to fall back to.
            let complete = slot(1, b"value-1");
            let len = complete.len();
            for at in [0, 8, 14, NONCE_AT, SEALED_LEN_AT, SEALED_LEN_AT + 1, HEADER_LEN - 1, HEADER_LEN + 2, len - TAG_LEN, len - 1] {
                let disk = MemoryDisk::default();
                let mut damaged = complete.clone();
                damaged[at] ^= 0x01;
                disk.set_bytes(0, damaged.clone());
                assert_eq!(open_error(&disk), Some(OpenError::Corrupt), "byte {at}");
                assert!(!writes_anything(&disk.take_ops()));
                assert_eq!(disk.bytes(0), damaged, "byte {at}");
            }
            // A first commit that stopped part way applied nothing, and the next open is of a
            // new store.
            for cut in [1, HEADER_LEN - 1, HEADER_LEN + 3, len - 1] {
                let disk = MemoryDisk::default();
                let store = open(&disk);
                disk.tear_next_write_at(cut);
                assert_eq!(store.commit(put("ns", b"k", b"value-1")), Err(StoreError::Unavailable));
                drop(store);
                let store = open(&disk);
                assert_eq!(position(&store), (0, None), "cut {cut}");
                store.commit(put("ns", b"k", b"value-2")).unwrap();
                assert_eq!(position(&store), (1, Some(SlotId::A)), "cut {cut}");
            }
            // So is a first commit written whole over a longer torn one, whose cut failed.
            let disk = MemoryDisk::default();
            let store = open(&disk);
            disk.tear_next_write_at(slot(1, b"a much longer value").len() - 1);
            assert_eq!(store.commit(put("ns", b"k", b"a much longer value")), Err(StoreError::Unavailable));
            disk.fail_next(OpKind::Truncate, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.commit(put("ns", b"k", b"v")), Err(StoreError::Unavailable));
            drop(store);
            assert_eq!(position(&open(&disk)), (0, None));
        }
    }

    store_test! {
        fn test_store_open_reads_no_more_than_a_header_for_absurd_lengths() {
            let valid = slot(2, b"value-2");
            let size = u64::try_from(valid.len()).unwrap();
            // A sealed length of zero, shorter than a tag, larger than the file, over the cap,
            // and the largest.
            let tag_len = u64::try_from(TAG_LEN).unwrap();
            for sealed_len in [0, tag_len - 1, size, format::MAX_STATE_BYTES + tag_len + 1, u64::MAX] {
                let mut bytes = valid.clone();
                bytes[SEALED_LEN_AT..HEADER_LEN].copy_from_slice(&sealed_len.to_le_bytes());
                let disk = MemoryDisk::default();
                disk.set_bytes(0, slot(1, b"value-1"));
                disk.set_bytes(1, bytes);
                assert_eq!(position(&open(&disk)), (1, Some(SlotId::A)), "{sealed_len}");
                let reads: Vec<_> = disk.take_ops().into_iter().filter(|op| matches!(op, Op::Read(1, _))).collect();
                assert_eq!(reads, vec![Op::Read(1, HEADER_LEN)], "{sealed_len}");
            }
            // A file the browser reports as larger than any valid slot, with a header that asks
            // for that size.
            for claimed in [2 * format::MAX_STATE_BYTES, u64::MAX] {
                let mut header = slot(1, b"value-1")[..HEADER_LEN].to_vec();
                let header_len = u64::try_from(HEADER_LEN).unwrap();
                header[SEALED_LEN_AT..].copy_from_slice(&claimed.saturating_sub(header_len).to_le_bytes());
                let disk = MemoryDisk::default();
                disk.set_bytes(1, header);
                disk.claim_size(1, claimed);
                assert_eq!(position(&open(&disk)), (0, None), "{claimed}");
                let reads: Vec<_> = disk.take_ops().into_iter().filter(|op| matches!(op, Op::Read(..))).collect();
                assert_eq!(reads, vec![Op::Read(1, HEADER_LEN)], "{claimed}");
            }
        }
    }

    store_test! {
        fn test_store_open_ignores_a_slot_with_trailing_bytes() {
            let disk = MemoryDisk::default();
            disk.set_bytes(0, slot(1, b"value-1"));
            let mut longer = slot(2, b"value-2");
            longer.push(0);
            disk.set_bytes(1, longer);
            let store = open(&disk);
            assert_eq!(position(&store), (1, Some(SlotId::A)));
        }
    }

    store_test! {
        fn test_store_refuses_a_commit_past_the_last_generation() {
            let disk = MemoryDisk::default();
            disk.set_bytes(0, slot(u64::MAX - 1, b"value-1"));
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-2")).unwrap();
            assert_eq!(position(&store), (u64::MAX, Some(SlotId::B)));
            disk.take_ops();
            assert!(matches!(store.commit(put("ns", b"k", b"value-3")), Err(StoreError::Other(_))));
            assert!(!writes_anything(&disk.take_ops()));
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
            drop(store);
            let store = open(&disk);
            assert_eq!(position(&store), (u64::MAX, Some(SlotId::B)));
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
        }
    }

    store_test! {
        fn test_store_flushes_once_per_commit_after_the_write_and_the_cut() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            disk.take_ops();
            let long = slot(1, b"a longer value").len();
            store.commit(put("ns", b"k", b"a longer value")).unwrap();
            assert_eq!(disk.take_ops(), vec![Op::Size(0), Op::Write(0, long), Op::Flush(0)]);
            let short = slot(2, b"short").len();
            store.commit(put("ns", b"k", b"short")).unwrap();
            assert_eq!(disk.take_ops(), vec![Op::Size(1), Op::Write(1, short), Op::Flush(1)]);
            // Slot A still holds the longer slot of generation 1: the new one is cut to length.
            store.commit(put("ns", b"k", b"short")).unwrap();
            assert_eq!(
                disk.take_ops(),
                vec![
                    Op::Size(0),
                    Op::Write(0, short),
                    Op::Truncate(0, u64::try_from(short).unwrap()),
                    Op::Flush(0),
                ]
            );
            assert_eq!(disk.bytes(0).len(), short);
        }
    }

    store_test! {
        fn test_store_failed_flush_is_unknown_and_the_next_call_reads_disk() {
            let [owner] = owners();
            let disk = MemoryDisk::default();
            let store = open(&disk);
            let AcquireOutcome::Acquired(lease) = store.acquire("owned", owner).unwrap() else {
                panic!("the namespace was free");
            };
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.fail_next(OpKind::Flush, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unknown));
            // Memory changes only after a flush: it still holds the old state.
            assert_eq!(lock(&store.core).entries, state(b"value-1"));
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            // Memory and disk may now disagree, so the next call flushes both files and reads
            // disk first. While the flush fails, the slot read back may not survive a power
            // loss: the call fails, the new state is not taken, nothing is written and the
            // store stays marked. The same while the read fails.
            disk.take_ops();
            disk.fail_next(OpKind::Flush, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.get("ns", b"k"), Err(StoreError::Unavailable));
            disk.fail_next(OpKind::Flush, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.commit(put("ns", b"k", b"other")), Err(StoreError::Unavailable));
            assert!(lock(&store.core).broken);
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            assert!(!disk.take_ops().iter().any(|op| matches!(op, Op::Write(..) | Op::Truncate(..))));
            disk.fail_next(OpKind::Size, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.get("ns", b"k"), Err(StoreError::Unavailable));
            assert!(lock(&store.core).broken);
            // Here the complete slot stayed on disk, so once both files are flushed the next
            // call finds the new state, as opening the store again would: the outcome was
            // truly unknown. Reading writes nothing but the flushes.
            disk.take_ops();
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            assert!(!lock(&store.core).broken);
            let ops = disk.take_ops();
            assert_eq!(ops[..2], [Op::Flush(0), Op::Flush(1)]);
            assert!(!ops.iter().any(|op| matches!(op, Op::Write(..) | Op::Truncate(..))));
            // The owner is kept, and the next commit goes to the other slot.
            assert_eq!(store.commit(put("owned", b"k", b"raw")), Err(StoreError::InUse));
            store.commit(put("ns", b"k", b"value-3")).unwrap();
            assert_eq!(position(&store), (3, Some(SlotId::A)));
            assert_eq!(store.commit_if("owned", owner, put("owned", b"k", b"v")), Ok(CommitOutcome::Applied));
            drop(store);
            drop(lease);
            let store = open(&disk);
            assert_eq!(position(&store), (4, Some(SlotId::B)));
            assert_eq!(get(&store, b"k"), Some(b"value-3".to_vec()));
        }
    }

    store_test! {
        fn test_store_failed_flush_of_a_slot_lost_on_disk_reads_the_old_state() {
            let [owner] = owners();
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.fail_next(OpKind::Flush, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unknown));
            // The flush kept only part of the new slot.
            let partial = disk.bytes(1)[..HEADER_LEN + 3].to_vec();
            disk.set_bytes(1, partial);
            assert_eq!(get(&store, b"k"), Some(b"value-1".to_vec()));
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            // The next commit overwrites the lost slot.
            store.commit(put("ns", b"k", b"value-3")).unwrap();
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            drop(store);
            let store = open(&disk);
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            assert_eq!(get(&store, b"k"), Some(b"value-3".to_vec()));
            drop(store);

            // Slots that hold no state an open accepts fail every call with `Corrupt`.
            let store = open(&disk);
            disk.fail_next(OpKind::Flush, FileError::Failed("InvalidStateError".into()));
            assert_eq!(store.commit(put("ns", b"k", b"value-4")), Err(StoreError::Unknown));
            disk.set_bytes(0, b"torn".to_vec());
            disk.set_bytes(1, b"torn".to_vec());
            assert_eq!(store.get("ns", b"k"), Err(StoreError::Corrupt));
            assert_eq!(store.scan("ns", b"", None, 10), Err(StoreError::Corrupt));
            assert_eq!(store.commit(put("ns", b"k", b"value-5")), Err(StoreError::Corrupt));
            assert!(matches!(store.acquire("ns", owner), Err(StoreError::Corrupt)));
            assert_eq!(store.commit_if("ns", owner, put("ns", b"k", b"value-5")), Err(StoreError::Corrupt));
            assert_eq!(disk.bytes(0), b"torn");
        }
    }

    store_test! {
        fn test_store_failed_write_applies_nothing() {
            let cases = [
                (OpKind::Size, FileError::Failed("InvalidStateError".into()), StoreError::Unavailable),
                (OpKind::Write, FileError::Failed("InvalidStateError".into()), StoreError::Unavailable),
                (OpKind::Write, FileError::Quota, StoreError::Full),
            ];
            for (kind, error, expected) in cases {
                let disk = MemoryDisk::default();
                let store = open(&disk);
                store.commit(put("ns", b"k", b"value-1")).unwrap();
                disk.fail_next(kind, error);
                assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(expected.clone()));
                assert_eq!(get(&store, b"k"), Some(b"value-1".to_vec()), "{kind:?}");
                drop(store);
                let store = open(&disk);
                assert_eq!(position(&store), (1, Some(SlotId::A)), "{kind:?}");
                assert_eq!(get(&store, b"k"), Some(b"value-1".to_vec()), "{kind:?}");
                store.commit(put("ns", b"k", b"value-3")).unwrap();
                assert_eq!(position(&store), (2, Some(SlotId::B)), "{kind:?}");
            }
        }
    }

    store_test! {
        fn test_store_failed_write_of_a_complete_slot_is_unknown() {
            // The write fails after every byte of generation 2 reached slot B: read back, the
            // slot is complete and valid, so the commit may have happened.
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.tear_next_write_at(usize::MAX);
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unknown));
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            assert!(lock(&store.core).broken);
            // The next call flushes and reads disk, as opening does, and finds the new state.
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            drop(store);
            assert_eq!(position(&open(&disk)), (2, Some(SlotId::B)));

            // When the slot cannot be read back, the outcome is unknown too.
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.fail_next(OpKind::Write, FileError::Failed("InvalidStateError".into()));
            disk.claim_size(1, u64::MAX);
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unknown));
            assert!(lock(&store.core).broken);
        }
    }

    store_test! {
        fn test_store_failed_cut_applies_nothing() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"a longer value")).unwrap();
            store.commit(put("ns", b"k", b"value-2")).unwrap();
            disk.fail_next(OpKind::Truncate, FileError::Failed("InvalidStateError".into()));
            // Generation 3 is written complete over slot A, but the longer slot's last bytes
            // stay behind it.
            assert_eq!(store.commit(put("ns", b"k", b"value-3")), Err(StoreError::Unavailable));
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
            let complete = disk.bytes(0)[..slot(3, b"value-3").len()].to_vec();
            assert_eq!(decode(&complete), Slot::Valid { generation: 3, entries: state(b"value-3") });
            drop(store);
            let store = open(&disk);
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            assert_eq!(get(&store, b"k"), Some(b"value-2".to_vec()));
        }
    }

    store_test! {
        fn test_store_scan_cursor_before_inside_and_after_the_prefix() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            let mut batch = WriteBatch::new();
            for key in ["a", "b/", "b/1", "b/2", "c"] {
                batch.put("ns", key.as_bytes().to_vec(), key.as_bytes().to_vec()).unwrap();
            }
            store.commit(batch).unwrap();
            let scan = |after: Option<&str>, limit| -> Vec<String> {
                store
                    .scan("ns", b"b/", after.map(str::as_bytes), limit)
                    .unwrap()
                    .into_iter()
                    .map(|(key, _)| String::from_utf8(key).unwrap())
                    .collect()
            };
            let all = ["b/", "b/1", "b/2"];
            assert_eq!(scan(None, 10), all);
            // A cursor before the prefix excludes nothing.
            assert_eq!(scan(Some(""), 10), all);
            assert_eq!(scan(Some("a"), 10), all);
            assert_eq!(scan(Some("b"), 10), all);
            // A cursor equal to the prefix excludes only the key equal to it.
            assert_eq!(scan(Some("b/"), 10), ["b/1", "b/2"]);
            assert_eq!(scan(Some("b/"), 1), ["b/1"]);
            // A cursor inside the range, on a key or between keys.
            assert_eq!(scan(Some("b/1"), 10), ["b/2"]);
            assert_eq!(scan(Some("b/15"), 10), ["b/2"]);
            assert_eq!(scan(Some("b/2"), 10), Vec::<String>::new());
            // A cursor past the prefix range.
            assert_eq!(scan(Some("b0"), 10), Vec::<String>::new());
            assert_eq!(scan(Some("c"), 10), Vec::<String>::new());
            assert_eq!(scan(Some("d"), 10), Vec::<String>::new());
        }
    }

    store_test! {
        fn test_store_ownership_follows_the_memory_store_rules() {
            let [first, second] = owners();
            let disk = MemoryDisk::default();
            let store = open(&disk);
            let AcquireOutcome::Acquired(lease) = store.acquire("ns", first).unwrap() else {
                panic!("the namespace was free");
            };
            assert!(matches!(store.acquire("ns", second).unwrap(), AcquireOutcome::Busy));
            // A raw commit cannot touch an owned namespace; other namespaces stay writable.
            assert_eq!(store.commit(put("ns", b"k", b"raw")), Err(StoreError::InUse));
            store.commit(put("free", b"k", b"raw")).unwrap();
            // Only the exact owner commits, and a refusal applies nothing.
            assert_eq!(store.commit_if("ns", second, put("ns", b"k", b"v")), Ok(CommitOutcome::NotOwner));
            assert_eq!(get(&store, b"k"), None);
            assert_eq!(store.commit_if("ns", first, put("ns", b"k", b"v")), Ok(CommitOutcome::Applied));
            assert_eq!(get(&store, b"k"), Some(b"v".to_vec()));
            assert!(store.commit_if("ns", first, put("free", b"k", b"v")).is_err());

            drop(lease);
            let AcquireOutcome::Acquired(lease) = store.acquire("ns", second).unwrap() else {
                panic!("the lease was released");
            };
            assert_eq!(store.commit_if("ns", first, put("ns", b"k", b"w")), Ok(CommitOutcome::NotOwner));
            // A lease keeps the store's files open until it drops.
            disk.take_ops();
            drop(store);
            assert_eq!(disk.take_ops(), vec![]);
            drop(lease);
            assert_eq!(disk.take_ops(), vec![Op::Close(0), Op::Close(1)]);
        }
    }

    store_test! {
        fn test_store_seal_round_trip_keeps_no_plaintext_or_key_on_disk() {
            let marker = b"PLAINTEXT-MARKER-5f3a9c";
            let key = DataKey::generate().unwrap();
            let store_id = StoreId::generate().unwrap();
            let disk = MemoryDisk::default();
            let store = SlotStore::open(disk.files(), SnapshotSealer::new(&key, store_id)).unwrap();
            store.commit(put("ns", b"k", marker)).unwrap();
            store.commit(put("marker-namespace", b"marker-key", b"v")).unwrap();
            drop(store);
            // Neither file holds the value, the namespace, the key or the data key's bytes.
            for slot in [0, 1] {
                let bytes = disk.bytes(slot);
                assert!(!bytes.is_empty());
                for needle in [&marker[..], b"marker-namespace", b"marker-key", key.as_bytes()] {
                    assert!(!contains(&bytes, needle), "slot {slot}");
                }
            }
            // The same key and store identifier open the store again.
            let again = DataKey::from_bytes(key.as_bytes()).unwrap();
            let store = SlotStore::open(disk.files(), SnapshotSealer::new(&again, store_id)).unwrap();
            assert_eq!(position(&store), (2, Some(SlotId::B)));
            assert_eq!(get(&store, b"k"), Some(marker.to_vec()));
        }
    }

    store_test! {
        fn test_store_seal_every_changed_byte_is_refused() {
            let older = slot(1, b"value-1");
            let newer = slot(2, b"value-2");
            let store_id = STORE_ID_AT..STORE_ID_AT + STORE_ID_LEN;
            // Each byte of the newer slot, next to the older one: the store opens at the older
            // slot, or for a changed store identifier refuses with `KeyLost`.
            for at in 0..newer.len() {
                let disk = MemoryDisk::default();
                disk.set_bytes(0, older.clone());
                let mut changed = newer.clone();
                changed[at] ^= 0x01;
                disk.set_bytes(1, changed.clone());
                match SlotStore::open(disk.files(), sealer()) {
                    Ok(store) => {
                        assert!(!store_id.contains(&at), "byte {at}");
                        assert_eq!(position(&store), (1, Some(SlotId::A)), "byte {at}");
                    }
                    Err(error) => {
                        assert!(store_id.contains(&at), "byte {at}");
                        assert_eq!(error, OpenError::KeyLost, "byte {at}");
                    }
                }
                assert!(!writes_anything(&disk.take_ops()), "byte {at}");
                assert_eq!(disk.bytes(1), changed);
            }
            // Each byte of a store's only slot: corrupt, or `KeyLost` for the store identifier.
            for at in 0..older.len() {
                let disk = MemoryDisk::default();
                let mut changed = older.clone();
                changed[at] ^= 0x01;
                disk.set_bytes(0, changed);
                let expected = if store_id.contains(&at) { OpenError::KeyLost } else { OpenError::Corrupt };
                assert_eq!(open_error(&disk), Some(expected), "byte {at}");
                assert!(!writes_anything(&disk.take_ops()), "byte {at}");
            }
        }
    }

    store_test! {
        fn test_store_seal_slot_copied_between_slots_or_stores_is_refused() {
            let first = slot(1, b"value-1");
            let second = slot(2, b"value-2");
            // A slot copied whole over the other slot: two valid slots of one generation.
            let disk = MemoryDisk::default();
            disk.set_bytes(0, second.clone());
            disk.set_bytes(1, second.clone());
            assert_eq!(open_error(&disk), Some(OpenError::Corrupt));
            // The nonce, ciphertext and tag of generation 2 moved under a header of generation 3
            // in the other slot: it does not open, and the store opens at generation 2.
            let mut moved = slot(3, b"value-2")[..NONCE_AT].to_vec();
            moved.extend_from_slice(&second[NONCE_AT..]);
            let disk = MemoryDisk::default();
            disk.set_bytes(0, moved);
            disk.set_bytes(1, second.clone());
            assert_eq!(position(&open(&disk)), (2, Some(SlotId::B)));
            // The same with generation 1's blob under generation 2's header, with nothing else
            // to fall back to.
            let mut moved = second[..NONCE_AT].to_vec();
            moved.extend_from_slice(&first[NONCE_AT..]);
            let disk = MemoryDisk::default();
            disk.set_bytes(1, moved);
            assert_eq!(open_error(&disk), Some(OpenError::Corrupt));
            // A slot of another store, under the same key or another one, next to a valid slot
            // of this store: `KeyLost`, and nothing is written.
            let other_id = StoreId::from_bytes([1; STORE_ID_LEN]);
            let same_key = SnapshotSealer::new(&test_key(), other_id);
            let other_key = SnapshotSealer::new(&DataKey::generate().unwrap(), other_id);
            for other in [&same_key, &other_key] {
                let foreign = format::encode_slot(other, 2, &seal::random_nonce().unwrap(), &state(b"other")).unwrap();
                let disk = MemoryDisk::default();
                disk.set_bytes(0, first.clone());
                disk.set_bytes(1, foreign.clone());
                assert_eq!(open_error(&disk), Some(OpenError::KeyLost));
                assert!(!writes_anything(&disk.take_ops()));
                assert_eq!(disk.bytes(1), foreign);
                // Its blob under this store's header does not open either.
                let mut renamed = foreign.clone();
                renamed[STORE_ID_AT..NONCE_AT].copy_from_slice(&TEST_STORE.to_bytes());
                disk.set_bytes(1, renamed);
                assert_eq!(position(&open(&disk)), (1, Some(SlotId::A)));
            }
            // This store's files under a wrong key with the right store identifier: nothing
            // opens, and the store is corrupt rather than new.
            let wrong = SnapshotSealer::new(&DataKey::generate().unwrap(), TEST_STORE);
            let disk = MemoryDisk::default();
            disk.set_bytes(0, first);
            disk.set_bytes(1, second);
            assert_eq!(SlotStore::open(disk.files(), wrong).err(), Some(OpenError::Corrupt));
            assert!(!writes_anything(&disk.take_ops()));
        }
    }

    store_test! {
        fn test_store_seal_key_lost_for_another_store() {
            let other_id = StoreId::from_bytes([1; STORE_ID_LEN]);
            let other_key = DataKey::generate().unwrap();
            // A store of two commits under another key and store identifier.
            let disk = MemoryDisk::default();
            let other = SlotStore::open(disk.files(), SnapshotSealer::new(&other_key, other_id)).unwrap();
            other.commit(put("ns", b"k", b"value-1")).unwrap();
            other.commit(put("ns", b"k", b"value-2")).unwrap();
            drop(other);
            let before = [disk.bytes(0), disk.bytes(1)];
            disk.take_ops();
            assert_eq!(open_error(&disk), Some(OpenError::KeyLost));
            assert!(!writes_anything(&disk.take_ops()));
            assert_eq!([disk.bytes(0), disk.bytes(1)], before);
            // A torn first commit of another store, whose header is complete, next to an empty
            // file: `KeyLost`, not a new store.
            let foreign = format::encode_slot(&SnapshotSealer::new(&other_key, other_id), 1, &seal::random_nonce().unwrap(), &state(b"v")).unwrap();
            let disk = MemoryDisk::default();
            disk.set_bytes(0, foreign[..HEADER_LEN + 3].to_vec());
            assert_eq!(open_error(&disk), Some(OpenError::KeyLost));
            // Cut before its store identifier is complete, it cannot be told from this store's
            // own torn first commit, and opens as a new store.
            disk.set_bytes(0, foreign[..STORE_ID_AT + 3].to_vec());
            assert_eq!(position(&open(&disk)), (0, None));
            // The display says what happened, and that nothing was written.
            assert_eq!(
                OpenError::KeyLost.to_string(),
                "sealed data is present but its key is gone or wrong; nothing was written"
            );
        }
    }

    store_test! {
        fn test_store_seal_nonce_is_fresh_after_a_failed_write() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            // The write of generation 2 stops after its header, nonce included, and applies
            // nothing.
            disk.tear_next_write_at(HEADER_LEN);
            assert_eq!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Unavailable));
            let failed = disk.bytes(1);
            assert_eq!(failed[GENERATION_AT..STORE_ID_AT], 2u64.to_le_bytes());
            // Generation 2 is written again, with other content: under another nonce.
            store.commit(put("ns", b"k", b"other value")).unwrap();
            let written = disk.bytes(1);
            assert_eq!(written[GENERATION_AT..STORE_ID_AT], 2u64.to_le_bytes());
            assert_ne!(nonce_of(&failed), nonce_of(&written));
            assert_eq!(position(&store), (2, Some(SlotId::B)));
        }
    }

    store_test! {
        fn test_store_seal_nonces_never_repeat_over_many_commits() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            let mut nonces = std::collections::BTreeSet::new();
            for index in 0..200u32 {
                store.commit(put("ns", b"k", &index.to_le_bytes())).unwrap();
                let (_, Some(current)) = position(&store) else {
                    panic!("no slot after a commit");
                };
                assert!(nonces.insert(nonce_of(&disk.bytes(current.index()))), "commit {index}");
            }
            assert_eq!(nonces.len(), 200);
        }
    }

    store_test! {
        fn test_store_seal_unsealed_format_is_refused_untouched() {
            let unsealed = unsealed_slot(1, b"\x02\0\0\0ns\x01\0\0\0k\x01\0\0\0v");
            let cases = [
                (unsealed.clone(), Vec::new()),
                (Vec::new(), unsealed.clone()),
                (slot(1, b"value-1"), unsealed_slot(2, b"")),
                (unsealed.clone(), unsealed_slot(2, b"")),
            ];
            for (a, b) in cases {
                let disk = MemoryDisk::default();
                disk.set_bytes(0, a.clone());
                disk.set_bytes(1, b.clone());
                let mut files = disk.files();
                assert_eq!(SlotStore::inspect(&mut files), Ok(Inspection::UnsealedFormat));
                drop(files);
                assert_eq!(open_error(&disk), Some(OpenError::UnsealedFormat));
                let ops = disk.take_ops();
                assert!(!writes_anything(&ops));
                // Only headers were read: nothing was decrypted, not even a sealed slot next
                // to it.
                assert!(ops.iter().all(|op| !matches!(op, Op::Read(_, len) if *len > HEADER_LEN)), "{ops:?}");
                assert_eq!([disk.bytes(0), disk.bytes(1)], [a, b]);
            }
        }
    }

    store_test! {
        fn test_store_seal_torn_first_commit_next_to_an_empty_slot_is_a_new_store() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            disk.tear_next_write_at(HEADER_LEN + 3);
            assert_eq!(store.commit(put("ns", b"k", b"value-1")), Err(StoreError::Unavailable));
            drop(store);
            let mut files = disk.files();
            assert_eq!(
                SlotStore::inspect(&mut files),
                Ok(Inspection::Present([
                    SlotHeader::Versioned { version: FORMAT_VERSION, store_id: Some(TEST_STORE) },
                    SlotHeader::Empty,
                ]))
            );
            drop(files);
            let store = open(&disk);
            assert_eq!(position(&store), (0, None));
            store.commit(put("ns", b"k", b"value-2")).unwrap();
            assert_eq!(position(&store), (1, Some(SlotId::A)));
        }
    }

    store_test! {
        fn test_store_seal_randomness_failure_at_commit_writes_nothing() {
            let disk = MemoryDisk::default();
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            disk.take_ops();
            fault::fail_next_random();
            assert!(matches!(store.commit(put("ns", b"k", b"value-2")), Err(StoreError::Other(_))));
            assert_eq!(disk.take_ops(), Vec::new());
            assert_eq!(get(&store, b"k"), Some(b"value-1".to_vec()));
            assert_eq!(position(&store), (1, Some(SlotId::A)));
            // The next commit goes ahead.
            store.commit(put("ns", b"k", b"value-3")).unwrap();
            assert_eq!(position(&store), (2, Some(SlotId::B)));
        }
    }

    store_test! {
        fn test_store_seal_inspect_reads_only_headers() {
            let disk = MemoryDisk::default();
            let mut files = disk.files();
            assert_eq!(SlotStore::inspect(&mut files), Ok(Inspection::Empty));
            drop(files);
            assert_eq!(disk.take_ops(), vec![Op::Size(0), Op::Size(1), Op::Close(0), Op::Close(1)]);
            let store = open(&disk);
            store.commit(put("ns", b"k", b"value-1")).unwrap();
            store.commit(put("ns", b"k", b"value-2")).unwrap();
            drop(store);
            disk.take_ops();
            let mut files = disk.files();
            let header = SlotHeader::Versioned { version: FORMAT_VERSION, store_id: Some(TEST_STORE) };
            let inspection = SlotStore::inspect(&mut files).unwrap();
            assert_eq!(inspection, Inspection::Present([header, header]));
            assert_eq!(inspection.newer_version(), None);
            drop(files);
            let ops = disk.take_ops();
            assert!(!writes_anything(&ops));
            assert_eq!(ops.iter().filter(|op| matches!(op, Op::Read(..))).collect::<Vec<_>>(), [&Op::Read(0, HEADER_LEN), &Op::Read(1, HEADER_LEN)]);
            // A slot claiming a newer version.
            disk.set_bytes(1, seal_raw(&sealer(), FORMAT_VERSION + 1, 3, b"anything"));
            let mut files = disk.files();
            assert_eq!(SlotStore::inspect(&mut files).unwrap().newer_version(), Some(FORMAT_VERSION + 1));
            // A failed file call is `Unavailable`.
            disk.fail_next(OpKind::Size, FileError::Failed("InvalidStateError".into()));
            assert_eq!(
                SlotStore::inspect(&mut files),
                Err(OpenError::Unavailable { name: "InvalidStateError".into() })
            );
        }
    }
}
