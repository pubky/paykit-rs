//! The store across a page reload: [`OpfsStore`], opened again by a new WebAssembly instance
//! of the same origin, holds exactly the state of its last commit, and ignores a file that a
//! commit left torn.
//!
//! The target runs in reload mode, in a dedicated worker only (`rig/runner.mjs`, "Target
//! modes"), so the one test here runs twice:
//!
//! - Before the reload, it commits several batches, drops the store, tears the file the next
//!   commit would write, and prints [`BEFORE_RELOAD_MARKER`].
//! - After the reload, it opens the store again, reads its contents, commits once more and
//!   reads them again, removes the store with its key record, then checks what it read and
//!   prints
//!   [`AFTER_RELOAD_MARKER`].

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use paykit_browser::store::{OpfsStore, WrappingKey};
use pubky_switch_client::store::{KeyValue, StateStore, WriteBatch};
use wasm_bindgen_test::{console_log, wasm_bindgen_test, wasm_bindgen_test_configure};

wasm_bindgen_test_configure!(run_in_dedicated_worker);

#[allow(dead_code)]
#[path = "../opfs/mod.rs"]
mod opfs;

/// What the test prints once the run before the reload has done its part.
const BEFORE_RELOAD_MARKER: &str = "BEFORE RELOAD DONE";
/// What the test prints once the run after the reload has done its part.
const AFTER_RELOAD_MARKER: &str = "AFTER RELOAD DONE";

/// The store's name: its directory in the origin's OPFS root.
const STORE_NAME: &str = "reload";
/// The namespaces the test writes.
const NAMESPACES: [&str; 2] = ["one", "two"];

/// The file the fifth commit writes. Before the reload the test commits four times, and the
/// first commit goes to file `a`, the second to `b`, and so on, so file `b` holds the current
/// state (generation 4) and `a` the one before it.
const NEXT_FILE: &str = "a";
/// The generation of the fifth commit.
const NEXT_GENERATION: u64 = 5;

/// A store opened, filled and dropped by one WebAssembly instance is found again by the next
/// instance of the origin, after a reload, with its last commit's contents, although the file
/// the next commit writes was left torn.
///
/// The test picks its step from the Origin Private File System, the only state that outlives
/// the reload: the reloaded page runs the same test again, and the store's directory exists
/// only after the run before the reload.
#[wasm_bindgen_test]
async fn test_store_reload_reopens_last_commit() {
    if opfs::exists(&opfs::root().await, STORE_NAME).await {
        after_reload().await;
    } else {
        before_reload().await;
    }
}

/// Commit four batches, drop the store, tear the file the fifth commit would write, and leave
/// the store in place for the run after the reload.
async fn before_reload() {
    let store = OpfsStore::open(STORE_NAME, WrappingKey::Origin)
        .await
        .expect("open the store");

    // 1: entries in both namespaces.
    let mut batch = WriteBatch::new();
    batch
        .put(NAMESPACES[0], b"k1".to_vec(), b"v1".to_vec())
        .unwrap()
        .put(NAMESPACES[0], b"k2".to_vec(), b"v2".to_vec())
        .unwrap()
        .put(NAMESPACES[1], b"r1".to_vec(), b"x".to_vec())
        .unwrap();
    store.commit(batch).expect("commit 1");
    // 2: an overwrite and a delete.
    let mut batch = WriteBatch::new();
    batch
        .put(NAMESPACES[0], b"k1".to_vec(), b"v1-new".to_vec())
        .unwrap()
        .delete(NAMESPACES[0], b"k2".to_vec())
        .unwrap();
    store.commit(batch).expect("commit 2");
    // 3: keys under two prefixes.
    let mut batch = WriteBatch::new();
    batch
        .put(NAMESPACES[1], b"open/1".to_vec(), b"a".to_vec())
        .unwrap()
        .put(NAMESPACES[1], b"open/2".to_vec(), b"b".to_vec())
        .unwrap()
        .put(NAMESPACES[1], b"done/1".to_vec(), b"c".to_vec())
        .unwrap();
    store.commit(batch).expect("commit 3");
    // 4: a delete of one prefix. Its state differs from the third commit's, so the run after
    // the reload can tell which of the two it opened.
    let mut batch = WriteBatch::new();
    batch
        .delete_prefix(NAMESPACES[1], b"open/".to_vec())
        .unwrap();
    store.commit(batch).expect("commit 4");

    assert_eq!(contents(&store).unwrap(), contents_after_four_commits());
    // Closes both files, so the test can open one of them itself.
    drop(store);

    tear_next_file().await;
    console_log!("{BEFORE_RELOAD_MARKER}");
}

