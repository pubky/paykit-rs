//! Sealing on the store's real files and key record: what is on disk and in IndexedDB, a lost
//! or wrong key, a record without files, tampered and foreign slots, the earlier unsealed
//! format, and `OpfsStore::destroy`.
//!
//! Most rules are checked twice, in origin-held mode (`WrappingKey::Origin`, tests named
//! `test_store_seal_*`) and with a host's wrapping key (`WrappingKey::Host`, tests named
//! `test_store_host_key_*`). Only the host-key tests store no `CryptoKey` in IndexedDB, which
//! Playwright's WebKit on macOS cannot do without asking the user for Keychain access, so a
//! local WebKit run with `PAYKIT_BROWSER_SKIP_KEYCHAIN_TESTS=1` runs only those
//! (`rig/runner.mjs`).
//!
//! The tests search the raw files and the record for a known marker value and for the data
//! key's bytes. The data key is the store's own: the tests read it by unwrapping the record
//! with its wrapping key through `subtle.decrypt`, the store's construction, and check that it
//! opens the files' sealed payloads before searching for it. Every test removes the stores it
//! created, their key records included.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use js_sys::{Object, Reflect, Uint8Array};
use paykit_browser::store::{OpenError, OpfsStore, WrappingKey};
use pubky_switch_client::store::StateStore;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};
use web_sys::CryptoKey;

use crate::contract::put;
use crate::opening::two_commits;
use crate::opfs::{self, KeyRecord, NONCE_AT, STORE_ID_AT};
use crate::support::{self, GENERATION_AT, PAYLOAD_AT};

/// The value the tests store and then look for in the raw bytes.
const MARKER: &[u8] = b"PLAINTEXT-MARKER-5f3a9c";
/// The length of a data key, a slot's nonce, a store identifier and an AES-GCM IV.
const DATA_KEY_LEN: usize = 32;
const NONCE_LEN: usize = 24;
const STORE_ID_LEN: usize = 16;
const IV_LEN: usize = 12;
/// The length of a wrapped data key: the key and its 16-byte AES-GCM tag.
const WRAPPED_LEN: usize = DATA_KEY_LEN + 16;

