use super::*;
use std::time::Duration;

#[tokio::test]
async fn test_unavailable_registry_does_not_hide_other_payment_requests() {
    let storage = registered_test_storage();
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
    for _ in 0..2 {
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
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
        authorize_payment_request_app(&storage, counterparty, "bitkit").await;
    }
    let sdk = PaykitSdk::with_clock(
        storage,
        FailingPublicStorageProvider {
            successful_loads: 1.into(),
        },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert_eq!(sdk.payment_requests().await.unwrap().len(), 2);
    assert_eq!(
        sdk.actionable_received_payment_requests()
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sdk.payment_requests().await.unwrap().len(), 2);
}

#[derive(Clone)]
struct TransactionGateStorage {
    inner: InMemoryStorage,
    armed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    entered: std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>>,
    release: std::sync::Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
}

impl TransactionGateStorage {
    fn new(inner: InMemoryStorage) -> (Self, std::sync::mpsc::Receiver<()>) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        (
            Self {
                inner,
                armed: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                entered: std::sync::Arc::new(std::sync::Mutex::new(Some(entered_tx))),
                release: std::sync::Arc::new((
                    std::sync::Mutex::new(false),
                    std::sync::Condvar::new(),
                )),
            },
            entered_rx,
        )
    }

    fn release(&self) {
        let (released, condition) = &*self.release;
        *released.lock().unwrap() = true;
        condition.notify_one();
    }
}

#[async_trait::async_trait]
impl StorageAdapter for TransactionGateStorage {
    async fn transaction_erased<'a>(
        &self,
        callback: crate::storage::StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let (released, condition) = &*self.release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = condition.wait(released).unwrap();
            }
            drop(released);
        }
        self.inner.transaction_erased(callback).await
    }
}

#[derive(Clone)]
struct MutableClock(std::sync::Arc<std::sync::Mutex<DateTime<Utc>>>);

impl MutableClock {
    fn new(now: DateTime<Utc>) -> Self {
        Self(std::sync::Arc::new(std::sync::Mutex::new(now)))
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().unwrap() = now;
    }
}

impl Clock for MutableClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_operation_leases_start_when_storage_callback_runs() {
    let peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    for counterparty in [Some(peer), None] {
        let (storage, entered) = TransactionGateStorage::new(registered_test_storage());
        let initial = FixedClock.now();
        let clock = MutableClock::new(initial);
        let sdk = Arc::new(PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            clock.clone(),
        ));
        let task = tokio::spawn({
            let sdk = Arc::clone(&sdk);
            let counterparty = counterparty.clone();
            async move {
                if let Some(counterparty) = counterparty {
                    let lease = sdk.claim_peer_link_operation(&counterparty).await.unwrap();
                    (lease.claimed_at, lease.expires_at)
                } else {
                    let lease = sdk.claim_paykit_app_operation().await.unwrap();
                    (lease.claimed_at, lease.expires_at)
                }
            }
        });
        let entered_result = entered.recv_timeout(std::time::Duration::from_secs(2));
        let resumed_at = initial + ChronoDuration::minutes(5);
        clock.set(resumed_at);
        storage.release();
        entered_result.expect("lease claim must wait at the storage fence");

        let (claimed_at, expires_at) = task.await.unwrap();
        assert_eq!(claimed_at, resumed_at);
        assert_eq!(expires_at, resumed_at + ChronoDuration::seconds(60));

        if let Some(counterparty) = counterparty {
            assert!(sdk
                .claim_peer_link_operation(&counterparty)
                .await
                .unwrap_err()
                .is_concurrent_update());
        } else {
            assert!(matches!(
                sdk.claim_paykit_app_operation().await,
                Err(PaykitSdkError::Policy { .. })
            ));
        }
    }
}

#[tokio::test]
async fn test_payment_requests_with_allows_public_only_identity() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let records = sdk.payment_requests_with(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Proposed);
}

