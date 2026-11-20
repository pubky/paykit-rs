//! What the store targets share about the Origin Private File System and the store's key
//! records: fresh store names, the origin's OPFS root, the raw bytes of a store's files, a
//! store's key record in IndexedDB, a host's wrapping key, and removing a store with its key
//! record.
//!
//! Each store target includes this module; the items a target does not use allow `dead_code`
//! there.

use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use paykit_browser::store::OpfsStore;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AesGcmParams, AesKeyGenParams, Crypto, CryptoKey, DomException, FileSystemDirectoryHandle,
    FileSystemFileHandle, FileSystemReadWriteOptions, FileSystemRemoveOptions,
    FileSystemSyncAccessHandle, StorageManager, SubtleCrypto,
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

/// The offsets of a slot's store identifier and nonce (`src/store/format.rs`).
pub const STORE_ID_AT: usize = 20;
pub const NONCE_AT: usize = 36;

/// The store identifier in a slot's header.
pub fn store_id(slot: &[u8]) -> &[u8] {
    assert_eq!(&slot[..8], b"PKBSTORE", "not a slot");
    &slot[STORE_ID_AT..NONCE_AT]
}

/// Remove the store `name` with `OpfsStore::destroy`: its key record in IndexedDB, then its
/// directory and both files; and check that both are gone. Every store must be removed:
/// WebKit, as Playwright runs it on macOS, keeps every origin's OPFS in one folder of the
/// user's Library, whatever the profile, so a store left behind stays on the machine. The
/// store must be dropped first, as its open files hold the directory. The key database, which
/// holds the records of every store of the origin, stays, as `destroy` keeps it.
pub async fn remove_store(name: &str) {
    OpfsStore::destroy(name).await.expect("remove the store");
    let root = root().await;
    assert!(
        !exists(&root, name).await,
        "the store {name} is still there"
    );
    assert!(
        key_record(name).await.is_none(),
        "the key record of the store {name} is still there"
    );
}

/// Remove the directory of the store `name`, with both its files, from the OPFS root, leaving
/// its key record. The store must be closed.
pub async fn remove_directory(name: &str) {
    let options = FileSystemRemoveOptions::new();
    options.set_recursive(true);
    JsFuture::from(root().await.remove_entry_with_options(name, &options))
        .await
        .expect("remove the store's directory");
}

/// The bytes of both files of the store `name`, `a` then `b`. The store must be closed.
pub async fn read_files(name: &str) -> [Vec<u8>; 2] {
    [read_file(name, "a").await, read_file(name, "b").await]
}

/// The IndexedDB database of the stores' key records, and its object store, as
/// `src/store/keys.rs` names them.
const KEY_DATABASE: &str = "paykit-store-keys";
const KEY_OBJECT_STORE: &str = "keys";

/// One operation on the key database `databaseName`, run by JavaScript: `get`, `delete`
/// or `put` the record `name` in the object store `storeName` (`put` writes `value`).
/// Every call opens its own connection and closes it before it returns, as the store does. A
/// database that does not exist yet is not created: its upgrade is aborted, and it reads as
/// holding no record.
const KEY_RECORD_JS: &str = r#"
return (async () => {
  const database = await new Promise((resolve, reject) => {
    const request = indexedDB.open(databaseName);
    request.onupgradeneeded = () => request.transaction.abort();
    request.onsuccess = () => resolve(request.result);
    request.onerror = (event) => {
      event.preventDefault();
      if (request.error && request.error.name === "AbortError") resolve(null);
      else reject(request.error);
    };
    request.onblocked = () => reject(new Error("opening the key database is blocked"));
  });
  if (database === null) return undefined;
  try {
    const writes = op === "delete" || op === "put";
    const transaction = database.transaction(storeName, writes ? "readwrite" : "readonly");
    const store = transaction.objectStore(storeName);
    const request =
      op === "get" ? store.get(name)
      : op === "delete" ? store.delete(name)
      : store.put(value, name);
    return await new Promise((resolve, reject) => {
      let result;
      request.onsuccess = () => { result = request.result; };
      transaction.oncomplete = () => resolve(result);
      transaction.onerror = (event) => {
        event.preventDefault();
        reject(transaction.error || request.error);
      };
      transaction.onabort = () => reject(transaction.error || new Error("aborted"));
    });
  } finally {
    database.close();
  }
})();
"#;

/// Run [`KEY_RECORD_JS`] for `op` on the record `name`.
async fn key_record_op(op: &str, name: &str, value: &JsValue) -> JsValue {
    let function =
        Function::new_with_args("databaseName, storeName, op, name, value", KEY_RECORD_JS);
    let args = Array::of5(
        &JsValue::from_str(KEY_DATABASE),
        &JsValue::from_str(KEY_OBJECT_STORE),
        &JsValue::from_str(op),
        &JsValue::from_str(name),
        value,
    );
    let promise: Promise = function
        .apply(&JsValue::UNDEFINED, &args)
        .expect("call the key record function")
        .dyn_into()
        .expect("the key record function returns a promise");
    JsFuture::from(promise)
        .await
        .unwrap_or_else(|error| panic!("{op} the key record of {name}: {error:?}"))
}

