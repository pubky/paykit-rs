use super::*;

#[tokio::test]
async fn test_payment_requests_with_allows_public_only_identity() {
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

    let records = sdk.payment_requests_with(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Proposed);
}

#[tokio::test]
async fn test_payment_requests_with_marks_recovery_required_peer_state() {
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

    let records = sdk.payment_requests_with(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_list_payment_requests_filters_across_counterparties() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let blocked = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let blocked = blocked.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: blocked,
                    state: LinkedPeerState::Blocked,
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
        first.clone(),
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
    persist_private_stream_batch(
        &storage,
        second.clone(),
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440001",
            "550e8400-e29b-41d4-a716-446655440001",
            Some("2026-06-03T11:59:59Z"),
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        &storage,
        blocked,
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440002",
            "550e8400-e29b-41d4-a716-446655440002",
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

    let all = sdk.payment_requests().await.unwrap();
    let expired = sdk
        .list_payment_requests(PaymentRequestFilter {
            states: vec![PaymentRequestLifecycleState::ProposalExpired],
            ..PaymentRequestFilter::default()
        })
        .await
        .unwrap();
    let received = sdk.actionable_received_payment_requests().await.unwrap();

    assert_eq!(all.len(), 2);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].counterparty, second);
    assert_eq!(received.len(), 2);
    assert!(received
        .iter()
        .all(|record| record.local_role == Some(PaymentRequestLocalRole::Payer)));
}

#[tokio::test]
async fn test_active_recurring_payment_requests_filters_accepted_recurring_requests() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let recurring_peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let one_time_peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PublicOnly,
            local_secret_available: false,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    queue_recurring_request_with_inbound_acceptance(
        &storage,
        recurring_peer.clone(),
        "650e8400-e29b-41d4-a716-446655440010",
        "650e8400-e29b-41d4-a716-446655440011",
        "550e8400-e29b-41d4-a716-446655440010",
    )
    .await;
    queue_one_time_request_with_inbound_acceptance(
        &storage,
        one_time_peer,
        "650e8400-e29b-41d4-a716-446655440012",
        "650e8400-e29b-41d4-a716-446655440013",
        "550e8400-e29b-41d4-a716-446655440011",
    )
    .await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let active = sdk.active_recurring_payment_requests().await.unwrap();

    assert_eq!(active.len(), 1);
    assert_eq!(active[0].counterparty, recurring_peer);
    assert_eq!(
        active[0].state,
        PaymentRequestLifecycleState::ActiveRecurring
    );
    assert!(active[0]
        .terms
        .as_ref()
        .and_then(|terms| terms.recurrence.as_ref())
        .is_some());
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

async fn queue_recurring_request_with_inbound_acceptance(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    request_event_id: &str,
    acceptance_event_id: &str,
    request_id: &str,
) {
    queue_request_with_inbound_acceptance(
        storage,
        counterparty,
        request_event_id,
        acceptance_event_id,
        request_id,
        Some(
            r#"{"every":1,"unit":"month","starts_at":"2026-06-03T12:00:00Z","anchor":"2026-06-03T12:00:00Z","ends_at":null}"#,
        ),
    )
    .await;
}

async fn queue_one_time_request_with_inbound_acceptance(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    request_event_id: &str,
    acceptance_event_id: &str,
    request_id: &str,
) {
    queue_request_with_inbound_acceptance(
        storage,
        counterparty,
        request_event_id,
        acceptance_event_id,
        request_id,
        None,
    )
    .await;
}

async fn queue_request_with_inbound_acceptance(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    request_event_id: &str,
    acceptance_event_id: &str,
    request_id: &str,
    recurrence: Option<&str>,
) {
    let request_event = parsed_payment_request_event(payment_request_raw(
        request_event_id,
        request_id,
        recurrence,
    ));
    crate::payment_requests::enqueue_payment_request_event(
        storage,
        counterparty.clone(),
        &request_event,
        FixedClock.now(),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        storage,
        counterparty,
        vec![payment_request_acceptance_message(
            acceptance_event_id,
            request_id,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
}

fn parsed_payment_request_event(raw_json: String) -> PaymentRequestEvent {
    paykit_lib::parse_payment_request_event_message(&private_application_message(raw_json))
        .unwrap()
        .parsed_event()
        .unwrap()
        .clone()
}

fn private_application_message(raw_json: String) -> PrivateApplicationMessage {
    let value = serde_json::from_str::<serde_json::Value>(&raw_json).unwrap();
    PrivateApplicationMessage {
        version: value
            .get("version")
            .and_then(serde_json::Value::as_u64)
            .and_then(|version| u8::try_from(version).ok()),
        kind: value
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json,
    }
}

fn payment_request_raw(event_id: &str, request_id: &str, recurrence: Option<&str>) -> String {
    let recurrence = recurrence.unwrap_or("null");
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":{recurrence},"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
    )
}

fn payment_request_acceptance_message(
    event_id: &str,
    request_id: &str,
) -> PrivateApplicationMessage {
    private_application_message(format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    ))
}
