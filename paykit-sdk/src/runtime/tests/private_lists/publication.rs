use super::*;
use crate::runtime::outbound_private::PrivateSendReadiness;
use crate::runtime::private_lists::OutboundListRun;

#[test]
fn test_private_payment_list_delivery_futures_stay_stack_bounded() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sync = sdk.sync_private_payment_lists_with_reservations_and_process_outbound(
        vec![PrivatePaymentListReservationUpdate {
            counterparty: counterparty.clone(),
            reservations: Vec::new(),
        }],
        false,
    );
    let clear = sdk.clear_private_payment_list_and_process_outbound(counterparty);

    const MAX_INLINE_FUTURE_SIZE: usize = 16 * 1024;
    assert!(std::mem::size_of_val(&sync) <= MAX_INLINE_FUTURE_SIZE);
    assert!(std::mem::size_of_val(&clear) <= MAX_INLINE_FUTURE_SIZE);
}

#[tokio::test]
async fn test_private_list_delivery_preflight_preserves_unready_work() {
    for state in [
        LinkedPeerState::Linking,
        LinkedPeerState::Blocked,
        LinkedPeerState::RecoveryRequired,
    ] {
        let storage = registered_test_storage();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        seed_private_capable_identity_and_handshake(&storage, counterparty.clone()).await;
        queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            app_id(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-reserved".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            }],
            FixedClock.now(),
        )
        .await
        .unwrap();
        queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            app_id(),
            Vec::new(),
            FixedClock.now(),
        )
        .await
        .unwrap();
        storage
            .transaction(|tx| {
                let mut peer = tx.linked_peer(&counterparty).unwrap();
                peer.state = state.clone();
                tx.save_linked_peer(peer);
                Ok(())
            })
            .await
            .unwrap();
        let before = storage.snapshot().unwrap();
        let canceled = Arc::new(Mutex::new(Vec::new()));
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            InvalidReservedPrivateListPaymentAdapter {
                canceled: canceled.clone(),
            },
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );

        let result = sdk
            .process_outbound_private_messages_with_readiness(
                counterparty,
                PrivateSendReadiness::PrivatePaymentList,
            )
            .await;

        match state {
            LinkedPeerState::Linking => {
                let report = result.unwrap();
                assert!(report.sent.is_empty());
                assert!(report.failed.is_empty());
                assert!(report.reservation_cleanup_failures.is_empty());
            }
            LinkedPeerState::Blocked => {
                assert!(matches!(result, Err(PaykitSdkError::Policy { .. })))
            }
            _ => assert!(matches!(
                result,
                Err(PaykitSdkError::RecoveryRequired { .. })
            )),
        }
        assert_eq!(storage.snapshot().unwrap(), before);
        assert!(canceled.lock().unwrap().is_empty());
    }
}

type TestPrivateListSdk =
    PaykitSdk<InMemoryStorage, TestPubkySessionProvider, PrivateListPaymentAdapter, FixedClock>;

/// Seed a peer with one tracked empty Private Payment List in `status`.
///
/// With `backoff_head`, an older Event Message whose prepared send failed and
/// is still in retry backoff holds the queue head first.
async fn seed_tracked_private_list(
    linked: bool,
    backoff_head: bool,
    status: OutboundPrivateMessageStatus,
) -> (
    TestPrivateListSdk,
    PubkyPublicKey,
    PrivatePaymentListDeliveryReport,
) {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    if linked {
        seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
        storage
            .transaction(|tx| {
                let mut peer = default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                tx.save_linked_peer(peer);
                Ok(())
            })
            .await
            .unwrap();
    } else {
        seed_private_capable_identity_and_handshake(&storage, counterparty.clone()).await;
    }
    if backoff_head {
        storage
            .transaction(|tx| {
                let event = payment_request_message(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                    "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                    None,
                );
                let mut message =
                    tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                        counterparty.clone(),
                        app_id(),
                        event.kind.unwrap(),
                        event.raw_json,
                        FixedClock.now(),
                    ))?;
                message.attempt_count = 1;
                message.last_attempt_at = Some(FixedClock.now());
                message.prepared_send = Some(PreparedOutboundPrivateSend {
                    destination_path: "/pub/paykit/v0/private/send/0".into(),
                    ciphertext: vec![1, 2, 3],
                });
                let message = mark_outbound_failed(message, "timed out".into(), FixedClock.now());
                tx.save_outbound_private_message(message)
            })
            .await
            .unwrap();
    }
    let list = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        Vec::new(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction(|tx| {
            let mut list = list.clone();
            list.status = status;
            tx.save_outbound_private_message(list)
        })
        .await
        .unwrap();
    let report = PrivatePaymentListDeliveryReport {
        cleared: vec![PrivatePaymentListSyncChange {
            counterparty: counterparty.clone(),
            outbound_message_id: Some(list.outbound_message_id),
            error: None,
        }],
        ..PrivatePaymentListDeliveryReport::default()
    };
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    (sdk, counterparty, report)
}

