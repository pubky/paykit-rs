use super::super::*;

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

    assert_eq!(result.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(result.state, PrivatePaymentResolutionState::Available);
    assert_eq!(result.private_payment_list_version, Some(0));
    assert_eq!(result.payable_endpoints[0].endpoint.payload, "ln-private");
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
        .recover_private_candidates_for_resolution(&counterparty, None, None)
        .await
        .unwrap();

    assert!(matches!(outcome, PrivateRecoveryOutcome::Pending));
}
