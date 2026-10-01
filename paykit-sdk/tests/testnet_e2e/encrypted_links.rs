use std::{
    any::Any,
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use async_trait::async_trait;
use paykit_sdk::{
    load_encrypted_link_state,
    storage::{StorageKeyRotationCallback, StorageTransactionCallback},
    EncryptedLinkHandshakeRole, InMemoryStorage, LinkedPeerState, PaykitIdentitySecretKey,
    PaykitSdk, PaykitSdkConfig, PaykitSdkError, PubkyPublicKey, PubkySessionAccess,
    PubkySessionProvider, Result, StorageAdapter,
};
use pubky_testnet::pubky::Keypair;
use tokio::sync::oneshot;

use crate::harness::{
    build_testnet, drive_link_to_linked, linked_two_party, two_party, TestUser,
    TestnetSessionProvider,
};

#[tokio::test]
async fn test_link_handshake_two_party_reaches_linked() {
    let pair = two_party().await;

    let initiated = pair
        .alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .expect("initiating the handshake should succeed");
    assert_eq!(initiated.state, LinkedPeerState::Linking);
    assert_eq!(
        initiated.handshake_role,
        Some(EncryptedLinkHandshakeRole::Initiator)
    );

    let accepted = pair
        .bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .expect("accepting the handshake should succeed");
    assert_eq!(accepted.state, LinkedPeerState::Linking);
    assert_eq!(
        accepted.handshake_role,
        Some(EncryptedLinkHandshakeRole::Responder)
    );

    drive_link_to_linked(&pair.alice, &pair.bob).await;

    let peers = pair
        .alice
        .sdk
        .linked_peers()
        .await
        .expect("loading linked peers should succeed");
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].state, LinkedPeerState::Linked);

    // The durable link state holds an active link snapshot and no leftover
    // handshake state.
    let link_state = load_encrypted_link_state(&pair.alice.storage, &pair.bob.public_key)
        .await
        .expect("loading link state should succeed")
        .expect("link state should exist after handshake completion");
    assert!(link_state.link_snapshot.is_some());
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
}