#[tokio::test]
async fn test_private_list_delivery_reports_list_left_queued_behind_retry_backoff() {
    let (sdk, counterparty, mut report) =
        seed_tracked_private_list(true, true, OutboundPrivateMessageStatus::Pending).await;
    let list_id = report.cleared[0].outbound_message_id.unwrap();

    let send_report = sdk
        .process_outbound_private_messages_with_readiness(
            counterparty.clone(),
            PrivateSendReadiness::PrivatePaymentList,
        )
        .await
        .unwrap();
    assert_eq!(send_report, OutboundPrivateSendReport::default());
    sdk.report_unsent_private_payment_lists(
        &mut report,
        &counterparty,
        &OutboundListRun::from_send_report(&send_report),
    )
    .await
    .unwrap();

    assert_eq!(report.failed_to_deliver.len(), 1);
    let failure = &report.failed_to_deliver[0];
    assert_eq!(failure.counterparty, counterparty);
    assert_eq!(failure.outbound_message_id, Some(list_id));
    assert_eq!(
        failure.error,
        format!("Private Payment List {list_id} remains queued")
    );
}

#[tokio::test]
async fn test_private_list_delivery_reports_superseded_list() {
    // Another runtime or device of the same Paykit App queued a newer list.
    let (sdk, counterparty, mut report) =
        seed_tracked_private_list(true, false, OutboundPrivateMessageStatus::Superseded).await;
    let list_id = report.cleared[0].outbound_message_id.unwrap();

    sdk.report_unsent_private_payment_lists(
        &mut report,
        &counterparty,
        &OutboundListRun::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.failed_to_deliver.len(), 1);
    assert_eq!(
        report.failed_to_deliver[0].outbound_message_id,
        Some(list_id)
    );
    assert!(report.failed_to_deliver[0]
        .error
        .contains("superseded before it was sent"));
}

#[tokio::test]
async fn test_private_list_delivery_skips_sent_and_pending_handshake_lists() {
    // A reused list sent by an earlier run, and a list sent by this run.
    for (status, sent_in_run) in [
        (OutboundPrivateMessageStatus::Sent, false),
        (OutboundPrivateMessageStatus::Pending, true),
    ] {
        let (sdk, counterparty, mut report) = seed_tracked_private_list(true, false, status).await;
        let list_id = report.cleared[0].outbound_message_id.unwrap();
        let run = OutboundListRun::from_send_report(&OutboundPrivateSendReport {
            sent: if sent_in_run {
                vec![list_id]
            } else {
                Vec::new()
            },
            ..OutboundPrivateSendReport::default()
        });
        sdk.report_unsent_private_payment_lists(&mut report, &counterparty, &run)
            .await
            .unwrap();
        assert!(report.failed_to_deliver.is_empty());
    }

    // The list stays eligible for a later run once the handshake completes.
    let (sdk, counterparty, mut report) =
        seed_tracked_private_list(false, false, OutboundPrivateMessageStatus::Pending).await;
    sdk.report_unsent_private_payment_lists(
        &mut report,
        &counterparty,
        &OutboundListRun::default(),
    )
    .await
    .unwrap();
    assert!(report.failed_to_deliver.is_empty());
}

#[tokio::test]
async fn test_private_list_delivery_does_not_duplicate_reported_list() {
    let (sdk, counterparty, mut report) =
        seed_tracked_private_list(true, false, OutboundPrivateMessageStatus::Failed).await;
    let list_id = report.cleared[0].outbound_message_id.unwrap();
    report
        .failed_to_deliver
        .push(PrivatePaymentListDeliveryFailure {
            counterparty: counterparty.clone(),
            outbound_message_id: Some(list_id),
            reservation_id: None,
            error: "transport failed".into(),
        });

    sdk.report_unsent_private_payment_lists(
        &mut report,
        &counterparty,
        &OutboundListRun::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.failed_to_deliver.len(), 1);
    assert_eq!(report.failed_to_deliver[0].error, "transport failed");
}

#[tokio::test]
async fn test_enqueue_private_payment_list_requires_live_session_for_stored_link() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk.enqueue_private_payment_list(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.encrypted_link_states.len(), 1);
}

