//! The browser's side of the store: [`OpfsStore`], opening its two slot files in the Origin
//! Private File System and its key (`keys.rs`), destroying it, and [`OpfsFile`], a
//! [`SlotFile`] on a `FileSystemSyncAccessHandle`.

use std::fmt;

use js_sys::Promise;
use pubky_switch_client::store::{
    AcquireOutcome, CommitOutcome, KeyValue, StateStore, StoreError, StoreOwner, WriteBatch,
};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DedicatedWorkerGlobalScope, DomException, FileSystemDirectoryHandle, FileSystemFileHandle,
    FileSystemGetDirectoryOptions, FileSystemGetFileOptions, FileSystemReadWriteOptions,
    FileSystemRemoveOptions, FileSystemSyncAccessHandle,
};

use super::core::SlotStore;
use super::file::{FileError, SlotFile};
use super::format::Inspection;
use super::keys::{check_wrapping_key, KeyRecords, Record, WrappingKey};
use super::{check_name, OpenError};

/// The Switch client's [`StateStore`] on the browser's Origin Private File System (OPFS), for
/// a host that runs in a dedicated worker. Every snapshot it writes is sealed: encrypted and
/// authenticated under a data key that is in the clear only in WebAssembly memory.
///
/// # Where it runs
///
/// Open it with [`OpfsStore::open`] in a dedicated worker, and run the SDK and the Switch
/// client in that same worker. The [`StateStore`] calls are synchronous, and only a dedicated
/// worker has the browser's synchronous file handles, so a page or a service worker cannot
/// host the store. Every call runs synchronously on the worker, and a commit rewrites the
/// whole state, so a commit takes time in proportion to everything stored.
///
/// # What sealing protects, and what it does not
///
/// Every snapshot is sealed with XChaCha20-Poly1305, with a fresh random nonce for each write,
/// so the files hold no plaintext: namespaces, keys and values are all inside the sealed part.
/// Its tag binds the snapshot to its header: a snapshot that was changed, written without the
/// key, or moved to another store, generation or format version does not open.
///
/// It does not protect against:
///
/// - **Script in the origin.** Any script that runs in the origin can do everything the SDK
///   can, the store's key included. Sealing does not defend against it.
/// - **A copied browser profile**, in origin-held mode ([`WrappingKey::Origin`]). Chromium and
///   Firefox keep a non-extractable key's raw bytes in the profile, so someone who copies the
///   whole profile there can unwrap the data key. WebKit on Apple platforms wraps stored keys
///   with a Keychain key; Playwright's WebKit on Linux uses an empty one. These three
///   statements come from reading the engines, and are not verified by the tests here. A host
///   key kept outside the profile ([`WrappingKey::Host`]) is not in the copy.
/// - **Rollback.** Someone who can write the files can put back an older snapshot of the same
///   store, which opens. Sealing does not detect that.
/// - **Memory.** While the store is open, the data key and every value are in WebAssembly
///   memory in the clear, and they stay there while a lease it granted lives, after the store
///   itself was dropped.
///
/// # The keys
///
/// Each store has a store identifier (16 random bytes, not secret) and a data key (32 random
/// bytes) made with it in WebAssembly. The data key is never written to disk in the clear: a
/// record in the origin's IndexedDB, under the store's name, keeps it wrapped with AES-GCM
/// under an AES-GCM wrapping key (256-bit in origin-held mode), with a fresh random IV and the
/// store identifier as associated data. In origin-held mode the record also holds the wrapping
/// key, a non-extractable `CryptoKey`; in host-held mode the host passes its own `CryptoKey`
/// to every open and the record holds none ([`WrappingKey`]). The record is written in one
/// IndexedDB transaction that asks for strict durability, and counts as written only once that
/// transaction completes. Once the data key is unwrapped, the buffer WebCrypto returned it in
/// is wiped; the data key and the cipher's copy of it are kept on the heap and wiped when the
/// store and every lease it granted have been dropped. Copies the compiler leaves in
/// WebAssembly memory while it builds them, and copies the engine makes inside WebCrypto while
/// it wraps or unwraps the key, are not wiped.
///
/// # How it stores
///
/// The store is a directory in the origin's OPFS root, named by the caller, holding two files.
/// Each holds a whole sealed snapshot of the store with a generation number and its length. A
/// commit encodes and seals the whole new state, writes it over the file that does not hold
/// the current state, cuts that file to the new length when it was longer, flushes it, and
/// only then makes it the current state and returns `Ok`. Opening takes the valid file with
/// the higher generation, and ignores a file whose size or tag is wrong next to it: a torn
/// write or damage, which the next commit overwrites. Damage to the current file after the
/// fact therefore opens the store at the other file's older state, without the last commit
/// that returned `Ok`: what that commit recorded, such as the outcome of a payment, is then
/// unknown to the SDK. With no valid file, a first commit that stopped part way next to an
/// empty file is a new store (with the key already in its record), and anything else is
/// [`OpenError::Corrupt`]. A store's only committed file that loses its end after the fact,
/// before a second commit, reads the same as a first commit that stopped part way, and opens
/// as a new store: the two cannot be told apart.
///
/// # Opening
///
/// [`OpfsStore::open`] first opens both files, taking their exclusive locks, so no other
/// worker or tab can create or read the key record at the same time. It then reads the files'
/// headers without a key, and only then reads the key record. "Files present" means at least
/// one file is not empty. A record that lacks a field, or has one of the wrong type or length,
/// counts as a record that fails to unwrap.
///
/// | Files | Record missing | Record unwraps | Record fails to unwrap | Record of the other mode |
/// | --- | --- | --- | --- | --- |
/// | Both empty | New store: new identifier and data key, record written, opens empty | New store with the record's key and identifier, opens empty | [`OpenError::KeyLost`] | [`OpenError::KeyLost`] |
/// | Present, a complete header of this format in either | [`OpenError::KeyLost`] | The two-slot rules above; a complete header naming another store is [`OpenError::KeyLost`] | [`OpenError::KeyLost`] | [`OpenError::KeyLost`] |
/// | Present, no complete header of this format (nothing sealed) | [`OpenError::Corrupt`] | The two-slot rules above | [`OpenError::Corrupt`] | [`OpenError::Corrupt`] |
/// | A slot of the earlier unsealed format | [`OpenError::UnsealedFormat`] | [`OpenError::UnsealedFormat`] | [`OpenError::UnsealedFormat`] | [`OpenError::UnsealedFormat`] |
/// | A slot of a newer format | [`OpenError::NewerFormat`] | [`OpenError::NewerFormat`], unless the slot opens under the key with its version set back to this build's: then it is a damaged slot, under the two-slot rules | [`OpenError::NewerFormat`] | [`OpenError::NewerFormat`] |
///
/// A key record of a newer record format is [`OpenError::NewerFormat`] with that format
/// number, whatever the files hold, unless a file is of the unsealed format. A host key that
/// is not AES-GCM, or lacks the `encrypt` or `decrypt` usage, is [`OpenError::Unavailable`],
/// checked before anything else.
///
/// The unsealed format is refused on a file's header alone, before the record is read, and
/// wins over a newer format in the other file. A newer format wins over a lost key, as a newer
/// build may keep its key differently. Every error
/// leaves the files and the record as they were: no failed open writes either, and a record
/// is never replaced. A failure of IndexedDB or WebCrypto itself, rather than a key that does
/// not unwrap, is [`OpenError::Unavailable`].
///
/// # The contract's guarantees
///
/// - **Atomic:** a commit switches the current file once. A write or cut that stops part way
///   leaves the new file invalid and the old one current.
/// - **Durable before `Ok`:** every commit calls `flush()` on the file before `Ok`, and a new
///   store's key record is committed before its first snapshot can be written. Whether a flush
///   or a strict IndexedDB transaction reaches the storage device before a power loss is up to
///   the engine, and cannot be tested here.
/// - **Ordered and consistent:** calls run one at a time under one lock, and reads are served
///   from memory, which takes a commit's state only once it is flushed.
/// - **Exclusive:** both files are held open, each with the browser's exclusive lock, until
///   the store and every lease it granted have been dropped, so no other worker or tab can
///   open it meanwhile ([`OpenError::InUse`]).
///   [`acquire`](StateStore::acquire) and [`commit_if`](StateStore::commit_if) keep owners in
///   memory with the rules of the Switch client's `MemoryStore`.
/// - **Encrypted at rest:** met within the limits above: no plaintext in the files, and
///   tampering detected, but not against script in the origin, a copied profile in Chromium
///   and Firefox in origin-held mode, or rollback.
/// - **Real deletion:** [`OpfsStore::destroy`] deletes the key record first, then the files,
///   so whatever is left of the files can no longer be opened, as long as the engine erases
///   the deleted record: an IndexedDB engine may keep deleted bytes in its own files until it
///   compacts them. The key database, shared by the origin's stores, stays. Within a store,
///   each commit
///   rewrites one file whole, so a deleted value is overwritten in both files within two
///   commits. The browser cannot promise that the disk blocks it frees are erased.
/// - **Sizes:** `WriteBatch` refuses an oversized batch, and `commit_if` checks
///   `check_owned_batch` first. A commit whose encoded state would be over 256 MiB, or hold
///   more than 2^20 entries, is refused with [`StoreError::Full`].
///
/// # Failures
///
/// A commit that fails before its new file is complete applies nothing: [`StoreError::Full`]
/// for the origin's quota, [`StoreError::Unavailable`] for any other failure. A write or cut
/// that fails is checked by reading the new file back: if it holds the complete new state, or
/// cannot be read, the outcome is unknown. So is that of a failed flush of a complete file.
/// The commit then returns [`StoreError::Unknown`], and since memory and disk may disagree,
/// the next call first
/// flushes both files again and reads them through the handles the store holds, as opening
/// does, and serves the call from what it finds: the new state if its file is valid once
/// flushed, the old one if not. Owners and their leases are kept. While that flush or read
/// fails, the call fails with it: [`StoreError::Unavailable`] for a file error,
/// [`StoreError::Corrupt`] for files that hold no state an open would accept. After 2^64 - 1
/// commits the generation counter is spent, and a commit is refused with
/// [`StoreError::Other`], writing nothing.
///
/// # `Send` and `Sync`
///
/// The trait needs `Send + Sync`. The store keeps the browser's file handles, which are
/// JavaScript objects, inside a `Mutex`. That is `Send + Sync` without `unsafe` code here only
/// because wasm-bindgen marks a JavaScript value (`JsValue`) as `Send` and `Sync` on targets
/// without threads, and this crate refuses to build with WebAssembly threads. A compile-time
/// assertion below the type checks it.
///
/// # Eviction
///
/// The browser may evict an origin's storage, for example under storage pressure, unless the
/// origin was granted persistent storage (`navigator.storage.persist()`), which the store does
/// not ask for. A store whose files were evicted or removed opens again as a new, empty store,
/// with no error (the first row of the opening table above): a host that expects state must
/// check for it itself.
///
/// Dropping the store closes both files and releases their locks once every lease from
/// [`acquire`](StateStore::acquire) has been dropped too: a lease keeps the store's files
/// open, as the contract's exclusion lasts until its last holder drops.
pub struct OpfsStore {
    store: SlotStore<OpfsFile>,
}

