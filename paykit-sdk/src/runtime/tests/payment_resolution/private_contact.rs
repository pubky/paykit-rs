use super::super::*;
use crate::runtime::outbound_private::PrivateSendReadiness;

#[tokio::test]
async fn test_private_contact_preparation_rechecks_sent_retry_time_and_registration() {
    #[derive(Clone)]
    struct AdvancingClock(Arc<Mutex<DateTime<Utc>>>);
    impl Clock for AdvancingClock {
        fn now(&self) -> DateTime<Utc> {
            *self.0.lock().unwrap()
        }
    }

    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let started_at = FixedClock.now();
    let sent = storage
        .transaction(|tx| {
            let event = payment_request_message(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                None,
            );
            let mut sent = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty.clone(),
                app_id(),
                event.kind.unwrap(),
                event.raw_json,
                started_at,
            ))?;
            sent.last_attempt_at = Some(started_at);
            sent.attempt_count = 1;
            sent = mark_outbound_sent(sent, started_at);
            tx.save_outbound_private_message(sent.clone())?;
            Ok(vec![sent])
        })
        .await
        .unwrap();
    let now = Arc::new(Mutex::new(started_at));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        AdvancingClock(now.clone()),
    );
    let before = storage.snapshot().unwrap();
    let backoff = ChronoDuration::from_std(OUTBOUND_PRIVATE_RETRY_BACKOFF).unwrap();
    for (elapsed, needs_outbound) in [
        (ChronoDuration::zero(), false),
        (backoff - ChronoDuration::milliseconds(1), false),
        (backoff, true),
        (backoff + ChronoDuration::milliseconds(1), true),
    ] {
        *now.lock().unwrap() = started_at + elapsed;
        let actual = storage
            .transaction(|tx| {
                sdk.private_contact_preparation_needs_outbound(
                    tx,
                    &counterparty,
                    &sent,
                    PrivateSendReadiness::Queued,
                )
            })
            .await
            .unwrap();
        assert_eq!(actual, needs_outbound);
        assert_eq!(storage.snapshot().unwrap(), before);
    }
    for active in [false, true] {
        let actual = storage
            .transaction(|tx| {
                if active {
                    tx.activate_paykit_app(&app_id());
                } else {
                    tx.retire_paykit_app(app_id());
                }
                assert_eq!(tx.outbound_private_messages(&counterparty), sent);
                sdk.private_contact_preparation_needs_outbound(
                    tx,
                    &counterparty,
                    &sent,
                    PrivateSendReadiness::Queued,
                )
            })
            .await
            .unwrap();
        assert_eq!(actual, active);
    }
}

#[tokio::test]
async fn test_private_contact_preparation_keeps_unsent_and_cleanup_fallbacks() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "unused".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "private-invoice".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    for case in ["pending", "failed", "sending", "prepared", "cleanup"] {
        storage
            .transaction(|tx| {
                let mut message = queued.clone();
                message.last_attempt_at = Some(FixedClock.now());
                message.status = match case {
                    "pending" => OutboundPrivateMessageStatus::Pending,
                    "failed" => OutboundPrivateMessageStatus::Failed,
                    "sending" => OutboundPrivateMessageStatus::Sending,
                    "prepared" => OutboundPrivateMessageStatus::Sent,
                    _ => OutboundPrivateMessageStatus::Invalid,
                };
                if case == "prepared" {
                    message.prepared_send =
                        Some(PreparedOutboundPrivateSend {
                            destination_path: "/pub/paykit/v0/private/prepared/0".into(),
                            ciphertext:
                                vec![1; pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN],
                        });
                }
                tx.save_outbound_private_message(message)?;
                assert!(
                    sdk.private_contact_preparation_needs_outbound(
                        tx,
                        &counterparty,
                        &tx.outbound_private_messages(&counterparty),
                        PrivateSendReadiness::Queued,
                    )?,
                    "{case}"
                );
                Ok(())
            })
            .await
            .unwrap();
    }
}

