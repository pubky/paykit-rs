//! The browser side of sealing: [`WrappingKey`], the store's key record in IndexedDB, and the
//! WebCrypto calls that wrap and unwrap its data key.
//!
//! Each store has one record in one IndexedDB database of the origin, whose name holds no
//! identity, under the store's name, which the host already chose as an opaque name. The record
//! holds the data key wrapped with AES-GCM (`subtle.encrypt`) under an AES-GCM wrapping key
//! (256-bit in origin-held mode), with a fresh random 96-bit IV and the 16-byte store
//! identifier as associated data, so a record cannot be moved to another store. In origin-held
//! mode the record also holds the wrapping key itself, a non-extractable `CryptoKey`, which
//! IndexedDB keeps by structured clone; in host-held mode it holds no key. The data key itself
//! is never written: it is in the clear only in WebAssembly memory, inside a [`DataKey`], and
//! inside the cipher of the [`SnapshotSealer`] this module returns.
//!
//! The database is never deleted, even once it holds no record: see
//! [`OpfsStore::destroy`](super::OpfsStore::destroy).
//!
//! Every operation opens its own connection to the database and closes it before it returns,
//! so nothing holds the database open once [`OpfsStore::open`](super::OpfsStore::open) or
//! [`OpfsStore::destroy`](super::OpfsStore::destroy) has returned. A connection also closes
//! itself when another one asks for a newer version (`versionchange`), so it never blocks one.
//!
//! IndexedDB reports through events. Each wait adds its listeners, as Rust closures, and removes
//! them again when the wait ends or its future is dropped, before the closures are freed: no
//! closure is leaked, and none can be called after it was freed. The one exception is an open
//! request this module stops waiting for, because the browser reports it `blocked` or because
//! the future of `open` or `destroy` is dropped while it waits; see [`finish_abandoned_open`].

use std::fmt;

use js_sys::{Array, ArrayBuffer, Function, Object, Promise, Reflect, Uint8Array};
use wasm_bindgen::closure::Closure;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AesGcmParams, AesKeyGenParams, Crypto, CryptoKey, EventTarget, IdbDatabase, IdbFactory,
    IdbObjectStore, IdbOpenDbRequest, IdbRequest, IdbTransaction, SubtleCrypto, WorkerGlobalScope,
};
use zeroize::Zeroizing;

use super::opfs::{error_name, named, resolve, unavailable};
use super::seal::{DataKey, SnapshotSealer, StoreId, KEY_LEN, STORE_ID_LEN, TAG_LEN};
use super::OpenError;

/// Which key wraps a store's data key: an argument of
/// [`OpfsStore::open`](super::OpfsStore::open).
///
/// Every snapshot of a store is sealed under its data key, 32 random bytes made with the store.
/// The store keeps that key only wrapped (encrypted with AES-GCM under the wrapping key) in a
/// record in the origin's IndexedDB. A store opens only with the mode it was created with:
/// opening it with the other mode, or with another host key, fails with
/// [`OpenError::KeyLost`] and writes nothing.
#[derive(Clone)]
#[non_exhaustive]
pub enum WrappingKey {
    /// The origin holds the wrapping key. Creating the store makes a non-extractable AES-GCM
    /// 256-bit `CryptoKey`, which the record keeps next to the wrapped data key, so the store
    /// opens again with no step from the host. Its bytes stay inside WebCrypto: not even this
    /// code can read them.
    Origin,
    /// The host holds the wrapping key: an AES-GCM `CryptoKey` with the `encrypt` and `decrypt`
    /// usages, for example one the host keeps behind its own unlock step. Make it
    /// non-extractable, and keep it outside this store: the store keeps only the wrapped data
    /// key, so without this key the store cannot be opened again. Every later open needs the
    /// same key. A key that is not AES-GCM, or lacks either usage, is refused with
    /// [`OpenError::Unavailable`], whose `name` says so, before anything is opened or written.
    Host(CryptoKey),
}