#[tokio::test]
async fn test_enqueue_private_payment_list_requires_private_capable_identity() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk.enqueue_private_payment_list(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_sync_contact_private_payment_lists_reports_contact_and_clear_failures() {
    let storage = registered_test_storage();
    let contact = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let unlisted = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, unlisted.clone()).await;
    storage
        .transaction({
            let unlisted = unlisted.clone();
            move |tx| {
                let mut peer = default_linked_peer(unlisted.clone());
                peer.state = LinkedPeerState::Linked;
                tx.save_linked_peer(peer);
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    sdk.save_contact(ContactUpdate {
        public_key: contact.clone(),
        label: None,
    })
    .await
    .unwrap();

    let report = sdk.sync_contact_private_payment_lists(true).await.unwrap();

    assert!(report.queued.is_empty());
    assert!(report.cleared.is_empty());
    let failed = report
        .failed
        .iter()
        .map(|change| change.counterparty.clone())
        .collect::<HashSet<_>>();
    assert_eq!(failed, HashSet::from([contact, unlisted]));
}

#[tokio::test]
async fn test_sync_private_payment_lists_with_reservations_reports_queue_failures() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let report = sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![PrivatePaymentListReservationUpdate {
                counterparty: counterparty.clone(),
                reservations: vec![PrivatePaymentEndpointReservation {
                    reservation_id: "reservation-1".into(),
                    receiving_detail: PrivateReceivingDetail {
                        identifier: "btc-lightning-bolt11".into(),
                        payload: "ln-reserved".into(),
                    },
                    expires_at: None,
                    attribution: HashMap::from([("payment_hash".into(), "hash-1".into())]),
                }],
            }],
            false,
        )
        .await
        .unwrap();

    assert!(report.queued.is_empty());
    assert!(report.cleared.is_empty());
    assert_eq!(report.failed_to_queue.len(), 1);
    assert_eq!(report.failed_to_queue[0].counterparty, counterparty);
    assert!(report.failed_to_deliver.is_empty());
}

#[tokio::test]
async fn test_enqueue_private_payment_list_with_reservations_cancels_on_preflight_error() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_with_reservations(
            counterparty,
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-reserved".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            }],
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert_eq!(*canceled.lock().unwrap(), vec!["reservation-1".to_string()]);
}

#[tokio::test]
async fn test_reservation_preflight_failure_does_not_cancel_when_peer_lease_is_busy() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + ChronoDuration::seconds(60),
                )?
                .ok_or_else(|| PaykitSdkError::Policy {
                    context: "failed to seed peer lease".into(),
                    source: None,
                })?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_with_reservations(
            counterparty,
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-reserved".into(),
                },
                expires_at: None,
                attribution: HashMap::new(),
            }],
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(canceled.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_sync_private_payment_lists_with_reservations_reports_duplicate_updates() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let report = sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![
                PrivatePaymentListReservationUpdate {
                    counterparty: counterparty.clone(),
                    reservations: vec![PrivatePaymentEndpointReservation {
                        reservation_id: "reservation-1".into(),
                        receiving_detail: PrivateReceivingDetail {
                            identifier: "btc-lightning-bolt11".into(),
                            payload: "one".into(),
                        },
                        expires_at: None,
                        attribution: HashMap::new(),
                    }],
                },
                PrivatePaymentListReservationUpdate {
                    counterparty,
                    reservations: vec![PrivatePaymentEndpointReservation {
                        reservation_id: "reservation-2".into(),
                        receiving_detail: PrivateReceivingDetail {
                            identifier: "btc-onchain-address".into(),
                            payload: "two".into(),
                        },
                        expires_at: None,
                        attribution: HashMap::new(),
                    }],
                },
            ],
            false,
        )
        .await
        .unwrap();

    assert!(report.queued.is_empty());
    assert!(report.cleared.is_empty());
    assert_eq!(report.failed_to_queue.len(), 2);
    assert!(report.failed_to_queue.iter().all(|change| change
        .error
        .as_deref()
        .is_some_and(|error| error.contains("duplicate Private Payment List update"))));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["reservation-1".to_string(), "reservation-2".to_string()]
    );
}

#[tokio::test]
async fn test_enqueue_private_payment_list_uses_fallback_details() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let outbound = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await
        .unwrap();

    let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
    assert_eq!(
        list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
            .unwrap()
            .as_str(),
        "ln-private"
    );
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test]
async fn test_enqueue_private_payment_list_waits_for_peer_operation_lease() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                assert!(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + ChronoDuration::seconds(60),
                    )?
                    .is_some());
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        ReservedPrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(result.unwrap_err().is_concurrent_update());
}

#[tokio::test]
async fn test_enqueue_private_payment_list_uses_reserved_details() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        ReservedPrivateListPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let outbound = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty.clone())
        .await
        .unwrap();

    let list = paykit_lib::parse_private_payment_list_json(&outbound.raw_json).unwrap();
    assert_eq!(
        list.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
            .unwrap()
            .as_str(),
        "ln-reserved"
    );
    let reservations = storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .into_values()
        .collect::<Vec<_>>();
    assert_eq!(reservations.len(), 1);
    assert_eq!(
        reservations[0].outbound_message_id,
        outbound.outbound_message_id
    );
    assert_ne!(reservations[0].payload_hash, "ln-reserved");
    assert!(!format!("{:?}", reservations[0]).contains("ln-reserved"));
}

#[tokio::test]
async fn test_enqueue_private_payment_list_cancels_invalid_reservations() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["reservation-1".to_string(), "reservation-2".to_string()]
    );
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.payment_endpoint_reservations.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
}

#[tokio::test]
async fn test_enqueue_private_payment_list_cancels_unpersisted_reservations_after_lease_change() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        LeaseChangingInvalidReservedPrivateListPaymentAdapter {
            storage: storage.clone(),
            counterparty: counterparty.clone(),
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["reservation-1".to_string(), "reservation-2".to_string()]
    );
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.payment_endpoint_reservations.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
}