#[tokio::test]
async fn test_payment_requests_with_marks_recovery_required_peer_state() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let records = sdk.payment_requests_with(&counterparty).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::RecoveryRequired
    );
    let record = sdk
        .load_payment_request_record(
            &counterparty,
            &PaymentRequestId::new(&records[0].payment_request_id).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(record, records[0]);
    assert_eq!(
        sdk.received_payment_requests_from(&counterparty)
            .await
            .unwrap(),
        records
    );
}

#[tokio::test]
async fn test_list_payment_requests_filters_across_counterparties() {
    let storage = registered_test_storage();
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
                    initialized_at: FixedClock.now(),
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
                    noise_key_authorization: None,
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
    authorize_payment_request_app(&storage, first.clone(), "bitkit").await;
    authorize_payment_request_app(&storage, second.clone(), "bitkit").await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
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
async fn test_actionable_received_payment_requests_do_not_treat_payee_app_as_executor() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![
            payment_request_message_for_required_app(
                "650e8400-e29b-41d4-a716-446655440010",
                "550e8400-e29b-41d4-a716-446655440010",
                None,
            ),
            payment_request_message_for_required_app(
                "650e8400-e29b-41d4-a716-446655440011",
                "550e8400-e29b-41d4-a716-446655440011",
                Some("test-app"),
            ),
            payment_request_message_for_required_app(
                "650e8400-e29b-41d4-a716-446655440012",
                "550e8400-e29b-41d4-a716-446655440012",
                Some("other-app"),
            ),
        ],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    authorize_payment_request_app(&storage, counterparty.clone(), "paykit-server").await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let actionable = sdk.actionable_received_payment_requests().await.unwrap();

    assert_eq!(actionable.len(), 3);
    assert!(actionable.iter().any(|record| {
        record
            .terms
            .as_ref()
            .and_then(|terms| terms.required_app_id.as_ref())
            .is_some_and(|required_app_id| required_app_id.as_str() == "other-app")
    }));
}

#[tokio::test]
async fn test_actionable_received_payment_requests_include_unclaimed_accepted_request() {
    assert_local_response_actionability(
        "550e8400-e29b-41d4-a716-446655440030",
        "650e8400-e29b-41d4-a716-446655440030",
        parsed_payment_request_event(payment_request_acceptance_raw(
            "650e8400-e29b-41d4-a716-446655440031",
            "550e8400-e29b-41d4-a716-446655440030",
        )),
        true,
    )
    .await;
}

#[tokio::test]
async fn test_actionable_received_payment_requests_excludes_locally_rejected_request() {
    assert_local_response_actionability(
        "550e8400-e29b-41d4-a716-446655440040",
        "650e8400-e29b-41d4-a716-446655440040",
        parsed_payment_request_event(payment_request_rejection_raw(
            "650e8400-e29b-41d4-a716-446655440041",
            "550e8400-e29b-41d4-a716-446655440040",
        )),
        false,
    )
    .await;
}

#[tokio::test]
async fn test_list_payment_requests_counterparty_filter_preserves_blocked_error() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let blocked = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let local_public_key = local_public_key.clone();
            let blocked = blocked.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
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
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440020",
            "550e8400-e29b-41d4-a716-446655440020",
            None,
        )],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440021",
            "550e8400-e29b-41d4-a716-446655440021",
            None,
        )],
        None,
        FixedClock.now() - chrono::Duration::seconds(1),
    )
    .await
    .unwrap();
    persist_private_stream_batch(
        &storage,
        blocked.clone(),
        vec![payment_request_message(
            "650e8400-e29b-41d4-a716-446655440022",
            "550e8400-e29b-41d4-a716-446655440022",
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let records = sdk
        .list_payment_requests(PaymentRequestFilter {
            counterparty: Some(counterparty.clone()),
            ..PaymentRequestFilter::default()
        })
        .await
        .unwrap();
    let blocked_result = sdk
        .list_payment_requests(PaymentRequestFilter {
            counterparty: Some(blocked.clone()),
            ..PaymentRequestFilter::default()
        })
        .await;

    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.counterparty == counterparty));
    assert_eq!(
        sdk.payment_requests_with(&counterparty).await.unwrap()[0].payment_request_id,
        "550e8400-e29b-41d4-a716-446655440020"
    );
    assert_eq!(
        sdk.received_payment_requests_from(&counterparty)
            .await
            .unwrap()[0]
            .payment_request_id,
        "550e8400-e29b-41d4-a716-446655440021"
    );
    assert!(matches!(blocked_result, Err(PaykitSdkError::Policy { .. })));
    assert!(matches!(
        sdk.payment_requests_with(&blocked).await,
        Err(PaykitSdkError::Policy { .. })
    ));
    assert!(matches!(
        sdk.received_payment_requests_from(&blocked).await,
        Err(PaykitSdkError::Policy { .. })
    ));
}