impl fmt::Debug for WrappingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WrappingKey::Origin => f.write_str("Origin"),
            WrappingKey::Host(_) => f.write_str("Host(..)"),
        }
    }
}

/// The name of the key database. It names no identity or app.
const DATABASE_NAME: &str = "paykit-store-keys";
/// The key database's version; the object store is created in the upgrade to it. It must never
/// change: a build that opened the database at a newer version would leave older builds unable
/// to open it at all (`VersionError`). A different layout takes a new database name instead.
const DATABASE_VERSION: u32 = 1;
/// The object store of key records, keyed by store name.
const OBJECT_STORE: &str = "keys";

/// The format number of a key record this build writes and reads.
const RECORD_FORMAT: u32 = 1;
/// The length of the AES-GCM IV, in bytes.
const IV_LEN: usize = 12;
/// The length of a wrapped data key: the key and the 16-byte AES-GCM tag.
const WRAPPED_LEN: usize = KEY_LEN + TAG_LEN;

// The fields of a key record.
const FORMAT_FIELD: &str = "format";
const STORE_ID_FIELD: &str = "storeId";
const IV_FIELD: &str = "iv";
const WRAPPED_FIELD: &str = "wrappedKey";
const WRAPPING_KEY_FIELD: &str = "wrappingKey";

// web-sys 0.3.107 binds the `transaction` call with options, and `durability`, only behind its
// unstable APIs flag, so this binding is written here, on a local name for `IDBDatabase` (Rust
// allows no new methods on web-sys's type). The options are a plain object.
#[wasm_bindgen]
extern "C" {
    /// An `IDBDatabase`, for the one call web-sys binds only behind its unstable flag.
    #[wasm_bindgen(js_name = IDBDatabase)]
    type DatabaseWithOptions;

    #[wasm_bindgen(method, catch, js_name = transaction)]
    fn transaction_with_options(
        this: &DatabaseWithOptions,
        store_names: &str,
        mode: &str,
        options: &Object,
    ) -> Result<IdbTransaction, JsValue>;
}

/// A store's key record, as read from IndexedDB.
pub(super) enum Record {
    /// The store has no record.
    Missing,
    /// A record of a newer record format than this build reads, which it must not treat as a
    /// lost key: the format number found.
    Newer(u32),
    /// The record lacks a field, or has one of the wrong type or length, or another record
    /// format. It counts as a key that fails to unwrap.
    Malformed,
    /// A well-formed record.
    Present(KeyRecord),
}

/// The fields of a well-formed key record.
pub(super) struct KeyRecord {
    store_id: StoreId,
    iv: [u8; IV_LEN],
    wrapped: [u8; WRAPPED_LEN],
    /// The wrapping key in origin-held mode; `None` in host-held mode.
    wrapping_key: Option<CryptoKey>,
}

/// The worker's IndexedDB and WebCrypto, for the store's key records.
pub(super) struct KeyRecords {
    indexed_db: IdbFactory,
    crypto: Crypto,
}

impl KeyRecords {
    /// The key records of this worker's origin. The caller has checked that it runs in a
    /// dedicated worker.
    pub(super) fn new() -> Result<KeyRecords, OpenError> {
        let scope: WorkerGlobalScope = js_sys::global()
            .dyn_into()
            .map_err(|_| OpenError::NotDedicatedWorker)?;
        let indexed_db = scope
            .indexed_db()
            .map_err(unavailable)?
            .ok_or_else(|| named("IndexedDB is not available"))?;
        let crypto = scope.crypto().map_err(unavailable)?;
        Ok(KeyRecords { indexed_db, crypto })
    }

    fn subtle(&self) -> SubtleCrypto {
        self.crypto.subtle()
    }

