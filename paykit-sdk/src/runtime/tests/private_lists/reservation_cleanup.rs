use super::*;

#[tokio::test]
async fn test_unattempted_superseded_reservation_cleanup_cancels_without_claimed_message() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
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
    let latest = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-2".into(),
            receiving_detail: PrivateReceivingDetail {
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
                    .outbound_private_messages(&counterparty)
                    .into_iter()
                    .find(|message| message.outbound_message_id == latest.outbound_message_id)
                    .unwrap();
                sent.status = crate::OutboundPrivateMessageStatus::Sent;
                tx.save_outbound_private_message(sent)?;
                let claimed = tx.claim_next_outbound_private_message(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() - ChronoDuration::seconds(1),
                    FixedClock.now() - ChronoDuration::seconds(1),
                );
                assert!(claimed.is_none());
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let failures = sdk
        .cancel_unattempted_superseded_reservations(&counterparty, None)
        .await;

    assert!(failures.is_empty());
    assert_eq!(*canceled.lock().unwrap(), vec!["reservation-1".to_string()]);
    let reservations = storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .into_values()
        .collect::<Vec<_>>();
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0].reservation_id, "reservation-2");
}

#[tokio::test]
async fn test_terminal_private_list_reservation_cleanup_cancels_invalid_message_reservations() {
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
            expires_at: Some(FixedClock.now() - ChronoDuration::seconds(1)),
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
                invalid.status = crate::OutboundPrivateMessageStatus::Invalid;
                invalid.last_error =
                    Some("Payment Endpoint Reservation expired before private list send".into());
                tx.save_outbound_private_message(invalid)?;
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let failures = sdk
        .cancel_terminal_private_list_reservations(&counterparty, None)
        .await;

    assert!(failures.is_empty());
    assert_eq!(*canceled.lock().unwrap(), vec!["reservation-1".to_string()]);
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .is_empty());
}

#[tokio::test]
async fn test_reservation_cleanup_skips_reused_reservation_from_newer_outbound_message() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty,
                        app_id: app_id(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: reservation_payload_hash("one"),
                        outbound_message_id: 2,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: None,
                        created_at: FixedClock.now(),
                    },
                );
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        app_id: app_id(),
        cancellation: PrivatePaymentEndpointReservationCancellation {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("one"),
            attribution: HashMap::new(),
        },
    };

    let failures = sdk
        .cancel_reservation_records(vec![cancellation], None)
        .await;

    assert!(failures.is_empty());
    assert!(canceled.lock().unwrap().is_empty());
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .get(&(counterparty, app_id(), "reservation-1".into()))
            .unwrap()
            .outbound_message_id,
        2
    );
}

#[tokio::test]
async fn test_reservation_cleanup_skips_another_apps_reservation() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let server_app_id = paykit_lib::PaykitAppId::new("server").unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let server_app_id = server_app_id.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "server-reservation".into(),
                        counterparty,
                        app_id: server_app_id,
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: reservation_payload_hash("one"),
                        outbound_message_id: 1,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: None,
                        created_at: FixedClock.now(),
                    },
                );
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        app_id: server_app_id.clone(),
        cancellation: PrivatePaymentEndpointReservationCancellation {
            reservation_id: "server-reservation".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("one"),
            attribution: HashMap::new(),
        },
    };

    let failures = sdk
        .cancel_reservation_records(vec![cancellation], None)
        .await;

    assert!(failures.is_empty());
    assert!(canceled.lock().unwrap().is_empty());
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .contains_key(&(counterparty, server_app_id, "server-reservation".into())));
}

#[tokio::test]
async fn test_reservation_cleanup_rejects_stale_peer_operation_lease_before_adapter_cancellation() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
                        app_id: app_id(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: reservation_payload_hash("one"),
                        outbound_message_id: 1,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: None,
                        created_at: FixedClock.now(),
                    },
                );
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + ChronoDuration::seconds(10),
                    )
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
                    FixedClock.now() + ChronoDuration::seconds(11),
                    FixedClock.now() + ChronoDuration::seconds(71),
                );
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        app_id: app_id(),
        cancellation: PrivatePaymentEndpointReservationCancellation {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("one"),
            attribution: HashMap::new(),
        },
    };

    let failures = sdk
        .cancel_reservation_records(vec![cancellation], Some(&stale_lease))
        .await;

    assert_eq!(failures.len(), 1);
    assert!(canceled.lock().unwrap().is_empty());
    assert!(storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .contains_key(&(counterparty, app_id(), "reservation-1".into())));
}

#[tokio::test]
async fn test_reservation_cleanup_failure_keeps_cancellation_claim() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
                        app_id: app_id(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: reservation_payload_hash("one"),
                        outbound_message_id: 1,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: Some(
                            FixedClock.now() - ChronoDuration::seconds(61),
                        ),
                        created_at: FixedClock.now(),
                    },
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        FailingCancellationPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        app_id: app_id(),
        cancellation: PrivatePaymentEndpointReservationCancellation {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("one"),
            attribution: HashMap::new(),
        },
    };

    let failures = sdk
        .cancel_reservation_records(vec![cancellation.clone()], None)
        .await;

    assert_eq!(failures.len(), 1);
    assert!(sdk
        .cancel_reservation_records(vec![cancellation], None)
        .await
        .is_empty());
    let record = storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .get(&(counterparty, app_id(), "reservation-1".into()))
        .unwrap()
        .clone();
    assert_eq!(record.cancellation_started_at, Some(FixedClock.now()));
}

#[tokio::test]
async fn test_reservation_cleanup_removes_claimed_record_after_lease_changes() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
                        app_id: app_id(),
                        identifier: "btc-lightning-bolt11".into(),
                        payload_hash: reservation_payload_hash("one"),
                        outbound_message_id: 1,
                        attribution: HashMap::new(),
                        expires_at: None,
                        cancellation_started_at: None,
                        created_at: FixedClock.now(),
                    },
                );
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + ChronoDuration::seconds(10),
                    )
                    .unwrap())
            }
        })
        .await
        .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        LeaseChangingCancellationPaymentAdapter {
            storage: storage.clone(),
            counterparty: counterparty.clone(),
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        app_id: app_id(),
        cancellation: PrivatePaymentEndpointReservationCancellation {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: reservation_payload_hash("one"),
            attribution: HashMap::new(),
        },
    };

    let failures = sdk
        .cancel_reservation_records(vec![cancellation], Some(&lease))
        .await;

    assert!(failures.is_empty());
    assert_eq!(*canceled.lock().unwrap(), vec!["reservation-1".to_string()]);
    assert!(!storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .contains_key(&(counterparty, app_id(), "reservation-1".into())));
}

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_superseded_reservations_without_session()
{
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
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
    let latest = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-2".into(),
            receiving_detail: PrivateReceivingDetail {
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
                    .outbound_private_messages(&counterparty)
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone())
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
