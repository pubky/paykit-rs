use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms,
};
use paykit_sdk::{
    InMemoryStorage, LinkedPeerState, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentRequestLifecycleState, PrivatePaymentListReservationUpdate, PubkyPublicKey,
    PubkySessionAccess, PubkySessionProvider, StorageAdapter,
};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

use crate::harness::{
    drive_link_to_linked, linked_two_party, private_receiving_detail, two_party,
    TestnetSessionProvider,
};

struct PausedPublicReadProvider {
    inner: TestnetSessionProvider,
    public_reads: Arc<AtomicUsize>,
    pause_after: usize,
    fail_paused_read: bool,
    pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[tokio::test]
async fn test_private_messages_preserve_state_on_invalid_link_metadata() {
    for metadata in ["authorization", "marker"] {
        let pair = linked_two_party().await;
        pair.bob
            .sdk
            .clear_private_payment_list_and_process_outbound(pair.alice.public_key.clone())
            .await
            .unwrap();
        pair.alice
            .sdk
            .clear_private_payment_list(pair.bob.public_key.clone())
            .await
            .unwrap();
        let path = if metadata == "authorization" {
            paykit_lib::PAYKIT_NOISE_KEY_AUTHORIZATION_PATH.to_owned()
        } else {
            let noise_key = |access: &PubkySessionAccess| {
                let key = access
                    .local_secret_key
                    .as_ref()
                    .unwrap()
                    .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
                    .unwrap();
                paykit_lib::derive_paykit_noise_secret_key(key.as_bytes())
            };
            let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
                &noise_key(&pair.bob.access),
                pair.bob.access.session.info().public_key(),
                pair.alice.access.session.info().public_key(),
                &pubky::Keypair::from_secret(&noise_key(&pair.alice.access)).public_key(),
            );
            path
        };
        let remote = pair.bob.access.session.storage();
        let original = match remote.get(&path).await {
            Ok(response) => Some(response.bytes().await.unwrap()),
            Err(_) => None,
        };
        remote
            .put(&path, b"invalid link metadata".to_vec())
            .await
            .unwrap();
        let before = pair.alice.storage.snapshot().unwrap();
        for receive in [false, true] {
            let result = if receive {
                pair.alice
                    .sdk
                    .receive_private_messages(pair.bob.public_key.clone())
                    .await
                    .map(|_| ())
            } else {
                pair.alice
                    .sdk
                    .process_outbound_private_messages(pair.bob.public_key.clone())
                    .await
                    .map(|_| ())
            };
            assert!(
                matches!(result, Err(PaykitSdkError::LinkObservation { .. })),
                "{metadata}, receive={receive}: {result:?}"
            );
            let after = pair.alice.storage.snapshot().unwrap();
            assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
            assert_eq!(after.linked_peers, before.linked_peers);
            assert_eq!(
                after.outbound_private_messages,
                before.outbound_private_messages
            );
            assert!(after.peer_link_operation_leases.is_empty());
        }
        match original {
            Some(bytes) => {
                remote.put(&path, bytes).await.unwrap();
            }
            None => {
                remote.delete(&path).await.unwrap();
            }
        }
        let sent = pair
            .alice
            .sdk
            .process_outbound_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(sent.sent.len(), 1);
        assert!(sent.failed.is_empty());
        let received = pair
            .alice
            .sdk
            .receive_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(received.stream_item_ids.len(), 1);
    }
}

#[tokio::test]
async fn test_invalid_recovery_observation_preserves_handshake_and_retries_after_correction() {
    for state in ["new", "pending", "linked"] {
        let pair = if state == "linked" {
            linked_two_party().await
        } else {
            two_party().await
        };
        if state == "pending" {
            pair.alice
                .sdk
                .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
                .await
                .unwrap();
        }
        let noise_key = |access: &PubkySessionAccess| {
            let key = access
                .local_secret_key
                .as_ref()
                .unwrap()
                .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
                .unwrap();
            paykit_lib::derive_paykit_noise_secret_key(key.as_bytes())
        };
        let alice_key = noise_key(&pair.alice.access);
        let bob_key = noise_key(&pair.bob.access);
        let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
            &bob_key,
            pair.bob.access.session.info().public_key(),
            pair.alice.access.session.info().public_key(),
            &pubky::Keypair::from_secret(&alice_key).public_key(),
        );
        let storage = pair.bob.access.session.storage();
        let original = match storage.get(&path).await {
            Ok(response) => Some(response.bytes().await.unwrap()),
            Err(_) => None,
        };
        storage
            .put(&path, b"invalid recovery marker".to_vec())
            .await
            .unwrap();
        let before = pair.alice.storage.snapshot().unwrap();
        let error = pair
            .alice
            .sdk
            .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
            .await
            .unwrap_err();
        assert!(
            matches!(error, PaykitSdkError::LinkObservation { .. }),
            "{state}: {error:?}"
        );
        let after = pair.alice.storage.snapshot().unwrap();
        assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
        assert_eq!(
            after.outbound_private_messages,
            before.outbound_private_messages
        );
        assert!(after.peer_link_operation_leases.is_empty());
        if let Some(peer) = before.linked_peers.get(&pair.bob.public_key) {
            assert_eq!(after.linked_peers[&pair.bob.public_key], *peer);
        }
        match original {
            Some(bytes) => {
                storage.put(&path, bytes).await.unwrap();
            }
            None => {
                storage.delete(&path).await.unwrap();
            }
        }
        pair.alice
            .sdk
            .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
            .await
            .unwrap();
    }
}

#[async_trait::async_trait]
impl PubkySessionProvider for PausedPublicReadProvider {
    async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
        self.inner.load_session_access().await
    }

    async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
        let pause = if self.public_reads.fetch_add(1, Ordering::SeqCst) == self.pause_after {
            self.pause.lock().unwrap().take()
        } else {
            None
        };
        if let Some((ready, resume)) = pause {
            ready.send(()).expect("receive observer should remain live");
            resume.await.expect("receive should be released");
            if self.fail_paused_read {
                return Err(PaykitSdkError::RecoveryRequired {
                    context: "injected registry lookup failure".into(),
                    source: None,
                });
            }
        }
        self.inner.load_public_storage().await
    }

    async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
        self.inner.clear_session_access().await
    }
}