    /// The record of the store `name`.
    pub(super) async fn read(&self, name: &str) -> Result<Record, OpenError> {
        let connection = self.connect().await?;
        let transaction = connection
            .database
            .transaction_with_str(OBJECT_STORE)
            .map_err(unavailable)?;
        let request = transaction
            .object_store(OBJECT_STORE)
            .map_err(unavailable)?
            .get(&JsValue::from_str(name))
            .map_err(unavailable)?;
        connection.finish(&request, "success", &transaction).await?;
        Ok(parse(request.result().map_err(unavailable)?))
    }

    /// Create the key record of the new store `name`: a new data key and store identifier, the
    /// data key wrapped under `wrapping` (in origin-held mode, a new wrapping key), written
    /// durably. Returns the new store's sealer once the record is committed.
    pub(super) async fn create(
        &self,
        name: &str,
        wrapping: &WrappingKey,
    ) -> Result<SnapshotSealer, OpenError> {
        let data_key = DataKey::generate().map_err(|_| randomness_unavailable())?;
        let store_id = StoreId::generate().map_err(|_| randomness_unavailable())?;
        // A fresh random IV for every wrap: an IV used twice under one AES-GCM key breaks it.
        let mut iv = [0; IV_LEN];
        self.crypto
            .get_random_values_with_u8_array(&mut iv)
            .map_err(unavailable)?;
        let wrapping_key = match wrapping {
            WrappingKey::Origin => self.generate_wrapping_key().await?,
            WrappingKey::Host(key) => key.clone(),
        };

        // The data key goes to WebCrypto as a view of WebAssembly memory, which `encrypt`
        // copies before it returns: no JavaScript buffer of it is made here.
        let params = aes_gcm_params(&iv, store_id);
        let wrapped: ArrayBuffer = resolve(
            self.subtle()
                .encrypt_with_object_and_u8_array(&params, &wrapping_key, data_key.as_bytes())
                .map_err(unavailable)?,
        )
        .await
        .map_err(unavailable)?;
        let wrapped = Uint8Array::new(&wrapped);
        if wrapped.length() as usize != WRAPPED_LEN {
            return Err(unexpected_value());
        }

        let record = Object::new();
        set_field(&record, FORMAT_FIELD, &JsValue::from(RECORD_FORMAT))?;
        set_field(
            &record,
            STORE_ID_FIELD,
            &Uint8Array::from(&store_id.to_bytes()[..]),
        )?;
        set_field(&record, IV_FIELD, &Uint8Array::from(&iv[..]))?;
        set_field(&record, WRAPPED_FIELD, &wrapped)?;
        // Only the origin's own wrapping key is kept: a host's never is.
        if matches!(wrapping, WrappingKey::Origin) {
            set_field(&record, WRAPPING_KEY_FIELD, &wrapping_key)?;
        }
        self.write(name, &record).await?;
        Ok(SnapshotSealer::new(&data_key, store_id))
    }

    /// Unwrap the data key in `record` with `wrapping`, and return the store's sealer.
    ///
    /// # Errors
    ///
    /// [`OpenError::KeyLost`] when the record is of the other mode, or the key does not open
    /// it (AES-GCM's tag check fails, or the key cannot decrypt with AES-GCM);
    /// [`OpenError::Unavailable`] for any other WebCrypto failure.
    pub(super) async fn unwrap(
        &self,
        record: KeyRecord,
        wrapping: &WrappingKey,
    ) -> Result<SnapshotSealer, OpenError> {
        let key = match (wrapping, record.wrapping_key) {
            (WrappingKey::Origin, Some(key)) => key,
            (WrappingKey::Host(key), None) => key.clone(),
            _ => return Err(OpenError::KeyLost),
        };
        let params = aes_gcm_params(&record.iv, record.store_id);
        let promise = self
            .subtle()
            .decrypt_with_object_and_u8_array(&params, &key, &record.wrapped)
            .map_err(unwrap_failure)?;
        let unwrapped: ArrayBuffer = resolve(promise).await.map_err(unwrap_failure)?;
        let unwrapped = Uint8Array::new(&unwrapped);
        let mut bytes = Zeroizing::new([0; KEY_LEN]);
        let data_key = if unwrapped.length() as usize == KEY_LEN {
            unwrapped.copy_to(bytes.as_mut_slice());
            DataKey::from_bytes(bytes.as_slice())
        } else {
            None
        };
        // The key is in WebAssembly memory now (or the result was not a key): wipe the
        // JavaScript buffer that `decrypt` returned.
        unwrapped.fill(0, 0, unwrapped.length());
        let data_key = data_key.ok_or(OpenError::KeyLost)?;
        Ok(SnapshotSealer::new(&data_key, record.store_id))
    }

