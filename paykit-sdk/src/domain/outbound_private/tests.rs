use chrono::{TimeZone, Utc};

use super::*;
use crate::storage::InMemoryStorage;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn raw_private_list() -> String {
    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into()
}

#[test]
fn test_failure_report_debug_redacts_errors() {
    let report = OutboundPrivateCounterpartySendReport {
        counterparty: counterparty(),
        counterparty_receiver_path: receiver_path(),
        report: Some(OutboundPrivateSendReport {
            attempted: vec![1],
            sent: vec![],
            failed: vec![OutboundPrivateSendFailure {
                outbound_message_id: 1,
                error: "payment-reference-secret".into(),
            }],
            reservation_cleanup_failures: vec![ReservationCleanupFailure {
                reservation_id: Some("reservation-id".into()),
                error: "reservation-secret".into(),
            }],
            recovery_marker_failures: vec![RecoveryMarkerPublishFailure {
                outbound_message_id: Some(1),
                error: "marker-secret".into(),
            }],
        }),
        error: Some("counterparty-secret".into()),
    };

    let debug = format!("{report:?}");
    assert!(debug.contains("<redacted:"));
    assert!(!debug.contains("payment-reference-secret"));
    assert!(!debug.contains("reservation-secret"));
    assert!(!debug.contains("marker-secret"));
    assert!(!debug.contains("counterparty-secret"));
}

#[tokio::test]
async fn test_enqueue_private_message_stores_pending_record() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let record = enqueue_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        raw_private_list(),
        timestamp(),
    )
    .await
    .unwrap();

    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(record.outbound_message_id, 0);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_unknown_kind() {
    let result = enqueue_private_message(
        &InMemoryStorage::new(),
        counterparty(),
        receiver_path(),
        r#"{"version":1,"kind":"paykit.unknown"}"#.into(),
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_malformed_known_body() {
    let result = enqueue_private_message(
        &InMemoryStorage::new(),
        counterparty(),
        receiver_path(),
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"../bad":"ln"}}"#
            .into(),
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[test]
fn test_validate_queued_outbound_private_message_rejects_malformed_known_body() {
    let record = OutboundPrivateMessageRecord {
            outbound_message_id: 7,
            counterparty: counterparty(),
            counterparty_receiver_path: receiver_path(),
            kind: "paykit.private_payment_list".into(),
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"../bad":"ln"}}"#
                    .into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        };

    let result = validate_queued_outbound_private_message(&record);

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_claim_next_outbound_private_message_reclaims_stale_sending() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    enqueue_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        raw_private_list(),
        timestamp(),
    )
    .await
    .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    assert_eq!(claimed.attempt_count, 1);

    let duplicate_claim = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap();
    assert!(duplicate_claim.is_none());

    let stale_claim = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp() + chrono::Duration::seconds(61),
        timestamp(),
        timestamp(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stale_claim.outbound_message_id, claimed.outbound_message_id);
    assert_eq!(stale_claim.status, OutboundPrivateMessageStatus::Sending);
    assert_eq!(stale_claim.attempt_count, 2);
}

#[tokio::test]
async fn test_claim_next_outbound_private_message_rejects_stale_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    enqueue_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        raw_private_list(),
        timestamp(),
    )
    .await
    .unwrap();

    let first_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    )
                    .unwrap())
            }
        })
        .await
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

    let result = claim_next_outbound_private_message_with_peer_lease(
        &storage,
        &counterparty,
        timestamp() + chrono::Duration::seconds(12),
        timestamp(),
        timestamp(),
        first_lease,
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
    assert_eq!(queued[0].attempt_count, 0);
}

const SENTINEL: &str = "SENTINEL-9f4c-DO-NOT-PRINT";