/// Open the store again, commit once more, and open it once more, then remove the store and
/// check what each step read. The store is removed before any check, so a failing check, or a
/// directory left by an interrupted run of this origin, does not fail every later run.
async fn after_reload() {
    let read = reopen_commit_and_reopen().await;
    opfs::remove_store(STORE_NAME).await;

    let [reopened, committed, reopened_again] = read.unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(reopened, contents_after_four_commits());
    let expected = contents_after_five_commits();
    assert_eq!(committed, expected);
    assert_eq!(reopened_again, expected);
    console_log!("{AFTER_RELOAD_MARKER}");
}

/// The store's contents when opened again with one file torn, after the fifth commit, and
/// after another open, or the first error. Every store it opens is dropped when it returns.
async fn reopen_commit_and_reopen() -> Result<[Vec<Vec<KeyValue>>; 3], String> {
    let store = OpfsStore::open(STORE_NAME, WrappingKey::Origin)
        .await
        .map_err(|error| format!("open the store again, with one file torn: {error:?}"))?;
    let reopened = contents(&store)?;

    // The fifth commit writes over the torn file.
    let mut batch = WriteBatch::new();
    batch
        .put(NAMESPACES[0], b"k3".to_vec(), b"v3".to_vec())
        .map_err(|error| format!("batch 5: {error:?}"))?;
    store
        .commit(batch)
        .map_err(|error| format!("commit 5: {error:?}"))?;
    let committed = contents(&store)?;
    drop(store);

    let store = OpfsStore::open(STORE_NAME, WrappingKey::Origin)
        .await
        .map_err(|error| format!("open the store after the fifth commit: {error:?}"))?;
    let reopened_again = contents(&store)?;
    Ok([reopened, committed, reopened_again])
}

/// Write the start of a fifth commit into [`NEXT_FILE`]: a header of this store that promises
/// a sealed payload of 4096 bytes, then 16 bytes of it, as a write that stopped part way leaves
/// the file. The header's layout is the one `src/store/format.rs` describes; its store
/// identifier is copied from the other file, as the store's own commit would write it. The file
/// is cut to those bytes, so its size is wrong for its header, and it held generation 3 before.
async fn tear_next_file() {
    let current = opfs::read_file(STORE_NAME, "b").await;
    let mut torn = Vec::new();
    torn.extend_from_slice(b"PKBSTORE");
    torn.extend_from_slice(&2u32.to_le_bytes());
    torn.extend_from_slice(&NEXT_GENERATION.to_le_bytes());
    torn.extend_from_slice(&current[20..36]);
    torn.extend_from_slice(&[0x5a; 24]);
    torn.extend_from_slice(&4096u64.to_le_bytes());
    torn.extend_from_slice(&[0xa5; 16]);
    opfs::write_file(STORE_NAME, NEXT_FILE, &torn).await;
}

/// Every entry of the store, namespace by namespace.
fn contents(store: &OpfsStore) -> Result<Vec<Vec<KeyValue>>, String> {
    NAMESPACES
        .iter()
        .map(|ns| {
            store
                .scan(ns, b"", None, 100)
                .map_err(|error| format!("scan {ns}: {error:?}"))
        })
        .collect()
}

fn contents_after_four_commits() -> Vec<Vec<KeyValue>> {
    vec![
        vec![entry("k1", "v1-new")],
        vec![entry("done/1", "c"), entry("r1", "x")],
    ]
}

fn contents_after_five_commits() -> Vec<Vec<KeyValue>> {
    vec![
        vec![entry("k1", "v1-new"), entry("k3", "v3")],
        vec![entry("done/1", "c"), entry("r1", "x")],
    ]
}

fn entry(key: &str, value: &str) -> KeyValue {
    (key.as_bytes().to_vec(), value.as_bytes().to_vec())
}