// `StateStore` requires both; see "`Send` and `Sync`" in the type's docs.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<OpfsStore>();
};

impl OpfsStore {
    /// Open the store `name` in this origin's Origin Private File System, with its data key
    /// wrapped by `wrapping`, creating its directory and its two empty files if they are
    /// missing. Opening writes no state; for a new store it writes the key record, durably,
    /// before it returns. What each combination of files and key record opens as is in
    /// "Opening" on the type.
    ///
    /// Await it, in a dedicated worker, before handing the store to the SDK: the store opens no
    /// file and reads no key later. The returned future is not `Send`, since it awaits browser
    /// promises; run it on the worker, for example with `wasm_bindgen_futures::spawn_local`.
    ///
    /// `name` is 1 to 64 ASCII letters, digits, `-` or `_`. It is an opaque name of the host's
    /// choice, kept in the clear by the browser, as the directory's name and the key record's:
    /// it must never be an App ID or anything else derived from an identity.
    ///
    /// # Errors
    ///
    /// An [`OpenError`]: an invalid name, a context without synchronous file access, a store
    /// another worker or tab holds open, files that hold no trustworthy state, a newer or the
    /// earlier unsealed format, sealed data whose key is gone or wrong, or a browser error.
    pub async fn open(name: &str, wrapping: WrappingKey) -> Result<OpfsStore, OpenError> {
        check_name(name)?;
        check_wrapping_key(&wrapping)?;
        // The files first: their exclusive locks keep every other worker and tab out of the
        // store while its key record is read or written, so two of them cannot both create a
        // key for it.
        let mut files = open_files(name).await?;
        // Then what the files show without a key: a slot of the unsealed format is refused on
        // its header, before any key is touched.
        let inspection = SlotStore::inspect(&mut files)?;
        if inspection == Inspection::UnsealedFormat {
            return Err(OpenError::UnsealedFormat);
        }
        let keys = KeyRecords::new()?;
        let sealer = match keys.read(name).await? {
            // A newer build may keep its key differently: neither lost nor to be replaced.
            Record::Newer(version) => return Err(OpenError::NewerFormat { version }),
            // A new store: its key record is written durably before the first commit can write
            // a snapshot, so sealed data never exists without its key.
            Record::Missing if inspection == Inspection::Empty => {
                keys.create(name, &wrapping).await?
            }
            // With files present, a missing or malformed record is never replaced: that would
            // make a new store over sealed data and lose it.
            Record::Missing | Record::Malformed => return Err(inspection.key_lost_error()),
            // With no files, a record that does not unwrap is not replaced either.
            Record::Present(record) => match keys.unwrap(record, &wrapping).await {
                Err(OpenError::KeyLost) => return Err(inspection.key_lost_error()),
                unwrapped => unwrapped?,
            },
        };
        Ok(OpfsStore {
            store: SlotStore::open(files, sealer)?,
        })
    }

