use super::*;
use crate::storage::compaction::compact_private_payment_lists;

#[tokio::test]
async fn test_private_list_compaction_bounds_growth_and_preserves_views() {
    let peers = [counterparty(), counterparty()];
    let storage = registered_storage();
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(counterparty()),
                initialized_at: timestamp(),
            });
            Ok(())
        })
        .await
        .unwrap();
    for index in 0..200 {
        let peer = &peers[(index / 2) % 2];
        let app = if index % 2 == 0 {
            "bitkit"
        } else {
            "paykit-server"
        };
        let list = paykit_lib::PrivatePaymentList::new(
            paykit_lib::PaykitAppId::new(app).unwrap(),
            if index >= 196 {
                HashMap::new()
            } else {
                HashMap::from([(
                    paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
                    paykit_lib::PaymentEndpointPayload::new(format!("invoice-{index}")),
                )])
            },
        );
        let raw = paykit_lib::serialize_private_payment_list_json(&list).unwrap();
        storage
            .transaction(|tx| {
                let message = NewOutboundPrivateMessage::new(
                    peer.clone(),
                    paykit_lib::PaykitAppId::new(app).unwrap(),
                    "paykit.private_payment_list".into(),
                    raw.clone(),
                    timestamp(),
                );
                let mut record = tx.insert_outbound_private_message(message)?;
                record.attempt_count = 1;
                record.last_attempt_at = Some(timestamp());
                tx.save_outbound_private_message(mark_outbound_sent(record, timestamp()))
            })
            .await
            .unwrap();
        crate::domain::private_stream::persist_private_stream_batch(
            &storage,
            peer.clone(),
            vec![paykit_lib::PrivateApplicationMessage {
                version: Some(1),
                kind: Some("paykit.private_payment_list".into()),
                app_id: Some(app.into()),
                raw_json: raw,
            }],
            None,
            timestamp(),
        )
        .await
        .unwrap();
    }
    let request = outbound_payment_request_message(peers[0].clone());
    for raw in [request.raw_json(), request.raw_json(), "{invalid"] {
        let is_request = raw == request.raw_json();
        crate::domain::private_stream::persist_private_stream_batch(
            &storage,
            peers[0].clone(),
            vec![paykit_lib::PrivateApplicationMessage {
                version: is_request.then_some(1),
                kind: is_request.then(|| "paykit.payment_request".into()),
                app_id: is_request.then(|| "bitkit".into()),
                raw_json: raw.into(),
            }],
            None,
            timestamp(),
        )
        .await
        .unwrap();
    }
    let mut state = storage.snapshot().unwrap();
    crate::validate_storage_state(&state).unwrap();
    let before_size = encode_storage_state_blob(&state).unwrap().len();
    let views = |state: &StorageState| {
        peers
            .iter()
            .map(|peer| {
                crate::domain::private_lists::derive_private_payment_list_views(
                    state
                        .private_stream_items
                        .iter()
                        .filter(|item| &item.counterparty == peer)
                        .cloned()
                        .collect(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
    };
    let before_views = views(&state);
    let events = state.private_stream_items[200..].to_vec();
    let outbound_events = state
        .outbound_private_messages
        .iter()
        .filter(|message| message.kind != "paykit.private_payment_list")
        .cloned()
        .collect::<Vec<_>>();
    let next_outbound_id = state.next_outbound_private_message_id;
    let dedupe = state.event_dedup_records.clone();
    assert_eq!(dedupe.len(), 1);
    compact_private_payment_lists(&mut state);
    assert_eq!(
        state.outbound_private_messages.len(),
        4 + outbound_events.len()
    );
    assert_eq!(
        state
            .outbound_private_messages
            .iter()
            .filter(|message| message.kind != "paykit.private_payment_list")
            .cloned()
            .collect::<Vec<_>>(),
        outbound_events
    );
    assert_eq!(state.private_stream_items.len(), 7);
    assert_eq!(state.next_outbound_private_message_id, next_outbound_id);
    assert_eq!(state.next_private_stream_item_id, 203);
    assert_eq!(views(&state), before_views);
    assert_eq!(&state.private_stream_items[4..], events);
    assert_eq!(state.event_dedup_records, dedupe);
    assert!(before_views
        .iter()
        .flatten()
        .all(|view| view.payment_endpoints.is_empty()));
    crate::validate_storage_state(&state).unwrap();
    let encoded = encode_storage_state_blob(&state).unwrap();
    assert!(encoded.len() * 10 < before_size);
    assert_eq!(decode_storage_state_blob(&encoded).unwrap(), state);
    let compacted = state.clone();
    compact_private_payment_lists(&mut state);
    assert_eq!(state, compacted);

    let backup = crate::export_backup_state(&InMemoryStorage::from_state(state))
        .await
        .unwrap();
    let restored = InMemoryStorage::new();
    crate::backup::restore_backup_state(&restored, backup)
        .await
        .unwrap();
    let restored = restored.snapshot().unwrap();
    assert_eq!(views(&restored), before_views);
    assert_eq!(restored.event_dedup_records, dedupe);
    assert_eq!(restored.next_outbound_private_message_id, next_outbound_id);
    assert_eq!(restored.next_private_stream_item_id, 203);
}

#[test]
fn test_private_list_compaction_keeps_reservations_uncertain_sends_and_events() {
    let peer = counterparty();
    let mut state = StorageState::default();
    for id in 0..7 {
        let mut record =
            OutboundPrivateMessageRecord::from_new(id, outbound_private_message(peer.clone()));
        record.status = OutboundPrivateMessageStatus::Sent;
        state.outbound_private_messages.push(record);
    }
    // Keep an old reservation, a prepared slot, the last successful publication,
    // an uncertain clear, and newer unsent intent independently.
    let mut reservation = payment_endpoint_reservation_record(peer.clone());
    reservation.outbound_message_id = 0;
    state.payment_endpoint_reservations.insert(
        (peer.clone(), app_id(), reservation.reservation_id.clone()),
        reservation,
    );
    state.outbound_private_messages[1].prepared_send = Some(PreparedOutboundPrivateSend {
        destination_path: "reserved-slot".into(),
        ciphertext: vec![1],
    });
    state.outbound_private_messages[1].status = OutboundPrivateMessageStatus::Sending;
    state.outbound_private_messages[4].status = OutboundPrivateMessageStatus::Superseded;
    state.outbound_private_messages[5].status = OutboundPrivateMessageStatus::Failed;
    state.outbound_private_messages[5].last_attempt_at = Some(timestamp());
    state.outbound_private_messages[6].status = OutboundPrivateMessageStatus::Pending;
    let event = OutboundPrivateMessageRecord::from_new(7, outbound_payment_request_message(peer));
    state.outbound_private_messages.push(event.clone());
    compact_private_payment_lists(&mut state);
    assert_eq!(
        state
            .outbound_private_messages
            .iter()
            .map(|record| record.outbound_message_id)
            .collect::<Vec<_>>(),
        [0, 1, 3, 5, 6, 7]
    );
    assert_eq!(state.outbound_private_messages.last(), Some(&event));
}
