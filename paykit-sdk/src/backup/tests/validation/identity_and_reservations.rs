use super::*;

#[tokio::test]
async fn test_restore_backup_state_rejects_wrong_identity() {
    let storage = InMemoryStorage::new();
    let current = public_key();
    let backup_public_key = public_key();
    storage
        .save_identity_state(identity(current))
        .await
        .unwrap();

    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(backup_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_trusted_identity_switch() {
    let storage = InMemoryStorage::new();
    let stored_public_key = public_key();
    let backup_public_key = public_key();
    storage
        .save_identity_state(identity(stored_public_key.clone()))
        .await
        .unwrap();
    let trusted_identity = identity(backup_public_key.clone());
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(backup_public_key.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        payment_request_execution_claims: Vec::new(),
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

    let result = restore_backup_state_with_identity(
        &storage,
        backup,
        Some(trusted_identity),
        DateTime::<Utc>::MIN_UTC,
    )
    .await;

    let identity = storage.snapshot().unwrap().identity_state.unwrap();
    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert_eq!(identity.public_key.as_ref(), Some(&stored_public_key));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_orphan_endpoint_reservation() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty,
            app_id: app_id(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
        payment_request_execution_claims: Vec::new(),
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

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_endpoint_reservation_id() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation\n1".into(),
            counterparty: counterparty.clone(),
            app_id: app_id(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("ln-private"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
        payment_request_execution_claims: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_mismatched_endpoint_reservation_payload() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            app_id: app_id(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("different-payload"),
            outbound_message_id: 7,
            attribution: HashMap::new(),
            expires_at: None,
            cancellation_started_at: None,
            created_at: timestamp(),
        }],
        payment_request_execution_claims: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: vec![private_payment_list_outbound(
            counterparty,
            7,
            "ln-private",
        )],
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