    /// Delete the record of the store `name`, durably. A missing record is not an error.
    pub(super) async fn delete(&self, name: &str) -> Result<(), OpenError> {
        self.change(|records| records.delete(&JsValue::from_str(name)))
            .await
    }

    /// Write `record` as the record of the store `name`, durably.
    async fn write(&self, name: &str, record: &Object) -> Result<(), OpenError> {
        self.change(|records| records.put_with_key(record, &JsValue::from_str(name)))
            .await
    }

    /// Make the one change `request` asks of the object store, in its own `readwrite`
    /// transaction with strict durability, and wait for that transaction to complete.
    async fn change(
        &self,
        request: impl FnOnce(&IdbObjectStore) -> Result<IdbRequest, JsValue>,
    ) -> Result<(), OpenError> {
        let connection = self.connect().await?;
        let transaction = connection.strict_readwrite()?;
        request(
            &transaction
                .object_store(OBJECT_STORE)
                .map_err(unavailable)?,
        )
        .map_err(unavailable)?;
        connection
            .finish(&transaction, "complete", &transaction)
            .await
    }

    /// A new non-extractable AES-GCM 256-bit wrapping key, for encrypting and decrypting only.
    async fn generate_wrapping_key(&self) -> Result<CryptoKey, OpenError> {
        let algorithm = AesKeyGenParams::new("AES-GCM", 256);
        let usages = Array::of2(&JsValue::from_str("encrypt"), &JsValue::from_str("decrypt"));
        let promise = self
            .subtle()
            .generate_key_with_object(&algorithm, false, &usages)
            .map_err(unavailable)?;
        resolve(promise).await.map_err(unavailable)
    }

    /// Open a connection to the key database, creating it, with its object store, if it is
    /// missing.
    async fn connect(&self) -> Result<Connection, OpenError> {
        let request = self
            .indexed_db
            .open_with_u32(DATABASE_NAME, DATABASE_VERSION)
            .map_err(unavailable)?;
        let mut upgrade = Listeners::default();
        let upgrading = request.clone();
        upgrade.add(&request, "upgradeneeded", move |_| {
            create_object_store(&upgrading)
        })?;
        // Until it is disarmed, dropping this lets the request finish on its own and close the
        // connection it may still open: after `blocked`, or when this future is dropped.
        let mut pending = PendingOpen(Some(request.clone()));
        let events: &EventTarget = &request;
        let (fired, event) =
            first_event(&[(events, "success"), (events, "error"), (events, "blocked")]).await?;
        drop(upgrade);
        match fired {
            0 => {
                pending.disarm();
                let database: IdbDatabase = request
                    .result()
                    .map_err(unavailable)?
                    .dyn_into()
                    .map_err(|_| unexpected_value())?;
                Connection::new(database)
            }
            1 => {
                pending.disarm();
                Err(event_failure(&event))
            }
            _ => Err(named("IndexedDB blocked")),
        }
    }
}

/// An open request of the key database that this module may stop waiting for. Dropped while
/// still armed, it hands the request to [`finish_abandoned_open`].
struct PendingOpen(Option<IdbOpenDbRequest>);