#[tokio::test]
async fn test_private_receive_rejects_a_changed_checkpoint_before_commit() {
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
    let initial = pair.alice.storage.snapshot().unwrap();

    for change in [
        "receive",
        "lease",
        "blocked",
        "recovery",
        "authorization",
        "local-key",
    ] {
        let storage = InMemoryStorage::from_state(initial.clone());
        let (ready, reached) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let sdk = PaykitSdk::new(
            storage.clone(),
            PausedPublicReadProvider {
                inner: TestnetSessionProvider::new(pair.alice.access.clone()),
                public_reads: Arc::new(AtomicUsize::new(0)),
                pause_after: 1,
                fail_paused_read: false,
                pause: Mutex::new(Some((ready, paused))),
            },
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let receive = sdk.receive_private_messages(pair.bob.public_key.clone());
        tokio::pin!(receive);
        tokio::select! {
            reached = tokio::time::timeout(Duration::from_secs(30), reached) =>
                reached.expect("read-only preparation should reach restore").unwrap(),
            result = &mut receive => panic!("receive did not pause: {result:?}"),
        }
        assert!(storage
            .snapshot()
            .unwrap()
            .peer_link_operation_leases
            .is_empty());
        if change == "receive" {
            let other = PaykitSdk::new(
                storage.clone(),
                TestnetSessionProvider::new(pair.alice.access.clone()),
                pair.alice.adapter.clone(),
                PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
            );
            let committed = other
                .receive_private_messages(pair.bob.public_key.clone())
                .await
                .unwrap();
            assert_eq!(committed.stream_item_ids.len(), 1);
        } else {
            storage
                .transaction(|tx| {
                    let mut peer = tx.linked_peer(&pair.bob.public_key).unwrap();
                    match change {
                        "lease" => {
                            let now = chrono::Utc::now();
                            tx.claim_peer_link_operation(
                                &pair.bob.public_key,
                                now,
                                now + chrono::Duration::minutes(1),
                            )?
                            .unwrap();
                        }
                        "blocked" => peer.state = LinkedPeerState::Blocked,
                        "recovery" => peer.remote_recovery_attempt_id = Some("new-recovery".into()),
                        "authorization" => {
                            let identity = pair.bob.access.local_secret_key.as_ref().unwrap();
                            let key = identity.derive_paykit_identity_secret_key(2).unwrap();
                            peer.noise_key_authorization = Some(
                                paykit_lib::PaykitNoiseKeyAuthorization::sign(
                                    &pubky::Keypair::from_secret(identity.as_bytes()),
                                    &paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
                                    key.key_generation(),
                                )
                                .unwrap(),
                            );
                        }
                        "local-key" => tx.save_paykit_noise_public_key(
                            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
                        ),
                        _ => unreachable!(),
                    }
                    tx.save_linked_peer(peer);
                    Ok(())
                })
                .await
                .unwrap();
        }
        let changed = storage.snapshot().unwrap();
        resume.send(()).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(30), receive)
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            if change == "local-key" {
                matches!(error, PaykitSdkError::Identity { .. })
            } else {
                error.is_concurrent_update()
            },
            "unexpected {change} result: {error:?}",
        );
        assert_eq!(storage.snapshot().unwrap(), changed, "{change}");
    }
}

#[tokio::test]
async fn test_recovery_reads_overlap_without_bypassing_authorization_failure() {
    let pair = linked_two_party().await;
    let initial = pair.alice.storage.snapshot().unwrap();
    let storage = InMemoryStorage::from_state(initial.clone());
    let public_reads = Arc::new(AtomicUsize::new(0));
    let (ready, reached) = oneshot::channel();
    let (resume, paused) = oneshot::channel();
    let sdk = PaykitSdk::new(
        storage.clone(),
        PausedPublicReadProvider {
            inner: TestnetSessionProvider::new(pair.alice.access.clone()),
            public_reads: public_reads.clone(),
            pause_after: 0,
            fail_paused_read: true,
            pause: Mutex::new(Some((ready, paused))),
        },
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    let observe = sdk.observe_encrypted_link_recovery_marker(pair.bob.public_key.clone());
    tokio::pin!(observe);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), reached) =>
            reached.expect("authorization read should pause").unwrap(),
        result = &mut observe => panic!("observation did not pause: {result:?}"),
    }
    assert_eq!(public_reads.load(Ordering::SeqCst), 2);
    assert_eq!(storage.snapshot().unwrap(), initial);
    resume.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(30), observe)
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::RecoveryRequired { .. }));
    assert!(error
        .to_string()
        .contains("injected registry lookup failure"));
    assert_eq!(storage.snapshot().unwrap(), initial);
}

