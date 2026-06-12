use super::*;

#[test]
fn test_payable_from_batch_rejects_foreign_candidates() {
    let candidate = endpoint_candidate("ln-private");
    let foreign = endpoint_candidate("ln-foreign");

    let result = payable_from_batch(&[foreign], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[test]
fn test_payable_from_batch_rejects_duplicate_candidates() {
    let candidate = endpoint_candidate("ln-private");

    let result = payable_from_batch(&[candidate.clone(), candidate.clone()], &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[test]
fn test_payable_from_batch_preserves_adapter_order_for_private_and_public() {
    let private = endpoint_candidate("ln-private");
    let mut public = private.clone();
    public.source = PaymentEndpointSource::PublicPaymentEndpoint;
    public.payload = "ln-public".into();
    let candidates = vec![private.clone(), public.clone()];

    let result = payable_from_batch(&[public.clone(), private.clone()], &candidates).unwrap();

    assert_eq!(result, vec![public, private]);
}

#[tokio::test]
async fn test_resolve_candidate_batch_preserves_private_state() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let endpoint = endpoint_candidate("ln-public");
    let result = sdk
        .resolve_candidate_batch(
            endpoint.counterparty.clone(),
            None,
            vec![endpoint],
            ContactPaymentResolutionPrivateState::RecoveryPending,
        )
        .await
        .unwrap();

    assert_eq!(result.status, ContactPaymentResolutionStatus::Payable);
    assert_eq!(
        result.private_state,
        ContactPaymentResolutionPrivateState::RecoveryPending
    );
    assert_eq!(result.payable_endpoints.len(), 1);
}

#[tokio::test]
async fn test_resolve_candidate_batch_returns_ordered_payable_endpoints() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let private = endpoint_candidate("ln-private");
    let mut public = private.clone();
    public.source = PaymentEndpointSource::PublicPaymentEndpoint;
    public.payload = "ln-public".into();

    let result = sdk
        .resolve_candidate_batch(
            private.counterparty.clone(),
            Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            vec![private.clone(), public.clone()],
            ContactPaymentResolutionPrivateState::Available,
        )
        .await
        .unwrap();

    assert_eq!(result.status, ContactPaymentResolutionStatus::Payable);
    assert_eq!(
        result.private_state,
        ContactPaymentResolutionPrivateState::Available
    );
    assert_eq!(result.payable_endpoints.len(), 2);
    assert_eq!(result.payable_endpoints[0].endpoint, private);
    assert_eq!(result.payable_endpoints[0].target.payload, "ln-private");
    assert_eq!(result.payable_endpoints[1].endpoint, public);
    assert_eq!(result.payable_endpoints[1].target.payload, "ln-public");
}

#[tokio::test]
async fn test_resolve_contact_payment_hides_cached_private_list_without_identity() {
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_contact_payment(ContactPaymentResolutionRequest {
            counterparty: counterparty.clone(),
            amount: Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            include_public_endpoints: false,
        })
        .await;

    let result = result.unwrap();
    assert_eq!(result.status, ContactPaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        result.private_state,
        ContactPaymentResolutionPrivateState::PublicOnlySession
    );
    assert!(result.payable_endpoints.is_empty());
    assert!(sdk
        .current_private_payment_list(&counterparty)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_resolve_contact_payment_uses_cached_private_list_for_public_only_identity() {
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

    let result = sdk
        .resolve_contact_payment(ContactPaymentResolutionRequest {
            counterparty,
            amount: Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            include_public_endpoints: false,
        })
        .await
        .unwrap();

    assert_eq!(result.status, ContactPaymentResolutionStatus::Payable);
    assert_eq!(
        result.private_state,
        ContactPaymentResolutionPrivateState::Available
    );
    assert_eq!(
        result.payable_endpoints[0].endpoint.source,
        PaymentEndpointSource::PrivatePaymentList
    );
}

#[tokio::test]
async fn test_resolve_contact_payment_does_not_use_cached_private_list_while_linking() {
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
                    local_secret_available: true,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .resolve_contact_payment(ContactPaymentResolutionRequest {
            counterparty,
            amount: Some(crate::PaymentAmountContext {
                value: "10.00".into(),
                asset: "usd".into(),
            }),
            include_public_endpoints: false,
        })
        .await
        .unwrap();

    assert_eq!(result.status, ContactPaymentResolutionStatus::NoEndpoint);
    assert_eq!(
        result.private_state,
        ContactPaymentResolutionPrivateState::RecoveryPending
    );
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
                    local_secret_available: true,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let outcome = sdk
        .recover_private_candidates_for_resolution(&counterparty)
        .await
        .unwrap();

    assert!(matches!(outcome, PrivateRecoveryOutcome::Pending));
}
