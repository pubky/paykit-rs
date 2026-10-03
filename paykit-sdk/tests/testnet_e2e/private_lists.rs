use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use paykit_sdk::{
    LinkedPeerState, OutboundPrivateMessageStatus, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PrivatePaymentEndpointReservation, PrivatePaymentListReservationUpdate, PubkySessionAccess,
    PubkySessionProvider, Result as PaykitResult, StorageAdapter,
};
use tokio::sync::{Notify, Semaphore};

use crate::harness::{
    build_testnet, drive_link_to_linked, linked_two_party, private_receiving_detail, two_party,
    TestUser,
};

#[tokio::test]
async fn test_private_inbox_probes_are_bounded_and_isolate_a_slow_failed_peer() {
    struct GatedPublicStorage {
        access: PubkySessionAccess,
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        first: Arc<Semaphore>,
        rest: Arc<Semaphore>,
    }

    #[async_trait]
    impl PubkySessionProvider for GatedPublicStorage {
        async fn load_session_access(&self) -> PaykitResult<Option<PubkySessionAccess>> {
            Ok(Some(self.access.clone()))
        }

        async fn clear_session_access(&self) -> PaykitResult<()> {
            unreachable!("inbox polling must not sign out")
        }

        async fn load_public_storage(&self) -> PaykitResult<Option<pubky::PublicStorage>> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if index == 0 {
                self.first.acquire().await.unwrap().forget();
                return Err(PaykitSdkError::Transport {
                    context: "peer lookup unavailable".into(),
                    source: None,
                });
            }
            self.rest.acquire().await.unwrap().forget();
            Ok(Some(self.access.outbox_client.public_storage()))
        }
    }

    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    for _ in 0..17 {
        let peer = TestUser::sign_up(&testnet).await;
        alice
            .sdk
            .initiate_link_with_peer(peer.public_key.clone())
            .await
            .unwrap();
        peer.sdk
            .accept_link_with_peer(alice.public_key.clone())
            .await
            .unwrap();
        drive_link_to_linked(&alice, &peer).await;
    }
    let before = alice.storage.snapshot().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let first = Arc::new(Semaphore::new(0));
    let rest = Arc::new(Semaphore::new(0));
    let sdk = PaykitSdk::new(
        alice.storage.clone(),
        GatedPublicStorage {
            access: alice.access.clone(),
            calls: calls.clone(),
            started: started.clone(),
            first: first.clone(),
            rest: rest.clone(),
        },
        alice.adapter.clone(),
        PaykitSdkConfig::new(alice.app_id.clone()).unwrap(),
    );
    let mut receive = Box::pin(sdk.receive_private_messages_from_linked_peers());
    for expected in [16, 17] {
        tokio::select! {
            result = &mut receive => panic!("batch finished before releasing the slow peer: {result:?}"),
            ready = tokio::time::timeout(Duration::from_secs(10), async {
                while calls.load(Ordering::SeqCst) < expected {
                    started.notified().await;
                }
            }) => ready.expect("other peer probes must progress while the first is stalled"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        rest.add_permits(16);
    }
    first.add_permits(1);
    let reports = tokio::time::timeout(Duration::from_secs(10), receive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reports.len(), 17);
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.error.is_some())
            .count(),
        1
    );
    assert!(reports[0]
        .error
        .as_ref()
        .unwrap()
        .contains("peer lookup unavailable"));
    assert!(reports[1..].iter().all(|report| report
        .report
        .as_ref()
        .is_some_and(|intake| intake.stream_item_ids.is_empty())));
    assert!(reports
        .windows(2)
        .all(|pair| pair[0].counterparty.as_str() < pair[1].counterparty.as_str()));
    assert_eq!(alice.storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_idle_private_receive_detects_remote_recovery_and_rotation() {
    for rotate_key in [false, true] {
        let pair = linked_two_party().await;
        if rotate_key {
            let replacement = pair
                .alice
                .access
                .local_secret_key
                .as_ref()
                .unwrap()
                .derive_paykit_identity_secret_key(2)
                .unwrap();
            pair.alice
                .sdk
                .rotate_paykit_identity_key(replacement)
                .await
                .unwrap();
        } else {
            pair.alice
                .sdk
                .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
                .await
                .unwrap();
        }
        let error = pair
            .bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, PaykitSdkError::RecoveryRequired { .. }));
        assert_eq!(
            pair.bob.storage.snapshot().unwrap().linked_peers[&pair.alice.public_key].state,
            LinkedPeerState::RecoveryRequired
        );
    }
}

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
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect("enqueue should succeed for a linked peer");
    assert_eq!(queued.status, OutboundPrivateMessageStatus::Pending);

    let send_report = pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("processing the outbound queue should succeed");
    assert_eq!(send_report.sent, vec![queued.outbound_message_id]);
    assert!(send_report.failed.is_empty());
    let sent = pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .into_iter()
        .find(|record| record.outbound_message_id == queued.outbound_message_id)
        .expect("sent message should remain in the audit log");
    assert_eq!(sent.status, OutboundPrivateMessageStatus::Sent);
    assert!(sent.prepared_send.is_none());

    let intake = pair
        .bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
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

    let views = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .expect("reading Private Payment Lists should succeed");
    let view = views
        .iter()
        .find(|view| view.app_id == pair.alice.app_id)
        .expect("the sender app's valid list should be present after receive");
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
            .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
            .await
            .unwrap();
        pair.alice
            .sdk
            .advance_link_handshake(pair.bob.public_key.clone())
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
        .clear_private_payment_list_and_process_outbound(pair.alice.public_key.clone())
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
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.app_id == pair.alice.app_id)
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
            .clear_private_payment_list_and_process_outbound(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(explicit.cleared.len(), 1);
        assert!(explicit.failed_to_deliver.is_empty());
        assert_ne!(explicit.cleared[0].outbound_message_id, clear_id);
        clear_id = explicit.cleared[0].outbound_message_id;
    }
}

#[tokio::test]
async fn test_private_list_sync_corrupt_snapshot_marks_recovery_required() {
    let pair = linked_two_party().await;
    pair.alice
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            move |tx| {
                let mut link_state = tx
                    .encrypted_link_state(&counterparty)
                    .expect("linked peer should have Encrypted Link state");
                link_state.link_snapshot = Some(vec![1, 2, 3]);
                tx.save_encrypted_link_state(link_state);
                Ok(())
            }
        })
        .await
        .unwrap();

    let report = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![PrivatePaymentListReservationUpdate {
                counterparty: pair.bob.public_key.clone(),
                reservations: Vec::new(),
            }],
            false,
        )
        .await
        .unwrap();

    assert_eq!(report.cleared.len(), 1);
    assert!(report.failed_to_queue.is_empty());
    assert_eq!(report.failed_to_deliver.len(), 1);
    let peer = pair
        .alice
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            move |tx| Ok(tx.linked_peer(&counterparty))
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
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
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect_err("enqueue without an Encrypted Link must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );
}