    /// Delete the store `name`: first its key record in IndexedDB, then its directory and both
    /// files in the Origin Private File System.
    ///
    /// The key goes first, so whatever file removal leaves behind can no longer be opened. A
    /// missing record or directory is not an error, so calling `destroy` again after a failure
    /// finishes the deletion, and calling it for a store that does not exist succeeds. Once it
    /// returns `Ok`, nothing of the store is left in the origin's OPFS or IndexedDB, and
    /// opening the name again makes a new store, with a new data key and store identifier.
    /// Whether the browser erases the disk blocks it frees is up to the engine.
    ///
    /// The key database itself, which holds the records of every store of the origin, stays,
    /// even when it holds no record any more. Deleting it could delete the record of a store
    /// that another worker or tab creates at the same moment: IndexedDB lets that store's
    /// write finish, then deletes the database with it, and the new store's sealed data would
    /// have no key. An empty key database holds no key and no data.
    ///
    /// The files' locks are released before the directory is removed, as the browser removes
    /// no file that is open. If another worker or tab opens the name in that moment, it finds
    /// no record: with sealed data in the files it fails with [`OpenError::KeyLost`], and with
    /// empty files it makes a new store. If it drops that new store again before the removal,
    /// the removal deletes the new store's files and leaves its record, which the next open of
    /// the name takes as a record without files. Do not destroy a store that another context
    /// may open at the same time.
    ///
    /// Drop the store, and every lease it granted, first, and await this in a dedicated
    /// worker, as for [`OpfsStore::open`]. The store's files are taken with their exclusive
    /// locks before the record is deleted (created first, if missing, so that they can be
    /// locked), so a store that this or another worker or tab holds open, or that a lease still
    /// keeps open, is never touched.
    ///
    /// # Errors
    ///
    /// [`OpenError::InvalidName`], [`OpenError::NotDedicatedWorker`], [`OpenError::InUse`]
    /// for a store still open or kept open by a lease, or [`OpenError::Unavailable`] when the
    /// browser refuses a call. A removal of the directory refused because another context opened the store in the
    /// moment described above is `InUse` in Chromium and Firefox; what WebKit reports there is
    /// not tested, and may be `Unavailable`. Either way the record is already deleted, and
    /// calling `destroy` again once that store and its leases are dropped finishes the
    /// deletion.
    pub async fn destroy(name: &str) -> Result<(), OpenError> {
        check_name(name)?;
        // Holding both files' locks while the record is deleted proves that no one has the
        // store open, and keeps it so until the record is gone. Missing files are created to be
        // locked: otherwise another worker or tab could open the store, and create its record,
        // in the meantime, and this would delete that new store's key.
        let files = open_files(name).await?;
        KeyRecords::new()?.delete(name).await?;
        // The files must be closed before their directory can be removed. The root is fetched
        // first, so nothing is awaited between releasing the locks and asking for the removal,
        // which keeps the window described above as short as it can be.
        let root = opfs_root().await?;
        drop(files);
        let options = FileSystemRemoveOptions::new();
        options.set_recursive(true);
        match JsFuture::from(root.remove_entry_with_options(name, &options)).await {
            Ok(_) => Ok(()),
            Err(error) => match error_name(&error).as_str() {
                "NotFoundError" => Ok(()),
                "NoModificationAllowedError" => Err(OpenError::InUse),
                _ => Err(unavailable(error)),
            },
        }
    }
}

