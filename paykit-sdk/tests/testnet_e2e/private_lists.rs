use paykit_sdk::{
    OutboundPrivateMessageStatus, PaykitSdkError, PrivatePaymentEndpointReservation,
    PrivatePaymentListReservationUpdate,
};

use crate::harness::{linked_two_party, private_receiving_detail, two_party};

#[tokio::test]
async fn test_private_payment_list_roundtrip_between_linked_peers() {
    let pair = linked_two_party().await;
    let before_receive = pair.bob.sdk.export_backup_state().await.unwrap();
    for _ in 0..3 {
        let reports = pair
            .bob
            .sdk
            .receive_private_messages_from_linked_peers()
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_none());
        let intake = reports[0].report.as_ref().unwrap();
        assert_eq!(intake.receive_batch_id, None);
        assert!(intake.stream_item_ids.is_empty());
        assert_eq!(
            pair.bob.sdk.export_backup_state().await.unwrap(),
            before_receive
        );
    }
    pair.alice
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let queued = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
        .await
        .expect("enqueue should succeed for a linked peer");
    assert_eq!(queued.status, OutboundPrivateMessageStatus::Pending);

    let send_report = pair
        .alice
        .sdk
        .process_outbound_private_messages(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
        )
        .await
        .expect("processing the outbound queue should succeed");
    assert_eq!(send_report.sent, vec![queued.outbound_message_id]);
    assert!(send_report.failed.is_empty());

    let intake = pair
        .bob
        .sdk
        .receive_private_messages(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
        .await
        .expect("receiving private messages should succeed");
    assert!(!intake.stream_item_ids.is_empty());
    assert!(intake.event_conflicts.is_empty());
    assert!(intake.receive_batch_id.is_some());
    let after_receive = pair.bob.sdk.export_backup_state().await.unwrap();
    assert_ne!(after_receive, before_receive);
    pair.bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    assert_eq!(
        pair.bob.sdk.export_backup_state().await.unwrap(),
        after_receive
    );

    let view = pair
        .bob
        .sdk
        .current_private_payment_list(&pair.alice.public_key, &pair.alice.receiver_path)
        .await
        .expect("reading the Private Payment List should succeed")
        .expect("a valid list should be present after receive");
    assert_eq!(
        view.payment_endpoints
            .get("btc-lightning-bolt11")
            .map(String::as_str),
        Some("ln-private-alice")
    );
    assert!(view.latest_stream_item_id.is_some());
}

#[tokio::test]
async fn test_private_list_sync_only_sends_changed_details_on_current_link() {
    let pair = linked_two_party().await;
    let mut update = PrivatePaymentListReservationUpdate {
        counterparty: pair.bob.public_key.clone(),
        counterparty_receiver_path: pair.bob.receiver_path.clone(),
        reservations: vec![PrivatePaymentEndpointReservation {
            reservation_id: "invoice-1".into(),
            receiving_detail: private_receiving_detail("btc-lightning-bolt11", "ln-private-1"),
            expires_at: None,
            attribution: Default::default(),
        }],
    };
    let first = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(first.queued.len(), 1);
    assert!(first.failed_to_deliver.is_empty());
    let intake = pair
        .bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    assert_eq!(intake[0].report.as_ref().unwrap().stream_item_ids.len(), 1);
    let alice_backup = pair.alice.sdk.export_backup_state().await.unwrap();
    let bob_backup = pair.bob.sdk.export_backup_state().await.unwrap();
    for _ in 0..3 {
        pair.alice
            .sdk
            .ensure_link_with_peer(
                pair.bob.public_key.clone(),
                pair.bob.receiver_path.clone(),
                1,
            )
            .await
            .unwrap();
        pair.alice
            .sdk
            .advance_link_handshake(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
            .await
            .unwrap();
        let unchanged = pair
            .alice
            .sdk
            .sync_private_payment_lists_with_reservations_and_process_outbound(
                vec![update.clone()],
                false,
            )
            .await
            .unwrap();
        assert_eq!(unchanged.queued, first.queued);
        assert!(unchanged.failed_to_queue.is_empty());
        assert!(unchanged.failed_to_deliver.is_empty());
        let intake = pair
            .bob
            .sdk
            .receive_private_messages_from_linked_peers()
            .await
            .unwrap();
        assert!(intake[0]
            .report
            .as_ref()
            .unwrap()
            .stream_item_ids
            .is_empty());
        assert_eq!(
            pair.alice.sdk.export_backup_state().await.unwrap(),
            alice_backup
        );
        assert_eq!(
            pair.bob.sdk.export_backup_state().await.unwrap(),
            bob_backup
        );
    }

    pair.bob
        .sdk
        .clear_private_payment_list_and_process_outbound(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
        .await
        .unwrap();
    pair.alice
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    let after_receive = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(after_receive.queued, first.queued);

    update.reservations[0].reservation_id = "invoice-2".into();
    update.reservations[0].receiving_detail.payload = "ln-private-2".into();
    let changed = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(changed.queued.len(), 1);
    assert_ne!(
        changed.queued[0].outbound_message_id,
        first.queued[0].outbound_message_id
    );
    assert!(changed.failed_to_deliver.is_empty());
    pair.bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    let view = pair
        .bob
        .sdk
        .current_private_payment_list(&pair.alice.public_key, &pair.alice.receiver_path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view.payment_endpoints
            .get("btc-lightning-bolt11")
            .map(String::as_str),
        Some("ln-private-2")
    );

    update.reservations.clear();
    let mut clear_id = None;
    for _ in 0..2 {
        let cleared = pair
            .alice
            .sdk
            .sync_private_payment_lists_with_reservations_and_process_outbound(
                vec![update.clone()],
                false,
            )
            .await
            .unwrap();
        assert_eq!(cleared.cleared.len(), 1);
        assert!(cleared.failed_to_deliver.is_empty());
        if clear_id.is_some() {
            assert_eq!(cleared.cleared[0].outbound_message_id, clear_id);
        }
        clear_id = cleared.cleared[0].outbound_message_id;
    }
    for _ in 0..2 {
        let explicit = pair
            .alice
            .sdk
            .clear_private_payment_list_and_process_outbound(
                pair.bob.public_key.clone(),
                pair.bob.receiver_path.clone(),
            )
            .await
            .unwrap();
        assert_eq!(explicit.cleared.len(), 1);
        assert!(explicit.failed_to_deliver.is_empty());
        assert_ne!(explicit.cleared[0].outbound_message_id, clear_id);
        clear_id = explicit.cleared[0].outbound_message_id;
    }
}

#[tokio::test]
async fn test_enqueue_private_payment_list_without_link_fails() {
    let pair = two_party().await;
    pair.alice
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let err = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
        .await
        .expect_err("enqueue without an Encrypted Link must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );
}
