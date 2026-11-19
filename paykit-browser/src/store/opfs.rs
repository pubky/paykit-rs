//! The browser's side of the store: [`OpfsStore`], opening its two slot files in the Origin
//! Private File System, and [`OpfsFile`], a [`SlotFile`] on a `FileSystemSyncAccessHandle`.

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
    FileSystemSyncAccessHandle,
};

use super::core::SlotStore;
use super::file::{FileError, SlotFile};
use super::{check_name, OpenError};

/// The Switch client's [`StateStore`] on the browser's Origin Private File System (OPFS), for
/// a host that runs in a dedicated worker.
///
/// **Not for production use.** It writes every value to disk unencrypted, while the
/// [`StateStore`] contract requires the store to be encrypted at rest. A later change adds
/// sealing; the format version leaves room for it.
///
/// # Where it runs
///
/// Open it with [`OpfsStore::open`] in a dedicated worker, and run the SDK and the Switch
/// client in that same worker. The [`StateStore`] calls are synchronous, and only a dedicated
/// worker has the browser's synchronous file handles, so a page or a service worker cannot
/// host the store. Every call runs synchronously on the worker, and a commit rewrites the
/// whole state, so a commit takes time in proportion to everything stored.
///
/// # How it stores
///
/// The store is a directory in the origin's OPFS root, named by the caller, holding two files.
/// Each holds a whole snapshot of the store with a generation number, its length and an
/// integrity tag. A commit encodes the whole new state, writes it over the file that does not
/// hold the current state, cuts that file to the new length when it was longer, flushes it,
/// and only then makes it the current state and returns `Ok`. Opening takes the valid file with
/// the higher generation, and ignores a file whose size or tag is wrong next to it: a torn
/// write or damage, which the next commit overwrites. The tag detects torn and damaged writes,
/// not tampering. Damage to the current file after the fact therefore opens the store at the
/// other file's older state, without the last commit that returned `Ok`: what that commit
/// recorded, such as the outcome of a payment, is then unknown to the SDK. With no valid file, a first commit that stopped part way next to
/// an empty file is a new store, and anything else is [`OpenError::Corrupt`]. A store's only
/// committed file that loses its end after the fact, before a second commit, reads the same as
/// a first commit that stopped part way, and opens as a new store: the two cannot be told
/// apart.
///
/// # The contract's guarantees
///
/// Met:
///
/// - **Atomic:** a commit switches the current file once. A write or cut that stops part way
///   leaves the new file invalid and the old one current.
/// - **Durable before `Ok`:** every commit calls `flush()` on the file before `Ok`. Whether a
///   flush reaches the storage device before a power loss is up to the engine, and cannot be
///   tested here.
/// - **Ordered and consistent:** calls run one at a time under one lock, and reads are served
///   from memory, which takes a commit's state only once it is flushed.
/// - **Exclusive:** both files are held open, each with the browser's exclusive lock, until
///   the store and every lease it granted have been dropped, so no other worker or tab can
///   open it meanwhile ([`OpenError::InUse`]).
///   [`acquire`](StateStore::acquire) and [`commit_if`](StateStore::commit_if) keep owners in
///   memory with the rules of the Switch client's `MemoryStore`.
/// - **Sizes:** `WriteBatch` refuses an oversized batch, and `commit_if` checks
///   `check_owned_batch` first. A commit whose encoded state would be over 256 MiB, or hold
///   more than 2^20 entries, is refused with [`StoreError::Full`].
///
/// Not met yet:
///
/// - **Encrypted at rest:** values are plaintext on disk (above).
/// - **Real deletion:** each commit rewrites one file whole, so a deleted value is overwritten
///   in both files within two commits. The browser cannot promise that the disk blocks it
///   frees are erased.
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
/// with no error: a host that expects state must check for it itself.
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
    /// Open the store `name` in this origin's Origin Private File System, creating its
    /// directory and its two empty files if they are missing. Opening writes no state.
    ///
    /// Await it, in a dedicated worker, before handing the store to the SDK: the store opens no
    /// file later. The returned future is not `Send`, since it awaits browser promises; run it
    /// on the worker, for example with `wasm_bindgen_futures::spawn_local`.
    ///
    /// `name` is 1 to 64 ASCII letters, digits, `-` or `_`. It is an opaque name of the host's
    /// choice, kept in the clear by the browser: it must never be an App ID or anything else
    /// derived from an identity.
    ///
    /// # Errors
    ///
    /// An [`OpenError`]: an invalid name, a context without synchronous file access, a store
    /// another worker or tab holds open, files that hold no trustworthy state or a newer
    /// format, or a browser error.
    pub async fn open(name: &str) -> Result<OpfsStore, OpenError> {
        check_name(name)?;
        let files = open_files(name).await?;
        Ok(OpfsStore {
            store: SlotStore::open(files)?,
        })
    }
}

impl fmt::Debug for OpfsStore {
    // The store holds signed artifacts and private records: nothing of it is shown.
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
///
/// The check for a dedicated worker comes before any OPFS call, so a page or another kind of
/// worker gets [`OpenError::NotDedicatedWorker`] in every engine rather than an engine's error.
async fn open_files(name: &str) -> Result<[OpfsFile; 2], OpenError> {
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

    let root: FileSystemDirectoryHandle = resolve(scope.navigator().storage().get_directory())
        .await
        .map_err(unavailable)?;
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

/// Open one slot file with an exclusive synchronous access handle. The `mode` option is never
/// passed: engines treat it differently, and only the default mode is exclusive in all of them.
async fn open_file(
    directory: &FileSystemDirectoryHandle,
    name: &str,
) -> Result<OpfsFile, OpenError> {
    let options = FileSystemGetFileOptions::new();
    options.set_create(true);
    let file: FileSystemFileHandle =
        resolve(directory.get_file_handle_with_options(name, &options))
            .await
            .map_err(unavailable)?;
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
        name => OpenError::Unavailable {
            name: name.to_owned(),
        },
    }
}

/// Await `promise` and check that it resolved to a `T`.
async fn resolve<T: JsCast>(promise: Promise) -> Result<T, JsValue> {
    JsFuture::from(promise)
        .await?
        .dyn_into::<T>()
        .map_err(|_| js_sys::TypeError::new("unexpected value").into())
}

fn unavailable(error: JsValue) -> OpenError {
    OpenError::Unavailable {
        name: error_name(&error),
    }
}

/// The name of a JavaScript error, such as `QuotaExceededError`. Only the name is kept: it is
/// one of a fixed set the platform defines, while a message is free text.
fn error_name(error: &JsValue) -> String {
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
