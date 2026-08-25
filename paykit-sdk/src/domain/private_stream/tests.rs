use chrono::{TimeZone, Utc};

use super::normalize::{
    normalize_private_stream_classifications, PrivateStreamNormalizationReport,
};
use super::*;
use crate::{
    domain::linked_peers::LinkedPeerState,
    domain::receipts::{
        receipt_access_key_hash, ReceiptAccessRecord, ReceiptRecord, ReceiptRetrievalStatus,
    },
    storage::{
        run_storage_state_transaction, EncryptedLinkStateRecord, InMemoryStorage, LinkedPeerRecord,
        StorageState,
    },
    PaykitSdkError, PrivateStreamParseStatus,
};

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn receiver_path() -> paykit_lib::PaykitReceiverPath {
    paykit_lib::PaykitReceiverPath::new("bitkit/wallet").unwrap()
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
        &paykit_lib::ReceiptAccess::location(&receiver_path(), &receipt_id),
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

// A cached decrypted Receipt consistent with the given access record.
fn receipt_record_from_access(access: &ReceiptAccessRecord) -> ReceiptRecord {
    ReceiptRecord {
        issuer: access.counterparty.clone(),
        issuer_receiver_path: access.counterparty_receiver_path.clone(),
        receipt_access_event_id: access.event_id.clone(),
        receipt_access_key_hash: receipt_access_key_hash(&access.key),
        receipt_id: access.receipt_id.clone(),
        payment_reference: access.payment_reference.clone(),
        payment_request_id: access.payment_request_id.clone(),
        billing_period: access.billing_period.clone(),
        recipient_public_key: access.counterparty.clone(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: serde_json::Map::new(),
        location: access.location.clone(),
        retrieved_at: timestamp(),
    }
}

fn receipt_record_key(
    record: &ReceiptRecord,
) -> (PubkyPublicKey, paykit_lib::PaykitReceiverPath, String) {
    (
        record.issuer.clone(),
        record.issuer_receiver_path.clone(),
        record.receipt_id.clone(),
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
                    counterparty_receiver_path: receiver_path(),
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
        counterparty_receiver_path: receiver_path(),
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
        receiver_path(),
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
    assert_eq!(
        snapshot.encrypted_link_states[&(counterparty.clone(), receiver_path())],
        link_state
    );
    assert_eq!(snapshot.event_dedup_records.len(), 1);
    let peer = snapshot
        .linked_peers
        .get(&(counterparty.clone(), receiver_path()))
        .unwrap();
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
                    counterparty_receiver_path: receiver_path(),
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
        counterparty_receiver_path: receiver_path(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp(),
    };

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        Vec::new(),
        Some(link_state),
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let peer = snapshot
        .linked_peers
        .get(&(counterparty.clone(), receiver_path()))
        .unwrap();
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
        receiver_path(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let records =
        crate::domain::receipts::receipt_access_records(&storage, &counterparty, &receiver_path())
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
        &receiver_path(),
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
        receiver_path(),
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

    let records =
        crate::domain::receipts::receipt_access_records(&storage, &counterparty, &receiver_path())
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
        .get(&(counterparty, receiver_path(), event_id.into()))
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
        "/pub/paykit/v0/private/bitkit/wallet/receipts/not-the-receipt-id",
    );

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
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
    let records =
        crate::domain::receipts::receipt_access_records(&storage, &counterparty, &receiver_path())
            .await
            .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn test_persist_private_stream_batch_skips_wrong_receiver_receipt_access_index() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let wrong_receiver_path = paykit_lib::PaykitReceiverPath::new("tether/wallet").unwrap();
    let raw = receipt_access_raw_with_location(
        "650e8400-e29b-41d4-a716-446655440000",
        receipt_id,
        "invoice-2026-0001",
        &paykit_lib::ReceiptAccess::location(
            &wrong_receiver_path,
            &paykit_lib::ReceiptId::new(receipt_id).unwrap(),
        ),
    );

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
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
    assert_eq!(
        snapshot.private_stream_items[0].parse_error.as_deref(),
        Some(RECEIPT_ACCESS_RECEIVER_SCOPE_PARSE_ERROR)
    );
    // The persisted summary must not echo either receiver path.
    let parse_error = snapshot.private_stream_items[0]
        .parse_error
        .as_deref()
        .unwrap();
    assert!(!parse_error.contains("bitkit/wallet"));
    assert!(!parse_error.contains("tether"));
    let records =
        crate::domain::receipts::receipt_access_records(&storage, &counterparty, &receiver_path())
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

    let report = persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        messages,
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    let record = snapshot
        .event_dedup_records
        .get(&(
            counterparty,
            receiver_path(),
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101".into(),
        ))
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
        receiver_path(),
        vec![private_message(&payment_request_raw("invoice-2026-0001"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let second_report = persist_private_stream_batch(
        &storage,
        second_counterparty,
        receiver_path(),
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
        receiver_path(),
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
    // The persisted summary is exactly the stable redacted category string,
    // never serde detail or the offending value.
    assert_eq!(
        item.parse_error.as_deref(),
        Some(paykit_lib::PrivateMessageParseCategory::InvalidStructure.as_str())
    );
    let parse_error = item.parse_error.as_deref().unwrap();
    assert!(!parse_error.contains("amount.value"));
    assert!(!parse_error.contains("decimal"));
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
        receiver_path(),
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
        receiver_path(),
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

/// SECURITY: intake must persist only stable redacted parse summaries. A
/// sentinel placed anywhere serde or validation errors used to echo message
/// content (values, map keys, field names, kind strings, locations) must
/// never reach a persisted `parse_error`.
#[tokio::test]
async fn test_intake_parse_summaries_never_contain_sentinels() {
    const SENTINEL: &str = "SENTINEL-9f4c-DO-NOT-PRINT";
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let payloads = [
        // serde type mismatch: the sentinel is the offending value.
        format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":"{SENTINEL}"}}"#
        ),
        // Invalid Payment Endpoint Identifier: the sentinel is the map key
        // (the trailing "!" makes the identifier fail validation).
        format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"{SENTINEL}!":"ln"}}}}"#
        ),
        // Payment Request structural failure: the sentinel is the amount value.
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{{"amount":{{"value":"{SENTINEL}","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
        ),
        // serde unknown-field error: the sentinel is the field name.
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","{SENTINEL}":true}}"#
        ),
        // Receipt Access invariant failure: the sentinel is the location.
        receipt_access_raw_with_location(
            "650e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            "invoice-2026-0001",
            SENTINEL,
        ),
        // Unknown kind: the sentinel is the kind string itself.
        format!(r#"{{"version":1,"kind":"{SENTINEL}"}}"#),
        // Invalid JSON: the sentinel is trailing garbage.
        format!(r#"{{"version":1,"kind":"paykit.payment_request","{SENTINEL}"#),
    ];
    let messages = payloads
        .iter()
        .map(|raw_json| {
            let (parsed_version, parsed_kind, _) = private_message_header(raw_json).unwrap();
            private_application_message_from_raw(raw_json.clone(), parsed_version, parsed_kind)
        })
        .collect::<Vec<_>>();

    persist_private_stream_batch(
        &storage,
        counterparty,
        receiver_path(),
        messages,
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.private_stream_items.len(), payloads.len());
    // Every persisted summary comes from a closed stable vocabulary.
    let allowed: Vec<&str> = paykit_lib::PrivateMessageParseCategory::ALL
        .iter()
        .map(|category| category.as_str())
        .chain([RECEIPT_ACCESS_RECEIVER_SCOPE_PARSE_ERROR])
        .collect();
    for item in &snapshot.private_stream_items {
        if let Some(parse_error) = item.parse_error.as_deref() {
            assert!(
                !parse_error.contains(SENTINEL),
                "sentinel leaked into parse_error: {parse_error}"
            );
            assert!(
                allowed.contains(&parse_error),
                "parse_error is not a stable redacted summary: {parse_error}"
            );
        }
        // Debug output must redact every channel that can echo message
        // content: raw_json, parse_error, and a non-canonical parsed kind
        // (the unknown-kind payload's sentinel is its kind string).
        let debug = format!("{item:?}");
        assert!(
            !debug.contains(SENTINEL),
            "sentinel leaked into Debug output: {debug}"
        );
    }
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
                    &receiver_path(),
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
                    &receiver_path(),
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
        counterparty_receiver_path: receiver_path(),
        link_snapshot: Some(vec![1, 2, 3]),
        handshake_snapshot: None,
        handshake_role: None,
        generation: 1,
        checkpointed_at: timestamp() + chrono::Duration::seconds(12),
    };

    let result = persist_private_stream_batch_with_link_lease(
        &storage,
        counterparty,
        receiver_path(),
        vec![private_message(r#"{"version":1,"kind":"paykit.unknown"}"#)],
        Some(link_state),
        Some(first_lease),
        timestamp() + chrono::Duration::seconds(12),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.private_stream_items.is_empty());
    assert!(snapshot.encrypted_link_states.is_empty());
    assert_eq!(snapshot.next_private_stream_item_id, 0);
}

fn normalize_state(state: StorageState) -> (StorageState, PrivateStreamNormalizationReport) {
    let (state, report) = run_storage_state_transaction(
        state,
        Box::new(|tx| {
            Ok(Box::new(normalize_private_stream_classifications(tx)?)
                as Box<dyn std::any::Any + Send>)
        }),
    )
    .unwrap();
    (
        state,
        *report
            .downcast::<PrivateStreamNormalizationReport>()
            .unwrap(),
    )
}

fn payment_request_event_key(
    counterparty: &PubkyPublicKey,
) -> (PubkyPublicKey, PaykitReceiverPath, String) {
    (
        counterparty.clone(),
        receiver_path(),
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101".to_owned(),
    )
}

#[tokio::test]
async fn test_normalization_of_current_classifier_state_is_noop() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let access_raw = receipt_access_raw(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
    );
    let conflicting_access_raw = receipt_access_raw(
        event_id,
        "750e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0002",
    );
    let out_of_scope_raw = receipt_access_raw_with_location(
        "850e8400-e29b-41d4-a716-446655440000",
        "950e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0003",
        &paykit_lib::ReceiptAccess::location(
            &paykit_lib::PaykitReceiverPath::new("tether/wallet").unwrap(),
            &paykit_lib::ReceiptId::new("950e8400-e29b-41d4-a716-446655440000").unwrap(),
        ),
    );
    let malformed = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![
            private_message(&payment_request_raw("invoice-2026-0001")),
            private_message(&payment_request_raw("invoice-2026-0002")),
            private_message(malformed),
            private_message(r#"{"version":1,"kind":"paykit.unknown","body":{}}"#),
            PrivateApplicationMessage {
                version: None,
                kind: None,
                raw_json: "not json".into(),
            },
            PrivateApplicationMessage {
                version: None,
                kind: None,
                raw_json: "paykit.invalid_utf8_private_message:_w".into(),
            },
        ],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![
            private_message(&access_raw),
            private_message(&access_raw),
            private_message(&conflicting_access_raw),
            private_message(&out_of_scope_raw),
        ],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let mut pristine = storage.snapshot().unwrap();
    // A cached Receipt matching its indexed access record is kept verbatim.
    let access = pristine.receipt_access_records
        [&(counterparty.clone(), receiver_path(), event_id.to_owned())]
        .clone();
    let receipt = receipt_record_from_access(&access);
    pristine
        .receipt_records
        .insert(receipt_record_key(&receipt), receipt);
    let (normalized, report) = normalize_state(pristine.clone());

    assert_eq!(report, PrivateStreamNormalizationReport::default());
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_normalization_is_idempotent() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let access_raw = receipt_access_raw(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&access_raw), private_message(&access_raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    let mut corrupted = storage.snapshot().unwrap();
    corrupted.private_stream_items[0].parse_error = Some("stale serde detail".into());
    let dedup = corrupted.event_dedup_records.get_mut(&key).unwrap();
    dedup.first_stream_item_id = 1;
    dedup.duplicate_stream_item_ids = vec![0];
    corrupted
        .receipt_access_records
        .get_mut(&key)
        .unwrap()
        .stream_item_id = 1;

    let (first_pass, first_report) = normalize_state(corrupted);
    assert_ne!(first_report, PrivateStreamNormalizationReport::default());

    let (second_pass, second_report) = normalize_state(first_pass.clone());
    assert_eq!(second_report, PrivateStreamNormalizationReport::default());
    assert_eq!(second_pass, first_pass);
}

#[tokio::test]
async fn test_normalization_rewrites_only_derived_fields() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&payment_request_raw("invoice-2026-0001"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let pristine = storage.snapshot().unwrap();
    let mut corrupted = pristine.clone();
    let item = &mut corrupted.private_stream_items[0];
    item.parsed_version = Some(99);
    item.parsed_kind = Some("paykit.stale".into());
    item.known_paykit_kind = None;
    item.parse_status = PrivateStreamParseStatus::InvalidJson;
    item.parse_error = Some("stale error".into());

    let (normalized, report) = normalize_state(corrupted);

    assert_eq!(report.items_reclassified, 1);
    assert_eq!(report.event_dedup_records_rewritten, 0);
    let item = &normalized.private_stream_items[0];
    let original = &pristine.private_stream_items[0];
    assert_eq!(item.stream_item_id, original.stream_item_id);
    assert_eq!(item.counterparty, original.counterparty);
    assert_eq!(
        item.counterparty_receiver_path,
        original.counterparty_receiver_path
    );
    assert_eq!(item.receive_batch_id, original.receive_batch_id);
    assert_eq!(item.raw_json, original.raw_json);
    assert_eq!(item.received_at, original.received_at);
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_normalization_rebuilds_event_dedupe_membership_in_stream_order() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let duplicate_raw = payment_request_raw("invoice-2026-0001");
    let conflicting_raw = payment_request_raw("invoice-2026-0002");
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
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

    let pristine = storage.snapshot().unwrap();
    let key = payment_request_event_key(&counterparty);

    // Wrong first carrier.
    let mut corrupted = pristine.clone();
    let record = corrupted.event_dedup_records.get_mut(&key).unwrap();
    record.first_stream_item_id = 1;
    record.duplicate_stream_item_ids = vec![0];
    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.event_dedup_records_rewritten, 1);
    let rebuilt = normalized.event_dedup_records.get(&key).unwrap();
    assert_eq!(rebuilt.first_stream_item_id, 0);
    assert_eq!(rebuilt.duplicate_stream_item_ids, vec![1]);
    assert_eq!(rebuilt.conflicting_stream_item_ids, vec![2]);
    assert_eq!(normalized, pristine);

    // Wrong payload hash.
    let mut corrupted = pristine.clone();
    corrupted
        .event_dedup_records
        .get_mut(&key)
        .unwrap()
        .payload_hash =
        "sha256:0000000000000000000000000000000000000000000000000000000000000000".into();
    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.event_dedup_records_rewritten, 1);
    assert_eq!(normalized, pristine);

    // Missing member.
    let mut corrupted = pristine.clone();
    corrupted
        .event_dedup_records
        .get_mut(&key)
        .unwrap()
        .duplicate_stream_item_ids
        .clear();
    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.event_dedup_records_rewritten, 1);
    assert_eq!(normalized, pristine);

    // Orphan record for an event no stream item carries.
    let mut corrupted = pristine.clone();
    corrupted.event_dedup_records.insert(
        (
            counterparty.clone(),
            receiver_path(),
            "no-such-event".to_owned(),
        ),
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: "no-such-event".into(),
            event_kind: "paykit.payment_request".into(),
            payload_hash: payload_hash(&duplicate_raw),
            first_stream_item_id: 0,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
    );
    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.event_dedup_records_removed, 1);
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_normalization_updates_membership_when_extractability_changes() {
    // An item stored as unextractable is indexed once it classifies as an event.
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&payment_request_raw("invoice-2026-0001"))],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let pristine = storage.snapshot().unwrap();
    let key = payment_request_event_key(&counterparty);
    let mut corrupted = pristine.clone();
    let item = &mut corrupted.private_stream_items[0];
    item.parsed_version = None;
    item.parsed_kind = None;
    item.known_paykit_kind = None;
    item.parse_status = PrivateStreamParseStatus::InvalidJson;
    item.parse_error = None;
    corrupted.event_dedup_records.remove(&key).unwrap();

    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.items_reclassified, 1);
    assert_eq!(report.event_dedup_records_rewritten, 1);
    assert_eq!(normalized, pristine);

    // An indexed item loses its index once it no longer classifies as an event.
    let storage = InMemoryStorage::new();
    let counterparty = self::counterparty();
    let unknown_raw = r#"{"version":1,"kind":"paykit.unknown","event_id":"evt-1"}"#;
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(unknown_raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();
    let pristine = storage.snapshot().unwrap();
    let mut corrupted = pristine.clone();
    corrupted.event_dedup_records.insert(
        (counterparty.clone(), receiver_path(), "evt-1".to_owned()),
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: "evt-1".into(),
            event_kind: "paykit.unknown".into(),
            payload_hash: payload_hash(unknown_raw),
            first_stream_item_id: 0,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
    );

    let (normalized, report) = normalize_state(corrupted);
    assert_eq!(report.event_dedup_records_removed, 1);
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_receipt_access_retrieval_state_preserved_when_first_carrier_unchanged() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let access_raw = receipt_access_raw(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&access_raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    let mut state = storage.snapshot().unwrap();
    let record = state.receipt_access_records.get_mut(&key).unwrap();
    record.retrieval_status = ReceiptRetrievalStatus::Retrieved;
    record.retrieval_attempted_at = Some(timestamp());
    record.retrieved_at = Some(timestamp());
    let retrieved = record.clone();
    // Stale derived item metadata forces a real normalization pass.
    state.private_stream_items[0].parse_error = Some("stale serde detail".into());

    let (normalized, report) = normalize_state(state);

    assert_eq!(report.items_reclassified, 1);
    assert_eq!(report.receipt_access_records_rewritten, 0);
    assert_eq!(report.receipt_retrieval_states_reset, 0);
    assert_eq!(
        normalized.receipt_access_records.get(&key).unwrap(),
        &retrieved
    );
}

#[tokio::test]
async fn test_receipt_access_retrieval_state_reset_when_first_carrier_changes() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let access_raw = receipt_access_raw(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&access_raw), private_message(&access_raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    let mut state = storage.snapshot().unwrap();
    let record = state.receipt_access_records.get_mut(&key).unwrap();
    record.stream_item_id = 1;
    record.retrieval_status = ReceiptRetrievalStatus::Retrieved;
    record.retrieval_attempted_at = Some(timestamp());
    record.retrieved_at = Some(timestamp());

    let (normalized, report) = normalize_state(state);

    assert_eq!(report.receipt_access_records_rewritten, 1);
    assert_eq!(report.receipt_retrieval_states_reset, 1);
    let record = normalized.receipt_access_records.get(&key).unwrap();
    assert_eq!(record.stream_item_id, 0);
    assert_eq!(record.retrieval_status, ReceiptRetrievalStatus::Pending);
    assert!(record.retrieval_attempted_at.is_none());
    assert!(record.retrieved_at.is_none());
    assert!(record.last_retrieval_error.is_none());
}

#[tokio::test]
async fn test_receipt_access_record_removed_when_event_no_longer_indexed() {
    // An out-of-scope Receipt Access event that an older classifier indexed anyway.
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let wrong_receiver_path = paykit_lib::PaykitReceiverPath::new("tether/wallet").unwrap();
    let raw = receipt_access_raw_with_location(
        event_id,
        receipt_id,
        "invoice-2026-0001",
        &paykit_lib::ReceiptAccess::location(
            &wrong_receiver_path,
            &paykit_lib::ReceiptId::new(receipt_id).unwrap(),
        ),
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let pristine = storage.snapshot().unwrap();
    let key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    let mut corrupted = pristine.clone();
    corrupted.event_dedup_records.insert(
        key.clone(),
        EventDedupRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            event_id: event_id.into(),
            event_kind: "paykit.receipt_access".into(),
            payload_hash: payload_hash(&raw),
            first_stream_item_id: 0,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        },
    );
    corrupted.receipt_access_records.insert(
        key.clone(),
        crate::domain::receipts::ReceiptAccessRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path(),
            stream_item_id: 0,
            receive_batch_id: 0,
            event_id: event_id.into(),
            receipt_id: receipt_id.into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            location: "stale-location".into(),
            key: "stale-key".into(),
            retrieval_status: ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        },
    );
    // A Receipt retrieved and cached under that stale index is dropped with
    // it; the plaintext is not re-derivable, but keeping it would leave a
    // dangling reference that fails every later backup restore.
    let receipt = receipt_record_from_access(corrupted.receipt_access_records.get(&key).unwrap());
    corrupted
        .receipt_records
        .insert(receipt_record_key(&receipt), receipt);

    let (normalized, report) = normalize_state(corrupted);

    assert_eq!(report.event_dedup_records_removed, 1);
    assert_eq!(report.receipt_access_records_removed, 1);
    assert_eq!(report.receipt_records_removed, 1);
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_receipt_record_removed_when_access_authority_changes() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let event_id = "650e8400-e29b-41d4-a716-446655440000";
    let access_raw = receipt_access_raw(
        event_id,
        "550e8400-e29b-41d4-a716-446655440000",
        "invoice-2026-0001",
    );
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(&access_raw)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let key = (counterparty.clone(), receiver_path(), event_id.to_owned());
    let pristine = storage.snapshot().unwrap();
    let mut corrupted = pristine.clone();
    // An older classifier derived different access data; the cached Receipt
    // is consistent with that stale record, not with re-derivation.
    let record = corrupted.receipt_access_records.get_mut(&key).unwrap();
    record.location = "stale-location".into();
    record.key = "stale-key".into();
    let receipt = receipt_record_from_access(record);
    corrupted
        .receipt_records
        .insert(receipt_record_key(&receipt), receipt);

    let (normalized, report) = normalize_state(corrupted);

    assert_eq!(report.receipt_access_records_rewritten, 1);
    assert_eq!(report.receipt_retrieval_states_reset, 1);
    assert_eq!(report.receipt_records_removed, 1);
    assert_eq!(normalized, pristine);
}

#[tokio::test]
async fn test_dangling_receipt_record_left_for_restore_validation() {
    // A cached Receipt referencing an event normalization never touched is
    // pre-existing corruption, not orphaning: it stays so backup restore
    // still rejects it instead of silently losing evidence of tampering.
    let access = ReceiptAccessRecord {
        counterparty: counterparty(),
        counterparty_receiver_path: receiver_path(),
        stream_item_id: 0,
        receive_batch_id: 0,
        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
        receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
        payment_reference: "invoice-2026-0001".into(),
        payment_request_id: None,
        billing_period: None,
        location: "stale-location".into(),
        key: "stale-key".into(),
        retrieval_status: ReceiptRetrievalStatus::Pending,
        retrieval_attempted_at: None,
        retrieved_at: None,
        last_retrieval_error: None,
        received_at: timestamp(),
    };
    let receipt = receipt_record_from_access(&access);
    let mut state = StorageState::default();
    state
        .receipt_records
        .insert(receipt_record_key(&receipt), receipt);

    let (normalized, report) = normalize_state(state.clone());

    assert_eq!(report, PrivateStreamNormalizationReport::default());
    assert_eq!(normalized, state);
}

#[tokio::test]
async fn test_frozen_classification_fixture_matches_current_classifier() {
    let actual = classification_fixture::replay_classification_matrix().await;
    let expected: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_JSON)
            .expect("frozen classification fixture must parse");

    if actual != expected {
        panic!(
            "classification decisions drifted from fixtures/classification_matrix_expected.json; \
             an intentional classifier change must re-freeze the file from this actual output:\n{}",
            serde_json::to_string_pretty(&actual).unwrap()
        );
    }
}

#[tokio::test]
async fn test_inspection_and_boundary_decisions_match_frozen_fixtures() {
    // The shared inspection entry point is now the single classification core
    // behind intake, outbound validation, and backup validation. This proof
    // replays the frozen fixture through all three boundaries (byte-comparing
    // the full decision set) and then checks, message by message, that direct
    // inspection agrees with the frozen intake decisions under the documented
    // mapping. The one deliberate divergence is the Receipt Access
    // receiver-scope POLICY pass: it is not inspection, so intake and backup
    // downgrade a payload-intrinsically Valid message after classification
    // while outbound validation never applies it.
    let actual = classification_fixture::replay_classification_matrix().await;
    let expected: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_JSON)
            .expect("frozen classification fixture must parse");
    assert_eq!(
        actual, expected,
        "boundary replay must match the frozen fixture"
    );

    let messages = classification_fixture::classification_fixture_messages();
    let frozen = expected["messages"]
        .as_array()
        .expect("frozen fixture must list messages");
    assert_eq!(messages.len(), frozen.len());

    for (message, decision) in messages.iter().zip(frozen) {
        let label = decision["label"].as_str().expect("label");
        assert_eq!(label, message.label, "fixture order drifted");
        let intake = &decision["intake"];
        let inspection = paykit_lib::inspect_private_application_message(&message.raw_json);

        // The recognized kind is exactly the persisted `known_paykit_kind`
        // column: both classify from the body, never the envelope.
        assert_eq!(
            inspection.known_kind.map(PrivateMessageKind::as_str),
            intake["known_paykit_kind"].as_str(),
            "{label}: known kind"
        );

        let frozen_status = intake["parse_status"].as_str().expect("parse status");
        let frozen_parse_error = intake["parse_error"].as_str();
        let scope_downgraded =
            frozen_parse_error == Some(RECEIPT_ACCESS_RECEIVER_SCOPE_PARSE_ERROR);

        if scope_downgraded {
            // Receiver-scope divergence pin: the payload is intrinsically
            // valid with a recoverable Event ID, and the policy pass (not
            // inspection) downgraded the persisted status and cleared the
            // event header.
            assert_eq!(
                inspection.structure,
                PrivateMessageStructure::Valid,
                "{label}: out-of-scope Receipt Access is payload-intrinsically valid"
            );
            assert_eq!(
                inspection.known_kind,
                Some(PrivateMessageKind::ReceiptAccess),
                "{label}: scope policy applies only to Receipt Access"
            );
            assert_eq!(
                frozen_status, "MalformedRecognized",
                "{label}: policy status"
            );
            assert!(
                inspection.event_id.is_some() && intake["event_id"].is_null(),
                "{label}: policy pass must clear the persisted event header"
            );
        } else {
            // structure <-> parse_status mapping, exhaustively.
            let expected_status = match inspection.structure {
                PrivateMessageStructure::Valid => "Valid",
                PrivateMessageStructure::MalformedRecognized => "MalformedRecognized",
                PrivateMessageStructure::UnknownKind => "UnknownKind",
                PrivateMessageStructure::InvalidJson => "InvalidJson",
            };
            assert_eq!(expected_status, frozen_status, "{label}: parse status");

            // Category string <-> persisted parse summary, byte-for-byte
            // (absent on both sides for Valid, UnknownKind, and non-sentinel
            // InvalidJson payloads).
            assert_eq!(
                inspection
                    .error_category
                    .map(paykit_lib::PrivateMessageParseCategory::as_str),
                frozen_parse_error,
                "{label}: parse summary"
            );

            // Recoverable Event ID <-> persisted event header, including for
            // malformed recognized Event Message bodies.
            assert_eq!(
                inspection.event_id.as_deref(),
                intake["event_id"].as_str(),
                "{label}: event id"
            );
            let expected_event_kind = if inspection.event_id.is_some() {
                inspection.known_kind.map(PrivateMessageKind::as_str)
            } else {
                None
            };
            assert_eq!(
                intake["event_kind"].as_str(),
                expected_event_kind,
                "{label}: event kind"
            );

            // Semantics tie-in: for valid payloads, exactly the Event
            // Message kinds carry a persisted event header.
            if inspection.structure == PrivateMessageStructure::Valid {
                assert_eq!(
                    inspection.semantics == Some(paykit_lib::PrivateMessageSemantics::Event),
                    intake["event_id"].as_str().is_some(),
                    "{label}: semantics vs event header"
                );
            }
        }

        // Outbound boundary: every fixture payload fits the size limit, so
        // outbound accepts exactly the payload-intrinsically Valid messages
        // (receiver scope is not enforced there) and reports the canonical
        // kind string.
        let outbound = &decision["outbound"];
        if inspection.structure == PrivateMessageStructure::Valid {
            assert_eq!(
                outbound["ok"].as_str(),
                inspection.known_kind.map(PrivateMessageKind::as_str),
                "{label}: outbound accept"
            );
        } else {
            assert!(
                outbound["ok"].is_null() && outbound["error"].is_string(),
                "{label}: outbound must reject non-Valid structures"
            );
        }

        // Backup boundary: the fixture state was persisted by the current
        // classifier, so restore validation accepts every item regardless of
        // structure.
        assert_eq!(
            decision["backup"]["ok"].as_bool(),
            Some(true),
            "{label}: backup accept"
        );
    }
}

// Null out the two error-text fields (`intake.parse_error` and
// `outbound.error`) on every fixture message so the remaining decision trees
// can be compared for byte-identity across classifier generations.
fn mask_fixture_error_text(mut decisions: serde_json::Value) -> serde_json::Value {
    for message in decisions["messages"]
        .as_array_mut()
        .expect("fixture decisions must list messages")
    {
        message["intake"]
            .as_object_mut()
            .expect("fixture intake must be an object")
            .insert("parse_error".into(), serde_json::Value::Null);
        let outbound = message["outbound"]
            .as_object_mut()
            .expect("fixture outbound must be an object");
        if outbound.contains_key("error") {
            outbound.insert("error".into(), serde_json::Value::Null);
        }
    }
    decisions
}

#[test]
fn test_legacy_and_current_fixture_decisions_differ_only_in_error_text() {
    // The redaction commit changed only error TEXT: parse summaries and
    // outbound error contexts. Every other decision - parse status, parsed
    // and recognized kinds, event headers, dedupe membership, Receipt Access
    // indexing, backup accept/reject, outbound ok/err direction - must be
    // byte-identical between the legacy generation and the re-frozen one.
    let legacy: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_LEGACY_JSON)
            .expect("legacy classification fixture must parse");
    let current: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_JSON)
            .expect("current classification fixture must parse");

    assert_ne!(
        legacy, current,
        "the generations must actually differ, or the legacy fixture is stale"
    );
    assert_eq!(
        mask_fixture_error_text(legacy),
        mask_fixture_error_text(current),
        "a non-error-text decision changed between classifier generations"
    );
}

