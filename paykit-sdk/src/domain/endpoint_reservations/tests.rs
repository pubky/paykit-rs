use chrono::{Duration as ChronoDuration, TimeZone};

use super::*;
use crate::storage::InMemoryStorage;
use paykit_lib::PaymentEndpointIdentifier;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn reservation(id: &str, payload: &str) -> PrivatePaymentEndpointReservation {
    PrivatePaymentEndpointReservation {
        reservation_id: id.into(),
        receiving_detail: PrivateReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: payload.into(),
        },
        expires_at: None,
        attribution: HashMap::from([("contact".into(), "alice".into())]),
    }
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_stores_linked_records() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "ln-secret")],
        timestamp(),
    )
    .await
    .unwrap();

    let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
    let records = payment_endpoint_reservations(&storage, &counterparty, &receiver_path())
        .await
        .unwrap();

    assert_eq!(
        list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
            .unwrap()
            .as_str(),
        "ln-secret"
    );
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].outbound_message_id, outbound.outbound_message_id);
    assert_ne!(records[0].payload_hash, "ln-secret");
    assert!(!format!("{:?}", records[0]).contains("ln-secret"));
    assert!(!format!("{:?}", records[0]).contains("alice"));
}

#[tokio::test]
async fn test_sync_private_payment_list_preserves_delivery_and_reservation_state() {
    for status in [
        OutboundPrivateMessageStatus::Pending,
        OutboundPrivateMessageStatus::Sending,
        OutboundPrivateMessageStatus::Failed,
        OutboundPrivateMessageStatus::Sent,
    ] {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let reservations = vec![reservation("res-1", "ln-secret")];
        let mut original = queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            &receiver_path(),
            reservations.clone(),
            timestamp(),
        )
        .await
        .unwrap();
        original.status = status.clone();
        if status != OutboundPrivateMessageStatus::Pending {
            original.attempt_count = 1;
            original.last_attempt_at = Some(timestamp());
        }
        if status == OutboundPrivateMessageStatus::Sent {
            original.sent_at = Some(timestamp());
        }
        if status == OutboundPrivateMessageStatus::Failed {
            original.last_error = Some("send uncertain".into());
        }
        storage
            .transaction(|tx| {
                tx.save_outbound_private_message(original.clone())?;
                Ok(())
            })
            .await
            .unwrap();
        let before = payment_endpoint_reservations(&storage, &counterparty, &receiver_path())
            .await
            .unwrap();
        let reused = queue_private_payment_list_with_reservations_inner(
            &storage,
            &counterparty,
            &receiver_path(),
            reservations.clone(),
            timestamp() + ChronoDuration::minutes(1),
            None,
            PrivatePaymentListQueuePolicy::Sync {
                sent_message_id: Some(original.outbound_message_id),
            },
        )
        .await
        .unwrap();
        assert_eq!(reused, original);
        assert_eq!(
            payment_endpoint_reservations(&storage, &counterparty, &receiver_path())
                .await
                .unwrap(),
            before
        );

        if status == OutboundPrivateMessageStatus::Sent {
            let republished = queue_private_payment_list_with_reservations_inner(
                &storage,
                &counterparty,
                &receiver_path(),
                reservations,
                timestamp() + ChronoDuration::minutes(2),
                None,
                PrivatePaymentListQueuePolicy::Sync {
                    sent_message_id: None,
                },
            )
            .await
            .unwrap();
            assert_ne!(
                republished.outbound_message_id,
                original.outbound_message_id
            );
        }
    }
}