#[tokio::test]
async fn test_active_recurring_payment_requests_filters_accepted_recurring_requests() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let recurring_peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let one_time_peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
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
    authorize_payment_request_app(&storage, recurring_peer.clone(), "bitkit").await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
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
async fn test_active_recurring_payment_requests_require_current_remote_payer_app() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    queue_recurring_request_with_inbound_acceptance(
        &storage,
        counterparty.clone(),
        "650e8400-e29b-41d4-a716-446655440020",
        "650e8400-e29b-41d4-a716-446655440021",
        "550e8400-e29b-41d4-a716-446655440020",
    )
    .await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let historical = sdk.payment_requests_with(&counterparty).await.unwrap();
    let unauthorized = sdk.active_recurring_payment_requests().await.unwrap();
    authorize_payment_request_app(&storage, counterparty.clone(), "bitkit").await;
    let authorized = sdk.active_recurring_payment_requests().await.unwrap();

    assert_eq!(historical.len(), 1);
    assert_eq!(
        historical[0].state,
        PaymentRequestLifecycleState::ActiveRecurring
    );
    assert!(unauthorized.is_empty());
    assert_eq!(authorized.len(), 1);
    assert_eq!(authorized[0].counterparty, counterparty);
}

#[tokio::test]
async fn test_enqueue_payment_request_event_requires_private_capable_identity() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
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
    let storage = registered_test_storage();
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.accept_payment_request(counterparty, &request_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .iter()
        .all(|message| message.is_delivery_confirmation()));
}

#[tokio::test]
async fn test_reject_payment_request_allows_expired_proposal_before_readiness_check() {
    let storage = registered_test_storage();
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
    authorize_payment_request_app(&storage, counterparty.clone(), "bitkit").await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
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
        .iter()
        .all(|message| message.is_delivery_confirmation()));
}

#[tokio::test]
async fn test_accept_payment_request_does_not_queue_without_private_send_readiness() {
    let storage = registered_test_storage();
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
    authorize_payment_request_app(&storage, counterparty.clone(), "bitkit").await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.accept_payment_request(counterparty, &request_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .iter()
        .all(|message| message.is_delivery_confirmation()));
}

#[tokio::test]
async fn test_accept_payment_request_rejects_unregistered_origin_app() {
    let storage = registered_test_storage();
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.accept_payment_request(counterparty, &request_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .iter()
        .all(|message| message.is_delivery_confirmation()));
}

