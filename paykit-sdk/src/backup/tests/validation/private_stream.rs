use super::*;

#[tokio::test]
async fn test_restore_backup_state_rejects_stale_private_stream_metadata() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
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
            counterparty,
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_stale_private_stream_parse_status() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
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
            counterparty,
            receive_batch_id: 0,
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
                    .into(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.private_payment_list".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.private_payment_list".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_stale_private_stream_parse_error() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, _) = receipt_access_raw_with_context(
        "650e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "750e8400-e29b-41d4-a716-446655440000",
        &period,
    );
    let raw_json = raw_json.replace(
        &location,
        "/pub/paykit/v0/private/receipts/not-the-receipt-id",
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
            counterparty,
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("stale".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_stale_dedupe_event_header() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
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
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_overlapping_event_dedupe_membership() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
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
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: vec![1],
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_wrong_receipt_access_location_dedupe_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, original_location, _) = receipt_access_raw_with_context(
        event_id,
        receipt_id.as_str(),
        "invoice-2026-0001",
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
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::MalformedRecognized,
            parse_error: Some("Receipt Access location does not match Receipt ID".into()),
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_accepts_cross_kind_event_id_conflict() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let request_json = payment_request_json(event_id);
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (receipt_access_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "550e8400-e29b-41d4-a716-446655440000",
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
        private_stream_items: vec![
            PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json: request_json.clone(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.payment_request".into()),
                parsed_app_id: Some("bitkit".into()),
                known_paykit_kind: Some("paykit.payment_request".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
            PrivateStreamItemRecord {
                stream_item_id: 2,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json: receipt_access_json,
                parsed_version: Some(1),
                parsed_kind: Some("paykit.receipt_access".into()),
                parsed_app_id: Some("bitkit".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            },
        ],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: event_id.into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&request_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: vec![2],
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 3,
    };

    restore_backup_state(&storage, backup).await.unwrap();
}

#[tokio::test]
async fn test_restore_backup_state_rejects_missing_event_dedupe_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
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
            counterparty,
            receive_batch_id: 0,
            raw_json,
            parsed_version: Some(1),
            parsed_kind: Some("paykit.payment_request".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.payment_request".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
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
async fn test_restore_backup_state_rejects_missing_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, _, _) = receipt_access_raw_with_context(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
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
            raw_json: raw_json.clone(),
            parsed_version: Some(1),
            parsed_kind: Some("paykit.receipt_access".into()),
            parsed_app_id: Some("bitkit".into()),
            known_paykit_kind: Some("paykit.receipt_access".into()),
            parse_status: PrivateStreamParseStatus::Valid,
            parse_error: None,
            received_at: timestamp(),
        }],
        event_dedup_records: vec![EventDedupRecord {
            counterparty,
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
