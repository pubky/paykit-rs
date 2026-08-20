use super::*;

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
async fn test_reservation_enqueue_does_not_cancel_when_peer_lease_is_busy() {
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
                )
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

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
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
                    )
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

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
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
