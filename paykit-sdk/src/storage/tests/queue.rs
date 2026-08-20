use super::*;

#[tokio::test]
async fn test_invalid_outbound_private_message_does_not_block_later_records() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
        .await
        .unwrap();
    storage
        .transaction({
            let invalid =
                mark_outbound_invalid(first, "invalid private message JSON".into(), timestamp());
            move |tx| {
                tx.save_outbound_private_message(invalid)?;
                Ok(())
            }
        })
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

    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].outbound_message_id, second.outbound_message_id);
}

#[tokio::test]
async fn test_private_payment_list_queue_sends_only_latest_state() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
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

    assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let snapshot = storage.snapshot().unwrap();
    let first = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == first.outbound_message_id)
        .unwrap();
    assert_eq!(first.status, OutboundPrivateMessageStatus::Superseded);
}

#[tokio::test]
async fn test_private_payment_list_queue_supersedes_only_the_same_app() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let (bitkit_first, server, bitkit_latest) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let bitkit_first = tx.insert_outbound_private_message(
                    outbound_private_message_for_app(counterparty.clone(), "bitkit"),
                );
                let server = tx.insert_outbound_private_message(outbound_private_message_for_app(
                    counterparty.clone(),
                    "paykit-server",
                ));
                let bitkit_latest = tx.insert_outbound_private_message(
                    outbound_private_message_for_app(counterparty, "bitkit"),
                );
                Ok((bitkit_first, server, bitkit_latest))
            }
        })
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

    assert_eq!(claimed.outbound_message_id, server.outbound_message_id);
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot
            .outbound_private_messages
            .iter()
            .find(|message| message.outbound_message_id == bitkit_first.outbound_message_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::Superseded
    );
    assert_eq!(
        snapshot
            .outbound_private_messages
            .iter()
            .find(|message| message.outbound_message_id == bitkit_latest.outbound_message_id)
            .unwrap()
            .status,
        OutboundPrivateMessageStatus::Pending
    );
}

#[tokio::test]
async fn test_private_payment_list_queue_reclaims_stale_sending_before_newer_list() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut first = tx.insert_outbound_private_message(outbound_private_message(
                    counterparty.clone(),
                ));
                first.status = OutboundPrivateMessageStatus::Sending;
                first.last_attempt_at = Some(timestamp() - chrono::Duration::seconds(120));
                tx.save_outbound_private_message(first.clone())?;
                let second =
                    tx.insert_outbound_private_message(outbound_private_message(counterparty));
                Ok((first, second))
            }
        })
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
    .unwrap();

    let claimed = claimed.unwrap();
    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    assert_eq!(claimed.attempt_count, 1);
    let snapshot = storage.snapshot().unwrap();
    let first = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == first.outbound_message_id)
        .unwrap();
    assert_eq!(first.status, OutboundPrivateMessageStatus::Sending);
    let second = snapshot
        .outbound_private_messages
        .iter()
        .find(|message| message.outbound_message_id == second.outbound_message_id)
        .unwrap();
    assert_eq!(second.status, OutboundPrivateMessageStatus::Pending);
}

#[tokio::test]
async fn test_unregistered_attempted_message_blocks_later_registered_app() {
    let storage =
        InMemoryStorage::with_registered_apps([
            paykit_lib::PaykitAppId::new("paykit-server").unwrap()
        ]);
    let counterparty = counterparty();
    let (bitkit_failed, server_pending) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut bitkit_failed = tx.insert_outbound_private_message(
                    outbound_private_message_for_app(counterparty.clone(), "bitkit"),
                );
                bitkit_failed.status = OutboundPrivateMessageStatus::Failed;
                bitkit_failed.last_attempt_at = Some(timestamp() - chrono::Duration::seconds(120));
                tx.save_outbound_private_message(bitkit_failed.clone())?;
                let server_pending = tx.insert_outbound_private_message(
                    outbound_private_message_for_app(counterparty, "paykit-server"),
                );
                Ok((bitkit_failed, server_pending))
            }
        })
        .await
        .unwrap();

    assert!(claim_next_outbound_private_message(
        &storage,
        &counterparty,
        timestamp(),
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
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
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(
        claimed.outbound_message_id,
        bitkit_failed.outbound_message_id
    );
    assert_ne!(
        claimed.outbound_message_id,
        server_pending.outbound_message_id
    );
}

#[tokio::test]
async fn test_event_message_queue_preserves_fifo() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let (first, second) = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let first = tx.insert_outbound_private_message(outbound_payment_request_message(
                    counterparty.clone(),
                ));
                let second = tx.insert_outbound_private_message(outbound_payment_request_message(
                    counterparty,
                ));
                Ok((first, second))
            }
        })
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

    assert_eq!(claimed.outbound_message_id, first.outbound_message_id);
    assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert!(queued
        .iter()
        .any(|message| message.outbound_message_id == second.outbound_message_id));
}

#[tokio::test]
async fn test_restored_event_head_blocks_discovery_until_its_app_is_republished() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(outbound_payment_request_message_for_app(
                    counterparty.clone(),
                    "bitkit",
                ));
                tx.insert_outbound_private_message(outbound_payment_request_message_for_app(
                    counterparty,
                    "paykit-server",
                ));
                tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("paykit-server").unwrap());
                Ok(())
            }
        })
        .await
        .unwrap();

    let snapshot = storage.snapshot().unwrap();
    assert!(!outbound_private_queue_head_is_claimable(
        &snapshot.outbound_private_messages,
        &snapshot.registered_paykit_apps,
        &snapshot.retired_paykit_apps,
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    ));

    storage
        .transaction(|tx| {
            tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("bitkit").unwrap());
            Ok(())
        })
        .await
        .unwrap();
    let snapshot = storage.snapshot().unwrap();
    assert!(outbound_private_queue_head_is_claimable(
        &snapshot.outbound_private_messages,
        &snapshot.registered_paykit_apps,
        &snapshot.retired_paykit_apps,
        timestamp() - chrono::Duration::seconds(60),
        timestamp() - chrono::Duration::seconds(60),
    ));
}

#[tokio::test]
async fn test_inactive_private_payment_list_does_not_block_another_apps_event() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let server_event = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(outbound_private_message_for_app(
                    counterparty.clone(),
                    "bitkit",
                ));
                let server_event = tx.insert_outbound_private_message(
                    outbound_payment_request_message_for_app(counterparty, "paykit-server"),
                );
                tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("paykit-server").unwrap());
                Ok(server_event)
            }
        })
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

    assert_eq!(
        claimed.outbound_message_id,
        server_event.outbound_message_id
    );
}

#[tokio::test]
async fn test_retired_event_head_does_not_block_another_apps_event() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let server_event = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(outbound_payment_request_message_for_app(
                    counterparty.clone(),
                    "bitkit",
                ));
                let server_event = tx.insert_outbound_private_message(
                    outbound_payment_request_message_for_app(counterparty, "paykit-server"),
                );
                tx.retire_paykit_app(paykit_lib::PaykitAppId::new("bitkit").unwrap());
                tx.activate_paykit_app(&paykit_lib::PaykitAppId::new("paykit-server").unwrap());
                Ok(server_event)
            }
        })
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

    assert_eq!(
        claimed.outbound_message_id,
        server_event.outbound_message_id
    );
}
