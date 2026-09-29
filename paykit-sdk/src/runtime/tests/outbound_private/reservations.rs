use super::super::*;

#[tokio::test]
async fn test_expired_private_reservation_recovers_only_after_allocating_noise_slot() {
    for prepared in [false, true] {
        let storage = registered_test_storage();
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
        let queued = queue_private_payment_list_with_reservations(
            &storage,
            &counterparty,
            app_id(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "reservation-1".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-private".into(),
                },
                expires_at: Some(FixedClock.now() - ChronoDuration::seconds(1)),
                attribution: HashMap::new(),
            }],
            FixedClock.now(),
        )
        .await
        .unwrap();
        let (sending, lease, later) = storage
            .transaction(|tx| {
                let lease = tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + ChronoDuration::seconds(60),
                    )?
                    .unwrap();
                let mut sending = tx
                    .claim_next_outbound_private_message(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now(),
                        FixedClock.now(),
                    )
                    .unwrap();
                if prepared {
                    sending.prepared_send =
                        Some(PreparedOutboundPrivateSend {
                            destination_path: format!(
                                "{}/{}/0",
                                paykit_lib::PAYKIT_PRIVATE_PATH_PREFIX,
                                "0".repeat(64)
                            ),
                            ciphertext:
                                vec![0; pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN],
                        });
                    tx.save_outbound_private_message(sending.clone())?;
                }
                let later = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty.clone(),
                    app_id(),
                    queued.kind.clone(),
                    queued.raw_json.clone(),
                    FixedClock.now(),
                ))?;
                Ok((sending, lease, later))
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );
        let mut report = OutboundPrivateSendReport::default();

        assert!(sdk
            .claimed_message_ready_for_send(
                &counterparty,
                sending,
                &lease,
                &mut report,
                FixedClock.now()
            )
            .await
            .unwrap()
            .is_none());

        let state = storage.snapshot().unwrap();
        assert_eq!(
            state.outbound_private_messages[0].status,
            OutboundPrivateMessageStatus::Invalid
        );
        assert!(state.outbound_private_messages[0].prepared_send.is_none());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(
            state.encrypted_link_states[&counterparty]
                .link_snapshot
                .is_none(),
            prepared
        );
        assert_eq!(
            state.outbound_private_messages[1].outbound_message_id,
            later.outbound_message_id
        );
        assert_eq!(
            state.outbound_private_messages[1].status,
            if prepared {
                OutboundPrivateMessageStatus::RecoveryRequired
            } else {
                OutboundPrivateMessageStatus::Pending
            }
        );
        if prepared {
            assert_eq!(
                state.linked_peers[&counterparty].state,
                LinkedPeerState::RecoveryRequired
            );
        }
    }
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
        PaykitSdkConfig::new("test-app").unwrap(),
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
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "existing-reservation".into(),
            receiving_detail: PrivateReceivingDetail {
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
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
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