async fn assert_local_response_actionability(
    request_id: &str,
    request_event_id: &str,
    response: PaymentRequestEvent,
    expected_actionable: bool,
) {
    let storage = registered_test_storage();
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
        vec![payment_request_message(request_event_id, request_id, None)],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    authorize_payment_request_app(&storage, counterparty.clone(), "bitkit").await;
    crate::domain::payment_requests::enqueue_payment_request_event(
        &storage,
        counterparty,
        &app_id(),
        &response,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let actionable = sdk.actionable_received_payment_requests().await.unwrap();

    assert_eq!(!actionable.is_empty(), expected_actionable);
}

async fn authorize_payment_request_app(
    storage: &InMemoryStorage,
    counterparty: PubkyPublicKey,
    app_id: &str,
) {
    let app_id = paykit_lib::PaykitAppId::new(app_id).unwrap();
    storage
        .transaction(move |tx| {
            save_authorized_paykit_app(
                tx,
                counterparty,
                app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: false,
                    payment_requests: true,
                    receipts: false,
                    outgoing_payments: false,
                },
            );
            Ok(())
        })
        .await
        .unwrap();
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
    crate::domain::payment_requests::enqueue_payment_request_event(
        storage,
        counterparty.clone(),
        &app_id(),
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
        app_id: value
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        raw_json,
    }
}

fn payment_request_message_for_required_app(
    event_id: &str,
    request_id: &str,
    required_app_id: Option<&str>,
) -> PrivateApplicationMessage {
    let required_app_id = required_app_id
        .map(|value| format!(r#""{value}""#))
        .unwrap_or_else(|| "null".into());
    private_application_message(format!(
        r#"{{"version":1,"kind":"paykit.payment_request","app_id":"paykit-server","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":{required_app_id},"metadata":{{}}}}}}"#
    ))
}

fn payment_request_raw(event_id: &str, request_id: &str, recurrence: Option<&str>) -> String {
    let recurrence = recurrence.unwrap_or("null");
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":{recurrence},"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"required_app_id":null,"metadata":{{}}}}}}"#
    )
}

fn payment_request_acceptance_message(
    event_id: &str,
    request_id: &str,
) -> PrivateApplicationMessage {
    private_application_message(format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    ))
}

fn payment_request_acceptance_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_acceptance","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

