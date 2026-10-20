use super::super::*;

async fn pending_counterparties<P: PaymentAdapter>(
    sdk: &PaykitSdk<InMemoryStorage, TestPubkySessionProvider, P, FixedClock>,
) -> Vec<PubkyPublicKey> {
    let before = sdk.storage.snapshot().unwrap();
    let expected = sdk.pending_outbound_private_counterparties().await.unwrap();
    let snapshot = sdk.private_message_scheduling_snapshot().await.unwrap();
    assert_eq!(snapshot.pending_outbound_counterparties, expected);
    assert_eq!(snapshot.linked_peers, sdk.linked_peers().await.unwrap());
    assert_eq!(sdk.storage.snapshot().unwrap(), before);
    expected
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_dedupes_work() {
    let storage = registered_test_storage();
    let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let first = first.clone();
            let second = second.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    first.clone(),
                    app_id(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                let mut sent =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        second,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                sent.status = OutboundPrivateMessageStatus::Sent;
                tx.save_outbound_private_message(sent)?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    first,
                    app_id(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let counterparties = pending_counterparties(&sdk).await;

    assert_eq!(counterparties, vec![(first)]);
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_unregistered_app() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_does_not_skip_attempted_inactive_head() {
    let storage = InMemoryStorage::with_registered_apps([app_id()]);
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let inactive_app = paykit_lib::PaykitAppId::new("inactive-app").unwrap();
                let mut failed = tx.insert_outbound_private_message(
                    NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        inactive_app,
                        PrivateMessageKind::PrivatePaymentList.as_str().into(),
                        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"inactive-app","payment_endpoints":{}}"#.into(),
                        FixedClock.now(),
                    ),
                )?;
                failed.status = OutboundPrivateMessageStatus::Failed;
                failed.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(60));
                tx.save_outbound_private_message(failed)?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    PrivateMessageKind::PrivatePaymentList.as_str().into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());

    sdk.storage
        .transaction(|tx| {
            tx.retire_paykit_app(paykit_lib::PaykitAppId::new("inactive-app").unwrap());
            Ok(())
        })
        .await
        .unwrap();

    assert_eq!(pending_counterparties(&sdk).await, vec![counterparty]);
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_includes_cleanup_only_work() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut invalid = tx
                    .outbound_private_messages(&counterparty)
                    .into_iter()
                    .find(|message| message.outbound_message_id == queued.outbound_message_id)
                    .unwrap();
                invalid.status = OutboundPrivateMessageStatus::Invalid;
                tx.save_outbound_private_message(invalid)?;
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: None,
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: None,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    assert_eq!(
        pending_counterparties(&sdk).await,
        vec![(counterparty.clone())]
    );
    for state in [LinkedPeerState::Blocked, LinkedPeerState::RecoveryRequired] {
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    let mut peer = tx.linked_peer(&counterparty).unwrap();
                    peer.state = state;
                    tx.save_linked_peer(peer);
                    Ok(())
                }
            })
            .await
            .unwrap();
        assert_eq!(
            pending_counterparties(&sdk).await,
            vec![counterparty.clone()]
        );
    }
    let report = sdk
        .process_outbound_private_messages(counterparty)
        .await
        .unwrap();
    assert!(report.reservation_cleanup_failures.is_empty());
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_other_app_cleanup() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        paykit_lib::PaykitAppId::new("other-app").unwrap(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut invalid = tx
                    .outbound_private_messages(&counterparty)
                    .into_iter()
                    .find(|message| message.outbound_message_id == queued.outbound_message_id)
                    .unwrap();
                invalid.status = OutboundPrivateMessageStatus::Invalid;
                tx.save_outbound_private_message(invalid)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_waits_for_stale_sending() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut sending =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                sending.status = OutboundPrivateMessageStatus::Sending;
                sending.last_attempt_at = Some(FixedClock.now());
                tx.save_outbound_private_message(sending)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut sending = tx.outbound_private_messages(&counterparty)[0].clone();
                sending.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(120));
                tx.save_outbound_private_message(sending)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(pending_counterparties(&sdk).await, vec![(counterparty)]);
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_recovery_required_and_blocked_peers() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: None,
                });
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());

    sdk.storage
        .transaction(move |tx| {
            let mut peer = tx.linked_peer(&counterparty).unwrap();
            peer.state = LinkedPeerState::Blocked;
            tx.save_linked_peer(peer);
            Ok(())
        })
        .await
        .unwrap();

    assert!(pending_counterparties(&sdk).await.is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_linking_peer() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: None,
                });
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_waits_for_failed_backoff() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        app_id(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ))?;
                failed.status = OutboundPrivateMessageStatus::Failed;
                failed.last_attempt_at = Some(FixedClock.now());
                tx.save_outbound_private_message(failed)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed = tx.outbound_private_messages(&counterparty)[0].clone();
                failed.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(31));
                tx.save_outbound_private_message(failed)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(pending_counterparties(&sdk).await, vec![(counterparty)]);
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_respects_queue_head() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed_head =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id(),
                        "paykit.payment_request".into(),
                        payment_request_message(
                            "650e8400-e29b-41d4-a716-446655440000",
                            "550e8400-e29b-41d4-a716-446655440000",
                            None,
                        )
                        .raw_json,
                        FixedClock.now(),
                    ))?;
                failed_head.status = OutboundPrivateMessageStatus::Failed;
                failed_head.last_attempt_at = Some(FixedClock.now());
                tx.save_outbound_private_message(failed_head)?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id(),
                    "paykit.payment_request".into(),
                    payment_request_message(
                        "650e8400-e29b-41d4-a716-446655440001",
                        "550e8400-e29b-41d4-a716-446655440001",
                        None,
                    )
                    .raw_json,
                    FixedClock.now(),
                ))?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert!(pending_counterparties(&sdk).await.is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed_head = tx.outbound_private_messages(&counterparty)[0].clone();
                failed_head.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(31));
                tx.save_outbound_private_message(failed_head)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(pending_counterparties(&sdk).await, vec![(counterparty)]);
}
