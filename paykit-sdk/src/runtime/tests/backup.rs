use super::*;

fn empty_backup_state() -> SdkBackupState {
    SdkBackupState {
        version: crate::SDK_BACKUP_VERSION,
        identity_state: None,
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    }
}

#[tokio::test]
async fn test_restore_backup_state_requires_active_identity() {
    let storage = InMemoryStorage::new();
    let existing_public_key =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(existing_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let backup_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let backup = SdkBackupState {
        version: crate::SDK_BACKUP_VERSION,
        identity_state: Some(IdentityState {
            public_key: Some(backup_public_key),
            initialized_at: FixedClock.now(),
        }),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.restore_backup_state(backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage.snapshot().unwrap().identity_state.is_some());
}

#[tokio::test]
async fn test_restore_backup_state_rejects_concurrent_identity_operation() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("test operation").unwrap();

    let result = sdk.restore_backup_state(empty_backup_state()).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}
