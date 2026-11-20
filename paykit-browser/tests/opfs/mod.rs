//! What the store targets share about the Origin Private File System: fresh store names, the
//! origin's OPFS root, the raw bytes of a store's files, and removing a store.
//!
//! Each store target includes this module; the items a target does not use allow `dead_code`
//! there.

use js_sys::{Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DomException, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemReadWriteOptions,
    FileSystemRemoveOptions, FileSystemSyncAccessHandle, StorageManager,
};

/// A store name no other test or run uses: `label`, a dash and 16 random hex digits. A run
/// that stops part way leaves its store behind, and a fixed name would then open that store's
/// state in the next run.
pub fn fresh_name(label: &str) -> String {
    let mut bytes = [0u8; 8];
    getrandom03::fill(&mut bytes).expect("the browser gives random bytes");
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{label}-{hex}")
}

/// Await `promise`, which must resolve to a `T`.
pub async fn resolve<T: JsCast>(promise: Promise) -> T {
    JsFuture::from(promise)
        .await
        .expect("the browser call succeeds")
        .dyn_into()
        .expect("the browser call returns the expected type")
}

/// The origin's OPFS root, or the engine's refusal. `navigator.storage` is read through
/// `Reflect`, so this works in a page and in a worker without bindings for either global.
pub async fn try_root() -> Result<FileSystemDirectoryHandle, JsValue> {
    let navigator = Reflect::get(&js_sys::global(), &JsValue::from_str("navigator"))
        .expect("the global scope has a navigator");
    let storage: StorageManager = Reflect::get(&navigator, &JsValue::from_str("storage"))
        .expect("the navigator has storage")
        .dyn_into()
        .expect("navigator.storage is a StorageManager");
    let root = JsFuture::from(storage.get_directory()).await?;
    Ok(root.dyn_into().expect("the OPFS root is a directory"))
}

/// The origin's OPFS root.
pub async fn root() -> FileSystemDirectoryHandle {
    try_root().await.expect("the browser gives the OPFS root")
}

/// Whether the directory `name` is in `root`. Any refusal other than "not found" fails the
/// test, so a browser error cannot pass for a missing directory.
pub async fn exists(root: &FileSystemDirectoryHandle, name: &str) -> bool {
    match JsFuture::from(root.get_directory_handle(name)).await {
        Ok(_) => true,
        Err(error) => {
            let error_name = error.dyn_ref::<DomException>().map(DomException::name);
            assert_eq!(
                error_name.as_deref(),
                Some("NotFoundError"),
                "look up {name}"
            );
            false
        }
    }
}

/// Remove the store `name`, its directory and both files, and check that it is gone. Every
/// store must be removed: WebKit, as Playwright runs it on macOS, keeps every origin's OPFS in
/// one folder of the user's Library, whatever the profile, so a store left behind stays on the
/// machine. The store must be dropped first, as its open files hold the directory.
pub async fn remove_store(name: &str) {
    let root = root().await;
    let options = FileSystemRemoveOptions::new();
    options.set_recursive(true);
    JsFuture::from(root.remove_entry_with_options(name, &options))
        .await
        .expect("remove the store");
    assert!(
        !exists(&root, name).await,
        "the store {name} is still there"
    );
}

/// A synchronous access handle on the file `file` of the store `name`, which must exist and be
/// closed.
async fn sync_handle(name: &str, file: &str) -> FileSystemSyncAccessHandle {
    let directory: FileSystemDirectoryHandle =
        resolve(root().await.get_directory_handle(name)).await;
    let file: FileSystemFileHandle = resolve(directory.get_file_handle(file)).await;
    resolve(file.create_sync_access_handle()).await
}

/// Read and write options at the byte offset `offset`.
fn options_at(offset: usize) -> FileSystemReadWriteOptions {
    let options = FileSystemReadWriteOptions::new();
    options.set_at(offset as f64);
    options
}

/// The bytes of the file `file` of the store `name`. The store must be closed.
pub async fn read_file(name: &str, file: &str) -> Vec<u8> {
    let handle = sync_handle(name, file).await;
    let size = handle.get_size().expect("the file's size");
    let mut bytes = vec![0; size as usize];
    let read = handle
        .read_with_u8_array_and_options(&mut bytes, &options_at(0))
        .expect("read the file");
    handle.close();
    assert_eq!(read, size, "short read of {file}");
    bytes
}

/// Replace the contents of the file `file` of the store `name` with `bytes`, cutting the file
/// to their length. The store must be closed.
pub async fn write_file(name: &str, file: &str, bytes: &[u8]) {
    let handle = sync_handle(name, file).await;
    let written = handle
        .write_with_u8_array_and_options(bytes, &options_at(0))
        .expect("write the file");
    handle
        .truncate_with_f64(bytes.len() as f64)
        .expect("cut the file");
    handle.flush().expect("flush the file");
    let size = handle.get_size().expect("the file's size");
    handle.close();
    assert_eq!(written, bytes.len() as f64, "short write of {file}");
    assert_eq!(size, bytes.len() as f64, "size of {file}");
}

/// Flip the lowest bit of the byte at `offset` in the file `file` of the store `name`, as
/// damage on disk would. The store must be closed.
pub async fn damage_file(name: &str, file: &str, offset: usize) {
    let handle = sync_handle(name, file).await;
    let mut byte = [0u8; 1];
    let read = handle
        .read_with_u8_array_and_options(&mut byte, &options_at(offset))
        .expect("read the byte");
    if read != 1.0 {
        handle.close();
        panic!("no byte at {offset} in {file}");
    }
    byte[0] ^= 0x01;
    let written = handle
        .write_with_u8_array_and_options(&byte, &options_at(offset))
        .expect("write the byte");
    handle.flush().expect("flush the file");
    handle.close();
    assert_eq!(written, 1.0, "short write of {file}");
}
