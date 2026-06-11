use super::*;

#[tokio::test]
async fn test_enqueue_private_payment_list_requires_live_session_for_stored_link() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.enqueue_private_payment_list(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.encrypted_link_states.len(), 1);
}

#[tokio::test]
async fn test_enqueue_private_payment_list_requires_private_capable_identity() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.enqueue_private_payment_list(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_enqueue_private_payment_list_uses_fallback_details() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        PrivateListPaymentAdapter,
        PaykitSdkConfig::default(),
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
    let storage = InMemoryStorage::new();
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}

#[tokio::test]
async fn test_enqueue_private_payment_list_uses_reserved_details() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        ReservedPrivateListPaymentAdapter,
        PaykitSdkConfig::default(),
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
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
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
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
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
    let storage = InMemoryStorage::new();
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["reservation-1".to_string(), "reservation-2".to_string()]
    );
    let snapshot = storage.snapshot().unwrap();
    assert!(snapshot.payment_endpoint_reservations.is_empty());
    assert!(snapshot.outbound_private_messages.is_empty());
}

#[tokio::test]
async fn test_unattempted_superseded_reservation_cleanup_cancels_without_claimed_message() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
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
        PaykitSdkConfig::default(),
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
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        vec![PaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: ReceivingDetail {
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
        PaykitSdkConfig::default(),
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
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        cancellation: PaymentEndpointReservationCancellation {
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
            .get(&(counterparty, "reservation-1".into()))
            .unwrap()
            .outbound_message_id,
        2
    );
}

#[tokio::test]
async fn test_reservation_cleanup_rejects_stale_peer_operation_lease_before_adapter_cancellation() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        cancellation: PaymentEndpointReservationCancellation {
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
        .contains_key(&(counterparty, "reservation-1".into())));
}

#[tokio::test]
async fn test_reservation_cleanup_failure_keeps_cancellation_claim() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
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
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        FailingCancellationPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        cancellation: PaymentEndpointReservationCancellation {
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

    assert_eq!(failures.len(), 1);
    let record = storage
        .snapshot()
        .unwrap()
        .payment_endpoint_reservations
        .get(&(counterparty, "reservation-1".into()))
        .unwrap()
        .clone();
    assert_eq!(record.cancellation_started_at, Some(FixedClock.now()));
}

#[tokio::test]
async fn test_reservation_cleanup_removes_claimed_record_after_lease_changes() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_payment_endpoint_reservation(
                    crate::storage::PaymentEndpointReservationRecord {
                        reservation_id: "reservation-1".into(),
                        counterparty: counterparty.clone(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let cancellation = PaymentEndpointReservationCancellationRecord {
        outbound_message_id: 1,
        cancellation: PaymentEndpointReservationCancellation {
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
        .contains_key(&(counterparty, "reservation-1".into())));
}

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_superseded_reservations_without_session()
{
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
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
        PaykitSdkConfig::default(),
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

#[tokio::test]
async fn test_enqueue_private_payment_list_keeps_existing_reservation_on_error() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
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
        .enqueue_private_payment_list_from_receiving_details(counterparty)
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
async fn test_current_private_payment_list_respects_private_sharing_policy() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig {
            private_sharing: PrivateSharingPolicy::Disabled,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    let result = sdk.current_private_payment_list(&counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
}

#[tokio::test]
async fn test_current_private_payment_list_reads_cached_view_for_public_only_identity() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                capability: PubkyIdentityCapability::PublicOnly,
                local_secret_available: false,
                initialized_at: FixedClock.now(),
                sign_out_generation: 0,
            });
            Ok(())
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let view = sdk
        .current_private_payment_list(&counterparty)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-private");
}