#[tokio::test]
async fn test_recovery_observation_rechecks_state_after_public_lookup() {
    let pair = linked_two_party().await;
    let initial = pair.alice.storage.snapshot().unwrap();
    let bob_key = pair
        .bob
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let authorization = pair
        .bob
        .sdk
        .paykit_noise_key_authorization(pair.alice.public_key.clone())
        .await
        .unwrap();
    let observed_marker = paykit_lib::EncryptedLinkRecoveryMarker::new(
        initial.linked_peers[&pair.bob.public_key]
            .remote_recovery_attempt_id
            .clone()
            .unwrap(),
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
    .unwrap();
    paykit_lib::publish_encrypted_link_recovery_marker(
        &pair.bob.access.session,
        &paykit_lib::derive_paykit_noise_secret_key(bob_key.as_bytes()),
        &pair.alice.public_key.to_public_key().unwrap(),
        authorization.noise_public_key(),
        &observed_marker,
    )
    .await
    .unwrap();
    for marker_present in [true, false] {
        if !marker_present {
            paykit_lib::remove_encrypted_link_recovery_marker(
                &pair.bob.access.session,
                &paykit_lib::derive_paykit_noise_secret_key(bob_key.as_bytes()),
                &pair.alice.public_key.to_public_key().unwrap(),
                authorization.noise_public_key(),
            )
            .await
            .unwrap();
        }
        for change in [
            "lease",
            "blocked",
            "recovery",
            "authorization",
            "local-key",
            "cancel",
        ] {
            let storage = InMemoryStorage::from_state(initial.clone());
            let (ready, reached) = oneshot::channel();
            let (resume, paused) = oneshot::channel();
            let sdk = PaykitSdk::new(
                storage.clone(),
                PausedPublicReadProvider {
                    inner: TestnetSessionProvider::new(pair.alice.access.clone()),
                    public_reads: Arc::new(AtomicUsize::new(0)),
                    pause_after: 1,
                    fail_paused_read: false,
                    pause: Mutex::new(Some((ready, paused))),
                },
                pair.alice.adapter.clone(),
                PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
            );
            let mut observe =
                Box::pin(sdk.observe_encrypted_link_recovery_marker(pair.bob.public_key.clone()));
            tokio::select! {
                reached = tokio::time::timeout(Duration::from_secs(30), reached) =>
                    reached.expect("observation should reach public lookup").unwrap(),
                result = &mut observe => panic!("observation did not pause: {result:?}"),
            }
            assert_eq!(storage.snapshot().unwrap(), initial);
            if change == "cancel" {
                drop(observe);
                assert_eq!(storage.snapshot().unwrap(), initial);
                continue;
            }
            storage
                .transaction(|tx| {
                    let mut peer = tx.linked_peer(&pair.bob.public_key).unwrap();
                    match change {
                        "lease" => {
                            let now = chrono::Utc::now();
                            tx.claim_peer_link_operation(
                                &pair.bob.public_key,
                                now,
                                now + chrono::Duration::minutes(1),
                            )?
                            .unwrap();
                        }
                        "blocked" => peer.state = LinkedPeerState::Blocked,
                        "recovery" => {
                            peer.state = LinkedPeerState::RecoveryRequired;
                            let mut state = tx.encrypted_link_state(&pair.bob.public_key).unwrap();
                            state.link_snapshot = None;
                            state.generation += 1;
                            tx.save_encrypted_link_state(state);
                        }
                        "authorization" => {
                            let identity = pair.bob.access.local_secret_key.as_ref().unwrap();
                            let key = identity.derive_paykit_identity_secret_key(2).unwrap();
                            peer.noise_key_authorization = Some(
                                paykit_lib::PaykitNoiseKeyAuthorization::sign(
                                    &pubky::Keypair::from_secret(identity.as_bytes()),
                                    &paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
                                    key.key_generation(),
                                )
                                .unwrap(),
                            );
                        }
                        "local-key" => tx.save_paykit_noise_public_key(
                            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
                        ),
                        _ => unreachable!(),
                    }
                    tx.save_linked_peer(peer);
                    Ok(())
                })
                .await
                .unwrap();
            let changed = storage.snapshot().unwrap();
            resume.send(()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(30), observe)
                .await
                .unwrap();
            match change {
                "recovery" => {
                    let report = result.unwrap();
                    assert_eq!(report.state, LinkedPeerState::RecoveryRequired);
                    assert!(!report.remote_marker_changed);
                }
                "lease" => assert!(result.unwrap_err().is_concurrent_update()),
                "local-key" => assert!(matches!(result, Err(PaykitSdkError::Identity { .. }))),
                _ => assert!(result.is_err(), "unexpected {change} result: {result:?}"),
            }
            let after = storage.snapshot().unwrap();
            assert_eq!(after.identity_state, changed.identity_state, "{change}");
            assert_eq!(
                after.paykit_noise_public_key, changed.paykit_noise_public_key,
                "{change}"
            );
            assert_eq!(after.linked_peers, changed.linked_peers, "{change}");
            assert_eq!(
                after.encrypted_link_states, changed.encrypted_link_states,
                "{change}"
            );
            assert_eq!(
                after.peer_link_operation_leases, changed.peer_link_operation_leases,
                "{change}"
            );
        }
    }
}

#[tokio::test]
async fn test_private_resolution_rechecks_checkpoint_before_app_authorization() {
    let pair = linked_two_party().await;
    pair.bob
        .sdk
        .enqueue_private_payment_list_with_receiving_details(
            pair.alice.public_key.clone(),
            vec![private_receiving_detail(
                "btc-lightning-bolt11",
                "ln-private-bob",
            )],
        )
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let mut initial = pair.alice.storage.snapshot().unwrap();
    let authorized_apps = initial.authorized_paykit_apps[&pair.bob.public_key].clone();
    initial.authorized_paykit_apps.remove(&pair.bob.public_key);

    for change in [
        "unchanged",
        "registry-error",
        "lease",
        "recovery",
        "snapshot",
        "cancel",
    ] {
        let storage = InMemoryStorage::from_state(initial.clone());
        let (ready, reached) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let sdk = PaykitSdk::new(
            storage.clone(),
            PausedPublicReadProvider {
                inner: TestnetSessionProvider::new(pair.alice.access.clone()),
                public_reads: Arc::new(AtomicUsize::new(0)),
                // Noise authorization and the final recovery marker precede the registry.
                pause_after: 2,
                fail_paused_read: !matches!(change, "unchanged" | "cancel"),
                pause: Mutex::new(Some((ready, paused))),
            },
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let mut resolve =
            Box::pin(sdk.resolve_private_contact_payment(pair.bob.public_key.clone(), None, None));
        tokio::select! {
            reached = tokio::time::timeout(Duration::from_secs(30), reached) =>
                reached.expect("resolution should reach registry lookup").unwrap(),
            result = &mut resolve => panic!("resolution did not pause: {result:?}"),
        }
        assert_eq!(storage.snapshot().unwrap(), initial);
        if change == "cancel" {
            drop(resolve);
            assert_eq!(storage.snapshot().unwrap(), initial);
            continue;
        }
        storage
            .transaction(|tx| {
                let mut peer = tx.linked_peer(&pair.bob.public_key).unwrap();
                match change {
                    "lease" => {
                        let now = chrono::Utc::now();
                        tx.claim_peer_link_operation(
                            &pair.bob.public_key,
                            now,
                            now + chrono::Duration::minutes(1),
                        )?
                        .unwrap();
                    }
                    "recovery" => peer.state = LinkedPeerState::RecoveryRequired,
                    "snapshot" => {
                        let mut link = tx.encrypted_link_state(&pair.bob.public_key).unwrap();
                        link.generation += 1;
                        tx.save_encrypted_link_state(link);
                    }
                    "unchanged" | "registry-error" => {}
                    _ => unreachable!(),
                }
                tx.save_linked_peer(peer);
                Ok(())
            })
            .await
            .unwrap();
        let mut expected = storage.snapshot().unwrap();
        resume.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), resolve)
            .await
            .unwrap();
        match change {
            "unchanged" | "snapshot" | "recovery" => {
                let resolution = result.unwrap();
                if change == "recovery" {
                    assert_eq!(
                        resolution.state,
                        paykit_sdk::PrivatePaymentResolutionState::RecoveryPending
                    );
                    assert!(resolution.payable_endpoints.is_empty());
                } else {
                    assert_eq!(resolution.payable_endpoints.len(), 1);
                    assert_eq!(
                        resolution.payable_endpoints[0].endpoint.payload,
                        "ln-private-bob"
                    );
                }
                expected
                    .authorized_paykit_apps
                    .insert(pair.bob.public_key.clone(), authorized_apps.clone());
            }
            _ => {
                let error = result.unwrap_err();
                assert_eq!(
                    error
                        .to_string()
                        .contains("injected registry lookup failure"),
                    change == "registry-error",
                    "{change}: {error:?}"
                );
                if change == "lease" {
                    assert!(error.is_concurrent_update());
                }
            }
        }
        let after = storage.snapshot().unwrap();
        // Recovery fallback can allocate and release a lease without changing live state.
        expected.next_peer_link_operation_lease_id = after.next_peer_link_operation_lease_id;
        assert_eq!(after, expected, "{change}");
    }
}