fn payment_request_rejection_raw(event_id: &str, request_id: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.payment_request_rejection","app_id":"bitkit","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_acceptance_samples_time_after_transaction_fence() {
    let inner = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new_v4();
    let initial = FixedClock.now();
    let expiry =
        (initial + ChronoDuration::seconds(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    persist_private_stream_batch(
        &inner,
        counterparty.clone(),
        vec![payment_request_message(
            EventId::new_v4().as_str(),
            request_id.as_str(),
            Some(&expiry),
        )],
        None,
        initial,
    )
    .await
    .unwrap();
    authorize_payment_request_app(&inner, counterparty.clone(), "bitkit").await;
    crate::domain::payment_requests::claim_payment_request_execution(
        &inner,
        counterparty.clone(),
        &app_id(),
        &request_id,
        initial,
    )
    .await
    .unwrap();
    let queued_before = inner.snapshot().unwrap().outbound_private_messages;
    let (storage, entered) = TransactionGateStorage::new(inner.clone());
    let clock = MutableClock::new(initial);
    let task = tokio::spawn({
        let storage = storage.clone();
        let clock = clock.clone();
        async move {
            crate::domain::payment_requests::enqueue_checked_payment_request_action_with_identity(
                &storage,
                counterparty,
                &app_id(),
                &PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
                    EventId::new_v4(),
                    request_id,
                )),
                || clock.now(),
                None,
            )
            .await
        }
    });
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("acceptance waits for storage");
    clock.set(initial + ChronoDuration::seconds(2));
    storage.release();
    assert!(matches!(
        task.await.unwrap(),
        Err(PaykitSdkError::Policy { .. })
    ));
    assert_eq!(
        inner.snapshot().unwrap().outbound_private_messages,
        queued_before
    );
}

#[tokio::test]
async fn test_proof_correction_after_handoff_reaches_send_readiness() {
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    let storage = InMemoryStorage::with_registered_apps([app_id(), server.clone()]);
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new_v4();
    persist_private_stream_batch(
        &storage, counterparty.clone(),
        vec![private_application_message(payment_request_raw(
            EventId::new_v4().as_str(), request_id.as_str(),
            Some(r#"{"every":1,"unit":"month","starts_at":"2026-06-01T00:00:00Z","anchor":"2026-06-01T00:00:00Z","ends_at":null}"#),
        ))], None, FixedClock.now(),
    ).await.unwrap();
    authorize_payment_request_app(&storage, counterparty.clone(), "bitkit").await;
    let period = BillingPeriod::new("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z").unwrap();
    for event in [
        PaymentRequestEvent::Acceptance(PaymentRequestAcceptance::new(
            EventId::new_v4(),
            request_id.clone(),
        )),
        PaymentRequestEvent::Proof(PaymentProof::new(
            EventId::new_v4(),
            request_id.clone(),
            paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
            Some(period.clone()),
            paykit_lib::PaykitAppId::new("merchant").unwrap(),
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            JsonMap::new(),
        )),
    ] {
        crate::domain::payment_requests::enqueue_payment_request_event(
            &storage,
            counterparty.clone(),
            &app_id(),
            &event,
            FixedClock.now(),
        )
        .await
        .unwrap();
    }
    crate::domain::payment_requests::claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &server,
        &request_id,
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction(|tx| {
            tx.save_authorized_paykit_apps(counterparty.clone(), HashMap::new());
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let error = sdk
        .submit_payment_proof(
            counterparty,
            &request_id,
            Some(period),
            paykit_lib::PaykitAppId::new("merchant").unwrap(),
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            JsonMap::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, PaykitSdkError::Identity { .. }),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_quote_payment_request_samples_time_after_transaction_fence() {
    let inner = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
    queue_recurring_request_with_inbound_acceptance(
        &inner,
        counterparty.clone(),
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id.as_str(),
    )
    .await;
    let before = inner.snapshot().unwrap().outbound_private_messages.len();
    let (storage, entered) = TransactionGateStorage::new(inner.clone());
    let initial = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
    let clock = MutableClock::new(initial);
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        clock.clone(),
    );

    let task = tokio::spawn(async move {
        sdk.enqueue_payment_conversion_quote(
            &counterparty,
            &request_id,
            paykit_lib::BillingPeriod::new("2026-06-03T12:00:00Z", "2026-07-03T12:00:00Z").unwrap(),
            vec![paykit_lib::ConversionRate {
                asset: "usdt".into(),
                value: "1".into(),
            }],
            "2026-06-03T12:00:01Z".into(),
        )
        .await
    });
    entered
        .recv_timeout(Duration::from_secs(2))
        .expect("quote transaction must wait at the storage fence");
    clock.set(initial + ChronoDuration::seconds(2));
    storage.release();

    let result = task.await.unwrap();

    assert!(matches!(
        result,
        Err(PaykitSdkError::Policy { context, .. })
            if context == "cannot issue an expired conversion quote"
    ));
    assert_eq!(
        inner.snapshot().unwrap().outbound_private_messages.len(),
        before
    );
}

#[tokio::test]
async fn test_quote_payment_request_requires_private_send_readiness() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let result = sdk
        .quote_payment_request(
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            &PaymentRequestId::new_v4(),
            paykit_lib::BillingPeriod::new("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z").unwrap(),
            vec![paykit_lib::ConversionRate {
                asset: "usdt".into(),
                value: "1".into(),
            }],
            "2026-06-04T00:00:00Z".into(),
        )
        .await;
    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .is_empty());
}

#[tokio::test]
async fn test_quote_payment_request_rejects_identity_change_in_progress() {
    let sdk = PaykitSdk::with_clock(
        registered_test_storage(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let _guard = sdk.claim_identity_operation("sign out").unwrap();
    let result = sdk
        .quote_payment_request(
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
            &PaymentRequestId::new_v4(),
            paykit_lib::BillingPeriod::new("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z").unwrap(),
            vec![paykit_lib::ConversionRate {
                asset: "usdt".into(),
                value: "1".into(),
            }],
            "2026-06-04T00:00:00Z".into(),
        )
        .await;
    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}
