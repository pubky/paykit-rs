use super::*;

#[tokio::test]
async fn test_pending_outbound_private_counterparties_dedupes_work() {
    let storage = InMemoryStorage::new();
    let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let first = first.clone();
            let second = second.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    first.clone(),
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                let mut sent = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    second,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                sent.status = OutboundPrivateMessageStatus::Sent;
                tx.save_outbound_private_message(sent)?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    first,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let counterparties = sdk.pending_outbound_private_counterparties().await.unwrap();

    assert_eq!(counterparties, vec![(first, receiver_path())]);
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_includes_cleanup_only_work() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: ReceivingDetail {
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
                    .outbound_private_messages(&counterparty, &receiver_path())
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    assert_eq!(
        sdk.pending_outbound_private_counterparties().await.unwrap(),
        vec![(counterparty, receiver_path())]
    );
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_waits_for_stale_sending() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut sending =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        receiver_path(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ));
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut sending =
                    tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
                sending.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(120));
                tx.save_outbound_private_message(sending)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        sdk.pending_outbound_private_counterparties().await.unwrap(),
        vec![(counterparty, receiver_path())]
    );
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_recovery_required_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_skips_linking_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: receiver_path(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_waits_for_failed_backoff() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty,
                        receiver_path(),
                        "paykit.private_payment_list".into(),
                        private_list_json(),
                        FixedClock.now(),
                    ));
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
        PaykitSdkConfig {
            outbound_private_retry_backoff: Duration::from_secs(30),
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed =
                    tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
                failed.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(31));
                tx.save_outbound_private_message(failed)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        sdk.pending_outbound_private_counterparties().await.unwrap(),
        vec![(counterparty, receiver_path())]
    );
}

#[tokio::test]
async fn test_pending_outbound_private_counterparties_respects_queue_head() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed_head =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        receiver_path(),
                        "paykit.payment_request".into(),
                        payment_request_message(
                            "650e8400-e29b-41d4-a716-446655440000",
                            "550e8400-e29b-41d4-a716-446655440000",
                            None,
                        )
                        .raw_json,
                        FixedClock.now(),
                    ));
                failed_head.status = OutboundPrivateMessageStatus::Failed;
                failed_head.last_attempt_at = Some(FixedClock.now());
                tx.save_outbound_private_message(failed_head)?;
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.payment_request".into(),
                    payment_request_message(
                        "650e8400-e29b-41d4-a716-446655440001",
                        "550e8400-e29b-41d4-a716-446655440001",
                        None,
                    )
                    .raw_json,
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig {
            outbound_private_retry_backoff: Duration::from_secs(30),
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    assert!(sdk
        .pending_outbound_private_counterparties()
        .await
        .unwrap()
        .is_empty());

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut failed_head =
                    tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
                failed_head.last_attempt_at = Some(FixedClock.now() - ChronoDuration::seconds(31));
                tx.save_outbound_private_message(failed_head)?;
                Ok(())
            }
        })
        .await
        .unwrap();

    assert_eq!(
        sdk.pending_outbound_private_counterparties().await.unwrap(),
        vec![(counterparty, receiver_path())]
    );
}

#[tokio::test]
async fn test_process_pending_private_messages_reports_counterparty_errors() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    receiver_path(),
                    "paykit.private_payment_list".into(),
                    private_list_json(),
                    FixedClock.now(),
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let reports = sdk.process_pending_private_messages().await.unwrap();

    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].counterparty, counterparty);
    assert!(reports[0].report.is_none());
    assert!(reports[0].error.is_some());
}

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_superseded_reservations_without_session()
{
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: ReceivingDetail {
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
    let latest = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PaymentEndpointReservation {
            reservation_id: "reservation-2".into(),
            receiving_detail: ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "two".into(),
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
                let mut sent = tx
                    .outbound_private_messages(&counterparty, &receiver_path())
                    .into_iter()
                    .find(|message| message.outbound_message_id == latest.outbound_message_id)
                    .unwrap();
                sent.status = crate::OutboundPrivateMessageStatus::Sent;
                tx.save_outbound_private_message(sent)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone(), receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(canceled.lock().unwrap().is_empty());
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        2
    );
}

#[tokio::test]
async fn test_enqueue_private_payment_list_keeps_existing_reservation_on_error() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        &receiver_path(),
        vec![PaymentEndpointReservation {
            reservation_id: "existing-reservation".into(),
            receiving_detail: ReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "existing".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        MixedExistingReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty, receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["conflicting-reservation".to_string()]
    );
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        1
    );
}

#[tokio::test]
async fn test_enqueue_payment_request_event_requires_private_capable_identity() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let event = PaymentRequestAcceptance::new(
        paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
        paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
    );

    let result = sdk
        .enqueue_raw_payment_request_acceptance(counterparty, receiver_path(), &event)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_untrusted_queue_without_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        private_list_json(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone(), receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let queued = crate::domain::outbound_private::queued_outbound_private_messages(
        &storage,
        &counterparty,
        &receiver_path(),
    )
    .await
    .unwrap();
    assert_eq!(queued.len(), 1);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_path()))
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_process_outbound_private_messages_blocks_recovery_required_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::linked_peers::save_linked_peer_state(
        &storage,
        counterparty.clone(),
        receiver_path(),
        LinkedPeerState::RecoveryRequired,
        FixedClock.now(),
    )
    .await
    .unwrap();
    crate::domain::outbound_private::enqueue_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        private_list_json(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone(), receiver_path())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
    let queued = crate::domain::outbound_private::queued_outbound_private_messages(
        &storage,
        &counterparty,
        &receiver_path(),
    )
    .await
    .unwrap();
    assert_eq!(queued.len(), 1);
}