#[tokio::test]
async fn test_contact_preparation_rechecks_local_inputs_before_resolution() {
    use paykit_sdk::{OutboundPrivateMessageStatus, PrivatePaymentEndpointReservation};

    #[derive(Clone)]
    struct PreparationClock(std::sync::Arc<Mutex<Option<chrono::DateTime<chrono::Utc>>>>);
    impl paykit_sdk::Clock for PreparationClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0.lock().unwrap().unwrap_or_else(chrono::Utc::now)
        }
    }

    let pair = linked_two_party().await;
    pair.bob
        .sdk
        .enqueue_private_payment_list_with_receiving_details(
            pair.alice.public_key.clone(),
            vec![private_receiving_detail(
                "btc-lightning-bolt11",
                "ln-private-bob",
            )],
        )
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let version = pair
        .alice
        .sdk
        .resolve_private_contact_payment(pair.bob.public_key.clone(), None, None)
        .await
        .unwrap()
        .private_payment_list_version;
    let mut initial = pair.alice.storage.snapshot().unwrap();
    let authorized_apps = initial
        .authorized_paykit_apps
        .remove(&pair.bob.public_key)
        .unwrap();

    for change in [
        "unchanged",
        "consumed",
        "registry-error",
        "lease",
        "blocked",
        "local-key",
        "snapshot",
        "queued",
        "sent-due",
        "cleanup",
        "cancel",
        "incoming",
    ] {
        let storage = InMemoryStorage::from_state(initial.clone());
        let other = PaykitSdk::new(
            storage.clone(),
            TestnetSessionProvider::new(pair.alice.access.clone()),
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let mut queued_id = None;
        let mut started_at = chrono::Utc::now();
        if change == "sent-due" {
            other
                .propose_payment_request(
                    pair.bob.public_key.clone(),
                    PaymentRequestTerms::builder(
                        PaymentAmount::new("0.001", "btc").unwrap(),
                        PaymentReference::new("contact-payment").unwrap(),
                        vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
                    )
                    .build()
                    .unwrap(),
                )
                .await
                .unwrap();
            let sent = other
                .process_outbound_private_messages(pair.bob.public_key.clone())
                .await
                .unwrap();
            assert_eq!(sent.sent.len(), 1);
            queued_id = Some(sent.sent[0]);
            let state = storage.snapshot().unwrap();
            let message = state
                .outbound_private_messages
                .iter()
                .find(|message| Some(message.outbound_message_id) == queued_id)
                .unwrap();
            assert_eq!(message.status, OutboundPrivateMessageStatus::Sent);
            assert!(message.confirmed_at.is_none());
            assert!(message.prepared_send.is_none());
            started_at = message.last_attempt_at.unwrap();
        }
        let now = std::sync::Arc::new(Mutex::new((change == "sent-due").then_some(started_at)));
        let before = storage.snapshot().unwrap();
        let (ready, reached) = oneshot::channel();
        let (resume, paused) = oneshot::channel();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            PausedPublicReadProvider {
                inner: TestnetSessionProvider::new(pair.alice.access.clone()),
                public_reads: Arc::new(AtomicUsize::new(0)),
                // Initial recovery and the inbox probe precede the joined final reads.
                pause_after: 5,
                fail_paused_read: !matches!(
                    change,
                    "unchanged" | "consumed" | "sent-due" | "cancel"
                ),
                pause: Mutex::new(Some((ready, paused))),
            },
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
            PreparationClock(now.clone()),
        );
        let mut prepare = Box::pin(sdk.prepare_and_resolve_private_contact_payment(
            pair.bob.public_key.clone(),
            None,
            if change == "consumed" { version } else { None },
            1,
        ));
        tokio::select! {
            reached = tokio::time::timeout(Duration::from_secs(30), reached) =>
                reached.expect("preparation should reach registry lookup").unwrap(),
            result = &mut prepare => panic!("preparation did not pause: {change}: {result:?}"),
        }
        assert_eq!(storage.snapshot().unwrap(), before, "{change}");
        if change == "cancel" {
            drop(prepare);
            assert_eq!(storage.snapshot().unwrap(), initial);
            continue;
        }
        match change {
            "sent-due" => {
                *now.lock().unwrap() = Some(started_at + chrono::Duration::seconds(30));
                assert_eq!(storage.snapshot().unwrap(), before);
            }
            "queued" => {
                queued_id = Some(
                    other
                        .clear_private_payment_list(pair.bob.public_key.clone())
                        .await
                        .unwrap()
                        .outbound_message_id,
                );
            }
            "cleanup" => {
                let mut record = other
                    .enqueue_private_payment_list_with_reservations(
                        pair.bob.public_key.clone(),
                        vec![PrivatePaymentEndpointReservation {
                            reservation_id: "obsolete-invoice".into(),
                            receiving_detail: private_receiving_detail(
                                "btc-lightning-bolt11",
                                "ln-obsolete",
                            ),
                            expires_at: None,
                            attribution: Default::default(),
                        }],
                    )
                    .await
                    .unwrap();
                record.status = OutboundPrivateMessageStatus::Invalid;
                storage
                    .transaction(|tx| tx.save_outbound_private_message(record))
                    .await
                    .unwrap();
            }
            "incoming" => {
                pair.bob
                    .sdk
                    .enqueue_private_payment_list_with_receiving_details(
                        pair.alice.public_key.clone(),
                        vec![private_receiving_detail(
                            "btc-lightning-bolt11",
                            "ln-current-bob",
                        )],
                    )
                    .await
                    .unwrap();
                pair.bob
                    .sdk
                    .process_outbound_private_messages(pair.alice.public_key.clone())
                    .await
                    .unwrap();
                let received = other
                    .receive_private_messages(pair.bob.public_key.clone())
                    .await
                    .unwrap();
                assert_eq!(received.stream_item_ids.len(), 1);
            }
            _ => storage
                .transaction(|tx| {
                    let mut peer = tx.linked_peer(&pair.bob.public_key).unwrap();
                    match change {
                        "lease" => {
                            let now = chrono::Utc::now();
                            tx.claim_peer_link_operation(
                                &pair.bob.public_key,
                                now,
                                now + chrono::Duration::minutes(1),
                            )?
                            .unwrap();
                        }
                        "blocked" => peer.state = LinkedPeerState::Blocked,
                        "local-key" => tx.save_paykit_noise_public_key(
                            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
                        ),
                        "snapshot" => {
                            let mut link = tx.encrypted_link_state(&pair.bob.public_key).unwrap();
                            link.generation += 1;
                            tx.save_encrypted_link_state(link);
                        }
                        _ => {}
                    }
                    tx.save_linked_peer(peer);
                    Ok(())
                })
                .await
                .unwrap(),
        }
        let mut expected = storage.snapshot().unwrap();
        resume.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(30), prepare)
            .await
            .unwrap();
        if matches!(change, "registry-error" | "lease" | "blocked" | "local-key") {
            let error = result.unwrap_err();
            assert_eq!(
                error
                    .to_string()
                    .contains("injected registry lookup failure"),
                change == "registry-error",
                "{change}: {error:?}"
            );
            match change {
                "lease" => assert!(error.is_concurrent_update()),
                "blocked" => assert!(matches!(error, PaykitSdkError::Policy { .. })),
                "local-key" => assert!(matches!(error, PaykitSdkError::Identity { .. })),
                _ => {}
            }
        } else {
            let prepared = result.unwrap();
            assert_eq!(prepared.link_report.unwrap().state, LinkedPeerState::Linked);
            assert!(prepared.receive_report.unwrap().stream_item_ids.is_empty());
            let sent = prepared.outbound_report.unwrap();
            if let Some(queued_id) = queued_id {
                assert_eq!(sent.sent, vec![queued_id]);
            } else {
                assert!(sent.attempted.is_empty());
            }
            assert!(sent.reservation_cleanup_failures.is_empty());
            if change == "consumed" {
                assert_eq!(
                    prepared.resolution.status,
                    paykit_sdk::PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList
                );
                assert!(prepared.resolution.payable_endpoints.is_empty());
            } else {
                assert_eq!(prepared.resolution.payable_endpoints.len(), 1);
                assert_eq!(
                    prepared.resolution.payable_endpoints[0].endpoint.payload,
                    if change == "incoming" {
                        "ln-current-bob"
                    } else {
                        "ln-private-bob"
                    }
                );
            }
            expected
                .authorized_paykit_apps
                .insert(pair.bob.public_key.clone(), authorized_apps.clone());
        }
        let after = storage.snapshot().unwrap();
        if matches!(change, "queued" | "sent-due") {
            let message = after
                .outbound_private_messages
                .iter()
                .find(|message| Some(message.outbound_message_id) == queued_id)
                .unwrap();
            assert_eq!(message.status, OutboundPrivateMessageStatus::Sent);
            assert!(message.prepared_send.is_none());
            assert!(after.peer_link_operation_leases.is_empty());
            if change == "sent-due" {
                let original = before
                    .outbound_private_messages
                    .iter()
                    .find(|original| original.outbound_message_id == message.outbound_message_id)
                    .unwrap();
                assert_eq!(message.raw_json, original.raw_json);
                assert_eq!(message.attempt_count, original.attempt_count + 1);
                assert_eq!(message.last_attempt_at, *now.lock().unwrap());
                assert!(message.confirmed_at.is_none());
            }
        } else {
            if change == "cleanup" {
                assert!(after.payment_endpoint_reservations.is_empty());
                expected.payment_endpoint_reservations.clear();
            }
            expected.next_peer_link_operation_lease_id = after.next_peer_link_operation_lease_id;
            assert_eq!(after, expected, "{change}");
        }
    }
}

