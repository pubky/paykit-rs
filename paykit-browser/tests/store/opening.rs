//! `OpfsStore` on its real files in the Origin Private File System: opening again, the
//! browser's exclusive locks, damaged files and invalid names. Each test removes the store it
//! created, its key record included.

use paykit_browser::store::{OpenError, OpfsStore, WrappingKey};
use pubky_switch_client::store::{CommitOutcome, StateStore, StoreError};
use wasm_bindgen_test::{console_log, wasm_bindgen_test};

use crate::contract::{acquire, is_busy, owners, put};
use crate::opfs;
use crate::support::{self, PAYLOAD_AT};

fn get(store: &OpfsStore, ns: &str, key: &[u8]) -> Option<Vec<u8>> {
    store.get(ns, key).unwrap()
}

/// A store whose file `a` holds `k = value-1` at generation 1 and file `b` holds
/// `k = value-2` at generation 2, under `wrapping`, closed.
pub async fn two_commits(name: &str, wrapping: &WrappingKey) {
    let store = OpfsStore::open(name, wrapping.clone()).await.unwrap();
    store.commit(put("ns", b"k", b"value-1")).unwrap();
    store.commit(put("ns", b"k", b"value-2")).unwrap();
    drop(store);
    assert_eq!(support::generation(&opfs::read_file(name, "a").await), 1);
    assert_eq!(support::generation(&opfs::read_file(name, "b").await), 2);
}

#[wasm_bindgen_test]
async fn test_store_opfs_commits_survive_reopening() {
    let name = opfs::fresh_name("reopen");
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(store.scan("ns", b"", None, 10), Ok(Vec::new()));
    store.commit(put("ns", b"a", b"1")).unwrap();
    let mut batch = put("ns", b"b", b"2");
    batch.delete("ns", b"a").unwrap();
    batch.put("other", b"c", b"3").unwrap();
    store.commit(batch).unwrap();
    drop(store);
    // The commits went to alternate files, generation 1 to `a`.
    assert_eq!(support::generation(&opfs::read_file(&name, "a").await), 1);
    assert_eq!(support::generation(&opfs::read_file(&name, "b").await), 2);

    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(get(&store, "ns", b"a"), None);
    assert_eq!(get(&store, "ns", b"b"), Some(b"2".to_vec()));
    assert_eq!(
        store.scan("other", b"", None, 10),
        Ok(vec![(b"c".to_vec(), b"3".to_vec())])
    );
    store.commit(put("ns", b"b", b"4")).unwrap();
    drop(store);
    assert_eq!(support::generation(&opfs::read_file(&name, "a").await), 3);
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(get(&store, "ns", b"b"), Some(b"4".to_vec()));
    assert_eq!(get(&store, "other", b"c"), Some(b"3".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_opfs_second_open_is_in_use() {
    let name = opfs::fresh_name("in-use");
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    store.commit(put("ns", b"k", b"1")).unwrap();
    assert_eq!(
        OpfsStore::open(&name, WrappingKey::Origin).await.err(),
        Some(OpenError::InUse)
    );
    // The refused open leaves the first store working.
    store.commit(put("ns", b"k", b"2")).unwrap();
    assert_eq!(get(&store, "ns", b"k"), Some(b"2".to_vec()));

    // Another worker is refused the files as well, with the engine's lock error.
    let engine = support::engine();
    for file in ["a", "b"] {
        let answer = support::open_in_nested_worker(&name, file).await;
        console_log!("[store] nested worker in {engine}, file {file}, store open: {answer}");
        assert_eq!(
            answer,
            support::lock_error_name(),
            "a nested dedicated worker in {engine} must be refused file {file} while the \
             store is open"
        );
    }

    // Dropping the store releases the files: the nested worker gets them, and then this
    // worker opens the store again.
    drop(store);
    let answer = support::open_in_nested_worker(&name, "a").await;
    console_log!("[store] nested worker in {engine}, file a, store dropped: {answer}");
    assert_eq!(
        answer, "ok",
        "a nested dedicated worker in {engine}, store dropped"
    );
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(get(&store, "ns", b"k"), Some(b"2".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_opfs_lease_keeps_the_store_open() {
    let name = opfs::fresh_name("lease-open");
    let [first, second, third] = owners();
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    let lease = acquire(&store, "ns", first);
    // The lease keeps the store, and its files' locks, until it drops, so its ownership
    // cannot overlap with an owner of the store opened again, and the store cannot be
    // destroyed under it: `destroy` deletes nothing, not even the key record.
    drop(store);
    assert_eq!(
        OpfsStore::open(&name, WrappingKey::Origin).await.err(),
        Some(OpenError::InUse)
    );
    assert_eq!(OpfsStore::destroy(&name).await, Err(OpenError::InUse));
    drop(lease);
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    // Owners live in memory only, so the store opened again has none.
    let newer = acquire(&store, "ns", second);
    assert!(is_busy(&store, "ns", third));
    assert_eq!(
        store.commit(put("ns", b"k", b"raw")),
        Err(StoreError::InUse)
    );
    assert_eq!(
        store.commit_if("ns", first, put("ns", b"k", b"stale")),
        Ok(CommitOutcome::NotOwner)
    );
    assert_eq!(
        store.commit_if("ns", second, put("ns", b"k", b"newer")),
        Ok(CommitOutcome::Applied)
    );
    assert_eq!(get(&store, "ns", b"k"), Some(b"newer".to_vec()));
    drop(newer);
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_opfs_damaged_newest_file_falls_back() {
    let name = opfs::fresh_name("damaged");
    two_commits(&name, &WrappingKey::Origin).await;
    opfs::damage_file(&name, "b", PAYLOAD_AT + 2).await;

    // The newest file fails its tag, so the store opens at the older one.
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-1".to_vec()));
    // The next commit goes over the damaged file, at the next generation.
    store.commit(put("ns", b"k", b"value-3")).unwrap();
    drop(store);
    assert_eq!(support::generation(&opfs::read_file(&name, "a").await), 1);
    assert_eq!(support::generation(&opfs::read_file(&name, "b").await), 2);
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert_eq!(get(&store, "ns", b"k"), Some(b"value-3".to_vec()));
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_opfs_both_files_damaged_is_corrupt() {
    let name = opfs::fresh_name("corrupt");
    two_commits(&name, &WrappingKey::Origin).await;
    opfs::damage_file(&name, "a", PAYLOAD_AT + 2).await;
    opfs::damage_file(&name, "b", PAYLOAD_AT + 2).await;
    let a = opfs::read_file(&name, "a").await;
    let b = opfs::read_file(&name, "b").await;

    for _ in 0..2 {
        assert_eq!(
            OpfsStore::open(&name, WrappingKey::Origin).await.err(),
            Some(OpenError::Corrupt)
        );
    }
    // The refused opens wrote nothing, and released both files: they open here.
    assert_eq!(opfs::read_file(&name, "a").await, a);
    assert_eq!(opfs::read_file(&name, "b").await, b);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_opfs_invalid_name_creates_nothing() {
    let before = support::root_names().await;
    let long = "x".repeat(65);
    for name in ["", ".", "..", "a.b", "a/b", "a b", "\u{e9}", long.as_str()] {
        assert_eq!(
            OpfsStore::open(name, WrappingKey::Origin).await.err(),
            Some(OpenError::InvalidName),
            "{name:?}"
        );
    }
    assert_eq!(support::root_names().await, before);
}
