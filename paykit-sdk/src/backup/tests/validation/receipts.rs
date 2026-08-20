use super::*;

#[test]
fn test_receipt_validation_rejects_later_mismatched_authorized_access() {
    let recipient = public_key();
    let issuer = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let key = "receipt-secret";
    let access = ReceiptAccessRecord {
        app_id: app_id(),
        app_authorized: true,
        counterparty: issuer.clone(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: format!("/pub/paykit/v0/private/receipts/{receipt_id}"),
        key: key.into(),
        retrieval_status: crate::ReceiptRetrievalStatus::Retrieved,
        retrieval_attempted_at: Some(timestamp()),
        retrieved_at: Some(timestamp()),
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: issuer.clone(),
        app_id: app_id(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash(key),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: recipient.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    };
    let mut mismatched = access.clone();
    mismatched.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
    mismatched.stream_item_id = 2;
    mismatched.payment_reference = "other-invoice".into();
    let records = HashMap::from([((issuer.clone(), receipt_id.into()), receipt)]);
    let accesses = HashMap::from([
        ((issuer.clone(), access.event_id.clone()), access),
        ((issuer, mismatched.event_id.clone()), mismatched),
    ]);

    let result = validate_receipt_records(&records, &accesses, &HashMap::new(), Some(&recipient));

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_key_hash_mismatch() {
    let storage = InMemoryStorage::new();
    let counterparty = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let access = ReceiptAccessRecord {
        app_id: app_id(),
        app_authorized: false,
        counterparty: counterparty.clone(),
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: format!("/pub/paykit/v0/private/receipts/{receipt_id}"),
        key: "receipt-secret".into(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: counterparty.clone(),
        app_id: app_id(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash("wrong-secret"),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: counterparty.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(counterparty)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: Vec::new(),
        event_dedup_records: Vec::new(),
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 1,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_recipient_mismatch() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let issuer = public_key();
    let wrong_recipient = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let (raw_json, location, key) = receipt_access_raw_with_context(
        event_id,
        receipt_id,
        "invoice-2026-0001",
        payment_request_id,
        &period,
    );
    let access = ReceiptAccessRecord {
        app_id: app_id(),
        app_authorized: false,
        counterparty: issuer.clone(),
        stream_item_id: 1,
        receive_batch_id: 0,
        event_id: event_id.into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some(payment_request_id.into()),
        billing_period: Some(period.clone()),
        location: location.clone(),
        key: key.clone(),
        retrieval_status: crate::ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: issuer.clone(),
        app_id: app_id(),
        receipt_access_event_id: event_id.into(),
        receipt_access_key_hash: receipt_access_key_hash(&key),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: wrong_recipient,
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location,
        retrieved_at: timestamp(),
    };
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        retired_paykit_apps: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items: vec![PrivateStreamItemRecord {
            stream_item_id: 1,
            counterparty: issuer.clone(),
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
            counterparty: issuer,
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw_json),
            first_stream_item_id: 1,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        }],
        receipt_access_records: vec![access],
        receipt_records: vec![receipt],
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 2,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_rejects_receipt_issuance_access_mismatch() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        paykit_lib::ReceiptDraft {
            receipt_id: Some(
                paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            ),
            payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
            payment_request_id: None,
            billing_period: None,
            payment_endpoint_identifier: Some(
                paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
            metadata: serde_json::Map::new(),
        },
    )
    .unwrap();
    let mut issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, app_id(), prepared, timestamp())
            .unwrap();
    issuance.payment_reference = "different-reference".into();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_restore_backup_state_redacts_invalid_receipt_issuance() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let counterparty = public_key();
    let prepared = paykit_lib::prepare_receipt_for_recipient(
        counterparty.to_public_key().unwrap(),
        paykit_lib::ReceiptDraft {
            receipt_id: Some(
                paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            ),
            payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
            payment_request_id: None,
            billing_period: None,
            payment_endpoint_identifier: Some(
                paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
            metadata: serde_json::Map::new(),
        },
    )
    .unwrap();
    let mut issuance =
        ReceiptIssuanceRecord::from_prepared(counterparty, app_id(), prepared, timestamp())
            .unwrap();
    let sentinel = "SENTINEL_PRIVATE_RECEIPT_CONTENT";
    issuance.encrypted_receipt = format!(r#"{{"sentinel":"{sentinel}""#);
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![issuance],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let err = restore_backup_state(&storage, backup).await.unwrap_err();

    assert!(matches!(
        &err,
        PaykitSdkError::Protocol { context, source }
            if context == "stored encrypted receipt is invalid" && source.is_none()
    ));
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "stored encrypted Receipt leaked into Display/Debug: {rendered}"
    );
}

#[tokio::test]
async fn test_restore_backup_state_rejects_duplicate_receipt_issuance_ids() {
    let storage = InMemoryStorage::new();
    let local_public_key = public_key();
    let first_counterparty = public_key();
    let second_counterparty = public_key();
    let receipt_id = paykit_lib::ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let draft = || paykit_lib::ReceiptDraft {
        receipt_id: Some(receipt_id.clone()),
        payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: serde_json::Map::new(),
    };
    let first = ReceiptIssuanceRecord::from_prepared(
        first_counterparty.clone(),
        app_id(),
        paykit_lib::prepare_receipt_for_recipient(
            first_counterparty.to_public_key().unwrap(),
            draft(),
        )
        .unwrap(),
        timestamp(),
    )
    .unwrap();
    let second = ReceiptIssuanceRecord::from_prepared(
        second_counterparty.clone(),
        app_id(),
        paykit_lib::prepare_receipt_for_recipient(
            second_counterparty.to_public_key().unwrap(),
            draft(),
        )
        .unwrap(),
        timestamp(),
    )
    .unwrap();
    let backup = SdkBackupState {
        version: SDK_BACKUP_VERSION,
        identity_state: Some(identity(local_public_key)),
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
        receipt_issuance_records: vec![first, second],
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 0,
        next_private_stream_item_id: 0,
    };

    let result = restore_backup_state(&storage, backup).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