#[tokio::test]
async fn test_published_events_survive_relink_and_lost_confirmations() {
    let mut pair = linked_two_party().await;
    let request = pair
        .alice
        .sdk
        .propose_payment_request(
            pair.bob.public_key.clone(),
            PaymentRequestTerms::builder(
                PaymentAmount::new("0.001", "btc").unwrap(),
                PaymentReference::new("reliable-delivery").unwrap(),
                vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
            )
            .build()
            .unwrap(),
        )
        .await
        .unwrap();
    let request_id = PaymentRequestId::new(request.payment_request_id).unwrap();
    pair.alice
        .sdk
        .cancel_payment_request(pair.bob.public_key.clone(), &request_id, None)
        .await
        .unwrap();
    let published = pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert_eq!(published.sent.len(), 2);
    assert!(pair
        .bob
        .sdk
        .payment_requests_with(&pair.alice.public_key)
        .await
        .unwrap()
        .is_empty());
    let original = pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .outbound_private_messages;

    for round in 0..2 {
        // Recover before reading events, then again before reading their confirmations.
        pair.alice
            .sdk
            .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
            .await
            .unwrap();
        let observed = pair
            .bob
            .sdk
            .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
            .await
            .unwrap();
        assert_eq!(observed.state, LinkedPeerState::RecoveryRequired);
        pair.alice
            .sdk
            .initiate_link_with_peer(pair.bob.public_key.clone())
            .await
            .unwrap();
        pair.bob
            .sdk
            .accept_link_with_peer(pair.alice.public_key.clone())
            .await
            .unwrap();
        drive_link_to_linked(&pair.alice, &pair.bob).await;

        let replay = pair
            .alice
            .sdk
            .process_outbound_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(
            replay.sent, published.sent,
            "unconfirmed events retain their original order and IDs"
        );
        let received = pair
            .bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap();
        assert_eq!(received.stream_item_ids.len(), 2);
        assert!(received.event_conflicts.is_empty());
        let requests = pair
            .bob
            .sdk
            .payment_requests_with(&pair.alice.public_key)
            .await
            .unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].state, PaymentRequestLifecycleState::Canceled);
        assert_eq!(requests[0].last_stream_item_id, Some(1));

        // Restart after intake but before sending confirmations, through the actual state codec.
        let state = pair.bob.storage.snapshot().unwrap();
        let bytes = paykit_sdk::storage::encode_storage_state_blob(&state).unwrap();
        pair.bob.storage = InMemoryStorage::from_state(
            paykit_sdk::storage::decode_storage_state_blob(&bytes).unwrap(),
        );
        pair.bob.sdk = PaykitSdk::new(
            pair.bob.storage.clone(),
            TestnetSessionProvider::new(pair.bob.access.clone()),
            pair.bob.adapter.clone(),
            PaykitSdkConfig::new(pair.bob.app_id.clone()).unwrap(),
        );
        let confirmations = pair
            .bob
            .sdk
            .process_outbound_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap();
        assert_eq!(confirmations.sent.len(), 2);
        assert!(confirmations.failed.is_empty());
        if round == 0 {
            assert!(pair
                .alice
                .storage
                .snapshot()
                .unwrap()
                .outbound_private_messages
                .iter()
                .all(|event| event.confirmed_at.is_none()));
        }
    }
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let final_state = pair.alice.storage.snapshot().unwrap();
    assert_eq!(final_state.outbound_private_messages.len(), 2);
    for (event, original) in final_state.outbound_private_messages.iter().zip(original) {
        assert!(event.confirmed_at.is_some());
        assert_eq!(event.outbound_message_id, original.outbound_message_id);
        assert_eq!(event.app_id, original.app_id);
        assert_eq!(event.raw_json, original.raw_json);
    }
    assert!(pair
        .alice
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap()
        .attempted
        .is_empty());
    assert!(pair
        .bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap()
        .attempted
        .is_empty());
    let receiver_state = pair.bob.storage.snapshot().unwrap();
    assert_eq!(receiver_state.event_dedup_records.len(), 2);
    assert!(receiver_state
        .event_dedup_records
        .values()
        .all(|event| event.duplicate_stream_item_ids.len() == 1));
}

