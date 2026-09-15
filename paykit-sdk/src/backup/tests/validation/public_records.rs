use super::*;

#[tokio::test]
async fn test_restore_backup_state_rejects_invalid_public_endpoint_record() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            app_id: app_id(),
            identifier: "private".into(),
            payload: Some("ln".into()),
            status: crate::PublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_inconsistent_public_endpoint_status() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: vec![PublicEndpointRecord {
            app_id: app_id(),
            identifier: "btc-lightning-bolt11".into(),
            payload: None,
            status: crate::PublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }],
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
