//! The contract cases on `OpfsStore`, in a browser. Each case gets a fresh store under a name
//! of its own; afterwards the store is dropped and opened again, the namespaces the case wrote
//! must read back the same from disk, and the store is removed.
//!
//! Every case runs here but `contract::concurrent_acquisition_has_one_owner`, which needs
//! threads; its docs say what covers it in a browser.

use std::sync::Arc;

use paykit_browser::store::{OpfsStore, WrappingKey};
use pubky_switch_client::store::StateStore;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::contract::{self, Namespaces};
use crate::opfs;

/// Run `case` on a fresh store, then check that what it left survives opening the store again.
async fn run(case: impl FnOnce(Arc<dyn StateStore>) -> Namespaces) {
    let name = opfs::fresh_name("contract");
    let store: Arc<dyn StateStore> =
        Arc::new(OpfsStore::open(&name, WrappingKey::Origin).await.unwrap());
    let namespaces = case(Arc::clone(&store));
    let left = contract::contents(store.as_ref(), namespaces);
    // The case kept no handle on the store, so this drop closes its files.
    assert_eq!(Arc::strong_count(&store), 1);
    drop(store);
    let store = OpfsStore::open(&name, WrappingKey::Origin).await.unwrap();
    assert!(
        contract::contents(&store, namespaces) == left,
        "the contents read back after opening the store again differ"
    );
    drop(store);
    opfs::remove_store(&name).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_batch_order() {
    run(|store| contract::batch_order(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_scan_prefix_after_limit() {
    run(|store| contract::scan_prefix_after_limit(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_delete_and_delete_prefix() {
    run(|store| contract::delete_and_delete_prefix(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_empty_namespaces_keys_and_values() {
    run(|store| contract::empty_namespaces_keys_and_values(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_plain_commit_refused_on_owned_namespace() {
    run(|store| contract::plain_commit_refused_on_owned_namespace(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_wrong_and_stale_owner_apply_nothing() {
    run(|store| contract::wrong_and_stale_owner_apply_nothing(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_lease_drop_releases() {
    run(|store| contract::lease_drop_releases(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_value_and_batch_size_caps() {
    run(|store| contract::value_and_batch_size_caps(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_owned_batch_size_cap() {
    run(|store| contract::owned_batch_size_cap(store.as_ref())).await;
}

#[wasm_bindgen_test]
async fn test_store_contract_owned_handle_keeps_exclusion() {
    run(contract::owned_handle_keeps_exclusion).await;
}