#[tokio::test]
async fn test_handshake_rechecks_marker_after_advancement() {
    use paykit_sdk::{PaykitSdk, PaykitSdkConfig, PubkySessionAccess, PubkySessionProvider};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecoverDuringAdvance<'a> {
        local: &'a crate::harness::TestUser,
        remote: &'a crate::harness::TestUser,
        reads: AtomicUsize,
        observation: &'static str,
    }

    #[async_trait::async_trait]
    impl PubkySessionProvider for RecoverDuringAdvance<'_> {
        async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
            Ok(Some(self.local.access.clone()))
        }

        async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
            // The probe and initial marker lookup precede advancement.
            // Publish before the marker recheck, before the completed link is saved.
            if self.reads.fetch_add(1, Ordering::SeqCst) == 2 {
                if self.observation == "conflicting_key" {
                    let conflicting = paykit_lib::PaykitNoiseKeyAuthorization::sign(
                        &pubky::Keypair::from_secret(
                            self.remote
                                .access
                                .local_secret_key
                                .as_ref()
                                .unwrap()
                                .as_bytes(),
                        ),
                        &[11; 32],
                        1,
                    )
                    .unwrap();
                    self.remote
                        .access
                        .session
                        .storage()
                        .put(
                            paykit_lib::PAYKIT_NOISE_KEY_AUTHORIZATION_PATH,
                            serde_json::to_vec(&conflicting).unwrap(),
                        )
                        .await
                        .unwrap();
                } else if self.observation == "invalid_marker" {
                    let noise_key = |user: &crate::harness::TestUser| {
                        paykit_lib::derive_paykit_noise_secret_key(
                            user.access
                                .local_secret_key
                                .as_ref()
                                .unwrap()
                                .derive_paykit_identity_secret_key(1)
                                .unwrap()
                                .as_bytes(),
                        )
                    };
                    let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
                        &noise_key(self.remote),
                        self.remote.access.session.info().public_key(),
                        self.local.access.session.info().public_key(),
                        &pubky::Keypair::from_secret(&noise_key(self.local)).public_key(),
                    );
                    self.remote
                        .access
                        .session
                        .storage()
                        .put(&path, b"invalid marker".to_vec())
                        .await
                        .unwrap();
                } else {
                    self.remote
                        .sdk
                        .publish_encrypted_link_recovery_marker(self.local.public_key.clone())
                        .await?;
                }
            }
            Ok(Some(self.local.access.outbox_client.public_storage()))
        }

        async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
            unreachable!("this test does not sign out")
        }
    }

    for observation in ["changed_marker", "invalid_marker", "conflicting_key"] {
        let pair = two_party().await;
        pair.alice
            .sdk
            .initiate_link_with_peer(pair.bob.public_key.clone())
            .await
            .unwrap();
        pair.bob
            .sdk
            .accept_link_with_peer(pair.alice.public_key.clone())
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "initiator did not complete");
            if pair
                .alice
                .sdk
                .advance_link_handshake(pair.bob.public_key.clone())
                .await
                .unwrap()
                .state
                == LinkedPeerState::Linked
            {
                break;
            }
            assert_eq!(
                pair.bob
                    .sdk
                    .advance_link_handshake(pair.alice.public_key.clone())
                    .await
                    .unwrap()
                    .state,
                LinkedPeerState::Linking
            );
        }

        let sdk = PaykitSdk::new(
            pair.bob.storage.clone(),
            RecoverDuringAdvance {
                local: &pair.bob,
                remote: &pair.alice,
                reads: AtomicUsize::new(0),
                observation,
            },
            pair.bob.adapter.clone(),
            PaykitSdkConfig::new(pair.bob.app_id.clone()).unwrap(),
        );
        let before = pair.bob.storage.snapshot().unwrap();
        let result = sdk
            .advance_link_handshake(pair.alice.public_key.clone())
            .await;
        if observation != "changed_marker" {
            assert!(
                matches!(result, Err(PaykitSdkError::LinkObservation { .. })),
                "{observation}: {result:?}"
            );
            let after = pair.bob.storage.snapshot().unwrap();
            let state = &after.encrypted_link_states[&pair.alice.public_key];
            assert_eq!(
                state.generation,
                before.encrypted_link_states[&pair.alice.public_key].generation
            );
            assert!(state.link_snapshot.is_none());
            assert!(state.handshake_snapshot.is_some());
            assert_eq!(
                after.linked_peers[&pair.alice.public_key].state,
                LinkedPeerState::Linking
            );
            assert!(after.peer_link_operation_leases.is_empty());
            continue;
        }
        assert!(
            matches!(result, Err(PaykitSdkError::RecoveryRequired { .. })),
            "{result:?}"
        );
        let bob = pair.bob.storage.snapshot().unwrap();
        let alice = pair.alice.storage.snapshot().unwrap();
        assert_eq!(
            bob.linked_peers[&pair.alice.public_key].state,
            LinkedPeerState::RecoveryRequired
        );
        assert_eq!(
            bob.linked_peers[&pair.alice.public_key].remote_recovery_attempt_id,
            alice.linked_peers[&pair.bob.public_key].local_recovery_attempt_id
        );
        assert!(bob.encrypted_link_states[&pair.alice.public_key]
            .link_snapshot
            .is_none());
        assert!(bob.peer_link_operation_leases.is_empty());
        crate::harness::drive_recovery_to_linked(&pair.alice, &pair.bob).await;
    }
}

