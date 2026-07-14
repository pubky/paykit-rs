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

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
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

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
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

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
    assert_eq!(queued[0].attempt_count, 0);
}
