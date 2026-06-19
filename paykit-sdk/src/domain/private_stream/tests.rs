use chrono::{TimeZone, Utc};

use super::*;
use crate::{
    domain::linked_peers::LinkedPeerState,
    storage::{EncryptedLinkStateRecord, InMemoryStorage, LinkedPeerRecord},
    PaykitSdkError, PrivateStreamParseStatus,
};

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn private_message(raw_json: &str) -> PrivateApplicationMessage {
    let value: serde_json::Value = serde_json::from_str(raw_json).unwrap();
    PrivateApplicationMessage {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok()),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json: raw_json.to_owned(),
    }
}

fn payment_request_raw(reference: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
    )
}

fn receipt_access_raw(event_id: &str, receipt_id: &str, reference: &str) -> String {
    let receipt_id = paykit_lib::ReceiptId::new(receipt_id).unwrap();
    receipt_access_raw_with_location(
        event_id,
        receipt_id.as_str(),
        reference,
        &paykit_lib::ReceiptAccess::location_for(&receipt_id),
    )
}

fn receipt_access_raw_with_location(
    event_id: &str,
    receipt_id: &str,
    reference: &str,
    location: &str,
) -> String {
    let key = paykit_lib::ReceiptDecryptionKey::generate();
    format!(
        r#"{{"version":1,"kind":"paykit.receipt_access","event_id":"{event_id}","receipt_id":"{receipt_id}","payment_reference":"{reference}","location":"{location}","key":"{}"}}"#,
        key.as_str()
    )
}

#[tokio::test]
async fn test_persist_private_stream_batch_stores_messages_and_checkpoint() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::Linked,
                    last_sync_at: None,
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };
    let messages = vec![
        private_message(r#"{"version":1,"kind":"paykit.unknown","body":{}}"#),
        private_message(&payment_request_raw("invoice-2026-0001")),
    ];

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        messages,
        Some(link_state.clone()),
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(report.receive_batch_id, 0);
    assert_eq!(report.stream_item_ids, vec![0, 1]);
    assert_eq!(snapshot.private_stream_items.len(), 2);
    assert_eq!(
        snapshot.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::UnknownKind
    );
    assert_eq!(
        snapshot.private_stream_items[1].parse_status,
        PrivateStreamParseStatus::Valid
    );
    assert_eq!(snapshot.encrypted_link_states[&counterparty], link_state);
    assert_eq!(snapshot.event_dedup_records.len(), 1);
    let peer = snapshot.linked_peers.get(&counterparty).unwrap();
    assert_eq!(peer.last_private_receive_at, Some(timestamp()));
    assert_eq!(peer.last_sync_at, Some(timestamp()));
}

#[tokio::test]
async fn test_persist_private_stream_batch_empty_checkpoint_updates_sync_time() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::Linked,
                    last_sync_at: None,
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        Vec::new(),
        Some(link_state),
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let peer = snapshot.linked_peers.get(&counterparty).unwrap();
    assert!(report.stream_item_ids.is_empty());
    assert_eq!(peer.last_private_receive_at, None);
    assert_eq!(peer.last_sync_at, Some(timestamp()));
}

#[tokio::test]
async fn test_persist_private_stream_batch_indexes_receipt_access() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let raw = receipt_access_raw(event_id, receipt_id, "invoice-2026-0001");

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].stream_item_id, 0);
    assert_eq!(records[0].receive_batch_id, 0);
    assert_eq!(records[0].event_id, event_id);
    assert_eq!(records[0].receipt_id, receipt_id);
    assert_eq!(records[0].payment_reference, "invoice-2026-0001");
    assert!(records[0].payment_request_id.is_none());
    assert!(records[0].billing_period.is_none());
    assert!(records[0].location.ends_with(receipt_id));
    let debug = format!("{:?}", records[0]);
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains(&records[0].key));

    let indexed = crate::domain::receipts::receipt_access_record_by_receipt_id(
        &storage,
        &counterparty,
        receipt_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(indexed.event_id, event_id);
}

#[tokio::test]
async fn test_persist_private_stream_batch_dedupes_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let duplicate_raw = receipt_access_raw(event_id, receipt_id, "invoice-2026-0001");
    let conflicting_raw = receipt_access_raw(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0002",
    );

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            private_message(&duplicate_raw),
            private_message(&duplicate_raw),
            private_message(&conflicting_raw),
        ],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].stream_item_id, 0);
    assert_eq!(records[0].receipt_id, receipt_id);
    assert_eq!(report.event_conflicts.len(), 1);
    assert_eq!(report.event_conflicts[0].conflicting_stream_item_id, 2);
    let snapshot = storage.snapshot().unwrap();
    let dedupe = snapshot
        .event_dedup_records
        .get(&(counterparty, event_id.into()))
        .unwrap();
    assert_eq!(dedupe.duplicate_stream_item_ids, vec![1]);
    assert_eq!(dedupe.conflicting_stream_item_ids, vec![2]);
}

