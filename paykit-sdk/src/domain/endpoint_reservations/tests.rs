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

fn app_id() -> PaykitAppId {
    PaykitAppId::new("bitkit").unwrap()
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps([app_id()])
}

async fn storage_without_private_payment_capability() -> InMemoryStorage {
    let storage = InMemoryStorage::new();
    storage
        .transaction(|tx| {
            let app_id = app_id();
            tx.save_paykit_app_capabilities(
                &app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: false,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            );
            tx.activate_paykit_app(&app_id);
            Ok(())
        })
        .await
        .unwrap();
    storage
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
    let storage = registered_storage();
    let counterparty = counterparty();
    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "ln-secret")],
        timestamp(),
    )
    .await
    .unwrap();

    let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
    let records = payment_endpoint_reservations(&storage, &counterparty)
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
async fn test_queue_private_payment_list_requires_app_capability() {
    let storage = storage_without_private_payment_capability().await;
    let counterparty = counterparty();

    let error = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "ln-secret")],
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Policy { .. }));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_sync_private_payment_list_preserves_delivery_and_reservation_state() {
    for status in [
        OutboundPrivateMessageStatus::Pending,
        OutboundPrivateMessageStatus::Sending,
        OutboundPrivateMessageStatus::Failed,
        OutboundPrivateMessageStatus::Sent,
    ] {
        let storage = registered_storage();
        let counterparty = counterparty();
        let reservations = vec![reservation("res-1", "ln-secret")];
        let mut original = queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            app_id(),
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
        let before = payment_endpoint_reservations(&storage, &counterparty)
            .await
            .unwrap();
        let reused = queue_private_payment_list_with_reservations_inner(
            &storage,
            &counterparty,
            app_id(),
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
            payment_endpoint_reservations(&storage, &counterparty)
                .await
                .unwrap(),
            before
        );

        if status == OutboundPrivateMessageStatus::Sent {
            let republished = queue_private_payment_list_with_reservations_inner(
                &storage,
                &counterparty,
                app_id(),
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
async fn test_sync_private_payment_list_reuses_only_the_same_app() {
    let other_app = PaykitAppId::new("paykit-server").unwrap();
    let storage = InMemoryStorage::with_registered_apps([app_id(), other_app.clone()]);
    let counterparty = counterparty();
    let first = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        Vec::new(),
        timestamp(),
    )
    .await
    .unwrap();
    let other = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        other_app.clone(),
        Vec::new(),
        timestamp(),
    )
    .await
    .unwrap();

    for (app, expected) in [(app_id(), first), (other_app, other)] {
        let reused = queue_private_payment_list_with_reservations_inner(
            &storage,
            &counterparty,
            app,
            Vec::new(),
            timestamp(),
            None,
            PrivatePaymentListQueuePolicy::Sync {
                sent_message_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(reused, expected);
    }
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        2
    );
}

#[tokio::test]
async fn test_sync_private_payment_list_replaces_unparseable_stored_message() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let reservations = vec![reservation("res-1", "ln-secret")];
    let original = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        reservations.clone(),
        timestamp(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let original = original.clone();
            move |tx| {
                let mut corrupt = original;
                corrupt.raw_json = "{}".into();
                tx.save_outbound_private_message(corrupt)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let replacement = queue_private_payment_list_with_reservations_inner(
        &storage,
        &counterparty,
        app_id(),
        reservations,
        timestamp() + ChronoDuration::minutes(1),
        None,
        PrivatePaymentListQueuePolicy::Sync {
            sent_message_id: None,
        },
    )
    .await
    .unwrap();

    assert_ne!(
        replacement.outbound_message_id,
        original.outbound_message_id
    );
    let stored = payment_endpoint_reservations(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].outbound_message_id,
        replacement.outbound_message_id
    );
}

#[tokio::test]
async fn test_sync_private_payment_list_keeps_new_reservations_and_explicit_enqueues() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let mut onchain = reservation("res-2", "bc1-private");
    onchain.receiving_detail.identifier = "btc-mainnet-p2wpkh".into();
    let reservations = vec![reservation("res-1", "ln-secret"), onchain];
    let original = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
        app_id(),
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
        app_id(),
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
        app_id(),
        reordered,
        timestamp(),
    )
    .await
    .unwrap();
    assert_ne!(explicit.outbound_message_id, changed.outbound_message_id);
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_stale_lease() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + ChronoDuration::seconds(10),
                    )?
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
                    timestamp() + ChronoDuration::seconds(11),
                    timestamp() + ChronoDuration::seconds(71),
                )?;
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = queue_private_payment_list_with_reservations_with_link_lease(
        &storage,
        &counterparty,
        app_id(),
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
    let storage = registered_storage();
    let counterparty = counterparty();
    let result = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
    let storage = registered_storage();
    let counterparty = counterparty();
    let long_id = "x".repeat(MAX_RESERVATION_ID_LEN + 1);

    for reservation_id in [" ", "res\n1", long_id.as_str()] {
        let result = queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            app_id(),
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
    let storage = registered_storage();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();

    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
            .get(&(counterparty.clone(), app_id(), "res-1".into()))
            .unwrap()
            .outbound_message_id,
        outbound.outbound_message_id
    );
    let record = snapshot
        .payment_endpoint_reservations
        .get(&(counterparty.clone(), app_id(), "res-1".into()))
        .unwrap();
    assert_eq!(record.attribution.get("contact").unwrap(), "alice");
    assert_eq!(record.expires_at, None);
    assert_eq!(record.created_at, timestamp());
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_rejects_cancellation_claimed_id() {
    let storage = registered_storage();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
                    .payment_endpoint_reservation(&counterparty, &app_id(), "res-1")
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
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_unattempted_superseded_reservation_cancellations() {
    let storage = registered_storage();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-2", "two")],
        timestamp(),
    )
    .await
    .unwrap();
    crate::domain::outbound_private::claim_next_outbound_private_message(
        &storage,
        &counterparty,
        timestamp(),
        timestamp() - chrono::Duration::seconds(1),
        timestamp() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    let cancellations = unattempted_superseded_reservation_cancellations(&storage, &counterparty)
        .await
        .unwrap();

    assert_eq!(cancellations.len(), 1);
    assert_eq!(cancellations[0].cancellation.reservation_id, "res-1");
}

#[tokio::test]
async fn test_unattempted_superseded_reservation_cancellations_skip_attempted_lists() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let first = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
                    .outbound_private_messages(&counterparty)
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
        timestamp(),
        timestamp() - chrono::Duration::seconds(1),
        timestamp() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();

    let cancellations = unattempted_superseded_reservation_cancellations(&storage, &counterparty)
        .await
        .unwrap();

    assert!(cancellations.is_empty());
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.payment_endpoint_reservations.len(), 2);
}

#[tokio::test]
async fn test_expired_outbound_reservation_cancellations() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let outbound = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
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
        outbound.outbound_message_id,
        timestamp()
    )
    .await
    .unwrap()
    .is_empty());
    let cancellations = expired_outbound_reservation_cancellations(
        &storage,
        &counterparty,
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
    let storage = registered_storage();
    let counterparty = counterparty();
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();

    let result = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("res-1", "two")],
        timestamp(),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_queue_private_payment_list_with_reservations_scopes_ids_by_counterparty() {
    let storage = registered_storage();
    let first = counterparty();
    let second = counterparty();

    queue_private_payment_list_with_reservations(
        &storage,
        &first,
        app_id(),
        vec![reservation("res-1", "one")],
        timestamp(),
    )
    .await
    .unwrap();
    queue_private_payment_list_with_reservations(
        &storage,
        &second,
        app_id(),
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
