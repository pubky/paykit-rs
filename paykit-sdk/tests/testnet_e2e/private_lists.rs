use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use paykit_sdk::{
    storage::{StorageOperation, StorageTransactionCallback},
    InMemoryStorage, LinkedPeerState, OutboundPrivateMessageStatus, PaykitSdk, PaykitSdkConfig,
    PaykitSdkError, PrivatePaymentEndpointReservation, PrivatePaymentListReservationUpdate,
    PubkySessionAccess, PubkySessionProvider, Result as PaykitResult, StorageAdapter,
};
use tokio::sync::{Notify, Semaphore};

use crate::harness::{
    build_testnet, drive_link_to_linked, linked_two_party, private_receiving_detail, two_party,
    TestUser, TestnetSessionProvider,
};

#[tokio::test]
async fn test_private_receive_refreshes_cached_app_authorization() {
    let pair = linked_two_party().await;
    pair.bob
        .sdk
        .clear_private_payment_list(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let registry = pair
        .alice
        .sdk
        .paykit_app_registry(pair.bob.public_key.clone())
        .await
        .unwrap()
        .unwrap();
    let current_apps = registry
        .apps()
        .iter()
        .map(|(id, app)| (id.clone(), app.capabilities()))
        .collect::<std::collections::HashMap<_, _>>();
    let cached_apps = std::collections::HashMap::from([(
        paykit_lib::PaykitAppId::new("old-app").unwrap(),
        paykit_lib::PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    )]);
    let initial = pair.alice.storage.snapshot().unwrap();
    for registry_state in ["available", "unreadable", "missing"] {
        let remote = pair.bob.access.session.storage();
        match registry_state {
            "unreadable" => {
                remote
                    .put(
                        paykit_lib::PAYKIT_APP_REGISTRY_PATH,
                        b"invalid registry".to_vec(),
                    )
                    .await
                    .unwrap();
            }
            "missing" => {
                remote
                    .delete(paykit_lib::PAYKIT_APP_REGISTRY_PATH)
                    .await
                    .unwrap();
            }
            _ => {}
        }
        let storage = InMemoryStorage::from_state(initial.clone());
        storage
            .transaction(|tx| {
                tx.save_authorized_paykit_apps(pair.bob.public_key.clone(), cached_apps.clone());
                Ok(())
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::new(
            storage.clone(),
            TestnetSessionProvider::new(pair.alice.access.clone()),
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let received = sdk
            .receive_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(received.stream_item_ids.len(), 1);
        let expected = match registry_state {
            "available" => current_apps.clone(),
            "unreadable" => cached_apps.clone(),
            _ => std::collections::HashMap::new(),
        };
        assert_eq!(
            storage.snapshot().unwrap().authorized_paykit_apps[&pair.bob.public_key],
            expected
        );
    }
}

#[tokio::test]
async fn test_outbound_reservation_expiry_is_rechecked_before_publication() {
    use chrono::{DateTime, Utc};
    use std::sync::atomic::AtomicBool;

    #[derive(Clone)]
    struct ClaimClock {
        claimed: Arc<AtomicBool>,
        started_at: DateTime<Utc>,
    }
    impl paykit_sdk::Clock for ClaimClock {
        fn now(&self) -> DateTime<Utc> {
            self.started_at
                + chrono::Duration::seconds(if self.claimed.load(Ordering::SeqCst) {
                    2
                } else {
                    0
                })
        }
    }
    struct ClaimStorage {
        inner: InMemoryStorage,
        counterparty: paykit_sdk::PubkyPublicKey,
        claimed: Arc<AtomicBool>,
        prepared_path: Arc<std::sync::Mutex<Option<String>>>,
    }
    #[async_trait]
    impl StorageAdapter for ClaimStorage {
        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> PaykitResult<Box<dyn std::any::Any + Send>> {
            let result = self.inner.transaction_erased(f).await?;
            if let Some(message) = self
                .inner
                .snapshot()?
                .outbound_private_messages
                .iter()
                .find(|message| {
                    message.counterparty == self.counterparty
                        && message.status == OutboundPrivateMessageStatus::Sending
                })
            {
                self.claimed.store(true, Ordering::SeqCst);
                if let Some(prepared) = &message.prepared_send {
                    *self.prepared_path.lock().unwrap() = Some(prepared.destination_path.clone());
                }
            }
            Ok(result)
        }
    }

    let pair = linked_two_party().await;
    let started_at = Utc::now();
    let message = pair
        .alice
        .sdk
        .enqueue_private_payment_list_with_reservations(
            pair.bob.public_key.clone(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "invoice-1".into(),
                receiving_detail: private_receiving_detail("btc-lightning-bolt11", "ln-reserved"),
                expires_at: Some(started_at + chrono::Duration::seconds(1)),
                attribution: Default::default(),
            }],
        )
        .await
        .unwrap();
    let claimed = Arc::new(AtomicBool::new(false));
    let prepared_path = Arc::new(std::sync::Mutex::new(None));
    let sdk = PaykitSdk::with_clock(
        ClaimStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            claimed: claimed.clone(),
            prepared_path: prepared_path.clone(),
        },
        crate::harness::TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        ClaimClock {
            claimed: claimed.clone(),
            started_at,
        },
    );
    let report = sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert!(claimed.load(Ordering::SeqCst));
    assert_eq!(report.attempted, vec![message.outbound_message_id]);
    assert!(report.sent.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(
        report.failed[0].outbound_message_id,
        message.outbound_message_id
    );
    assert!(report.failed[0].error.contains("expired"));
    assert!(report.reservation_cleanup_failures.is_empty());
    let state = pair.alice.storage.snapshot().unwrap();
    let invalid = state
        .outbound_private_messages
        .iter()
        .find(|record| record.outbound_message_id == message.outbound_message_id)
        .unwrap();
    assert_eq!(invalid.last_attempt_at, Some(started_at));
    assert_eq!(invalid.status, OutboundPrivateMessageStatus::Invalid);
    assert!(invalid.prepared_send.is_none());
    assert!(state.payment_endpoint_reservations.is_empty());
    assert!(state.peer_link_operation_leases.is_empty());
    let path = prepared_path
        .lock()
        .unwrap()
        .clone()
        .expect("the expired send was prepared");
    assert!(matches!(
        pair.alice.access.session.storage().get(&path).await,
        Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
            if status == pubky::StatusCode::NOT_FOUND || status == pubky::StatusCode::GONE
    ));
    // The committed Noise slot cannot be skipped after the reservation expires.
    assert_eq!(
        state.linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::RecoveryRequired
    );
    assert!(matches!(
        pair.bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap_err(),
        PaykitSdkError::RecoveryRequired { .. }
    ));
    assert!(pair
        .bob
        .storage
        .snapshot()
        .unwrap()
        .private_stream_items
        .is_empty());
}

#[tokio::test]
async fn test_reservation_queue_commits_atomically_and_preserves_persisted_reservations_on_error() {
    struct InterruptedOperationStorage {
        inner: InMemoryStorage,
        fail_before_commit: Option<bool>,
        writes: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageAdapter for InterruptedOperationStorage {
        async fn run_operation_erased<'a>(
            &self,
            operation: StorageOperation<'a>,
        ) -> PaykitResult<Box<dyn std::any::Any + Send>> {
            let result = operation.await?;
            if self.fail_before_commit == Some(false) {
                return Err(PaykitSdkError::Transport {
                    context: "injected storage operation failure".into(),
                    source: None,
                });
            }
            Ok(result)
        }

        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> PaykitResult<Box<dyn std::any::Any + Send>> {
            let before = self.inner.snapshot()?;
            let message_count = before.outbound_private_messages.len();
            let result = self
                .inner
                .transaction_erased(Box::new(move |tx| {
                    let result = f(tx)?;
                    if self.fail_before_commit == Some(true)
                        && tx.export_storage_state().outbound_private_messages.len() > message_count
                    {
                        return Err(PaykitSdkError::Transport {
                            context: "injected storage operation failure".into(),
                            source: None,
                        });
                    }
                    Ok(result)
                }))
                .await?;
            if self.inner.snapshot()? != before {
                self.writes.fetch_add(1, Ordering::SeqCst);
            }
            Ok(result)
        }
    }

    for (fail_before_commit, peer_busy) in [
        (None, false),
        (Some(true), false),
        (Some(false), false),
        (None, true),
    ] {
        let pair = linked_two_party().await;
        if peer_busy {
            pair.alice
                .storage
                .transaction(|tx| {
                    let now = chrono::Utc::now();
                    tx.claim_peer_link_operation(
                        &pair.bob.public_key,
                        now,
                        now + chrono::Duration::minutes(1),
                    )?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let (mut cancelled, resume) = pair.alice.adapter.pause_next_reservation_cancellation();
        resume.send(()).unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::new(
            InterruptedOperationStorage {
                inner: pair.alice.storage.clone(),
                fail_before_commit,
                writes: writes.clone(),
            },
            crate::harness::TestnetSessionProvider::new(pair.alice.access.clone()),
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            sdk.enqueue_private_payment_list_with_reservations(
                pair.bob.public_key.clone(),
                vec![PrivatePaymentEndpointReservation {
                    reservation_id: "invoice-1".into(),
                    receiving_detail: private_receiving_detail(
                        "btc-lightning-bolt11",
                        "ln-reserved",
                    ),
                    expires_at: None,
                    attribution: Default::default(),
                }],
            ),
        )
        .await
        .unwrap();

        if peer_busy {
            assert!(result.unwrap_err().is_concurrent_update());
            assert_eq!(writes.load(Ordering::SeqCst), 0);
            assert!(matches!(
                cancelled.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            let state = pair.alice.storage.snapshot().unwrap();
            assert_eq!(state.peer_link_operation_leases.len(), 1);
            assert!(state.payment_endpoint_reservations.is_empty());
            assert!(state.outbound_private_messages.is_empty());
            continue;
        } else if fail_before_commit.is_some() {
            assert!(
                matches!(result, Err(PaykitSdkError::Transport { context, .. })
                if context == "injected storage operation failure")
            );
        } else {
            result.unwrap();
            assert_eq!(writes.load(Ordering::SeqCst), 1);
        }
        let state = pair.alice.storage.snapshot().unwrap();
        assert!(state.peer_link_operation_leases.is_empty());
        if fail_before_commit == Some(true) {
            assert_eq!(cancelled.try_recv(), Ok(()));
            assert!(state.payment_endpoint_reservations.is_empty());
            assert!(state.outbound_private_messages.is_empty());
        } else {
            assert!(matches!(
                cancelled.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            assert_eq!(state.payment_endpoint_reservations.len(), 1);
            assert_eq!(state.outbound_private_messages.len(), 1);
            assert_eq!(
                state.outbound_private_messages[0].status,
                OutboundPrivateMessageStatus::Pending
            );
        }
    }
}

#[tokio::test]
async fn test_private_inbox_probes_are_bounded_and_isolate_a_slow_failed_peer() {
    struct GatedPublicStorage {
        access: PubkySessionAccess,
        calls: Arc<AtomicUsize>,
        started: Arc<Notify>,
        first: Arc<Semaphore>,
        rest: Arc<Semaphore>,
    }

    #[async_trait]
    impl PubkySessionProvider for GatedPublicStorage {
        async fn load_session_access(&self) -> PaykitResult<Option<PubkySessionAccess>> {
            Ok(Some(self.access.clone()))
        }

        async fn clear_session_access(&self) -> PaykitResult<()> {
            unreachable!("inbox polling must not sign out")
        }

        async fn load_public_storage(&self) -> PaykitResult<Option<pubky::PublicStorage>> {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            if index == 0 {
                self.first.acquire().await.unwrap().forget();
                return Err(PaykitSdkError::Transport {
                    context: "peer lookup unavailable".into(),
                    source: None,
                });
            }
            self.rest.acquire().await.unwrap().forget();
            Ok(Some(self.access.outbox_client.public_storage()))
        }
    }

    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    for _ in 0..17 {
        let peer = TestUser::sign_up(&testnet).await;
        alice
            .sdk
            .initiate_link_with_peer(peer.public_key.clone())
            .await
            .unwrap();
        peer.sdk
            .accept_link_with_peer(alice.public_key.clone())
            .await
            .unwrap();
        drive_link_to_linked(&alice, &peer).await;
    }
    let before = alice.storage.snapshot().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let first = Arc::new(Semaphore::new(0));
    let rest = Arc::new(Semaphore::new(0));
    let sdk = PaykitSdk::new(
        alice.storage.clone(),
        GatedPublicStorage {
            access: alice.access.clone(),
            calls: calls.clone(),
            started: started.clone(),
            first: first.clone(),
            rest: rest.clone(),
        },
        alice.adapter.clone(),
        PaykitSdkConfig::new(alice.app_id.clone()).unwrap(),
    );
    let mut receive = Box::pin(sdk.receive_private_messages_from_linked_peers());
    for expected in [16, 17] {
        tokio::select! {
            result = &mut receive => panic!("batch finished before releasing the slow peer: {result:?}"),
            ready = tokio::time::timeout(Duration::from_secs(10), async {
                while calls.load(Ordering::SeqCst) < expected {
                    started.notified().await;
                }
            }) => ready.expect("other peer probes must progress while the first is stalled"),
        }
        assert_eq!(calls.load(Ordering::SeqCst), expected);
        rest.add_permits(16);
    }
    first.add_permits(1);
    let reports = tokio::time::timeout(Duration::from_secs(10), receive)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reports.len(), 17);
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.error.is_some())
            .count(),
        1
    );
    assert!(reports[0]
        .error
        .as_ref()
        .unwrap()
        .contains("peer lookup unavailable"));
    assert!(reports[1..].iter().all(|report| report
        .report
        .as_ref()
        .is_some_and(|intake| intake.stream_item_ids.is_empty())));
    assert!(reports
        .windows(2)
        .all(|pair| pair[0].counterparty.as_str() < pair[1].counterparty.as_str()));
    assert_eq!(alice.storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_recovery_intake_does_not_interrupt_other_probes() {
    struct NotifyingPublicStorage {
        access: PubkySessionAccess,
        completed: Arc<Semaphore>,
    }

    #[async_trait]
    impl PubkySessionProvider for NotifyingPublicStorage {
        async fn load_session_access(&self) -> PaykitResult<Option<PubkySessionAccess>> {
            Ok(Some(self.access.clone()))
        }

        async fn clear_session_access(&self) -> PaykitResult<()> {
            unreachable!("inbox polling must not sign out")
        }

        async fn load_public_storage(&self) -> PaykitResult<Option<pubky::PublicStorage>> {
            self.completed.add_permits(1);
            Ok(Some(self.access.outbox_client.public_storage()))
        }
    }

    struct GatedReceiveStorage {
        inner: InMemoryStorage,
        calls: AtomicUsize,
        completed: Arc<Semaphore>,
    }

    #[async_trait]
    impl StorageAdapter for GatedReceiveStorage {
        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> PaykitResult<Box<dyn std::any::Any + Send>> {
            // Delay the first receive transaction until the other probe completes.
            if self.calls.fetch_add(1, Ordering::SeqCst) == 1 {
                self.completed.acquire().await.unwrap().forget();
            }
            self.inner.transaction_erased(f).await
        }
    }

    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    let mut peers = Vec::new();
    for _ in 0..2 {
        let peer = TestUser::sign_up(&testnet).await;
        alice
            .sdk
            .initiate_link_with_peer(peer.public_key.clone())
            .await
            .unwrap();
        peer.sdk
            .accept_link_with_peer(alice.public_key.clone())
            .await
            .unwrap();
        drive_link_to_linked(&alice, &peer).await;
        peer.adapter
            .set_private_details(vec![private_receiving_detail(
                "btc-lightning-bolt11",
                "ln-private-peer",
            )]);
        peer.sdk
            .enqueue_private_payment_list(alice.public_key.clone())
            .await
            .unwrap();
        let sent = peer
            .sdk
            .process_outbound_private_messages(alice.public_key.clone())
            .await
            .unwrap();
        assert_eq!(sent.sent.len(), 1);
        peers.push(peer.public_key);
    }
    peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    alice
        .storage
        .transaction(|tx| {
            let mut state = tx.encrypted_link_state(&peers[0]).unwrap();
            state.link_snapshot = None;
            tx.save_encrypted_link_state(state);
            Ok(())
        })
        .await
        .unwrap();
    let completed = Arc::new(Semaphore::new(0));
    let sdk = PaykitSdk::new(
        GatedReceiveStorage {
            inner: alice.storage.clone(),
            calls: AtomicUsize::new(0),
            completed: completed.clone(),
        },
        NotifyingPublicStorage {
            access: alice.access.clone(),
            completed,
        },
        alice.adapter.clone(),
        PaykitSdkConfig::new(alice.app_id.clone()).unwrap(),
    );
    let reports = tokio::time::timeout(
        Duration::from_secs(10),
        sdk.receive_private_messages_from_linked_peers(),
    )
    .await
    .expect("a receive must not stop another probe from progressing")
    .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports[0].error.is_some());
    assert!(reports[1].error.is_none());
    assert_eq!(reports[1].report.as_ref().unwrap().stream_item_ids.len(), 1);
}

#[tokio::test]
async fn test_idle_private_receive_detects_remote_recovery_and_rotation() {
    for rotate_key in [false, true] {
        let pair = linked_two_party().await;
        if rotate_key {
            let replacement = pair
                .alice
                .access
                .local_secret_key
                .as_ref()
                .unwrap()
                .derive_paykit_identity_secret_key(2)
                .unwrap();
            pair.alice
                .sdk
                .rotate_paykit_identity_key(replacement)
                .await
                .unwrap();
        } else {
            pair.alice
                .sdk
                .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
                .await
                .unwrap();
        }
        let error = pair
            .bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap_err();
        assert!(matches!(error, PaykitSdkError::RecoveryRequired { .. }));
        assert_eq!(
            pair.bob.storage.snapshot().unwrap().linked_peers[&pair.alice.public_key].state,
            LinkedPeerState::RecoveryRequired
        );
    }
}

#[tokio::test]
async fn test_payment_preparation_refreshes_an_empty_private_list() {
    let pair = linked_two_party().await;
    let empty = pair
        .bob
        .sdk
        .prepare_and_resolve_private_contact_payment(pair.alice.public_key.clone(), None, None, 1)
        .await
        .unwrap();
    assert!(empty.resolution.payable_endpoints.is_empty());
    assert!(empty.receive_report.unwrap().stream_item_ids.is_empty());

    pair.alice
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);
    pair.alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let prepared = pair
        .bob
        .sdk
        .prepare_and_resolve_private_contact_payment(pair.alice.public_key.clone(), None, None, 1)
        .await
        .unwrap();
    assert!(!prepared.receive_report.unwrap().stream_item_ids.is_empty());
    assert_eq!(prepared.resolution.payable_endpoints.len(), 1);
    assert_eq!(
        prepared.resolution.payable_endpoints[0].endpoint.payload,
        "ln-private-alice"
    );

    struct RecoverAtResolution<'a> {
        local: &'a TestUser,
        remote: &'a TestUser,
        session_loads: AtomicUsize,
    }
    #[async_trait]
    impl PubkySessionProvider for RecoverAtResolution<'_> {
        async fn load_session_access(&self) -> PaykitResult<Option<PubkySessionAccess>> {
            // Availability, link preparation and receive load sessions first;
            // the next load starts resolution after the empty inbox probe.
            if self.session_loads.fetch_add(1, Ordering::SeqCst) == 3 {
                let before = self.local.storage.snapshot()?.encrypted_link_states
                    [&self.remote.public_key]
                    .clone();
                self.local
                    .sdk
                    .enqueue_private_payment_list(self.remote.public_key.clone())
                    .await?;
                let sent = self
                    .local
                    .sdk
                    .process_outbound_private_messages(self.remote.public_key.clone())
                    .await?;
                assert!(!sent.sent.is_empty());
                assert_ne!(
                    self.local.storage.snapshot()?.encrypted_link_states[&self.remote.public_key],
                    before
                );
                self.remote
                    .sdk
                    .publish_encrypted_link_recovery_marker(self.local.public_key.clone())
                    .await?;
            }
            Ok(Some(self.local.access.clone()))
        }

        async fn load_public_storage(&self) -> PaykitResult<Option<pubky::PublicStorage>> {
            Ok(Some(self.local.access.outbox_client.public_storage()))
        }

        async fn clear_session_access(&self) -> PaykitResult<()> {
            unreachable!("payment preparation must not sign out")
        }
    }

    struct NoWalletSelection;
    #[async_trait]
    impl paykit_sdk::PaymentAdapter for NoWalletSelection {
        async fn select_private_payment_endpoints(
            &self,
            _request: &paykit_sdk::PrivatePaymentEndpointSelectionRequest,
        ) -> PaykitResult<Vec<paykit_sdk::PrivatePaymentEndpointCandidate>> {
            panic!("recovery must be observed before cached endpoints reach the wallet")
        }
    }

    pair.bob
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-bob",
        )]);
    let sdk = PaykitSdk::new(
        pair.bob.storage.clone(),
        RecoverAtResolution {
            local: &pair.bob,
            remote: &pair.alice,
            session_loads: AtomicUsize::new(0),
        },
        NoWalletSelection,
        PaykitSdkConfig::new(pair.bob.app_id.clone()).unwrap(),
    );
    let recovered = tokio::time::timeout(
        Duration::from_secs(30),
        sdk.prepare_and_resolve_private_contact_payment(
            pair.alice.public_key.clone(),
            None,
            None,
            1,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(recovered.receive_report.unwrap().stream_item_ids.is_empty());
    assert_eq!(
        recovered.resolution.state,
        paykit_sdk::PrivatePaymentResolutionState::RecoveryPending
    );
    assert!(recovered.resolution.payable_endpoints.is_empty());
    let local = pair.bob.storage.snapshot().unwrap();
    let remote = pair.alice.storage.snapshot().unwrap();
    let peer = &local.linked_peers[&pair.alice.public_key];
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert!(peer.remote_recovery_attempt_id.is_some());
    assert_eq!(
        peer.remote_recovery_attempt_id,
        remote.linked_peers[&pair.bob.public_key].local_recovery_attempt_id
    );

    pair.bob
        .sdk
        .block_peer(pair.alice.public_key.clone())
        .await
        .unwrap();
    let error = pair
        .bob
        .sdk
        .prepare_and_resolve_private_contact_payment(pair.alice.public_key.clone(), None, None, 1)
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Policy { .. }));
}

#[tokio::test]
async fn test_private_payment_list_roundtrip_between_linked_peers() {
    let pair = linked_two_party().await;
    let before_receive = pair.bob.sdk.export_backup_state().await.unwrap();
    for _ in 0..3 {
        let reports = pair
            .bob
            .sdk
            .receive_private_messages_from_linked_peers()
            .await
            .unwrap();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_none());
        let intake = reports[0].report.as_ref().unwrap();
        assert_eq!(intake.receive_batch_id, None);
        assert!(intake.stream_item_ids.is_empty());
        assert_eq!(
            pair.bob.sdk.export_backup_state().await.unwrap(),
            before_receive
        );
    }
    pair.alice
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let queued = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect("enqueue should succeed for a linked peer");
    assert_eq!(queued.status, OutboundPrivateMessageStatus::Pending);

    let send_report = pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("processing the outbound queue should succeed");
    assert_eq!(send_report.sent, vec![queued.outbound_message_id]);
    assert!(send_report.failed.is_empty());
    let sent = pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .outbound_private_messages
        .into_iter()
        .find(|record| record.outbound_message_id == queued.outbound_message_id)
        .expect("sent message should remain in the audit log");
    assert_eq!(sent.status, OutboundPrivateMessageStatus::Sent);
    assert!(sent.prepared_send.is_none());

    let intake = pair
        .bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("receiving private messages should succeed");
    assert!(!intake.stream_item_ids.is_empty());
    assert!(intake.event_conflicts.is_empty());
    assert!(intake.receive_batch_id.is_some());
    let after_receive = pair.bob.sdk.export_backup_state().await.unwrap();
    assert_ne!(after_receive, before_receive);
    pair.bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    assert_eq!(
        pair.bob.sdk.export_backup_state().await.unwrap(),
        after_receive
    );

    let views = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .expect("reading Private Payment Lists should succeed");
    let view = views
        .iter()
        .find(|view| view.app_id == pair.alice.app_id)
        .expect("the sender app's valid list should be present after receive");
    assert_eq!(
        view.payment_endpoints
            .get("btc-lightning-bolt11")
            .map(String::as_str),
        Some("ln-private-alice")
    );
    assert!(view.latest_stream_item_id.is_some());
}

#[tokio::test]
async fn test_private_list_sync_only_sends_changed_details_on_current_link() {
    let pair = linked_two_party().await;
    let mut update = PrivatePaymentListReservationUpdate {
        counterparty: pair.bob.public_key.clone(),
        reservations: vec![PrivatePaymentEndpointReservation {
            reservation_id: "invoice-1".into(),
            receiving_detail: private_receiving_detail("btc-lightning-bolt11", "ln-private-1"),
            expires_at: None,
            attribution: Default::default(),
        }],
    };
    let first = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(first.queued.len(), 1);
    assert!(first.failed_to_deliver.is_empty());
    let intake = pair
        .bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    assert_eq!(intake[0].report.as_ref().unwrap().stream_item_ids.len(), 1);
    let alice_backup = pair.alice.sdk.export_backup_state().await.unwrap();
    let bob_backup = pair.bob.sdk.export_backup_state().await.unwrap();
    for _ in 0..3 {
        pair.alice
            .sdk
            .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
            .await
            .unwrap();
        pair.alice
            .sdk
            .advance_link_handshake(pair.bob.public_key.clone())
            .await
            .unwrap();
        let unchanged = pair
            .alice
            .sdk
            .sync_private_payment_lists_with_reservations_and_process_outbound(
                vec![update.clone()],
                false,
            )
            .await
            .unwrap();
        assert_eq!(unchanged.queued, first.queued);
        assert!(unchanged.failed_to_queue.is_empty());
        assert!(unchanged.failed_to_deliver.is_empty());
        let intake = pair
            .bob
            .sdk
            .receive_private_messages_from_linked_peers()
            .await
            .unwrap();
        assert!(intake[0]
            .report
            .as_ref()
            .unwrap()
            .stream_item_ids
            .is_empty());
        assert_eq!(
            pair.alice.sdk.export_backup_state().await.unwrap(),
            alice_backup
        );
        assert_eq!(
            pair.bob.sdk.export_backup_state().await.unwrap(),
            bob_backup
        );
    }

    pair.bob
        .sdk
        .clear_private_payment_list_and_process_outbound(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    let after_receive = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(after_receive.queued, first.queued);

    update.reservations[0].reservation_id = "invoice-2".into();
    update.reservations[0].receiving_detail.payload = "ln-private-2".into();
    let changed = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![update.clone()],
            false,
        )
        .await
        .unwrap();
    assert_eq!(changed.queued.len(), 1);
    assert_ne!(
        changed.queued[0].outbound_message_id,
        first.queued[0].outbound_message_id
    );
    assert!(changed.failed_to_deliver.is_empty());
    pair.bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    let view = pair
        .bob
        .sdk
        .current_private_payment_lists(&pair.alice.public_key)
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.app_id == pair.alice.app_id)
        .unwrap();
    assert_eq!(
        view.payment_endpoints
            .get("btc-lightning-bolt11")
            .map(String::as_str),
        Some("ln-private-2")
    );

    update.reservations.clear();
    let mut clear_id = None;
    for _ in 0..2 {
        let cleared = pair
            .alice
            .sdk
            .sync_private_payment_lists_with_reservations_and_process_outbound(
                vec![update.clone()],
                false,
            )
            .await
            .unwrap();
        assert_eq!(cleared.cleared.len(), 1);
        assert!(cleared.failed_to_deliver.is_empty());
        if clear_id.is_some() {
            assert_eq!(cleared.cleared[0].outbound_message_id, clear_id);
        }
        clear_id = cleared.cleared[0].outbound_message_id;
    }
    for _ in 0..2 {
        let explicit = pair
            .alice
            .sdk
            .clear_private_payment_list_and_process_outbound(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(explicit.cleared.len(), 1);
        assert!(explicit.failed_to_deliver.is_empty());
        assert_ne!(explicit.cleared[0].outbound_message_id, clear_id);
        clear_id = explicit.cleared[0].outbound_message_id;
    }
}

#[tokio::test]
async fn test_private_list_sync_corrupt_snapshot_marks_recovery_required() {
    let pair = linked_two_party().await;
    pair.alice
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            move |tx| {
                let mut link_state = tx
                    .encrypted_link_state(&counterparty)
                    .expect("linked peer should have Encrypted Link state");
                link_state.link_snapshot = Some(vec![1, 2, 3]);
                tx.save_encrypted_link_state(link_state);
                Ok(())
            }
        })
        .await
        .unwrap();

    let report = pair
        .alice
        .sdk
        .sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![PrivatePaymentListReservationUpdate {
                counterparty: pair.bob.public_key.clone(),
                reservations: Vec::new(),
            }],
            false,
        )
        .await
        .unwrap();

    assert_eq!(report.cleared.len(), 1);
    assert!(report.failed_to_queue.is_empty());
    assert_eq!(report.failed_to_deliver.len(), 1);
    let peer = pair
        .alice
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            move |tx| Ok(tx.linked_peer(&counterparty))
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
}

#[tokio::test]
async fn test_enqueue_private_payment_list_without_link_fails() {
    let pair = two_party().await;
    pair.alice
        .adapter
        .set_private_details(vec![private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);

    let err = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone())
        .await
        .expect_err("enqueue without an Encrypted Link must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );
}
