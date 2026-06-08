use super::*;

#[tokio::test]
async fn test_payment_request_records_allow_public_only_identity() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            None,
        )],
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

    let records = sdk.payment_request_records(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Proposed);
}

#[tokio::test]
async fn test_payment_request_records_mark_recovery_required_peer_state() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: None,
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
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440000",
            "550e8400-e29b-41d4-a716-446655440000",
            None,
        )],
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

    let records = sdk.payment_request_records(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::RecoveryRequired
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
        .enqueue_raw_payment_request_acceptance(counterparty, &event)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_enqueue_payment_request_event_respects_private_sharing_policy() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig {
            private_sharing: PrivateSharingPolicy::Disabled,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );
    let event = PaymentRequestAcceptance::new(
        paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
        paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
    );

    let result = sdk
        .enqueue_raw_payment_request_acceptance(counterparty, &event)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_process_outbound_private_messages_respects_private_sharing_policy() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let event = PaymentRequestAcceptance::new(
        paykit_lib::EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102").unwrap(),
        paykit_lib::PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
    );
    let event = PaymentRequestEvent::Acceptance(event);
    crate::payment_requests::enqueue_payment_request_event(
        &storage,
        counterparty.clone(),
        &event,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig {
            private_sharing: PrivateSharingPolicy::Disabled,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    let result = sdk.process_outbound_private_messages(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .first()
            .unwrap()
            .status,
        crate::OutboundPrivateMessageStatus::Pending
    );
}

#[tokio::test]
async fn test_accept_payment_request_rejects_expired_proposal_before_enqueue() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![payment_request_message(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id.as_str(),
            Some("2026-06-03T11:59:59Z"),
        )],
        None,
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

    let result = sdk.accept_payment_request(counterparty, &request_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_reject_payment_request_allows_expired_proposal_before_readiness_check() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![payment_request_message(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id.as_str(),
            Some("2026-06-03T11:59:59Z"),
        )],
        None,
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
        .reject_payment_request(counterparty, &request_id, None)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_accept_payment_request_does_not_queue_without_private_send_readiness() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![payment_request_message(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id.as_str(),
            None,
        )],
        None,
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

    let result = sdk.accept_payment_request(counterparty, &request_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}
