use super::*;

#[test]
fn test_selected_from_batch_requires_payable_evaluation() {
    let candidate = endpoint_candidate("ln-private");
    let selection = PaymentEndpointSelection {
        selected: Some(candidate.clone()),
        evaluations: vec![PaymentEndpointEvaluation {
            candidate: candidate.clone(),
            compatibility: EndpointCompatibility::Unsupported {
                reason: Some("unsupported".into()),
            },
            priority: None,
        }],
    };

    let result = selected_from_batch(&selection, &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[test]
fn test_selected_from_batch_rejects_foreign_evaluations() {
    let candidate = endpoint_candidate("ln-private");
    let foreign = endpoint_candidate("ln-foreign");
    let selection = PaymentEndpointSelection {
        selected: None,
        evaluations: vec![PaymentEndpointEvaluation {
            candidate: foreign,
            compatibility: EndpointCompatibility::Payable,
            priority: None,
        }],
    };

    let result = selected_from_batch(&selection, &[candidate]);

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_resolve_contact_payment_requires_initialized_identity_for_cached_private_list() {
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
        })
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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
        })
        .await
        .unwrap();

    assert_eq!(result.status, ContactPaymentResolutionStatus::Payable);
    assert_eq!(
        result.selected_endpoint.unwrap().source,
        PaymentEndpointSource::PrivatePaymentList
    );
    assert!(!result.used_public_fallback);
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
        })
        .await
        .unwrap();

    assert_eq!(
        result.status,
        ContactPaymentResolutionStatus::PrivateRecoveryPending
    );
    assert!(result.selected_endpoint.is_none());
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