#[tokio::test]
async fn test_sync_private_payment_list_keeps_new_reservations_and_explicit_enqueues() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let mut onchain = reservation("res-2", "bc1-private");
    onchain.receiving_detail.identifier = "btc-mainnet-p2wpkh".into();
    let reservations = vec![reservation("res-1", "ln-secret"), onchain];
    let original = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        reservations.clone(),
        timestamp(),
    )
    .await
    .unwrap();
    let mut reordered = reservations.clone();
    reordered.reverse();
    let reused = queue_private_payment_list_with_reservations_inner(
        &storage,
        &counterparty,
        &receiver_path(),
        reordered.clone(),
        timestamp(),
        None,
        PrivatePaymentListQueuePolicy::Sync {
            sent_message_id: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(reused, original);

    reordered[0].reservation_id = "res-3".into();
    let changed = queue_private_payment_list_with_reservations_inner(
        &storage,
        &counterparty,
        &receiver_path(),
        reordered.clone(),
        timestamp(),
        None,
        PrivatePaymentListQueuePolicy::Sync {
            sent_message_id: None,
        },
    )
    .await
    .unwrap();
    assert_ne!(changed.outbound_message_id, original.outbound_message_id);
    let explicit = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        reordered,
        timestamp(),
    )
    .await
    .unwrap();
    assert_ne!(explicit.outbound_message_id, changed.outbound_message_id);
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_stale_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
                        timestamp(),
                        timestamp() + ChronoDuration::seconds(10),
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
                let _ = tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_path(),
                    timestamp() + ChronoDuration::seconds(11),
                    timestamp() + ChronoDuration::seconds(71),
                );
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = queue_private_payment_list_with_reservations_with_link_lease(
        &storage,
        &counterparty,
        vec![reservation("res-1", "ln-secret")],
        timestamp(),
        &stale_lease,
        PrivatePaymentListQueuePolicy::Sync {
            sent_message_id: None,
        },
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.outbound_private_messages.is_empty());
    assert!(snapshot.payment_endpoint_reservations.is_empty());
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_duplicate_identifiers() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let result = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one"), reservation("res-2", "two")],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_invalid_ids() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let long_id = "x".repeat(MAX_RESERVATION_ID_LEN + 1);

    for reservation_id in [" ", "res\n1", long_id.as_str()] {
        let result = queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            &receiver_path(),
            vec![reservation(reservation_id, "one")],
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    }
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_preserves_existing_metadata() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();

    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "res-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            expires_at: Some(timestamp()),
            attribution: HashMap::from([("contact".into(), "bob".into())]),
        }],
        timestamp(),
    )
    .await
    .unwrap();
    let snapshot = storage.snapshot().unwrap();

    assert_eq!(snapshot.payment_endpoint_reservations.len(), 1);
    assert_eq!(
        snapshot
            .payment_endpoint_reservations
            .get(&(counterparty.clone(), receiver_path(), "res-1".into()))
            .unwrap()
            .outbound_message_id,
        outbound.outbound_message_id
    );
    let record = snapshot
        .payment_endpoint_reservations
        .get(&(counterparty.clone(), receiver_path(), "res-1".into()))
        .unwrap();
    assert_eq!(record.attribution.get("contact").unwrap(), "alice");
    assert_eq!(record.expires_at, None);
    assert_eq!(record.created_at, timestamp());
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_cancellation_claimed_id() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut record = tx
                    .payment_endpoint_reservation(&counterparty, &receiver_path(), "res-1")
                    .unwrap();
                record.cancellation_started_at = Some(timestamp());
                tx.save_payment_endpoint_reservation(record);
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_unattempted_superseded_reservation_cancellations() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-2", "two")],
        timestamp(),
    )
    .await
    .unwrap();
    crate::domain::outbound_private::claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(1),
        timestamp() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    let cancellations =
        unattempted_superseded_reservation_cancellations(&storage, &counterparty, &receiver_path())
            .await
            .unwrap();

    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0].cancellation.reservation_id, "res-1");
}

#[tokio::test]
async fn test_unattempted_superseded_reservation_cancellations_skip_attempted_lists() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let first = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-2", "two")],
        timestamp(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut attempted = tx
                    .outbound_private_messages(&counterparty, &receiver_path())
                    .into_iter()
                    .find(|message| message.outbound_message_id == first.outbound_message_id)
                    .unwrap();
                attempted.status = crate::OutboundPrivateMessageStatus::Failed;
                attempted.last_attempt_at = Some(timestamp() - ChronoDuration::seconds(2));
                tx.save_outbound_private_message(attempted)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    crate::domain::outbound_private::claim_next_outbound_private_message(
        &storage,
        &counterparty,
        &receiver_path(),
        timestamp(),
        timestamp() - chrono::Duration::seconds(1),
        timestamp() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    let cancellations =
        unattempted_superseded_reservation_cancellations(&storage, &counterparty, &receiver_path())
            .await
            .unwrap();

    assert!(cancellations.is_empty());
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.payment_endpoint_reservations.len(), 2);
}

#[tokio::test]
async fn test_expired_outbound_reservation_cancellations() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "res-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            expires_at: Some(timestamp() + chrono::Duration::seconds(5)),
            attribution: HashMap::new(),
        }],
        timestamp(),
    )
    .await
    .unwrap();

    assert!(expired_outbound_reservation_cancellations(
        &storage,
        &counterparty,
        &receiver_path(),
        outbound.outbound_message_id,
        timestamp()
    )
    .await
    .unwrap()
    .is_empty());
    let cancellations = expired_outbound_reservation_cancellations(
        &storage,
        &counterparty,
        &receiver_path(),
        outbound.outbound_message_id,
        timestamp() + chrono::Duration::seconds(6),
    )
    .await
    .unwrap();

    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0].cancellation.reservation_id, "res-1");
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_conflicting_existing_id() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();

    let result = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![reservation("res-1", "two")],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_scopes_ids_by_counterparty() {
    let storage = InMemoryStorage::new();
    let first = counterparty();
    let second = counterparty();

    queue_private_payment_list_with_reservations(
        &storage,
        &first,
        &receiver_path(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &second,
        &receiver_path(),
        vec![reservation("res-1", "two")],
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        2
    );
}