async fn cache_bitkit_private_app(storage: &InMemoryStorage, counterparty: &PubkyPublicKey) {
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                save_authorized_paykit_app(
                    tx,
                    counterparty,
                    paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                    private_app_capabilities(),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_resolve_private_contact_payment_hides_cached_list_without_identity() {
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
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty.clone(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await;

    let result = result.unwrap();
    assert_eq!(result.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        result.state,
        PrivatePaymentResolutionState::NoPrivateEndpoint
    );
    assert_eq!(result.private_payment_list_version, None);
    assert!(result.payable_endpoints.is_empty());
    assert!(sdk
        .current_private_payment_lists(&counterparty)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_resolve_private_contact_payment_uses_authorized_cache_without_live_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                initialized_at: FixedClock.now(),
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
    cache_bitkit_private_app(&storage, &counterparty).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        FailingPublicStorageProvider {
            successful_loads: 0.into(),
        },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty.clone(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.state, PrivatePaymentResolutionState::Available);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-private");
    let prepared = sdk
        .prepare_and_resolve_private_contact_payment(counterparty.clone(), None, None, 1)
        .await
        .unwrap();
    assert!(prepared.link_report.is_none());
    assert!(prepared.receive_report.is_none());
    assert!(prepared.outbound_report.is_none());
    assert_eq!(prepared.resolution.status, result.status);
    assert_eq!(prepared.resolution.state, result.state);
    assert_eq!(
        prepared.resolution.payable_endpoints[0].endpoint.payload,
        "ln-private"
    );
    assert_eq!(
        sdk.current_private_payment_lists(&counterparty)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_resolve_private_contact_payment_rejects_uncached_app_without_live_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, None, None)
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        result.state,
        PrivatePaymentResolutionState::NoPrivateEndpoint
    );
    assert!(result.payable_endpoints.is_empty());
}

#[tokio::test]
async fn test_resolve_private_contact_payment_waits_after_current_list_version() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
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
    cache_bitkit_private_app(&storage, &counterparty).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, None, Some(0))
        .await
        .unwrap();

    assert_eq!(
        result.status,
        PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList
    );
    assert_eq!(result.state, PrivatePaymentResolutionState::Available);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert!(result.payable_endpoints.is_empty());
}

#[tokio::test]
async fn test_resolve_private_contact_payment_accepts_newer_repeated_endpoint() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            private_list_message("ln-reusable"),
            private_list_message("ln-reusable"),
        ],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    cache_bitkit_private_app(&storage, &counterparty).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, None, Some(0))
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.private_payment_list_version, Some(1));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-reusable");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_uses_private_candidates_only() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                initialized_at: FixedClock.now(),
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
    cache_bitkit_private_app(&storage, &counterparty).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(counterparty, None, None)
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-private");
}

#[tokio::test]
async fn test_resolve_private_contact_payment_does_not_use_cached_list_while_linking() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(PubkyPublicKey::from_public_key(
                        &pubky::Keypair::random().public_key(),
                    )),
                    initialized_at: FixedClock.now(),
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
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
                Ok(())
            }
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .resolve_private_contact_payment(
            counterparty,
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            None,
        )
        .await
        .unwrap();

    assert_eq!(result.status, PrivatePaymentResolutionStatus::NoEndpoint);
    assert_eq!(result.state, PrivatePaymentResolutionState::RecoveryPending);
    assert_eq!(result.private_payment_list_version, None);
    assert!(result.payable_endpoints.is_empty());
}

#[tokio::test]
async fn test_recover_private_candidates_reports_pending_for_linking_peer() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(PubkyPublicKey::from_public_key(
                        &pubky::Keypair::random().public_key(),
                    )),
                    initialized_at: FixedClock.now(),
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
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

    let outcome = sdk
        .recover_private_candidates_for_resolution(&counterparty, None, None, None)
        .await
        .unwrap();

    assert!(matches!(outcome, PrivateRecoveryOutcome::Pending));
}
