use super::*;
use crate::domain::allowances::{
    allowance_record_from_state, AllowanceHistoryStatus, AllowanceLifecycleState,
    AllowanceLocalRole,
};
use paykit_lib::AllowanceId;

const SHARED_EVENT_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201";

fn legacy_allowance_item(
    stream_item_id: u64,
    counterparty: &PubkyPublicKey,
    raw_json: String,
) -> PrivateStreamItemRecord {
    let header: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
    PrivateStreamItemRecord {
        stream_item_id,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        receive_batch_id: 0,
        raw_json,
        parsed_version: header
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .map(|version| version as u32),
        parsed_kind: header
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        known_paykit_kind: None,
        parse_status: PrivateStreamParseStatus::UnknownKind,
        parse_error: None,
        received_at: timestamp(),
    }
}

fn known_event_item(
    stream_item_id: u64,
    counterparty: &PubkyPublicKey,
    kind: PrivateMessageKind,
    raw_json: String,
) -> PrivateStreamItemRecord {
    PrivateStreamItemRecord {
        stream_item_id,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        receive_batch_id: 0,
        raw_json,
        parsed_version: Some(1),
        parsed_kind: Some(kind.as_str().into()),
        known_paykit_kind: Some(kind.as_str().into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: None,
        received_at: timestamp(),
    }
}

fn backup_with_stream(
    local_public_key: PubkyPublicKey,
    private_stream_items: Vec<PrivateStreamItemRecord>,
) -> SdkBackupState {
    SdkBackupState {
        version: SDK_BACKUP_VERSION,
        local_receiver_path: receiver_path(),
        identity_state: Some(identity(local_public_key)),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: Vec::new(),
        outbound_private_messages: Vec::new(),
        private_stream_items,
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 0,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 0,
    }
}

#[tokio::test]
async fn test_restore_migrates_legacy_allowances_idempotently() {
    let counterparty = public_key();
    let items = allowance_event_jsons()
        .into_iter()
        .enumerate()
        .map(|(index, (_, raw_json))| {
            let mut item = legacy_allowance_item(index as u64 + 1, &counterparty, raw_json);
            if index == 0 {
                item.counterparty_receiver_path = other_receiver_path();
            }
            item
        })
        .collect();
    let storage = InMemoryStorage::new();

    restore_backup_state(&storage, backup_with_stream(public_key(), items))
        .await
        .unwrap();
    let restored = storage.snapshot().unwrap();
    let allowance = allowance_record_from_state(
        &restored,
        &counterparty,
        &other_receiver_path(),
        &AllowanceId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab44").unwrap(),
    )
    .unwrap();
    assert_eq!(allowance.counterparty, counterparty);
    assert_eq!(allowance.counterparty_receiver_path, other_receiver_path());
    assert_eq!(allowance.state, AllowanceLifecycleState::Proposed);
    assert_eq!(allowance.history_status, AllowanceHistoryStatus::Consistent);
    assert_eq!(allowance.local_role, Some(AllowanceLocalRole::Allowee));
    assert!(allowance.terms.is_some());

    let migrated = export_backup_state(&storage, receiver_path())
        .await
        .unwrap();
    let second_storage = InMemoryStorage::new();
    restore_backup_state(&second_storage, migrated.clone())
        .await
        .unwrap();
    let restored_again = export_backup_state(&second_storage, receiver_path())
        .await
        .unwrap();

    assert_eq!(migrated, restored_again);
    assert_eq!(migrated.event_dedup_records.len(), 4);
    assert!(migrated.private_stream_items.iter().all(|item| {
        item.parse_status == PrivateStreamParseStatus::Valid
            && item.known_paykit_kind == item.parsed_kind
    }));
}

#[tokio::test]
async fn test_restore_migrates_malformed_legacy_allowance_for_audit() {
    let counterparty = public_key();
    let raw_json = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID).replacen(
        '{',
        r#"{"unexpected":true,"#,
        1,
    );
    let item = legacy_allowance_item(1, &counterparty, raw_json.clone());
    let storage = InMemoryStorage::new();

    restore_backup_state(&storage, backup_with_stream(public_key(), vec![item]))
        .await
        .unwrap();
    let restored = storage.snapshot().unwrap();

    assert_eq!(restored.private_stream_items[0].raw_json, raw_json);
    assert_eq!(
        restored.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert!(restored.private_stream_items[0].parse_error.is_some());
    assert_eq!(restored.event_dedup_records.len(), 1);
}

#[tokio::test]
async fn test_restore_rebuilds_affected_dedupe_across_event_kinds() {
    let counterparty = public_key();
    let allowance_json = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID);
    let request_json = payment_request_json(SHARED_EVENT_ID);
    let mut other_link_request = known_event_item(
        4,
        &counterparty,
        PrivateMessageKind::PaymentRequest,
        request_json.clone(),
    );
    other_link_request.counterparty_receiver_path = other_receiver_path();
    let mut backup = backup_with_stream(
        public_key(),
        vec![
            other_link_request,
            known_event_item(
                3,
                &counterparty,
                PrivateMessageKind::PaymentRequest,
                request_json.clone(),
            ),
            legacy_allowance_item(2, &counterparty, allowance_json.clone()),
            legacy_allowance_item(1, &counterparty, allowance_json.clone()),
        ],
    );
    backup.event_dedup_records = vec![
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: SHARED_EVENT_ID.into(),
            event_kind: PrivateMessageKind::PaymentRequest.as_str().into(),
            payload_hash: payload_hash(&request_json),
            first_stream_item_id: 3,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: other_receiver_path(),
            event_id: SHARED_EVENT_ID.into(),
            event_kind: PrivateMessageKind::PaymentRequest.as_str().into(),
            payload_hash: payload_hash(&request_json),
            first_stream_item_id: 4,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
    ];
    let storage = InMemoryStorage::new();

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();
    let dedupe =
        &restored.event_dedup_records[&(counterparty, receiver_path(), SHARED_EVENT_ID.into())];

    assert_eq!(dedupe.event_kind, "paykit.allowance_proposal");
    assert_eq!(dedupe.first_stream_item_id, 1);
    assert_eq!(dedupe.duplicate_stream_item_ids, vec![2]);
    assert_eq!(dedupe.conflicting_stream_item_ids, vec![3]);
    let other_link_dedupe = &restored.event_dedup_records[&(
        dedupe.counterparty.clone(),
        other_receiver_path(),
        SHARED_EVENT_ID.into(),
    )];
    assert_eq!(other_link_dedupe.first_stream_item_id, 4);
    assert!(other_link_dedupe.conflicting_stream_item_ids.is_empty());
}