fn get(store: &OpfsStore, ns: &str, key: &[u8]) -> Option<Vec<u8>> {
    store.get(ns, key).unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// A new host wrapping key.
async fn host() -> WrappingKey {
    WrappingKey::Host(opfs::host_key().await)
}

/// The modes and keys other than `wrapping` that a test opens with: a new host key, and, for a
/// host-key store, the origin-held mode. Opening a host-key store in origin-held mode reads its
/// record, which holds no `CryptoKey`, and stores none.
async fn wrong_keys(wrapping: &WrappingKey) -> Vec<WrappingKey> {
    match wrapping {
        WrappingKey::Origin => vec![host().await],
        WrappingKey::Host(_) => vec![host().await, WrappingKey::Origin],
        other => panic!("no test covers {other:?}"),
    }
}

async fn open(name: &str, wrapping: &WrappingKey) -> OpfsStore {
    OpfsStore::open(name, wrapping.clone())
        .await
        .unwrap_or_else(|error| panic!("open {name} with {wrapping:?}: {error:?}"))
}

async fn open_error(name: &str, wrapping: &WrappingKey) -> Option<OpenError> {
    OpfsStore::open(name, wrapping.clone()).await.err()
}

/// The key record of the store `name`, which must have one.
async fn record(name: &str) -> KeyRecord {
    opfs::key_record(name)
        .await
        .unwrap_or_else(|| panic!("the store {name} has no key record"))
}

/// The data key in the record of the store `name`, unwrapped with the record's own wrapping
/// key in origin-held mode, or with the host's key.
async fn data_key(name: &str, wrapping: &WrappingKey) -> Vec<u8> {
    let record = record(name).await;
    let key: CryptoKey = match wrapping {
        WrappingKey::Origin => record
            .wrapping_key
            .clone()
            .expect("a record of origin-held mode holds its wrapping key"),
        WrappingKey::Host(key) => key.clone(),
        other => panic!("no test covers {other:?}"),
    };
    opfs::unwrap_data_key(&record, &key).await
}

/// A slot of the earlier unsealed format, version 1, built by hand as an earlier build wrote
/// it: the magic, the version, the generation, the payload's length, the payload in plaintext
/// (`ns`, `k` and the marker, each with its length) and a 32-byte tag.
fn unsealed_slot(generation: u64) -> Vec<u8> {
    let mut payload = Vec::new();
    for part in [&b"ns"[..], b"k", MARKER] {
        payload.extend_from_slice(&u32::try_from(part.len()).unwrap().to_le_bytes());
        payload.extend_from_slice(part);
    }
    let mut slot = b"PKBSTORE".to_vec();
    slot.extend_from_slice(&1u32.to_le_bytes());
    slot.extend_from_slice(&generation.to_le_bytes());
    slot.extend_from_slice(&u64::try_from(payload.len()).unwrap().to_le_bytes());
    slot.extend_from_slice(&payload);
    slot.extend_from_slice(&[0x77; 32]);
    slot
}

// Create, commit, drop, open again and read back; the record's fields.

async fn round_trip(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-round-trip");
    // Opening a new store writes its record, and no snapshot.
    drop(open(&name, &wrapping).await);
    let created = record(&name).await;
    assert_eq!(
        opfs::read_files(&name).await,
        [Vec::<u8>::new(), Vec::new()]
    );
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", MARKER)).unwrap();
    store.commit(put("ns", b"other", b"2")).unwrap();
    drop(store);

    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(MARKER.to_vec()));
    assert_eq!(get(&store, "ns", b"other"), Some(b"2".to_vec()));
    drop(store);

    let record = record(&name).await;
    assert_eq!(record.fields(), created.fields(), "the record changed");
    assert_eq!(record.format, 1.0);
    assert_eq!(record.store_id.len(), STORE_ID_LEN);
    assert_eq!(record.iv.len(), IV_LEN);
    assert_eq!(record.wrapped.len(), WRAPPED_LEN);
    for slot in opfs::read_files(&name).await {
        assert_eq!(opfs::store_id(&slot), record.store_id);
    }
    // Only a record of origin-held mode holds a `CryptoKey`.
    let keys = Object::values(record.raw.unchecked_ref())
        .iter()
        .filter(|value: &JsValue| value.is_instance_of::<CryptoKey>())
        .count();
    match wrapping {
        WrappingKey::Origin => assert_eq!(keys, 1),
        WrappingKey::Host(_) => {
            assert_eq!(keys, 0);
            assert!(record.wrapping_key.is_none());
        }
        other => panic!("no test covers {other:?}"),
    }
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_round_trip_reopens_and_reads_back() {
    round_trip(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_round_trip_reopens_and_reads_back() {
    round_trip(host().await).await;
}

// The stored wrapping key cannot be exported.

#[wasm_bindgen_test]
async fn test_store_seal_wrapping_key_is_not_extractable() {
    let name = opfs::fresh_name("seal-extract");
    let store = open(&name, &WrappingKey::Origin).await;
    store.commit(put("ns", b"k", MARKER)).unwrap();
    drop(store);

    let record = record(&name).await;
    let key = record
        .wrapping_key
        .clone()
        .expect("a record of origin-held mode holds its wrapping key");
    assert!(!key.extractable());
    assert_eq!(key.type_(), "secret");
    let algorithm = key.algorithm().unwrap();
    let algorithm_field = |name: &str| Reflect::get(&algorithm, &JsValue::from_str(name)).unwrap();
    assert_eq!(
        algorithm_field("name").as_string().as_deref(),
        Some("AES-GCM")
    );
    assert_eq!(algorithm_field("length").as_f64(), Some(256.0));
    let mut usages: Vec<String> = key
        .usages()
        .iter()
        .map(|usage| usage.as_string().unwrap())
        .collect();
    usages.sort();
    assert_eq!(usages, ["decrypt", "encrypt"]);
    // The key does what the store needs of it: it unwraps the data key.
    assert_eq!(
        opfs::unwrap_data_key(&record, &key).await.len(),
        DATA_KEY_LEN
    );
    // And nothing more: its bytes cannot be exported.
    for format in ["raw", "jwk"] {
        let exported = opfs::export_key(&key, format).await;
        console_log!("[store] exportKey({format}) of the stored wrapping key: {exported:?}");
        assert_eq!(exported, Err("InvalidAccessError".to_owned()), "{format}");
    }
    opfs::remove_store(&name).await;
}

// No marker and no data key in the files or the record.

async fn no_marker_or_data_key(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-plaintext");
    let marker = std::str::from_utf8(MARKER).unwrap();
    let store = open(&name, &wrapping).await;
    // The marker as a namespace, a key and a value, then as a value, so both files hold it.
    store.commit(put(marker, MARKER, MARKER)).unwrap();
    store.commit(put("ns", b"k", MARKER)).unwrap();
    drop(store);

    let files = opfs::read_files(&name).await;
    let record = record(&name).await;
    let data_key = data_key(&name, &wrapping).await;
    assert_eq!(data_key.len(), DATA_KEY_LEN);
    // The bytes searched for are the store's data key: they open each file's sealed payload,
    // with the header as associated data, and the payload holds the marker.
    let cipher = XChaCha20Poly1305::new_from_slice(&data_key).unwrap();
    for slot in &files {
        let payload = Payload {
            msg: &slot[PAYLOAD_AT..],
            aad: &slot[..PAYLOAD_AT],
        };
        let nonce = XNonce::from_slice(&slot[NONCE_AT..NONCE_AT + NONCE_LEN]);
        let plaintext = cipher
            .decrypt(nonce, payload)
            .expect("the data key opens the slot");
        assert!(contains(&plaintext, MARKER), "the slot's payload");
    }

    for (what, bytes) in [
        ("file a", &files[0]),
        ("file b", &files[1]),
        ("the record's store identifier", &record.store_id),
        ("the record's IV", &record.iv),
        ("the record's wrapped key", &record.wrapped),
    ] {
        assert!(!contains(bytes, MARKER), "{what} holds the marker");
        assert!(!contains(bytes, &data_key), "{what} holds the data key");
    }
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_files_and_record_hold_no_marker_or_data_key() {
    no_marker_or_data_key(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_files_and_record_hold_no_marker_or_data_key() {
    no_marker_or_data_key(host().await).await;
}

// A deleted record: `KeyLost`, files untouched, no record created.

async fn record_deleted(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-record-deleted");
    two_commits(&name, &wrapping).await;
    let files = opfs::read_files(&name).await;
    opfs::delete_key_record(&name).await;

    for _ in 0..2 {
        assert_eq!(open_error(&name, &wrapping).await, Some(OpenError::KeyLost));
    }
    for wrong in wrong_keys(&wrapping).await {
        assert_eq!(
            open_error(&name, &wrong).await,
            Some(OpenError::KeyLost),
            "{wrong:?}"
        );
    }
    assert_eq!(opfs::read_files(&name).await, files, "a refused open wrote");
    assert!(
        opfs::key_record(&name).await.is_none(),
        "a refused open created a record"
    );
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_record_deleted_is_key_lost_and_writes_nothing() {
    record_deleted(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_record_deleted_is_key_lost_and_writes_nothing() {
    record_deleted(host().await).await;
}

// The other mode, or another host key: `KeyLost`.

async fn wrong_key(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-wrong-key");
    two_commits(&name, &wrapping).await;
    let files = opfs::read_files(&name).await;
    let before = record(&name).await;

    for wrong in wrong_keys(&wrapping).await {
        assert_eq!(
            open_error(&name, &wrong).await,
            Some(OpenError::KeyLost),
            "{wrong:?}"
        );
    }
    assert_eq!(opfs::read_files(&name).await, files, "a refused open wrote");
    assert_eq!(record(&name).await.fields(), before.fields());

    // The store's own key still opens it.
    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-2".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_origin_store_refuses_host_mode() {
    wrong_key(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_wrong_key_is_key_lost() {
    wrong_key(host().await).await;
}

/// A record of host-held mode whose fields were changed: each is `KeyLost`, but a newer record
/// format is `NewerFormat`, and the record put back opens the store again. Host-held only, as
/// putting back a record of origin-held mode would store its `CryptoKey` again.
#[wasm_bindgen_test]
async fn test_store_host_key_changed_record_is_refused() {
    let name = opfs::fresh_name("seal-changed-record");
    let wrapping = host().await;
    two_commits(&name, &wrapping).await;
    let files = opfs::read_files(&name).await;
    let before = record(&name).await;

    // A field's value, as bytes for a byte field and as a number's text for the format.
    let read = |record: &JsValue, field: &str| -> String {
        let value = Reflect::get(record, &JsValue::from_str(field)).unwrap();
        match value.dyn_ref::<Uint8Array>() {
            Some(bytes) => format!("{:?}", bytes.to_vec()),
            None => format!("{:?}", value.as_f64()),
        }
    };
    // The record format: 0 is no format at all, 2 a newer one.
    for (field, format) in [
        ("wrappedKey", None),
        ("iv", None),
        ("storeId", None),
        ("format", Some(0)),
        ("format", Some(2)),
    ] {
        let changed = Object::assign(&Object::new(), before.raw.unchecked_ref());
        let value: JsValue = match format {
            Some(format) => JsValue::from(format),
            None => {
                let mut bytes = opfs::byte_field(&changed, field);
                bytes[0] ^= 0x01;
                Uint8Array::from(&bytes[..]).into()
            }
        };
        Reflect::set(&changed, &JsValue::from_str(field), &value).unwrap();
        opfs::put_key_record(&name, &changed).await;
        let expected = match format {
            Some(2) => OpenError::NewerFormat { version: 2 },
            _ => OpenError::KeyLost,
        };
        assert_eq!(
            open_error(&name, &wrapping).await,
            Some(expected),
            "{field} {format:?}"
        );
        assert_eq!(
            opfs::read_files(&name).await,
            files,
            "{field}: a refused open wrote"
        );
        // The changed record is left as it was, never replaced.
        let left = opfs::key_record(&name).await.expect("the record is kept");
        assert_eq!(read(&left.raw, field), read(&changed, field), "{field}");
        assert_ne!(read(&left.raw, field), read(&before.raw, field), "{field}");
    }

    opfs::put_key_record(&name, before.raw.unchecked_ref()).await;
    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-2".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

/// A host key that cannot decrypt, or is not AES-GCM, is refused before anything is opened: a
/// store made with it would never open again. Nothing is created, in OPFS or IndexedDB.
#[wasm_bindgen_test]
async fn test_store_host_key_that_cannot_unwrap_is_refused() {
    let name = opfs::fresh_name("seal-bad-host-key");
    let encrypt_only = opfs::generate_key("AES-GCM", &["encrypt"]).await;
    let other_algorithm = opfs::generate_key("AES-CBC", &["encrypt", "decrypt"]).await;
    for key in [encrypt_only, other_algorithm] {
        let error = open_error(&name, &WrappingKey::Host(key)).await;
        assert!(
            matches!(error, Some(OpenError::Unavailable { .. })),
            "{error:?}"
        );
    }
    assert!(!opfs::exists(&opfs::root().await, &name).await);
    assert!(opfs::key_record(&name).await.is_none());
}

// A record without files: a new store with the record's key and identifier.

/// Empty the store's files, by removing its directory or by cutting both files to nothing.
async fn lose_files(name: &str, remove_directory: bool) {
    if remove_directory {
        opfs::remove_directory(name).await;
    } else {
        opfs::write_file(name, "a", &[]).await;
        opfs::write_file(name, "b", &[]).await;
    }
}

async fn record_without_files(wrapping: WrappingKey, remove_directory: bool) {
    let name = opfs::fresh_name("seal-no-files");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    let before = record(&name).await;
    let key_before = data_key(&name, &wrapping).await;
    lose_files(&name, remove_directory).await;

    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), None);
    store.commit(put("ns", b"k", b"value-new")).unwrap();
    drop(store);

    let after = record(&name).await;
    assert_eq!(after.fields(), before.fields(), "the record was replaced");
    assert_eq!(data_key(&name, &wrapping).await, key_before);
    let [a, b] = opfs::read_files(&name).await;
    assert_eq!(opfs::store_id(&a), before.store_id);
    assert_eq!(support::generation(&a), 1);
    assert!(b.is_empty());
    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-new".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_record_without_files_reuses_key_and_store_id() {
    record_without_files(WrappingKey::Origin, true).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_record_without_files_reuses_key_and_store_id() {
    record_without_files(host().await, false).await;
}

async fn record_without_files_wrong_key(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-no-files-wrong");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    lose_files(&name, false).await;
    let before = record(&name).await;

    for wrong in wrong_keys(&wrapping).await {
        assert_eq!(
            open_error(&name, &wrong).await,
            Some(OpenError::KeyLost),
            "{wrong:?}"
        );
    }
    assert_eq!(
        opfs::read_files(&name).await,
        [Vec::<u8>::new(), Vec::new()]
    );
    assert_eq!(record(&name).await.fields(), before.fields());

    // The record's own key makes the new store.
    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), None);
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_record_without_files_and_host_mode_is_key_lost() {
    record_without_files_wrong_key(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_record_without_files_and_wrong_key_is_key_lost() {
    record_without_files_wrong_key(host().await).await;
}

// Tampered slots.

async fn tampered_newest_slot(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-tampered");
    two_commits(&name, &wrapping).await;
    let b = opfs::read_file(&name, "b").await;

    // A changed byte in the header (bound as associated data), the ciphertext or the tag fails
    // the tag: the store opens at the older file, and writes nothing.
    for (what, offset) in [
        ("generation", GENERATION_AT),
        ("nonce", NONCE_AT + 5),
        ("ciphertext", PAYLOAD_AT + 2),
        ("tag", b.len() - 1),
    ] {
        opfs::write_file(&name, "b", &b).await;
        opfs::damage_file(&name, "b", offset).await;
        let files = opfs::read_files(&name).await;
        let store = OpfsStore::open(&name, wrapping.clone())
            .await
            .unwrap_or_else(|error| panic!("{what}: {error:?}"));
        assert_eq!(get(&store, "ns", b"k"), Some(b"value-1".to_vec()), "{what}");
        drop(store);
        assert_eq!(
            opfs::read_files(&name).await,
            files,
            "{what}: opening wrote"
        );
    }

    // A changed store identifier names another store: refused, not taken as damage.
    opfs::write_file(&name, "b", &b).await;
    opfs::damage_file(&name, "b", STORE_ID_AT).await;
    let files = opfs::read_files(&name).await;
    assert_eq!(open_error(&name, &wrapping).await, Some(OpenError::KeyLost));
    assert_eq!(opfs::read_files(&name).await, files, "a refused open wrote");
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_tampered_newest_slot_falls_back() {
    tampered_newest_slot(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_tampered_newest_slot_falls_back() {
    tampered_newest_slot(host().await).await;
}

async fn tampered_only_slot(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-tampered-only");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    let a = opfs::read_file(&name, "a").await;

    // The only committed file, complete but with a changed tag, next to an empty one: not a
    // torn first commit, so corrupt.
    opfs::damage_file(&name, "a", a.len() - 1).await;
    let files = opfs::read_files(&name).await;
    assert_eq!(open_error(&name, &wrapping).await, Some(OpenError::Corrupt));
    assert_eq!(opfs::read_files(&name).await, files, "a refused open wrote");
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_tampered_only_slot_is_corrupt() {
    tampered_only_slot(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_tampered_only_slot_is_corrupt() {
    tampered_only_slot(host().await).await;
}

/// The newest file of another store, under the same wrapping key, copied over this store's:
/// whole, its header names the other store and the open is refused; with this store's
/// identifier written into it, it fails the tag under this store's data key and is ignored.
async fn slot_from_another_store(wrapping: WrappingKey) {
    let ours = opfs::fresh_name("seal-ours");
    let theirs = opfs::fresh_name("seal-theirs");
    two_commits(&ours, &wrapping).await;
    let store = open(&theirs, &wrapping).await;
    store.commit(put("ns", b"k", b"their-1")).unwrap();
    store.commit(put("ns", b"k", b"their-2")).unwrap();
    drop(store);
    let their_b = opfs::read_file(&theirs, "b").await;
    let our_a = opfs::read_file(&ours, "a").await;
    assert_ne!(opfs::store_id(&their_b), opfs::store_id(&our_a));

    opfs::write_file(&ours, "b", &their_b).await;
    let files = opfs::read_files(&ours).await;
    assert_eq!(open_error(&ours, &wrapping).await, Some(OpenError::KeyLost));
    assert_eq!(opfs::read_files(&ours).await, files, "a refused open wrote");

    let mut relabelled = their_b.clone();
    relabelled[STORE_ID_AT..NONCE_AT].copy_from_slice(opfs::store_id(&our_a));
    opfs::write_file(&ours, "b", &relabelled).await;
    let store = open(&ours, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-1".to_vec()));
    drop(store);

    opfs::remove_store(&ours).await;
    opfs::remove_store(&theirs).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_slot_from_another_store_is_refused() {
    slot_from_another_store(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_slot_from_another_store_is_refused() {
    slot_from_another_store(host().await).await;
}

// The earlier unsealed format.

async fn unsealed(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-unsealed");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    let sealed = opfs::read_file(&name, "a").await;
    let before = record(&name).await;

    // An unsealed slot next to a sealed one, then alone (as an earlier build's first commit).
    for (a, b) in [(sealed, unsealed_slot(2)), (unsealed_slot(1), Vec::new())] {
        opfs::write_file(&name, "a", &a).await;
        opfs::write_file(&name, "b", &b).await;
        assert_eq!(
            open_error(&name, &wrapping).await,
            Some(OpenError::UnsealedFormat)
        );
        assert_eq!(opfs::read_files(&name).await, [a.clone(), b.clone()]);
        assert_eq!(record(&name).await.fields(), before.fields());
    }

    // The header alone decides, before the record is read: without a record, the same, and
    // no record is made.
    opfs::delete_key_record(&name).await;
    let files = opfs::read_files(&name).await;
    assert_eq!(
        open_error(&name, &wrapping).await,
        Some(OpenError::UnsealedFormat)
    );
    assert_eq!(opfs::read_files(&name).await, files);
    assert!(
        opfs::key_record(&name).await.is_none(),
        "a refused open created a record"
    );
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_unsealed_slot_is_refused_untouched() {
    unsealed(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_unsealed_slot_is_refused_untouched() {
    unsealed(host().await).await;
}

// Files with no sealed header and no record: nothing sealed is there to lose, so the open is
// `Corrupt`, not `KeyLost`, and still writes nothing. One mode is enough: without a record,
// the mode is never used.
#[wasm_bindgen_test]
async fn test_store_host_key_no_sealed_header_without_record_is_corrupt() {
    let name = opfs::fresh_name("seal-no-header");
    let wrapping = host().await;
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    let sealed = opfs::read_file(&name, "a").await;
    opfs::delete_key_record(&name).await;
    // Bytes of no format, then a sealed slot's header cut short, each next to an empty file.
    for bytes in [
        b"not a slot at all".to_vec(),
        sealed[..PAYLOAD_AT / 2].to_vec(),
    ] {
        opfs::write_file(&name, "a", &bytes).await;
        opfs::write_file(&name, "b", &[]).await;
        assert_eq!(open_error(&name, &wrapping).await, Some(OpenError::Corrupt));
        assert_eq!(opfs::read_files(&name).await, [bytes, Vec::new()]);
        assert!(
            opfs::key_record(&name).await.is_none(),
            "a refused open created a record"
        );
    }
    // A complete sealed header is sealed data: its lost key is `KeyLost`.
    opfs::write_file(&name, "a", &sealed).await;
    assert_eq!(open_error(&name, &wrapping).await, Some(OpenError::KeyLost));
    opfs::remove_store(&name).await;
}

// `destroy`.

async fn destroy_leaves_nothing(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-destroy");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", MARKER)).unwrap();
    drop(store);
    let before = record(&name).await;
    let key_before = data_key(&name, &wrapping).await;

    opfs::remove_store(&name).await;

    // Opening the name again makes a new store, with a new identifier and data key.
    let store = open(&name, &wrapping).await;
    assert_eq!(store.scan("ns", b"", None, 10), Ok(Vec::new()));
    drop(store);
    let after = record(&name).await;
    assert_ne!(after.store_id, before.store_id);
    assert_ne!(after.wrapped, before.wrapped);
    assert_ne!(data_key(&name, &wrapping).await, key_before);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_destroy_leaves_nothing_and_next_open_is_new() {
    destroy_leaves_nothing(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_destroy_leaves_nothing_and_next_open_is_new() {
    destroy_leaves_nothing(host().await).await;
}

async fn destroy_open_store(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-destroy-open");
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    let before = record(&name).await;

    assert_eq!(
        OpfsStore::destroy(&name).await.err(),
        Some(OpenError::InUse)
    );
    // Nothing was removed, and the store still works.
    assert_eq!(record(&name).await.fields(), before.fields());
    store.commit(put("ns", b"k", b"value-2")).unwrap();
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-2".to_vec()));
    drop(store);
    let store = open(&name, &wrapping).await;
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-2".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_destroy_of_open_store_is_in_use() {
    destroy_open_store(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_destroy_of_open_store_is_in_use() {
    destroy_open_store(host().await).await;
}

async fn destroy_after_partial_removal(wrapping: WrappingKey) {
    let name = opfs::fresh_name("seal-destroy-partial");
    // The record already gone, as a destroy that failed after deleting it leaves the store.
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    opfs::delete_key_record(&name).await;
    opfs::remove_store(&name).await;

    // The directory already gone, the record left.
    let store = open(&name, &wrapping).await;
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    drop(store);
    opfs::remove_directory(&name).await;
    opfs::remove_store(&name).await;

    // Nothing left at all: a name with no store.
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_seal_destroy_after_partial_removal_finishes() {
    destroy_after_partial_removal(WrappingKey::Origin).await;
}

#[wasm_bindgen_test]
async fn test_store_host_key_destroy_after_partial_removal_finishes() {
    destroy_after_partial_removal(host().await).await;
}