#[tokio::test]
async fn test_old_serde_detail_parse_errors_normalize_to_categories() {
    // A state persisted by the pre-redaction classifier carries serde-detail
    // parse summaries (offending values, serde positions, interpolated
    // receiver paths). Normalization must rewrite every one of them to the
    // current stable category strings while leaving raw JSON, immutable
    // source context, dedupe membership, and local retrieval state untouched.
    let messages = classification_fixture::classification_fixture_messages();
    let legacy: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_LEGACY_JSON)
            .expect("legacy classification fixture must parse");
    let current: serde_json::Value =
        serde_json::from_str(classification_fixture::CLASSIFICATION_MATRIX_EXPECTED_JSON)
            .expect("current classification fixture must parse");

    let storage = InMemoryStorage::new();
    persist_private_stream_batch(
        &storage,
        counterparty(),
        classification_fixture::classification_fixture_receiver_path(),
        messages
            .iter()
            .map(|message| classification_fixture::fixture_private_message(&message.raw_json))
            .collect(),
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let pristine = storage.snapshot().unwrap();
    assert_eq!(pristine.private_stream_items.len(), messages.len());
    let mut stale = pristine.clone();
    let mut expected = pristine;

    // Local, non-rebuildable retrieval state recorded before the upgrade: the
    // first carrier's classification is unchanged, so it must be preserved.
    let retrieved_at = timestamp() + chrono::Duration::seconds(5);
    for state in [&mut stale, &mut expected] {
        assert_eq!(state.receipt_access_records.len(), 1);
        let record = state.receipt_access_records.values_mut().next().unwrap();
        record.retrieval_status = ReceiptRetrievalStatus::Retrieved;
        record.retrieval_attempted_at = Some(retrieved_at);
        record.retrieved_at = Some(retrieved_at);
    }

    // Overwrite every derived parse summary with the previous generation's
    // string, verbatim from the frozen legacy fixture.
    let mut stale_summaries = 0usize;
    for (index, item) in stale.private_stream_items.iter_mut().enumerate() {
        let legacy_error = legacy["messages"][index]["intake"]["parse_error"]
            .as_str()
            .map(str::to_owned);
        if item.parse_error != legacy_error {
            stale_summaries += 1;
        }
        item.parse_error = legacy_error;
    }
    assert!(
        stale_summaries > 0,
        "legacy fixture must carry pre-redaction parse summaries"
    );
    assert!(
        stale.private_stream_items.iter().any(|item| {
            item.parse_error
                .as_deref()
                .is_some_and(|error| error.contains("at line 1 column"))
        }),
        "legacy summaries must include serde positional detail"
    );

    let (normalized, report) = normalize_state(stale);

    assert_eq!(
        report,
        PrivateStreamNormalizationReport {
            items_reclassified: stale_summaries,
            ..Default::default()
        }
    );
    assert_eq!(normalized, expected);
    for (index, item) in normalized.private_stream_items.iter().enumerate() {
        assert_eq!(
            item.parse_error.as_deref(),
            current["messages"][index]["intake"]["parse_error"].as_str(),
            "item {index} must match the re-frozen classification"
        );
    }
}
