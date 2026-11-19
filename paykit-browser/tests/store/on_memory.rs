//! The contract cases on the Switch client's `MemoryStore`, natively: each case holds for the
//! client's own store before it is asked of `OpfsStore`.

use std::sync::Arc;

use pubky_switch_client::store::{MemoryStore, StateStore};

use crate::contract;

#[test]
fn test_store_contract_batch_order() {
    contract::batch_order(&MemoryStore::new());
}

#[test]
fn test_store_contract_scan_prefix_after_limit() {
    contract::scan_prefix_after_limit(&MemoryStore::new());
}

#[test]
fn test_store_contract_delete_and_delete_prefix() {
    contract::delete_and_delete_prefix(&MemoryStore::new());
}

#[test]
fn test_store_contract_empty_namespaces_keys_and_values() {
    contract::empty_namespaces_keys_and_values(&MemoryStore::new());
}

#[test]
fn test_store_contract_plain_commit_refused_on_owned_namespace() {
    contract::plain_commit_refused_on_owned_namespace(&MemoryStore::new());
}

#[test]
fn test_store_contract_wrong_and_stale_owner_apply_nothing() {
    contract::wrong_and_stale_owner_apply_nothing(&MemoryStore::new());
}

#[test]
fn test_store_contract_lease_drop_releases() {
    contract::lease_drop_releases(&MemoryStore::new());
}

#[test]
fn test_store_contract_value_and_batch_size_caps() {
    contract::value_and_batch_size_caps(&MemoryStore::new());
}

#[test]
fn test_store_contract_owned_batch_size_cap() {
    contract::owned_batch_size_cap(&MemoryStore::new());
}

#[test]
fn test_store_contract_owned_handle_keeps_exclusion() {
    let store: Arc<dyn StateStore> = Arc::new(MemoryStore::new());
    contract::owned_handle_keeps_exclusion(store);
}

#[test]
fn test_store_contract_concurrent_acquisition_has_one_owner() {
    contract::concurrent_acquisition_has_one_owner(&MemoryStore::new());
}