/// A store's key record in IndexedDB, field by field, as `src/store/keys.rs` writes it.
pub struct KeyRecord {
    /// The record format number.
    pub format: f64,
    /// The store identifier, the associated data of the wrap.
    pub store_id: Vec<u8>,
    /// The AES-GCM IV of the wrap.
    pub iv: Vec<u8>,
    /// The wrapped data key, with its AES-GCM tag.
    pub wrapped: Vec<u8>,
    /// The wrapping key, which only a record of origin-held mode has.
    pub wrapping_key: Option<CryptoKey>,
    /// The record as IndexedDB returned it.
    pub raw: JsValue,
}

impl KeyRecord {
    /// The record's fields that are bytes or numbers, to compare two reads of it. The wrapping
    /// key is a new object on every read, so only its presence is compared.
    pub fn fields(&self) -> (f64, &[u8], &[u8], &[u8], bool) {
        (
            self.format,
            &self.store_id,
            &self.iv,
            &self.wrapped,
            self.wrapping_key.is_some(),
        )
    }
}

fn field(record: &JsValue, name: &str) -> JsValue {
    Reflect::get(record, &JsValue::from_str(name)).expect("read a field of the key record")
}

/// The bytes of the field `name` of `record`, which must be a `Uint8Array`.
pub fn byte_field(record: &JsValue, name: &str) -> Vec<u8> {
    field(record, name)
        .dyn_into::<Uint8Array>()
        .unwrap_or_else(|_| panic!("the key record's {name} is a Uint8Array"))
        .to_vec()
}

/// The key record of the store `name`, or `None` when it has none.
///
/// Reading a record of origin-held mode reads its `CryptoKey`, which WebKit on macOS unwraps
/// with a Keychain key, asking the user: run such a read only in Chromium and Firefox.
pub async fn key_record(name: &str) -> Option<KeyRecord> {
    let raw = key_record_op("get", name, &JsValue::UNDEFINED).await;
    if raw.is_undefined() {
        return None;
    }
    let has_key = Reflect::has(&raw, &JsValue::from_str("wrappingKey")).unwrap();
    let wrapping_key = has_key.then(|| {
        field(&raw, "wrappingKey")
            .dyn_into::<CryptoKey>()
            .expect("the key record's wrappingKey is a CryptoKey")
    });
    Some(KeyRecord {
        format: field(&raw, "format")
            .as_f64()
            .expect("the key record's format is a number"),
        store_id: byte_field(&raw, "storeId"),
        iv: byte_field(&raw, "iv"),
        wrapped: byte_field(&raw, "wrappedKey"),
        wrapping_key,
        raw,
    })
}

/// Delete the key record of the store `name`, leaving its files.
pub async fn delete_key_record(name: &str) {
    key_record_op("delete", name, &JsValue::UNDEFINED).await;
}

/// Write `record` as the key record of the store `name`.
pub async fn put_key_record(name: &str, record: &Object) {
    key_record_op("put", name, record).await;
}

/// The global scope's WebCrypto, read through `Reflect` like `navigator.storage` above.
fn subtle() -> SubtleCrypto {
    Reflect::get(&js_sys::global(), &JsValue::from_str("crypto"))
        .expect("the global scope has crypto")
        .dyn_into::<Crypto>()
        .expect("crypto is a Crypto")
        .subtle()
}

/// A new host wrapping key: a non-extractable AES-GCM 256-bit `CryptoKey` for encrypting and
/// decrypting, as a host would make for `WrappingKey::Host`. It is kept in memory only.
pub async fn host_key() -> CryptoKey {
    generate_key("AES-GCM", &["encrypt", "decrypt"]).await
}

/// A new non-extractable 256-bit `CryptoKey` of the AES `algorithm` with `usages`, kept in
/// memory only.
pub async fn generate_key(algorithm: &str, usages: &[&str]) -> CryptoKey {
    let algorithm = AesKeyGenParams::new(algorithm, 256);
    let usages: Array = usages
        .iter()
        .map(|usage| JsValue::from_str(usage))
        .collect();
    resolve(
        subtle()
            .generate_key_with_object(&algorithm, false, &usages)
            .expect("generate a key"),
    )
    .await
}

/// The data key in `record`, unwrapped with `key` exactly as the store unwraps it: AES-GCM
/// with the record's IV and its store identifier as associated data.
pub async fn unwrap_data_key(record: &KeyRecord, key: &CryptoKey) -> Vec<u8> {
    let params = AesGcmParams::new("AES-GCM", &Uint8Array::from(&record.iv[..]));
    params.set_additional_data(&Uint8Array::from(&record.store_id[..]));
    let unwrapped: ArrayBuffer = resolve(
        subtle()
            .decrypt_with_object_and_u8_array(&params, key, &record.wrapped)
            .expect("call decrypt"),
    )
    .await;
    Uint8Array::new(&unwrapped).to_vec()
}

/// Whether `subtle.exportKey(format, key)` succeeds; the name of its error when it does not.
pub async fn export_key(key: &CryptoKey, format: &str) -> Result<(), String> {
    let promise = subtle()
        .export_key(format, key)
        .map_err(|error| error_name(&error))?;
    JsFuture::from(promise)
        .await
        .map(|_| ())
        .map_err(|error| error_name(&error))
}

fn error_name(error: &JsValue) -> String {
    if let Some(exception) = error.dyn_ref::<DomException>() {
        exception.name()
    } else if let Some(error) = error.dyn_ref::<js_sys::Error>() {
        error.name().into()
    } else {
        format!("{error:?}")
    }
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