impl fmt::Debug for OpfsStore {
    // The store holds signed artifacts, private records and its data key: nothing of it is
    // shown.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OpfsStore(..)")
    }
}

impl StateStore for OpfsStore {
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

/// The names of the two slot files inside the store's directory.
const SLOT_FILE_NAMES: [&str; 2] = ["a", "b"];

/// Open, creating them if they are missing, the store's directory `name` in this origin's OPFS
/// root and its two slot files, each with a synchronous access handle. The caller has checked
/// `name`.
async fn open_files(name: &str) -> Result<[OpfsFile; 2], OpenError> {
    let root = opfs_root().await?;
    let options = FileSystemGetDirectoryOptions::new();
    options.set_create(true);
    let directory: FileSystemDirectoryHandle =
        resolve(root.get_directory_handle_with_options(name, &options))
            .await
            .map_err(unavailable)?;
    // If the second file fails to open, dropping the first closes it again.
    let a = open_file(&directory, SLOT_FILE_NAMES[0]).await?;
    let b = open_file(&directory, SLOT_FILE_NAMES[1]).await?;
    Ok([a, b])
}

/// This origin's OPFS root, from a dedicated worker with synchronous access handles.
///
/// The check for a dedicated worker comes before any OPFS call, so a page or another kind of
/// worker gets [`OpenError::NotDedicatedWorker`] in every engine rather than an engine's error.
async fn opfs_root() -> Result<FileSystemDirectoryHandle, OpenError> {
    let global = js_sys::global();
    let scope = global
        .dyn_ref::<DedicatedWorkerGlobalScope>()
        .ok_or(OpenError::NotDedicatedWorker)?;
    // An engine, or an insecure context, without synchronous access handles.
    let has_sync_handles =
        js_sys::Reflect::has(&global, &JsValue::from_str("FileSystemSyncAccessHandle"))
            .unwrap_or(false);
    if !has_sync_handles {
        return Err(OpenError::NotDedicatedWorker);
    }
    resolve(scope.navigator().storage().get_directory())
        .await
        .map_err(unavailable)
}

/// Open one slot file, creating it if it is missing, with an exclusive synchronous access
/// handle.
async fn open_file(
    directory: &FileSystemDirectoryHandle,
    name: &str,
) -> Result<OpfsFile, OpenError> {
    let options = FileSystemGetFileOptions::new();
    options.set_create(true);
    let file = resolve(directory.get_file_handle_with_options(name, &options))
        .await
        .map_err(unavailable)?;
    lock_file(directory, name, file).await
}

/// Take the exclusive synchronous access handle of the slot file `name`. The `mode` option is
/// never passed: engines treat it differently, and only the default mode is exclusive in all of
/// them.
async fn lock_file(
    directory: &FileSystemDirectoryHandle,
    name: &str,
    file: FileSystemFileHandle,
) -> Result<OpfsFile, OpenError> {
    match resolve::<FileSystemSyncAccessHandle>(file.create_sync_access_handle()).await {
        Ok(handle) => Ok(OpfsFile { handle }),
        Err(error) => Err(refused_handle(directory, name, &error).await),
    }
}

/// Why the browser refused a synchronous access handle on the slot file `file_name`.
///
/// Another handle holding the file's lock is `NoModificationAllowedError` in Chromium and
/// Firefox, and `InvalidStateError` in WebKit. Chromium and Firefox use `InvalidStateError` for
/// other refusals too, so that name counts as the lock only if the file entry still exists when
/// it is looked up again right after; otherwise it is [`OpenError::Unavailable`]. The user
/// agent is not consulted. An engine that gives `InvalidStateError` for another reason while
/// the entry exists still reads as in use.
async fn refused_handle(
    directory: &FileSystemDirectoryHandle,
    file_name: &str,
    error: &JsValue,
) -> OpenError {
    match error_name(error).as_str() {
        "NoModificationAllowedError" => OpenError::InUse,
        "InvalidStateError"
            if resolve::<FileSystemFileHandle>(directory.get_file_handle(file_name))
                .await
                .is_ok() =>
        {
            OpenError::InUse
        }
        name => named(name),
    }
}

/// Await `promise` and check that it resolved to a `T`.
pub(super) async fn resolve<T: JsCast>(promise: Promise) -> Result<T, JsValue> {
    JsFuture::from(promise)
        .await?
        .dyn_into::<T>()
        .map_err(|_| js_sys::TypeError::new("unexpected value").into())
}

pub(super) fn unavailable(error: JsValue) -> OpenError {
    named(&error_name(&error))
}

/// [`OpenError::Unavailable`] with the error name or description `name`.
pub(super) fn named(name: &str) -> OpenError {
    OpenError::Unavailable {
        name: name.to_owned(),
    }
}

/// The name of a JavaScript error, such as `QuotaExceededError`. Only the name is kept: it is
/// one of a fixed set the platform defines, while a message is free text.
pub(super) fn error_name(error: &JsValue) -> String {
    if let Some(exception) = error.dyn_ref::<DomException>() {
        exception.name()
    } else if let Some(error) = error.dyn_ref::<js_sys::Error>() {
        error.name().into()
    } else {
        "a value that is not an error".to_owned()
    }
}

fn file_error(error: JsValue) -> FileError {
    match error_name(&error).as_str() {
        "QuotaExceededError" => FileError::Quota,
        name => FileError::Failed(name.to_owned()),
    }
}

/// Read and write options at offset 0.
fn at_start() -> FileSystemReadWriteOptions {
    let options = FileSystemReadWriteOptions::new();
    options.set_at(0.0);
    options
}

/// One slot file on an OPFS synchronous access handle, which holds the file's exclusive lock
/// until it is closed. Dropping it closes the handle.
struct OpfsFile {
    // A JavaScript object, which is `Send` and `Sync` here only because wasm-bindgen marks
    // `JsValue` so when the target has no threads, and this crate refuses a threaded build.
    handle: FileSystemSyncAccessHandle,
}

impl SlotFile for OpfsFile {
    fn size(&mut self) -> Result<u64, FileError> {
        let size = self.handle.get_size().map_err(file_error)?;
        // A file size is a whole number of bytes below 2^53, which a `u64` holds exactly.
        if size.is_finite() && size >= 0.0 && size.fract() == 0.0 && size <= 9_007_199_254_740_992.0
        {
            Ok(size as u64)
        } else {
            Err(FileError::Failed(
                "the file size is not a byte count".into(),
            ))
        }
    }

    fn read_all(&mut self, len: usize) -> Result<Vec<u8>, FileError> {
        let mut bytes = vec![0; len];
        let read = self
            .handle
            .read_with_u8_array_and_options(&mut bytes, &at_start())
            .map_err(file_error)?;
        if read != len as f64 {
            return Err(FileError::Failed("short read".into()));
        }
        Ok(bytes)
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), FileError> {
        let written = self
            .handle
            .write_with_u8_array_and_options(bytes, &at_start())
            .map_err(file_error)?;
        if written != bytes.len() as f64 {
            return Err(FileError::Failed("short write".into()));
        }
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> Result<(), FileError> {
        // Slot lengths are far below 2^53, so the conversion is exact.
        self.handle
            .truncate_with_f64(len as f64)
            .map_err(file_error)
    }

    fn flush(&mut self) -> Result<(), FileError> {
        self.handle.flush().map_err(file_error)
    }
}

impl Drop for OpfsFile {
    fn drop(&mut self) {
        // Releases the file's lock, so the store can be opened again. No flush is needed: every
        // commit flushed before it returned.
        self.handle.close();
    }
}