/// SECURITY: the persisted `last_error` and the send-report failure text for
/// an invalid queue head must never echo queued payload content. Records are
/// inserted directly (bypassing enqueue validation, like a record written by
/// a newer binary), then run through the exact validate-and-mark fold the
/// runtime flush applies to a claimed head in
/// `claimed_message_ready_for_send`. The full runtime flush entry point
/// requires a live Pubky session, so the fold is exercised at the domain
/// layer it lives in.
#[tokio::test]
async fn test_outbound_validation_and_last_error_never_contain_sentinels() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let unknown_kind_json = format!(r#"{{"version":1,"kind":"{SENTINEL}"}}"#);
    let records = vec![
        // Unknown body kind: the sentinel is the kind string (the old flush
        // path leaked it into `last_error` via a kind echo).
        (
            "paykit.private_payment_list".to_owned(),
            unknown_kind_json.clone(),
        ),
        // Kind column diverges from a valid body: the sentinel is the column.
        (SENTINEL.to_owned(), raw_private_list()),
        // Malformed recognized body: the sentinel is the offending value.
        (
            "paykit.private_payment_list".to_owned(),
            format!(
                r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":"{SENTINEL}"}}"#
            ),
        ),
    ];
    for (kind, raw_json) in records {
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        receiver_path(),
                        kind,
                        raw_json,
                        timestamp(),
                    ));
                    Ok(())
                }
            })
            .await
            .unwrap();
    }

    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(queued.len(), 3);
    for record in queued {
        let err = validate_queued_outbound_private_message(&record).unwrap_err();
        assert!(!format!("{err}").contains(SENTINEL));
        assert!(!format!("{err:?}").contains(SENTINEL));
        // The runtime fold: stringify, report, persist as `last_error`.
        let error = err.to_string();
        let failure = OutboundPrivateSendFailure {
            outbound_message_id: record.outbound_message_id,
            error: error.clone(),
        };
        assert!(!failure.error.contains(SENTINEL));
        let is_unknown_kind = record.raw_json == unknown_kind_json;
        let invalid = mark_outbound_invalid(record, error, timestamp());
        let last_error = invalid.last_error.as_deref().unwrap();
        assert!(
            !last_error.contains(SENTINEL),
            "sentinel leaked into last_error: {last_error}"
        );
        if is_unknown_kind {
            // The stable unsupported-kind block string, with no kind echo.
            assert_eq!(
                last_error,
                "protocol error: unsupported private message kind"
            );
        }
        storage
            .transaction(move |tx| {
                tx.save_outbound_private_message(invalid.clone())?;
                Ok(())
            })
            .await
            .unwrap();
    }

    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.outbound_private_messages.len(), 3);
    for message in &snapshot.outbound_private_messages {
        assert_eq!(message.status, OutboundPrivateMessageStatus::Invalid);
        assert!(!message.last_error.as_deref().unwrap().contains(SENTINEL));
    }
}

/// SECURITY: `PaykitSdkError` values produced by outbound validation and by
/// lib parse errors converted through `From<PaykitError>` must not echo
/// message content in `Display` or `Debug` (the `Debug` check proves no
/// retained source carries it either).
#[test]
fn test_sdk_error_contexts_never_contain_sentinels() {
    let payloads = vec![
        // Invalid JSON with sentinel content.
        format!(r#"{{"version":1,"kind":"paykit.private_payment_list","{SENTINEL}"#),
        // Unknown kind: the sentinel is the kind string.
        format!(r#"{{"version":1,"kind":"{SENTINEL}"}}"#),
        // Malformed recognized bodies: the sentinel is a value, a map key,
        // a field name, and a Receipt Access location.
        format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":"{SENTINEL}"}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"{SENTINEL}!":"ln"}}}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","{SENTINEL}":true}}"#
        ),
        format!(
            r#"{{"version":1,"kind":"paykit.receipt_access","event_id":"650e8400-e29b-41d4-a716-446655440000","receipt_id":"550e8400-e29b-41d4-a716-446655440000","payment_reference":"invoice-2026-0001","location":"{SENTINEL}","key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#
        ),
    ];
    for raw_json in &payloads {
        let err = validate_outbound_private_message(raw_json).unwrap_err();
        assert!(
            !format!("{err}").contains(SENTINEL),
            "sentinel leaked into Display: {err}"
        );
        assert!(!format!("{err:?}").contains(SENTINEL));
    }

    // Lib parse errors crossing into the SDK through `From<PaykitError>`.
    let lib_err = paykit_lib::parse_private_payment_list_json(&payloads[3]).unwrap_err();
    let sdk_err = PaykitSdkError::from(lib_err);
    assert!(!format!("{sdk_err}").contains(SENTINEL));
    assert!(!format!("{sdk_err:?}").contains(SENTINEL));
    let lib_err = paykit_lib::parse_receipt_access_json(&payloads[5]).unwrap_err();
    let sdk_err = PaykitSdkError::from(lib_err);
    assert!(!format!("{sdk_err}").contains(SENTINEL));
    assert!(!format!("{sdk_err:?}").contains(SENTINEL));
}
