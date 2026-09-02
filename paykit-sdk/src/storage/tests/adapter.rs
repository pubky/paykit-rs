use super::*;

#[tokio::test]
async fn test_storage_adapter_supports_erased_transactions() {
    let storage: std::sync::Arc<dyn StorageAdapter> = std::sync::Arc::new(InMemoryStorage::new());
    let identity_public_key = counterparty();
    let saved_identity = crate::IdentityState {
        public_key: Some(identity_public_key.clone()),
        initialized_at: timestamp(),
    };
    let value = storage
        .transaction_erased(Box::new(move |tx| {
            tx.save_identity_state(saved_identity);
            Ok(Box::new(42_u32) as Box<dyn std::any::Any + Send>)
        }))
        .await
        .unwrap();

    assert_eq!(*value.downcast::<u32>().unwrap(), 42);
    let loaded = storage
        .transaction_erased(Box::new(|tx| {
            Ok(Box::new(tx.load_identity_state()) as Box<dyn std::any::Any + Send>)
        }))
        .await
        .unwrap();
    let loaded = *loaded.downcast::<Option<crate::IdentityState>>().unwrap();
    assert_eq!(loaded.unwrap().public_key, Some(identity_public_key));
}

#[tokio::test]
async fn test_transaction_rolls_back_on_error() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let result: Result<()> = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(timestamp()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });

                Err(PaykitSdkError::Storage {
                    context: "forced rollback".into(),
                    source: None,
                })
            }
        })
        .await;

    assert!(result.is_err());
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.linked_peers.is_empty());
}