#[tokio::test]
async fn test_restore_removes_receipt_indexes_when_allowance_becomes_first() {
    let local_public_key = public_key();
    let counterparty = public_key();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let payment_request_id = "750e8400-e29b-41d4-a716-446655440000";
    let period = BillingPeriodRecord {
        starts_at: "2026-06-01T00:00:00Z".into(),
        ends_at: "2026-07-01T00:00:00Z".into(),
    };
    let allowance_json = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID);
    let (receipt_json, location, key) = receipt_access_raw_with_context(
        SHARED_EVENT_ID,
        receipt_id,
        "invoice-2026-0001",
        payment_request_id,
        &period,
    );
    let access = ReceiptAccessRecord {
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 2,
        receive_batch_id: 0,
        event_id: SHARED_EVENT_ID.into(),
        receipt_id: receipt_id.into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: Some(payment_request_id.into()),
        billing_period: Some(period.clone()),
        location: location.clone(),
        key: key.clone(),
        retrieval_status: ReceiptRetrievalStatus::Retrieved,
        retrieval_attempted_at: Some(timestamp()),
        retrieved_at: Some(timestamp()),
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = ReceiptRecord {
        issuer: counterparty.clone(),
        issuer_receiver_path: receiver_path(),
        receipt_access_event_id: SHARED_EVENT_ID.into(),
        receipt_access_key_hash: receipt_access_key_hash(&key),
        receipt_id: receipt_id.into(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: local_public_key.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location,
        retrieved_at: timestamp(),
    };
    let mut backup = backup_with_stream(
        local_public_key,
        vec![
            legacy_allowance_item(1, &counterparty, allowance_json),
            known_event_item(
                2,
                &counterparty,
                PrivateMessageKind::ReceiptAccess,
                receipt_json.clone(),
            ),
        ],
    );
    backup.event_dedup_records = vec![EventDedupRecord {
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path(),
        event_id: SHARED_EVENT_ID.into(),
        event_kind: PrivateMessageKind::ReceiptAccess.as_str().into(),
        payload_hash: payload_hash(&receipt_json),
        first_stream_item_id: 2,
        duplicate_stream_item_ids: Vec::new(),
        conflicting_stream_item_ids: Vec::new(),
    }];
    backup.receipt_access_records = vec![access.clone()];
    backup.receipt_records = vec![receipt];
    let storage = InMemoryStorage::new();

    restore_backup_state(&storage, backup).await.unwrap();
    let restored = storage.snapshot().unwrap();

    assert!(restored.receipt_access_records.is_empty());
    assert!(restored.receipt_records.is_empty());
    assert_eq!(
        restored.event_dedup_records[&(counterparty, receiver_path(), SHARED_EVENT_ID.into())]
            .conflicting_stream_item_ids,
        vec![2]
    );
    let stale_access_records = HashMap::from([(
        (
            access.counterparty.clone(),
            access.counterparty_receiver_path.clone(),
            access.event_id.clone(),
        ),
        access,
    )]);
    assert!(matches!(
        validate_required_private_stream_indexes(
            &restored.private_stream_items,
            &restored.event_dedup_records,
            &stale_access_records,
        ),
        Err(PaykitSdkError::Protocol { .. })
    ));
}

#[tokio::test]
async fn test_restore_rejects_non_exact_legacy_allowance_metadata() {
    let counterparty = public_key();
    let raw_json = allowance_event_json("paykit.allowance_proposal", SHARED_EVENT_ID).replacen(
        r#""version":1,"#,
        "",
        1,
    );
    let item = legacy_allowance_item(1, &counterparty, raw_json);
    let storage = InMemoryStorage::new();

    let result = restore_backup_state(&storage, backup_with_stream(public_key(), vec![item])).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}