impl PendingOpen {
    /// The request was seen to the end: nothing is left to finish.
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for PendingOpen {
    fn drop(&mut self) {
        if let Some(request) = &self.0 {
            finish_abandoned_open(request);
        }
    }
}

/// One connection to the key database, closed when dropped.
struct Connection {
    database: IdbDatabase,
    /// Closes the connection when another one asks for a newer version.
    _on_versionchange: Listeners,
}

impl Connection {
    fn new(database: IdbDatabase) -> Result<Connection, OpenError> {
        let mut on_versionchange = Listeners::default();
        let closing = database.clone();
        if let Err(error) =
            on_versionchange.add(&database, "versionchange", move |_| closing.close())
        {
            // No `Connection` closes it on drop yet.
            database.close();
            return Err(error);
        }
        Ok(Connection {
            database,
            _on_versionchange: on_versionchange,
        })
    }

    /// A `readwrite` transaction on the object store with `durability: "strict"`, which asks
    /// the browser to fire `complete` only once the changes are on the storage device. Whether
    /// an engine does so before a power loss cannot be tested here.
    fn strict_readwrite(&self) -> Result<IdbTransaction, OpenError> {
        let options = Object::new();
        set_field(&options, "durability", &JsValue::from_str("strict"))?;
        self.database
            .unchecked_ref::<DatabaseWithOptions>()
            .transaction_with_options(OBJECT_STORE, "readwrite", &options)
            .map_err(unavailable)
    }

