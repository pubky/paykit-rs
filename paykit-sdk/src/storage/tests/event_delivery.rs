use super::*;

fn confirmation(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
    let confirmation = paykit_lib::DeliveryConfirmation::new(
        app_id(),
        paykit_lib::EventId::new("650e8400-e29b-41d4-a716-446655440000").unwrap(),
        format!("sha256:{}", "0".repeat(64)),
    )
    .unwrap();
    NewOutboundPrivateMessage::new(
        counterparty,
        app_id(),
        paykit_lib::PrivateMessageKind::DeliveryConfirmation
            .as_str()
            .into(),
        paykit_lib::serialize_delivery_confirmation(&confirmation).unwrap(),
        timestamp(),
    )
}

fn published(mut record: OutboundPrivateMessageRecord) -> OutboundPrivateMessageRecord {
    record.attempt_count = 1;
    record.last_attempt_at = Some(timestamp());
    mark_outbound_sent(record, timestamp())
}

#[tokio::test]
async fn test_unconfirmed_event_backoff_does_not_block_newer_data() {
    let storage = registered_storage();
    let peer = counterparty();
    let (first, second) = storage
        .transaction(|tx| {
            let first =
                tx.insert_outbound_private_message(outbound_payment_request_message(peer.clone()))?;
            let first = published(first);
            tx.save_outbound_private_message(first.clone())?;
            let second =
                tx.insert_outbound_private_message(outbound_payment_request_message(peer.clone()))?;
            Ok((first, second))
        })
        .await
        .unwrap();
    let now = timestamp() + chrono::Duration::seconds(10);
    let retry_before = timestamp() - chrono::Duration::seconds(1);
    let claimed =
        claim_next_outbound_private_message(&storage, &peer, now, retry_before, retry_before)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    storage
        .transaction(|tx| tx.save_outbound_private_message(mark_outbound_sent(claimed, now)))
        .await
        .unwrap();
    let retry =
        claim_next_outbound_private_message(&storage, &peer, now, retry_before, timestamp())
            .await
            .unwrap()
            .unwrap();
    assert_eq!(retry.outbound_message_id, first.outbound_message_id);
    assert_eq!(retry.raw_json, first.raw_json);
    assert_eq!(retry.app_id, first.app_id);
    assert!(retry.prepared_send.is_none());
    let sent = mark_outbound_sent(retry, now);
    assert_eq!(sent.sent_at, first.sent_at);
    storage
        .transaction(|tx| tx.save_outbound_private_message(sent))
        .await
        .unwrap();
    assert!(
        claim_next_outbound_private_message(&storage, &peer, now, retry_before, timestamp())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_confirmed_event_is_not_retried() {
    let storage = registered_storage();
    let peer = counterparty();
    storage
        .transaction(|tx| {
            let mut record = published(
                tx.insert_outbound_private_message(outbound_payment_request_message(peer.clone()))?,
            );
            record.confirmed_at = Some(timestamp());
            tx.save_outbound_private_message(record)
        })
        .await
        .unwrap();
    let now = timestamp() + chrono::Duration::days(1);
    assert!(
        claim_next_outbound_private_message(&storage, &peer, now, now, now)
            .await
            .unwrap()
            .is_none()
    );
    assert!(queued_outbound_private_messages(&storage, &peer)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_retired_confirmation_bypasses_suspended_data_without_retry_loop() {
    let storage = InMemoryStorage::with_registered_apps([app_id()]);
    let peer = counterparty();
    let ack = storage
        .transaction(|tx| {
            tx.insert_outbound_private_message(outbound_payment_request_message_for_app(
                peer.clone(),
                "paykit-server",
            ))?;
            let ack = tx.insert_outbound_private_message(confirmation(peer.clone()))?;
            tx.retire_paykit_app(app_id());
            Ok(ack)
        })
        .await
        .unwrap();
    let now = timestamp() + chrono::Duration::days(1);
    let snapshot = storage.snapshot().unwrap();
    assert!(outbound_private_queue_head_is_claimable(
        &snapshot.outbound_private_messages,
        &snapshot.registered_paykit_apps,
        &snapshot.retired_paykit_apps,
        now,
        now,
    ));
    let claimed = claim_next_outbound_private_message(&storage, &peer, now, now, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.outbound_message_id, ack.outbound_message_id);
    storage
        .transaction(|tx| tx.save_outbound_private_message(mark_outbound_sent(claimed, now)))
        .await
        .unwrap();
    assert!(
        claim_next_outbound_private_message(&storage, &peer, now, now, now)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn test_confirmation_never_bypasses_confirmed_prepared_send() {
    let storage = registered_storage();
    let peer = counterparty();
    let (event, ack) = storage
        .transaction(|tx| {
            let ack = tx.insert_outbound_private_message(confirmation(peer.clone()))?;
            let mut event = published(
                tx.insert_outbound_private_message(outbound_payment_request_message(peer.clone()))?,
            );
            event.status = OutboundPrivateMessageStatus::Failed;
            event.last_error = Some("ambiguous publication".into());
            event.confirmed_at = Some(timestamp());
            event.prepared_send = Some(PreparedOutboundPrivateSend {
                destination_path: "reserved-slot".into(),
                ciphertext: vec![1, 2, 3],
            });
            tx.save_outbound_private_message(event.clone())?;
            Ok((event, ack))
        })
        .await
        .unwrap();
    let before = timestamp() - chrono::Duration::seconds(1);
    assert!(
        claim_next_outbound_private_message(&storage, &peer, timestamp(), before, before)
            .await
            .unwrap()
            .is_none()
    );
    let now = timestamp() + chrono::Duration::days(1);
    let claimed = claim_next_outbound_private_message(&storage, &peer, now, now, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.outbound_message_id, event.outbound_message_id);
    assert_eq!(claimed.prepared_send, event.prepared_send);
    assert_eq!(claimed.confirmed_at, event.confirmed_at);
    storage
        .transaction(|tx| tx.save_outbound_private_message(mark_outbound_sent(claimed, now)))
        .await
        .unwrap();
    let claimed = claim_next_outbound_private_message(&storage, &peer, now, now, now)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.outbound_message_id, ack.outbound_message_id);
}