#[tokio::test]
async fn test_private_send_and_receive_observe_remote_recovery_before_using_link() {
    let pair = linked_two_party().await;
    for receive in [false, true] {
        pair.alice
            .sdk
            .clear_private_payment_list(pair.bob.public_key.clone())
            .await
            .unwrap();
        let before = pair.alice.storage.snapshot().unwrap();
        let marker = pair
            .bob
            .sdk
            .publish_encrypted_link_recovery_marker(pair.alice.public_key.clone())
            .await
            .unwrap();

        let result = if receive {
            pair.alice
                .sdk
                .receive_private_messages(pair.bob.public_key.clone())
                .await
                .map(|_| ())
        } else {
            pair.alice
                .sdk
                .process_outbound_private_messages(pair.bob.public_key.clone())
                .await
                .map(|_| ())
        };
        assert!(
            matches!(result, Err(PaykitSdkError::RecoveryRequired { .. })),
            "receive={receive}: {result:?}"
        );
        let after = pair.alice.storage.snapshot().unwrap();
        let peer = &after.linked_peers[&pair.bob.public_key];
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(peer.remote_recovery_attempt_id, marker.local_attempt_id);
        assert_eq!(
            peer.local_recovery_attempt_id,
            before.linked_peers[&pair.bob.public_key].local_recovery_attempt_id
        );
        assert!(after.encrypted_link_states[&pair.bob.public_key]
            .link_snapshot
            .is_none());
        assert!(after.peer_link_operation_leases.is_empty());

        crate::harness::drive_recovery_to_linked(&pair.alice, &pair.bob).await;
        let sent = pair
            .alice
            .sdk
            .process_outbound_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap();
        assert_eq!(sent.sent.len(), 1);
        assert!(sent.failed.is_empty());
        pair.bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn test_recovery_marker_publish_observe_remove_roundtrip() {
    let pair = linked_two_party().await;
    let sent = pair
        .alice
        .sdk
        .clear_private_payment_list_and_process_outbound(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert_eq!(sent.cleared.len(), 1);
    assert!(sent.failed_to_deliver.is_empty());
    pair.bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();

    let published = pair
        .alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("publishing the recovery marker should succeed");
    assert_eq!(published.state, LinkedPeerState::RecoveryRequired);
    assert!(published.local_marker_last_error.is_none());
    let attempt_id = published
        .local_attempt_id
        .clone()
        .expect("a local recovery attempt id should be recorded");

    // Recovery fails closed: private automation is blocked until the link is
    // re-established.
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
        .expect_err("private automation must be blocked during recovery");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );

    // The counterparty observes the marker through public storage.
    let observed = pair
        .bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .expect("observing the recovery marker should succeed");
    assert!(observed.remote_marker_changed);
    assert_eq!(
        observed.remote_attempt_id.as_deref(),
        Some(attempt_id.as_str())
    );
    assert_eq!(observed.state, LinkedPeerState::RecoveryRequired);

    let repeated = pair
        .bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .unwrap();
    assert!(!repeated.remote_marker_changed);

    // Direct fetch through unauthenticated storage proves the marker file is
    // on the homeserver before removal. This also validates the fetch
    // arguments themselves, so the post-removal `None` below is meaningful.
    let storage = pair.bob.access.outbox_client.public_storage();
    let bob_paykit_identity_secret_key = pair
        .bob
        .access
        .local_secret_key
        .as_ref()
        .expect("bob's session should retain a local secret key")
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .expect("initial Bob Paykit key derivation should succeed");
    let bob_noise_secret_key =
        paykit_lib::derive_paykit_noise_secret_key(bob_paykit_identity_secret_key.as_bytes());
    let alice_public_key = pair
        .alice
        .public_key
        .to_public_key()
        .expect("public key conversion should succeed");
    let alice_paykit_identity_secret_key = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .expect("alice's session should retain a local secret key")
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .expect("initial Alice Paykit key derivation should succeed");
    let alice_noise_public_key =
        paykit_lib::derive_paykit_noise_public_key(alice_paykit_identity_secret_key.as_bytes());
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        &bob_noise_secret_key,
        pair.bob.access.session.info().public_key(),
        &alice_public_key,
        &alice_noise_public_key,
    )
    .await
    .expect("direct marker fetch should succeed")
    .expect("the published marker should be present on the homeserver");
    assert_eq!(marker.attempt_id(), attempt_id.as_str());

    assert!(matches!(
        pair.alice
            .sdk
            .remove_encrypted_link_recovery_marker(pair.bob.public_key.clone())
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));

    pair.alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();
    drive_link_to_linked(&pair.alice, &pair.bob).await;
    let republished = pair
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
    assert_eq!(republished.cleared.len(), 1);
    assert!(republished.failed_to_deliver.is_empty());
    assert_ne!(
        republished.cleared[0].outbound_message_id,
        sent.cleared[0].outbound_message_id
    );
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        &bob_noise_secret_key,
        pair.bob.access.session.info().public_key(),
        &alice_public_key,
        &alice_noise_public_key,
    )
    .await
    .unwrap()
    .expect("the current recovery marker remains after relinking");
    assert_eq!(marker.attempt_id(), attempt_id);
    pair.alice
        .sdk
        .block_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .remove_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert!(paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        &bob_noise_secret_key,
        pair.bob.access.session.info().public_key(),
        &alice_public_key,
        &alice_noise_public_key,
    )
    .await
    .unwrap()
    .is_none());
}

