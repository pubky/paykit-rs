use super::*;

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_access_context_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            app_id: app_id(),
            app_authorized: false,
            counterparty,
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some("850e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_access_location_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, original_location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let wrong_location = paykit_lib::ReceiptAccess::location_for(
        &ReceiptId::new("850e8400-e29b-41d4-a716-446655440000").unwrap(),
    );
    let raw_json = raw_json.replace(&original_location, &wrong_location);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            app_id: app_id(),
            app_authorized: false,
            counterparty,
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some("750e8400-e29b-41d4-a716-446655440000".into()),
            billing_period: Some(period),
            location: wrong_location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_inconsistent_receipt_access_status() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_reference = "invoice-2026-0001";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        payment_reference,
        payment_request_id,
        &period,
    );
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty.clone())),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: counterparty.clone(),
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![ReceiptAccessRecord {
            app_id: app_id(),
            app_authorized: false,
            counterparty,
            stream_item_id: 1,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: payment_reference.into(),
            payment_request_id: Some(payment_request_id.into()),
            billing_period: Some(period),
            location,
            key,
            retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
            retrieval_attempted_at: None,
            retrieved_at: Some(timestamp()),
            last_retrieval_error: None,
            received_at: timestamp(),
        }],
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