#[tokio::test]
async fn test_persist_private_stream_batch_skips_malformed_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw = receipt_access_raw_with_location(
        "650e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
        "/pub/paykit/v0/private/receipts/not-the-receipt-id",
    );

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.private_stream_items[0].parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    let records = crate::domain::receipts::receipt_access_records(&storage, &counterparty)
        .await
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn test_persist_private_stream_batch_marks_event_id_conflicts() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let messages = vec![
        private_message(&payment_request_raw("invoice-2026-0001")),
        private_message(&payment_request_raw("invoice-2026-0002")),
    ];

    let report =
        persist_private_stream_batch(&storage, counterparty.clone(), messages, None, timestamp())
            .await
            .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let record = snapshot
        .event_dedup_records
        .get(&(counterparty, "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101".into()))
        .unwrap();
    assert_eq!(report.event_conflicts.len(), 1);
    assert_eq!(record.first_stream_item_id, 0);
    assert_eq!(record.conflicting_stream_item_ids, vec![1]);
}

#[tokio::test]
async fn test_persist_private_stream_batch_scopes_event_dedupe_by_counterparty() {
    let storage = InMemoryStorage::new();
    let first_counterparty = counterparty();
    let second_counterparty = counterparty();

    let first_report = persist_private_stream_batch(
        &storage,
        first_counterparty,
        vec![private_message(&payment_request_raw("invoice-2026-0001"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let second_report = persist_private_stream_batch(
        &storage,
        second_counterparty,
        vec![private_message(&payment_request_raw("invoice-2026-0002"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert!(first_report.event_conflicts.is_empty());
    assert!(second_report.event_conflicts.is_empty());
    assert_eq!(snapshot.event_dedup_records.len(), 2);
}

#[tokio::test]
async fn test_persist_private_stream_batch_keeps_malformed_recognized_messages() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let malformed = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;

    persist_private_stream_batch(
        &storage,
        counterparty,
        vec![private_message(malformed)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert!(item
        .parse_error
        .as_ref()
        .is_some_and(|error| error.contains("amount.value")));
    assert_eq!(snapshot.event_dedup_records.len(), 1);
}

#[tokio::test]
async fn test_persist_private_stream_batch_keeps_invalid_json_payloads() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw_json = "not json";

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![PrivateApplicationMessage {
            version: None,
            kind: None,
            raw_json: raw_json.into(),
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(report.stream_item_ids, vec![0]);
    assert_eq!(item.counterparty, counterparty);
    assert_eq!(item.raw_json, raw_json);
    assert_eq!(item.parse_status, PrivateStreamParseStatus::InvalidJson);
}

#[tokio::test]
async fn test_persist_private_stream_batch_records_invalid_utf8_marker_error() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let raw_json = "paykit.invalid_utf8_private_message:_w";

    persist_private_stream_batch(
        &storage,
        counterparty,
        vec![PrivateApplicationMessage {
            version: None,
            kind: None,
            raw_json: raw_json.into(),
        }],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let item = &snapshot.private_stream_items[0];
    assert_eq!(item.parse_status, PrivateStreamParseStatus::InvalidJson);
    assert!(item
        .parse_error
        .as_ref()
        .is_some_and(|error| error.contains("valid UTF-8")));
}

#[tokio::test]
async fn test_persist_private_stream_batch_rolls_back_with_stale_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let first_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp(),
                    timestamp() + chrono::Duration::seconds(10),
                ))
            }
        })
        .await
        .unwrap()
        .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + chrono::Duration::seconds(11),
                    timestamp() + chrono::Duration::seconds(71),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let link_state = EncryptedLinkStateRecord {
        counterparty: counterparty.clone(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp() + chrono::Duration::seconds(12),
    };

    let result = persist_private_stream_batch_with_link_lease(
        &storage,
        counterparty,
        vec![private_message(r#"{"version":1,"kind":"paykit.unknown"}"#)],
        Some(link_state),
        Some(first_lease),
        timestamp() + chrono::Duration::seconds(12),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.private_stream_items.is_empty());
    assert!(snapshot.encrypted_link_states.is_empty());
    assert_eq!(snapshot.next_private_stream_item_id, 0);
}