#[tokio::test]
async fn test_publish_recovery_marker_without_private_link_state_fails() {
    let pair = two_party().await;

    let err = pair
        .alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect_err("publishing a marker without private link state must fail");
    assert!(
        matches!(err, PaykitSdkError::Policy { .. }),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn test_marker_publication_rechecks_peer_lease_after_waiting_for_lock() {
    let pair = linked_two_party().await;
    let state = pair.alice.storage.snapshot().unwrap();
    let link = &state.encrypted_link_states[&pair.bob.public_key];
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(link.link_snapshot.as_ref().unwrap())
            .unwrap();
    let key = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
        &paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
        pair.alice.access.session.info().public_key(),
        &pair.bob.public_key.to_public_key().unwrap(),
        snapshot.remote_noise_public_key(),
    );
    let storage = pair.alice.access.session.storage();
    let before = storage.get(&path).await.unwrap().text().await.unwrap();
    let lock = storage.lock(&path, Duration::from_secs(60)).await.unwrap();
    let previous_attempt = state.linked_peers[&pair.bob.public_key]
        .local_recovery_attempt_id
        .clone();
    let publication = pair
        .alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone());
    let lose_peer_lease = async {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "recovery was not staged");
            let staged = pair.alice.storage.snapshot().unwrap();
            if staged.linked_peers[&pair.bob.public_key].local_recovery_attempt_id
                != previous_attempt
            {
                let lease = &staged.peer_link_operation_leases[&pair.bob.public_key];
                pair.alice
                    .storage
                    .transaction(|tx| {
                        tx.release_peer_link_operation(&lease.counterparty, lease.lease_id);
                        Ok(())
                    })
                    .await
                    .unwrap();
                storage.unlock(&lock).await.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let (result, ()) = tokio::join!(publication, lose_peer_lease);
    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert_eq!(
        storage.get(&path).await.unwrap().text().await.unwrap(),
        before
    );
}

#[tokio::test]
async fn test_recovery_during_handshake_isolates_late_old_writes() {
    let pair = linked_two_party().await;
    let old_state =
        paykit_sdk::load_encrypted_link_state(&pair.alice.storage, &pair.bob.public_key)
            .await
            .unwrap()
            .unwrap();
    let old_snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(&old_state.link_snapshot.unwrap()).unwrap();
    let old_context = old_snapshot.recovery_context().clone();
    let key = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let old_link = paykit_lib::restore_encrypted_link(
        pair.alice.access.session.clone(),
        paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
        &pair.bob.public_key.to_public_key().unwrap(),
        pair.alice.access.outbox_client.clone(),
        old_snapshot,
    )
    .await
    .unwrap();
    let old_path = format!("{}/0", old_link.config().write_path);

    pair.alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
        .await
        .unwrap();
    pair.bob
        .sdk
        .ensure_link_with_peer(pair.alice.public_key.clone(), 1)
        .await
        .unwrap();
    let new_marker = pair
        .bob
        .sdk
        .publish_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .unwrap();
    let alice = pair
        .alice
        .restart_with_storage(pair.alice.storage.clone())
        .await;
    crate::harness::drive_recovery_to_linked(&alice, &pair.bob).await;

    // A delayed write from the retired attempt cannot replace a current slot.
    alice
        .access
        .session
        .storage()
        .put(&old_path, "stale ciphertext")
        .await
        .unwrap();
    let state = paykit_sdk::load_encrypted_link_state(&alice.storage, &pair.bob.public_key)
        .await
        .unwrap()
        .unwrap();
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(&state.link_snapshot.unwrap()).unwrap();
    assert_ne!(snapshot.recovery_context(), &old_context);
    assert_eq!(
        snapshot.recovery_context().remote_attempt_id(),
        new_marker.local_attempt_id.as_deref()
    );
    let generation = state.generation;
    let linked = alice
        .sdk
        .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
        .await
        .unwrap();
    assert_eq!(linked.state, LinkedPeerState::Linked);
    assert_eq!(linked.generation, generation);
    let sent = alice
        .sdk
        .clear_private_payment_list_and_process_outbound(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert_eq!(sent.cleared.len(), 1);
    assert!(sent.failed_to_deliver.is_empty());
    let received = pair
        .bob
        .sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();
    assert_eq!(received.len(), 1);
    assert!(received[0].error.is_none());
    assert!(received[0].report.is_some());
}

#[tokio::test]
async fn test_mutual_recovery_markers_do_not_block_relink() {
    let pair = linked_two_party().await;

    pair.alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("alice should publish a recovery marker");
    pair.bob
        .sdk
        .publish_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .expect("bob should publish a recovery marker");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alice_state = LinkedPeerState::RecoveryRequired;
    let mut bob_state = LinkedPeerState::RecoveryRequired;
    while alice_state != LinkedPeerState::Linked || bob_state != LinkedPeerState::Linked {
        assert!(Instant::now() < deadline, "relink timed out");

        if alice_state != LinkedPeerState::Linked {
            alice_state = pair
                .alice
                .sdk
                .ensure_link_with_peer(pair.bob.public_key.clone(), 1)
                .await
                .expect("alice relink should advance")
                .state;
        }
        if bob_state != LinkedPeerState::Linked {
            bob_state = pair
                .bob
                .sdk
                .ensure_link_with_peer(pair.alice.public_key.clone(), 1)
                .await
                .expect("bob relink should advance")
                .state;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
