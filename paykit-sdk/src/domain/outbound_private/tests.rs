use chrono::{TimeZone, Utc};

use super::*;
use crate::storage::InMemoryStorage;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn app_id() -> paykit_lib::PaykitAppId {
    paykit_lib::PaykitAppId::new("bitkit").unwrap()
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps([app_id()])
}

fn raw_private_list() -> String {
    r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#
        .into()
}

fn raw_payment_request(app_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","app_id":"{app_id}","event_id":"650e8400-e29b-41d4-a716-446655440000","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{{"amount":{{"value":"1","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{}}}}}}"#
    )
}

#[test]
fn test_failure_report_debug_redacts_errors() {
    let report = OutboundPrivateCounterpartySendReport {
        counterparty: counterparty(),
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
    assert!(!debug.contains("reservation-id"));
    assert!(!debug.contains("reservation-secret"));
    assert!(!debug.contains("marker-secret"));
    assert!(!debug.contains("counterparty-secret"));
}

#[tokio::test]
async fn test_enqueue_private_message_stores_pending_record() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let record = enqueue_private_message(
        &storage,
        counterparty.clone(),
        raw_private_list(),
        timestamp(),
    )
    .await
    .unwrap();

    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(record.outbound_message_id, 0);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_retired_app_until_reactivated() {
    let storage = registered_storage();
    storage
        .transaction(|tx| {
            tx.retire_paykit_app(app_id());
            Ok(())
        })
        .await
        .unwrap();

    let result =
        enqueue_private_message(&storage, counterparty(), raw_private_list(), timestamp()).await;
    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));

    storage
        .transaction(|tx| {
            tx.activate_paykit_app(&app_id());
            Ok(())
        })
        .await
        .unwrap();
    enqueue_private_message(&storage, counterparty(), raw_private_list(), timestamp())
        .await
        .unwrap();
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_unregistered_app() {
    let storage = InMemoryStorage::new();
    storage
        .transaction(|tx| {
            tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("other-app")?);
            Ok(())
        })
        .await
        .unwrap();

    let result =
        enqueue_private_message(&storage, counterparty(), raw_private_list(), timestamp()).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_claim_next_outbound_private_message_skips_unregistered_app() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    raw_private_list(),
                    timestamp(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        timestamp(),
        timestamp(),
        timestamp(),
    )
    .await
    .unwrap();

    assert!(claimed.is_none());
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Pending
    );
}

#[tokio::test]
async fn test_claim_restored_event_head_after_apps_are_republished_out_of_order() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    paykit_lib::PaykitAppId::new("bitkit")?,
                    PrivateMessageKind::PaymentRequest.as_str().into(),
                    raw_payment_request("bitkit"),
                    timestamp(),
                ));
                let second = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    paykit_lib::PaykitAppId::new("paykit-server")?,
                    PrivateMessageKind::PaymentRequest.as_str().into(),
                    raw_payment_request("paykit-server"),
                    timestamp(),
                ));
                tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("paykit-server").unwrap());
                Ok((first, second))
            }
        })
        .await
        .unwrap();

    assert!(claim_next_outbound_private_message(
        &storage,
        &counterparty,
        timestamp(),
        timestamp(),
        timestamp(),
    )
    .await
    .unwrap()
    .is_none());

    storage
        .transaction(|tx| {
            tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("bitkit").unwrap());
            Ok(())
        })
        .await
        .unwrap();
    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
        timestamp(),
        timestamp(),
        timestamp(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_ne!(claimed.outbound_message_id, second.outbound_message_id);
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_unknown_kind() {
    let result = enqueue_private_message(
        &registered_storage(),
        counterparty(),
        r#"{"version":1,"kind":"paykit.unknown","app_id":"bitkit"}"#.into(),
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_enqueue_private_message_rejects_malformed_known_body() {
    let result = enqueue_private_message(
        &registered_storage(),
        counterparty(),
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"../bad":"ln"}}"#
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
        app_id: app_id(),
        kind: "paykit.private_payment_list".into(),
        raw_json:
            r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"../bad":"ln"}}"#
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
    let storage = registered_storage();
    let counterparty = counterparty();
    enqueue_private_message(
        &storage,
        counterparty.clone(),
        raw_private_list(),
        timestamp(),
    )
    .await
    .unwrap();

    let claimed = claim_next_outbound_private_message(
        &storage,
        &counterparty,
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
    let storage = registered_storage();
    let counterparty = counterparty();
    enqueue_private_message(
        &storage,
        counterparty.clone(),
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
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
    assert_eq!(queued[0].attempt_count, 0);
}