    /// Wait for `target`, a request or `transaction` itself, to fire `success`. Only that event
    /// is success: an `error` of `target` (for a transaction, also one bubbled up from a request
    /// in it), an `abort` of `transaction` or the connection closing is a failure. A write
    /// passes the transaction and `complete`, so it succeeds only once the transaction is
    /// committed.
    async fn finish(
        &self,
        target: &EventTarget,
        success: &'static str,
        transaction: &IdbTransaction,
    ) -> Result<(), OpenError> {
        let (transaction, database): (&EventTarget, &EventTarget) = (transaction, &self.database);
        let (fired, event) = first_event(&[
            (target, success),
            (target, "error"),
            (transaction, "abort"),
            (database, "close"),
        ])
        .await?;
        match fired {
            0 => Ok(()),
            1 | 2 => Err(event_failure(&event)),
            _ => Err(closed()),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // A transaction still running finishes first: closing only stops new ones.
        self.database.close();
    }
}

/// Event listeners added by this module, removed again when dropped. Each closure is freed
/// only after its listener is removed, so the browser cannot call a freed closure.
#[derive(Default)]
struct Listeners(Vec<Listener>);

/// One event listener: the target, the event's name and the closure that handles it.
struct Listener {
    target: EventTarget,
    event: &'static str,
    handler: Closure<dyn FnMut(JsValue)>,
}

impl Listeners {
    fn add(
        &mut self,
        target: &EventTarget,
        event: &'static str,
        handler: impl FnMut(JsValue) + 'static,
    ) -> Result<(), OpenError> {
        let handler = Closure::<dyn FnMut(JsValue)>::new(handler);
        target
            .add_event_listener_with_callback(event, handler.as_ref().unchecked_ref())
            .map_err(unavailable)?;
        self.0.push(Listener {
            target: target.clone(),
            event,
            handler,
        });
        Ok(())
    }
}

impl Drop for Listeners {
    fn drop(&mut self) {
        for listener in &self.0 {
            // Removing a listener fails only for arguments of the wrong type.
            let _ = listener.target.remove_event_listener_with_callback(
                listener.event,
                listener.handler.as_ref().unchecked_ref(),
            );
        }
    }
}

/// Wait for the first of `events`, each an event name on a target, and return its index and
/// the event. The listeners are removed when it returns, or when its future is dropped.
async fn first_event(
    events: &[(&EventTarget, &'static str)],
) -> Result<(usize, JsValue), OpenError> {
    let mut listeners = Listeners::default();
    let mut added = Ok(());
    let promise = Promise::new(&mut |resolve: Function, _reject: Function| {
        for (index, (target, event)) in events.iter().enumerate() {
            let resolve = resolve.clone();
            let listening = listeners.add(target, event, move |event| {
                let fired = Array::of2(&JsValue::from(index), &event);
                // Resolving a promise again does nothing, so a later event is ignored.
                let _ = resolve.call1(&JsValue::UNDEFINED, &fired);
            });
            if listening.is_err() {
                added = listening;
                return;
            }
        }
    });
    added?;
    let fired: Array = JsFuture::from(promise)
        .await
        .map_err(unavailable)?
        .dyn_into()
        .map_err(|_| unexpected_value())?;
    drop(listeners);
    let index = fired
        .get(0)
        .as_f64()
        .map(|index| index as usize)
        .filter(|index| *index < events.len())
        .ok_or_else(unexpected_value)?;
    Ok((index, fired.get(1)))
}

/// Create the object store in the upgrade to version 1, which only a new database gets. If that
/// fails, the upgrade is aborted, so the open request fails rather than leave a database at
/// version 1 with no object store.
fn create_object_store(request: &IdbOpenDbRequest) {
    let created = request
        .result()
        .ok()
        .and_then(|database| database.dyn_into::<IdbDatabase>().ok())
        .is_some_and(|database| database.create_object_store(OBJECT_STORE).is_ok());
    if !created {
        if let Some(transaction) = request.transaction() {
            let _ = transaction.abort();
        }
    }
}

/// Let an open request that this module no longer waits for finish on its own, so no
/// connection is left open: a request that has already succeeded has its connection closed at
/// once; otherwise, if it later upgrades, it creates the object store, and if it later
/// succeeds, it closes the connection at once. These two handlers are the only closures this
/// module hands over to the browser: each frees itself when called, and one that is never
/// called (the request fails instead, or the worker ends first) stays allocated. That happens
/// only after a `blocked` event, which another connection holding the database at an older
/// version would cause (this database has only version 1), or when the future of `open` or
/// `destroy` is dropped while it waits for the database.
fn finish_abandoned_open(request: &IdbOpenDbRequest) {
    // `result` throws until the request is done; once done, it holds the connection, if any.
    if let Ok(result) = request.result() {
        if let Ok(database) = result.dyn_into::<IdbDatabase>() {
            database.close();
        }
        return;
    }
    let upgrading = request.clone();
    let on_upgrade = Closure::once_into_js(move |_: JsValue| create_object_store(&upgrading));
    request.set_onupgradeneeded(Some(on_upgrade.unchecked_ref()));
    let opened: IdbRequest = request.clone().into();
    let on_success = Closure::once_into_js(move |_: JsValue| {
        if let Ok(database) = opened.result() {
            if let Ok(database) = database.dyn_into::<IdbDatabase>() {
                database.close();
            }
        }
    });
    request.set_onsuccess(Some(on_success.unchecked_ref()));
}

/// Check a host's wrapping key before anything is opened: an AES-GCM key with both the
/// `encrypt` and `decrypt` usages. Without the check, a key that encrypts but cannot decrypt
/// would create a store that every later open reports as [`OpenError::KeyLost`].
pub(super) fn check_wrapping_key(wrapping: &WrappingKey) -> Result<(), OpenError> {
    let WrappingKey::Host(key) = wrapping else {
        return Ok(());
    };
    let algorithm = key.algorithm().map_err(unavailable)?;
    let aes_gcm = Reflect::get(&algorithm, &JsValue::from_str("name"))
        .ok()
        .and_then(|name| name.as_string())
        .is_some_and(|name| name == "AES-GCM");
    let usages = key.usages();
    let has = |usage: &str| {
        usages
            .iter()
            .any(|granted| granted.as_string().is_some_and(|granted| granted == usage))
    };
    if aes_gcm && has("encrypt") && has("decrypt") {
        Ok(())
    } else {
        Err(named(
            "the host's wrapping key is not an AES-GCM key for encrypt and decrypt",
        ))
    }
}

/// The record in `value`, as IndexedDB returned it: `undefined` for a missing record.
fn parse(value: JsValue) -> Record {
    if value.is_undefined() {
        return Record::Missing;
    }
    let field = |name: &str| Reflect::get(&value, &JsValue::from_str(name)).ok();
    let format = field(FORMAT_FIELD).and_then(|format| format.as_f64());
    let store_id = field(STORE_ID_FIELD).and_then(fixed_bytes::<STORE_ID_LEN>);
    let iv = field(IV_FIELD).and_then(fixed_bytes::<IV_LEN>);
    let wrapped = field(WRAPPED_FIELD).and_then(fixed_bytes::<WRAPPED_LEN>);
    // Absent in host-held mode; anything else but a `CryptoKey` is malformed.
    let wrapping_key = match field(WRAPPING_KEY_FIELD) {
        Some(key) if key.is_undefined() => Some(None),
        Some(key) => key.dyn_into::<CryptoKey>().ok().map(Some),
        None => None,
    };
    if !value.is_object() {
        return Record::Malformed;
    }
    match format {
        Some(format) if format == f64::from(RECORD_FORMAT) => {}
        Some(format)
            if format > f64::from(RECORD_FORMAT)
                && format <= f64::from(u32::MAX)
                && format.fract() == 0.0 =>
        {
            // A whole number in `u32`'s range, so the conversion is exact.
            return Record::Newer(format as u32);
        }
        _ => return Record::Malformed,
    }
    match (store_id, iv, wrapped, wrapping_key) {
        (Some(store_id), Some(iv), Some(wrapped), Some(wrapping_key)) => {
            Record::Present(KeyRecord {
                store_id: StoreId::from_bytes(store_id),
                iv,
                wrapped,
                wrapping_key,
            })
        }
        _ => Record::Malformed,
    }
}

/// The bytes of `value` if it is a `Uint8Array` of exactly `N` bytes.
fn fixed_bytes<const N: usize>(value: JsValue) -> Option<[u8; N]> {
    let array = value.dyn_into::<Uint8Array>().ok()?;
    if array.length() as usize != N {
        return None;
    }
    let mut bytes = [0; N];
    array.copy_to(&mut bytes);
    Some(bytes)
}

/// AES-GCM parameters with `iv`, and the store identifier as associated data, with the default
/// 128-bit tag.
fn aes_gcm_params(iv: &[u8; IV_LEN], store_id: StoreId) -> AesGcmParams {
    let params = AesGcmParams::new("AES-GCM", &Uint8Array::from(&iv[..]));
    params.set_additional_data(&Uint8Array::from(&store_id.to_bytes()[..]));
    params
}

fn set_field(object: &Object, name: &str, value: &JsValue) -> Result<(), OpenError> {
    Reflect::set(object, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(unavailable)
}

/// Why `decrypt` refused the wrapped key. `OperationError` is AES-GCM's failed tag check (a
/// wrong key, or a changed record), and `InvalidAccessError` a key that cannot decrypt with
/// AES-GCM: either way this key does not open the store. Any other error is the browser's.
fn unwrap_failure(error: JsValue) -> OpenError {
    match error_name(&error).as_str() {
        "OperationError" | "InvalidAccessError" => OpenError::KeyLost,
        name => named(name),
    }
}

/// The failure an `error` or `abort` event reports: the name of its target's `error`, or
/// `AbortError` for an abort with none.
fn event_failure(event: &JsValue) -> OpenError {
    let error = Reflect::get(event, &JsValue::from_str("target"))
        .and_then(|target| Reflect::get(&target, &JsValue::from_str("error")))
        .unwrap_or(JsValue::NULL);
    if error.is_null() || error.is_undefined() {
        named("AbortError")
    } else {
        unavailable(error)
    }
}

/// The connection to the key database closed under a running operation, for example because
/// the origin's storage was cleared.
fn closed() -> OpenError {
    named("IndexedDB closed")
}

fn randomness_unavailable() -> OpenError {
    named("the random number generator failed")
}

fn unexpected_value() -> OpenError {
    named("unexpected value")
}