#[tokio::test]
async fn test_private_send_preserves_failure_when_recording_contends() {
    use paykit_sdk::{storage::PreparedOutboundPrivateSend, OutboundPrivateMessageStatus};

    struct ContendedFailureStorage {
        inner: InMemoryStorage,
        rejected: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl StorageAdapter for ContendedFailureStorage {
        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn Any + Send>> {
            self.inner
                .transaction_erased(Box::new(|tx| {
                    let result = f(tx)?;
                    if tx
                        .export_storage_state()
                        .outbound_private_messages
                        .iter()
                        .any(|message| {
                            matches!(
                                message.status,
                                OutboundPrivateMessageStatus::Failed
                                    | OutboundPrivateMessageStatus::RecoveryRequired
                            )
                        })
                    {
                        self.rejected.fetch_add(1, Ordering::SeqCst);
                        return Err(PaykitSdkError::ConcurrentUpdate {
                            context: "failure-recording write contended".into(),
                            source: None,
                        });
                    }
                    Ok(result)
                }))
                .await
        }
    }

    let pair = linked_two_party().await;
    pair.alice
        .sdk
        .clear_private_payment_list(pair.bob.public_key.clone())
        .await
        .unwrap();
    let queued = pair.alice.storage.snapshot().unwrap();
    let provider = TestnetSessionProvider::with_session_secret(
        pair.alice.access.clone(),
        pair.alice.session_secret.clone(),
    );
    // Keep the cached access so publication reaches the revoked test grant.
    provider
        .revoke_session_access(&pair.alice.access)
        .await
        .unwrap();

    for malformed_packet in [false, true] {
        let mut state = queued.clone();
        if malformed_packet {
            state.outbound_private_messages[0].prepared_send = Some(PreparedOutboundPrivateSend {
                destination_path: "/not-this-link/0".into(),
                ciphertext: Vec::new(),
            });
        }
        let storage = InMemoryStorage::from_state(state);
        let rejected = Arc::new(AtomicUsize::new(0));
        let sdk = PaykitSdk::new(
            ContendedFailureStorage {
                inner: storage,
                rejected: rejected.clone(),
            },
            provider.clone(),
            pair.alice.adapter.clone(),
            PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
        );
        let error = sdk
            .process_outbound_private_messages(pair.bob.public_key.clone())
            .await
            .unwrap_err();

        assert!(
            rejected.load(Ordering::SeqCst) > 0,
            "failure recording must be reached"
        );
        assert!(
            !error.is_concurrent_update(),
            "send failure was hidden: {error:?}"
        );
        if malformed_packet {
            assert!(
                matches!(error, PaykitSdkError::Protocol { .. }),
                "{error:?}"
            );
        } else {
            assert!(
                matches!(error, PaykitSdkError::Transport { .. }),
                "{error:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_private_resolution_pending_retries_after_receive_contention() {
    use paykit_sdk::{PrivatePaymentResolutionState, PrivatePaymentResolutionStatus};

    struct CompetingReceiverStorage {
        inner: InMemoryStorage,
        counterparty: PubkyPublicKey,
        observations: AtomicUsize,
    }

    #[async_trait]
    impl StorageAdapter for CompetingReceiverStorage {
        async fn transaction_erased<'a>(
            &self,
            f: StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn Any + Send>> {
            self.inner
                .transaction_erased(Box::new(|tx| {
                    let before = tx.peer_link_operation_lease(&self.counterparty);
                    let result = f(tx)?;
                    if before.is_some()
                        && tx.peer_link_operation_lease(&self.counterparty).is_none()
                        && self.observations.fetch_add(1, Ordering::SeqCst) == 1
                    {
                        // Resolution observes recovery markers twice before receiving.
                        let now = chrono::Utc::now();
                        assert!(tx
                            .claim_peer_link_operation(
                                &self.counterparty,
                                now,
                                now + chrono::Duration::minutes(1),
                            )?
                            .is_some());
                    }
                    Ok(result)
                }))
                .await
        }
    }

    let pair = linked_two_party().await;
    pair.bob
        .adapter
        .set_private_details(vec![crate::harness::private_receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-bob",
        )]);
    pair.bob
        .sdk
        .enqueue_private_payment_list(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    let sdk = PaykitSdk::new(
        CompetingReceiverStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            observations: AtomicUsize::new(0),
        },
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    let pending = sdk
        .resolve_private_contact_payment(pair.bob.public_key.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(
        pending.state,
        PrivatePaymentResolutionState::RecoveryPending
    );
    assert!(pending.payable_endpoints.is_empty());
    pair.alice
        .storage
        .transaction(|tx| {
            let lease = tx.peer_link_operation_lease(&pair.bob.public_key).unwrap();
            assert_eq!(
                tx.linked_peer(&pair.bob.public_key).unwrap().state,
                LinkedPeerState::Linked
            );
            tx.release_peer_link_operation(&pair.bob.public_key, lease.lease_id);
            Ok(())
        })
        .await
        .unwrap();

    let retried = sdk
        .resolve_private_contact_payment(pair.bob.public_key.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(retried.status, PrivatePaymentResolutionStatus::Payable);
    assert_eq!(
        retried.payable_endpoints[0].target.payload,
        "ln-private-bob"
    );
}

#[tokio::test]
async fn test_advance_link_handshake_without_started_handshake_fails() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    let stranger = PubkyPublicKey::from_public_key(&Keypair::random().public_key());

    let err = user
        .sdk
        .advance_link_handshake(stranger)
        .await
        .expect_err("advancing without stored handshake state must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired { .. }),
        "unexpected error: {err:?}"
    );
}

enum LinkCheckpoint {
    Linked,
    PreparedSend,
}

struct PausedLinkCheckpointStorage {
    inner: InMemoryStorage,
    counterparty: PubkyPublicKey,
    pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    checkpoint: LinkCheckpoint,
}

#[async_trait]
impl StorageAdapter for PausedLinkCheckpointStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let result = self.inner.transaction_erased(f).await?;
        let state = self.inner.snapshot()?;
        let checkpoint_ready = match self.checkpoint {
            LinkCheckpoint::PreparedSend => state
                .outbound_private_messages
                .iter()
                .any(|message| message.prepared_send.is_some()),
            LinkCheckpoint::Linked => state
                .encrypted_link_states
                .get(&self.counterparty)
                .is_some_and(|state| state.link_snapshot.is_some()),
        };
        let pause = if checkpoint_ready {
            self.pause.lock().unwrap().take()
        } else {
            None
        };
        if let Some((ready, resume)) = pause {
            ready
                .send(())
                .expect("checkpoint observer should remain live");
            resume.await.expect("checkpoint should be released");
        }
        Ok(result)
    }
}

struct CountingSessionProvider {
    inner: TestnetSessionProvider,
    loads: Arc<AtomicUsize>,
    registry_unavailable: bool,
}

#[async_trait]
impl PubkySessionProvider for CountingSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.inner.load_session_access().await
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        if self.registry_unavailable {
            return Err(PaykitSdkError::Transport {
                context: "registry unavailable".into(),
                source: None,
            });
        }
        self.inner.load_public_storage().await
    }

    async fn clear_session_access(&self) -> Result<()> {
        self.inner.clear_session_access().await
    }
}

#[tokio::test]
async fn test_key_rotation_waits_for_handshake_checkpoint_without_nested_session_reads() {
    let pair = two_party().await;
    let (checkpoint_ready, checkpoint_reached) = oneshot::channel();
    let (resume_checkpoint, checkpoint_resume) = oneshot::channel();
    let session_loads = Arc::new(AtomicUsize::new(0));
    let sdk = PaykitSdk::new(
        PausedLinkCheckpointStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            pause: Mutex::new(Some((checkpoint_ready, checkpoint_resume))),
            checkpoint: LinkCheckpoint::Linked,
        },
        CountingSessionProvider {
            inner: TestnetSessionProvider::new(pair.alice.access.clone()),
            loads: Arc::clone(&session_loads),
            registry_unavailable: false,
        },
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    sdk.initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();

    let handshake = async {
        for _ in 0..8 {
            let report = sdk
                .advance_link_handshake(pair.bob.public_key.clone())
                .await
                .unwrap();
            if report.state == LinkedPeerState::Linked {
                return report;
            }
            pair.bob
                .sdk
                .advance_link_handshake(pair.alice.public_key.clone())
                .await
                .unwrap();
        }
        panic!("handshake should reach its linked-state checkpoint");
    };
    tokio::pin!(handshake);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), checkpoint_reached) => {
            reached.expect("handshake checkpoint timed out").unwrap();
        }
        _ = &mut handshake => panic!("handshake must wait for its checkpoint"),
    }

    let replacement = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(2)
        .unwrap();
    let before = pair.alice.storage.snapshot().unwrap();
    let competing_rotation = pair
        .alice
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await;
    assert!(matches!(
        competing_rotation,
        Err(PaykitSdkError::Policy { .. })
    ));
    assert_eq!(pair.alice.storage.snapshot().unwrap(), before);

    let loads_before_rotation = session_loads.load(Ordering::SeqCst);
    let rotation = sdk.rotate_paykit_identity_key(replacement);
    tokio::pin!(rotation);
    // Poll once to enqueue the writer before resuming handshake cleanup.
    assert!(matches!(
        rotation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert_eq!(
        session_loads.load(Ordering::SeqCst),
        loads_before_rotation,
        "rotation must wait before loading session keys"
    );
    resume_checkpoint.send(()).unwrap();
    let report = tokio::time::timeout(Duration::from_secs(10), &mut handshake)
        .await
        .expect("handshake cleanup must not reacquire a read behind the rotation writer");
    assert_eq!(report.state, LinkedPeerState::Linked);
    assert_eq!(session_loads.load(Ordering::SeqCst), loads_before_rotation);

    tokio::time::timeout(Duration::from_secs(30), rotation)
        .await
        .expect("rotation should finish after handshake cleanup")
        .unwrap();
    let state = pair.alice.storage.snapshot().unwrap();
    assert!(state.encrypted_link_states.is_empty());
    assert_eq!(
        state.linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::RecoveryRequired
    );
}

#[tokio::test]
async fn test_key_rotation_rejects_another_instances_prepared_send() {
    let pair = linked_two_party().await;
    pair.alice
        .sdk
        .clear_private_payment_list(pair.bob.public_key.clone())
        .await
        .unwrap();
    let (ready, reached) = oneshot::channel();
    let (resume, release) = oneshot::channel();
    let sdk = PaykitSdk::new(
        PausedLinkCheckpointStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            pause: Mutex::new(Some((ready, release))),
            checkpoint: LinkCheckpoint::PreparedSend,
        },
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    let send = sdk.process_outbound_private_messages(pair.bob.public_key.clone());
    tokio::pin!(send);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), reached) => reached.unwrap().unwrap(),
        _ = &mut send => panic!("send must pause after durable preparation"),
    }
    let before = pair.alice.storage.snapshot().unwrap();
    let replacement = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(2)
        .unwrap();
    assert!(matches!(
        pair.alice
            .sdk
            .rotate_paykit_identity_key(replacement.clone())
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));
    assert_eq!(pair.alice.storage.snapshot().unwrap(), before);
    resume.send(()).unwrap();
    let sent = tokio::time::timeout(Duration::from_secs(30), send)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent.sent.len(), 1);
    pair.alice
        .sdk
        .rotate_paykit_identity_key(replacement)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_malformed_handshake_packet_requires_recovery() {
    let pair = two_party().await;
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();
    let state = load_encrypted_link_state(&pair.bob.storage, &pair.alice.public_key)
        .await
        .unwrap()
        .unwrap();
    let key = pair
        .bob
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let handshake = paykit_lib::restore_encrypted_link_handshake(
        pair.bob.access.session.clone(),
        paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
        &pair.alice.public_key.to_public_key().unwrap(),
        pair.bob.access.outbox_client.clone(),
        paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(&state.handshake_snapshot.unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    pair.alice
        .access
        .session
        .storage()
        .put(format!("{}/0", handshake.config().read_path), vec![0u8])
        .await
        .unwrap();
    assert!(matches!(
        pair.bob
            .sdk
            .advance_link_handshake(pair.alice.public_key.clone())
            .await,
        Err(PaykitSdkError::Protocol { .. })
    ));
    let state = pair.bob.storage.snapshot().unwrap();
    let peer = &state.linked_peers[&pair.alice.public_key];
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert!(peer.local_recovery_attempt_id.is_some());
    assert!(state.encrypted_link_states[&pair.alice.public_key]
        .handshake_snapshot
        .is_none());
}

#[tokio::test]
async fn test_handshake_registry_transport_failure_preserves_checkpoint() {
    let pair = two_party().await;
    pair.alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    let before = pair.alice.storage.snapshot().unwrap();
    let sdk = PaykitSdk::new(
        pair.alice.storage.clone(),
        CountingSessionProvider {
            inner: TestnetSessionProvider::new(pair.alice.access.clone()),
            loads: Arc::new(AtomicUsize::new(0)),
            registry_unavailable: true,
        },
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    assert!(matches!(
        sdk.advance_link_handshake(pair.bob.public_key.clone())
            .await,
        Err(PaykitSdkError::Transport { .. })
    ));
    let after = pair.alice.storage.snapshot().unwrap();
    assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
    assert_eq!(after.linked_peers, before.linked_peers);
}

#[tokio::test]
async fn test_oversized_inbound_packet_requires_link_recovery() {
    let pair = linked_two_party().await;
    let state = load_encrypted_link_state(&pair.alice.storage, &pair.bob.public_key)
        .await
        .unwrap()
        .unwrap();
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(&state.link_snapshot.unwrap()).unwrap();
    let key = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let mut link = paykit_lib::restore_encrypted_link(
        pair.alice.access.session.clone(),
        paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
        &pair.bob.public_key.to_public_key().unwrap(),
        pair.alice.access.outbox_client.clone(),
        snapshot,
    )
    .await
    .unwrap();
    let prepared = link.prepare_private_application_message_json(
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#,
    ).unwrap();
    pair.alice
        .access
        .session
        .storage()
        .put(
            prepared.destination_path(),
            vec![0; pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN + 1],
        )
        .await
        .unwrap();

    assert!(matches!(
        pair.bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await,
        Err(PaykitSdkError::Protocol { .. })
    ));
    let received = pair.bob.storage.snapshot().unwrap();
    let peer = &received.linked_peers[&pair.alice.public_key];
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert!(peer.local_recovery_attempt_id.is_some());
    assert!(received.encrypted_link_states[&pair.alice.public_key]
        .link_snapshot
        .is_none());
    assert!(received.private_stream_items.is_empty());
    assert!(matches!(
        pair.bob
            .sdk
            .receive_private_messages(pair.alice.public_key.clone())
            .await,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
}

struct PausedRotationStorage {
    inner: InMemoryStorage,
    pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[async_trait]
impl StorageAdapter for PausedRotationStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        self.inner.transaction_erased(f).await
    }

    async fn rotate_paykit_identity_key_erased<'a>(
        &self,
        current: PaykitIdentitySecretKey,
        replacement: PaykitIdentitySecretKey,
        f: StorageKeyRotationCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let result = self
            .inner
            .rotate_paykit_identity_key_erased(current, replacement, f)
            .await?;
        let pause = self.pause.lock().unwrap().take();
        if let Some((ready, resume)) = pause {
            ready.send(()).unwrap();
            resume.await.unwrap();
        }
        Ok(result)
    }
}

#[tokio::test]
async fn test_key_rotation_retries_exact_replacement_after_registry_failure() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    let public_storage = user.access.outbox_client.public_storage();
    let registry = paykit_lib::get_paykit_app_registry(
        &public_storage,
        user.access.session.info().public_key(),
    )
    .await
    .unwrap()
    .unwrap();
    let (ready, reached) = oneshot::channel();
    let (resume, release) = oneshot::channel();
    let sdk = PaykitSdk::new(
        PausedRotationStorage {
            inner: user.storage.clone(),
            pause: Mutex::new(Some((ready, release))),
        },
        TestnetSessionProvider::new(user.access.clone()),
        user.adapter.clone(),
        PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
    );
    let replacement = user
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(2)
        .unwrap();
    let rotation = sdk.rotate_paykit_identity_key(replacement.clone());
    tokio::pin!(rotation);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), reached) => reached.unwrap().unwrap(),
        _ = &mut rotation => panic!("rotation must pause after shared state commit"),
    }
    user.access
        .session
        .storage()
        .delete(paykit_lib::PAYKIT_APP_REGISTRY_PATH)
        .await
        .unwrap();
    resume.send(()).unwrap();
    assert!(matches!(
        rotation.await,
        Err(PaykitSdkError::NotFound { .. })
    ));
    let committed = user.storage.snapshot().unwrap();
    assert_eq!(
        committed.paykit_noise_public_key,
        Some(PubkyPublicKey::from_public_key(
            &paykit_lib::derive_paykit_noise_public_key(replacement.as_bytes())
        ))
    );
    paykit_lib::create_paykit_app_registry(&user.access.session, &registry)
        .await
        .unwrap();
    assert!(sdk
        .rotate_paykit_identity_key(PaykitIdentitySecretKey::new([43; 32], 2).unwrap())
        .await
        .is_err());
    assert_eq!(user.storage.snapshot().unwrap(), committed);
    let rotated = sdk.rotate_paykit_identity_key(replacement).await.unwrap();
    assert_eq!(rotated.key_generation(), 2);
    assert_eq!(user.storage.snapshot().unwrap(), committed);
}
