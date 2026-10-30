use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use paykit_sdk::{
    load_encrypted_link_state, storage::StorageTransactionCallback, InMemoryStorage,
    LinkedPeerState, PaykitSdk, PaykitSdkConfig, PrivateIntakeConfig, PrivateIntakeTransportState,
    PubkyPublicKey, Result, StorageAdapter,
};

use crate::harness::{
    app_id, drive_recovery_to_linked, linked_two_party, two_party, TestUser,
    TestnetSessionProvider, TwoParty,
};

const RETAINED_PRIVATE_STREAM_ITEM_LIMIT: usize = 4096;

#[derive(Clone)]
struct RejectBufferedDrainCommit {
    inner: InMemoryStorage,
    reject: Arc<AtomicBool>,
}

#[async_trait]
impl StorageAdapter for RejectBufferedDrainCommit {
    async fn transaction_erased<'a>(
        &self,
        callback: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let reject = Arc::clone(&self.reject);
        self.inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state().private_stream_items.len();
                let result = callback(tx)?;
                if reject.load(Ordering::SeqCst)
                    && tx.export_storage_state().private_stream_items.len() > before
                {
                    return Err(paykit_sdk::PaykitSdkError::Storage {
                        context: "injected buffered drain commit failure".into(),
                        source: None,
                    });
                }
                Ok(result)
            }))
            .await
    }
}

#[derive(Clone)]
struct RejectSecondBufferedDrainCommit {
    inner: InMemoryStorage,
    attempted_drains: Arc<AtomicUsize>,
}

#[async_trait]
impl StorageAdapter for RejectSecondBufferedDrainCommit {
    async fn transaction_erased<'a>(
        &self,
        callback: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        let attempted_drains = Arc::clone(&self.attempted_drains);
        self.inner
            .transaction_erased(Box::new(move |tx| {
                let before = tx.export_storage_state().private_stream_items.len();
                let result = callback(tx)?;
                if tx.export_storage_state().private_stream_items.len() > before
                    && attempted_drains.fetch_add(1, Ordering::SeqCst) == 1
                {
                    return Err(paykit_sdk::PaykitSdkError::Storage {
                        context: "injected post-dequeue buffered receive failure".into(),
                        source: None,
                    });
                }
                Ok(result)
            }))
            .await
    }
}

async fn publish_private_lists(sender: &TestUser, receiver: &TestUser, count: usize) {
    for _ in 0..count {
        sender
            .sdk
            .clear_private_payment_list(receiver.public_key.clone())
            .await
            .unwrap();
        let report = sender
            .sdk
            .process_outbound_private_messages(receiver.public_key.clone())
            .await
            .unwrap();
        assert_eq!(report.sent.len(), 1);
    }
}

async fn wait_for_buffered(
    storage: &InMemoryStorage,
    peer: &paykit_sdk::PubkyPublicKey,
    count: usize,
) {
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let buffered = storage
                .snapshot()
                .unwrap()
                .private_intake_peers
                .get(peer)
                .map_or(0, |state| state.buffered_packets.len());
            if buffered >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if result.is_err() {
        let buffered = storage
            .snapshot()
            .unwrap()
            .private_intake_peers
            .get(peer)
            .map_or(0, |state| state.buffered_packets.len());
        panic!("private intake should durably buffer expected packets: expected {count}, got {buffered}");
    }
}

async fn advance_handshake_from_intake_after_restart(
    user: &TestUser,
    counterparty: &PubkyPublicKey,
) {
    let before = user.storage.snapshot().unwrap();
    let before_link = before.encrypted_link_states[counterparty].clone();
    let local = user.public_key.to_public_key().unwrap();
    let paykit_key = user
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    let noise_secret = paykit_lib::derive_paykit_noise_secret_key(paykit_key.as_bytes());
    let restarted = user.restart_with_storage(user.storage.clone()).await;
    let sdk = Arc::new(restarted.sdk);
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    let work = supervisor.subscribe_work();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            // Register before inspecting durable state so startup audit completion
            // cannot be lost between inspection and waiting. Pending-handshake work
            // wakes only after the bounded advancement attempt returns, including
            // a legitimate idle attempt waiting for the counterparty's next packet.
            let notified = work.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = user.storage.snapshot().unwrap();
            let peer = &state.linked_peers[counterparty];
            let link = &state.encrypted_link_states[counterparty];
            let publication_acknowledged = link.handshake_snapshot.as_ref().is_none_or(|bytes| {
                paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(bytes)
                    .unwrap()
                    .pending_publication(&local, &noise_secret)
                    .unwrap()
                    .is_none()
            });
            if peer.state != LinkedPeerState::Linking
                || (link != &before_link && publication_acknowledged)
            {
                break;
            }
            tokio::select! {
                () = notified => break,
                () = tokio::time::sleep(Duration::from_millis(20)) => {}
            }
        }
    })
    .await
    .expect("private intake should attempt pending handshake advancement");
    supervisor.stop().await;
}

async fn pending_handshake_write_path(user: &TestUser, counterparty: &PubkyPublicKey) -> String {
    let state = load_encrypted_link_state(&user.storage, counterparty)
        .await
        .unwrap()
        .unwrap();
    let paykit_key = user
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    paykit_lib::restore_encrypted_link_handshake(
        user.access.session.clone(),
        paykit_lib::derive_paykit_noise_secret_key(paykit_key.as_bytes()),
        &counterparty.to_public_key().unwrap(),
        user.access.outbox_client.clone(),
        paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(
            state.handshake_snapshot.as_ref().unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap()
    .config()
    .write_path
    .clone()
}

async fn publish_pending_handshake_packet_from_restarted_intake(
    user: &TestUser,
    counterparty: &PubkyPublicKey,
) -> String {
    for _ in 0..3 {
        advance_handshake_from_intake_after_restart(user, counterparty).await;
        let packet_path = format!(
            "{}/0",
            pending_handshake_write_path(user, counterparty).await
        );
        if user
            .access
            .outbox_client
            .public_storage()
            .exists(format!("pubky://{}{packet_path}", user.public_key))
            .await
            .unwrap()
        {
            return packet_path;
        }
    }
    panic!("restarted private intake did not publish pending handshake packet");
}

async fn drive_handshake_from_restarted_intake(alice: &TestUser, bob: &TestUser) {
    for _ in 0..8 {
        for (local, remote) in [(alice, bob), (bob, alice)] {
            if local.storage.snapshot().unwrap().linked_peers[&remote.public_key].state
                == LinkedPeerState::Linking
            {
                advance_handshake_from_intake_after_restart(local, &remote.public_key).await;
            }
        }
        let alice_linked = alice.storage.snapshot().unwrap().linked_peers[&bob.public_key].state
            == LinkedPeerState::Linked;
        let bob_linked = bob.storage.snapshot().unwrap().linked_peers[&alice.public_key].state
            == LinkedPeerState::Linked;
        if alice_linked && bob_linked {
            return;
        }
    }
    panic!("restarted private intake did not complete Encrypted Link Handshake");
}

#[tokio::test]
async fn test_pending_handshake_both_roles_advance_from_intake_across_restarts() {
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

    drive_handshake_from_restarted_intake(&pair.alice, &pair.bob).await;

    for (local, remote) in [(&pair.alice, &pair.bob), (&pair.bob, &pair.alice)] {
        let state = local.storage.snapshot().unwrap();
        assert_eq!(
            state.linked_peers[&remote.public_key].state,
            LinkedPeerState::Linked
        );
        assert!(state.encrypted_link_states[&remote.public_key]
            .link_snapshot
            .is_some());
        assert!(state.encrypted_link_states[&remote.public_key]
            .handshake_snapshot
            .is_none());
    }
}

#[tokio::test]
async fn test_cancel_then_fresh_handshake_ignores_late_old_packet_from_intake() {
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
    let old_packet_path =
        publish_pending_handshake_packet_from_restarted_intake(&pair.alice, &pair.bob.public_key)
            .await;
    let old_packet = pair
        .alice
        .access
        .session
        .storage()
        .get(&old_packet_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    let old_generation = pair.alice.storage.snapshot().unwrap().encrypted_link_states
        [&pair.bob.public_key]
        .generation;

    pair.alice
        .sdk
        .cancel_pending_handshake(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .cancel_pending_handshake(pair.alice.public_key.clone())
        .await
        .unwrap();
    let cancelled = pair.alice.storage.snapshot().unwrap();
    assert_eq!(
        cancelled.private_intake_peers[&pair.bob.public_key].cancelled_handshake_through_generation,
        Some(old_generation)
    );

    let fresh = pair
        .alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .unwrap();
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();
    assert!(fresh.generation > old_generation);
    let fresh_packet_path =
        publish_pending_handshake_packet_from_restarted_intake(&pair.alice, &pair.bob.public_key)
            .await;
    // Explicit cancellation advances the local generation fence, but it is not
    // an Encrypted Link recovery attempt. The unchanged recovery context
    // therefore reuses the stream path and the fresh `/0` publication replaces
    // the cancelled attempt's packet at that path.
    assert_eq!(fresh_packet_path, old_packet_path);
    let fresh_packet = pair
        .alice
        .access
        .session
        .storage()
        .get(&fresh_packet_path)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec();
    assert_ne!(fresh_packet, old_packet);

    // Let Bob consume the fresh generation's required `/0` before replacing
    // that reused path with the exact packet from the cancelled generation.
    advance_handshake_from_intake_after_restart(&pair.bob, &pair.alice.public_key).await;
    pair.alice
        .access
        .session
        .storage()
        .put(&old_packet_path, old_packet.clone())
        .await
        .unwrap();

    drive_handshake_from_restarted_intake(&pair.alice, &pair.bob).await;
    for (local, remote) in [(&pair.alice, &pair.bob), (&pair.bob, &pair.alice)] {
        assert_eq!(
            local.storage.snapshot().unwrap().linked_peers[&remote.public_key].state,
            LinkedPeerState::Linked
        );
    }
    assert_eq!(
        pair.alice
            .access
            .session
            .storage()
            .get(&old_packet_path)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        old_packet.as_slice(),
        "late cancelled-generation packet must remain published on the reused path"
    );
}

#[tokio::test]
async fn test_pending_handshake_intake_failure_persists_recovery_diagnostics() {
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
        .put(format!("{}/0", handshake.config().read_path), vec![0_u8])
        .await
        .unwrap();

    advance_handshake_from_intake_after_restart(&pair.bob, &pair.alice.public_key).await;

    let failed = pair.bob.storage.snapshot().unwrap();
    let peer = &failed.linked_peers[&pair.alice.public_key];
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert!(peer.local_recovery_attempt_id.is_some());
    assert!(failed.encrypted_link_states[&pair.alice.public_key]
        .handshake_snapshot
        .is_none());
}

#[tokio::test]
async fn test_buffered_receive_replays_atomic_commit_once_across_sdk_handles() {
    let pair = linked_two_party().await;
    publish_private_lists(&pair.bob, &pair.alice, 1).await;

    let reject = Arc::new(AtomicBool::new(false));
    let storage = RejectBufferedDrainCommit {
        inner: pair.alice.storage.clone(),
        reject: Arc::clone(&reject),
    };
    let sdk = Arc::new(PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let alternate = Arc::new(PaykitSdk::new(
        storage,
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&pair.alice.storage, &pair.bob.public_key, 1).await;
    assert!(pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .private_stream_items
        .is_empty());

    reject.store(true, Ordering::SeqCst);
    let failed = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await;
    assert!(failed.is_err());
    let after_failure = pair.alice.storage.snapshot().unwrap();
    assert!(after_failure.private_stream_items.is_empty());
    assert_eq!(
        after_failure.private_intake_peers[&pair.bob.public_key]
            .buffered_packets
            .len(),
        1
    );

    reject.store(false, Ordering::SeqCst);
    let (first, second) = tokio::join!(
        sdk.receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor),
        alternate.receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor),
    );
    let mut processed = 0;
    let mut excluded = 0;
    for result in [first, second] {
        match result {
            Ok(report) => processed += report.report.stream_item_ids.len(),
            Err(error) if error.is_concurrent_update() => excluded += 1,
            Err(error) => panic!("unexpected alternate-handle drain error: {error}"),
        }
    }
    assert_eq!(
        processed, 1,
        "alternate handles must not process one counter twice"
    );
    assert!(excluded <= 1);
    let state = pair.alice.storage.snapshot().unwrap();
    assert_eq!(state.private_stream_items.len(), 1);
    assert!(state.private_intake_peers[&pair.bob.public_key]
        .buffered_packets
        .is_empty());
    supervisor.stop().await;
}

#[tokio::test]
async fn test_buffered_receive_drains_while_transport_stale() {
    let TwoParty {
        _testnet,
        alice,
        bob,
    } = linked_two_party().await;
    let receive_limit = paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT;
    // Stay below the durable queue cap so the active stream remains attached;
    // capacity/backpressure behavior has separate focused coverage.
    let count = 1;
    publish_private_lists(&bob, &alice, count).await;
    let sdk = Arc::new(PaykitSdk::new(
        alice.storage.clone(),
        TestnetSessionProvider::new(alice.access.clone()),
        alice.adapter.clone(),
        PaykitSdkConfig::new(alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&alice.storage, &bob.public_key, count).await;

    drop(_testnet);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if supervisor.health().await.peers.get(&bob.public_key)
                == Some(&PrivateIntakeTransportState::Stale)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("closed homeserver should make private intake stale");

    let first = sdk
        .receive_buffered_private_messages(bob.public_key.clone(), &supervisor)
        .await
        .unwrap();
    let first_count = count.min(receive_limit);
    let remaining = count - first_count;
    assert_eq!(first.report.stream_item_ids.len(), first_count);
    assert_eq!(first.remaining_buffered_packets, remaining);
    assert_eq!(
        first.freshness.transport_state,
        PrivateIntakeTransportState::Stale
    );
    if remaining > 0 {
        let second = sdk
            .receive_buffered_private_messages(bob.public_key.clone(), &supervisor)
            .await
            .unwrap();
        assert_eq!(second.report.stream_item_ids.len(), remaining);
        assert_eq!(second.remaining_buffered_packets, 0);
    }
    supervisor.stop().await;
}

#[tokio::test]
async fn test_sign_out_and_stop_finish_with_healthy_private_intake_supervisor() {
    let pair = linked_two_party().await;
    let sdk = Arc::new(PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::with_session_secret(
            pair.alice.access.clone(),
            pair.alice.session_secret.clone(),
        ),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if supervisor.health().await.peers.get(&pair.bob.public_key)
                == Some(&PrivateIntakeTransportState::CaughtUp)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("private intake stream should become healthy");

    tokio::time::timeout(Duration::from_secs(5), sdk.sign_out())
        .await
        .expect("sign-out must not deadlock behind healthy private intake")
        .expect("sign-out should revoke the test grant and clear session access");
    tokio::time::timeout(Duration::from_secs(5), supervisor.stop())
        .await
        .expect("supervisor stop must finish after sign-out");
}

#[tokio::test]
async fn test_new_link_re_resolves_running_private_intake_topology_immediately() {
    let pair = linked_two_party().await;
    let sdk = Arc::new(PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if supervisor.health().await.peers.get(&pair.bob.public_key)
                == Some(&PrivateIntakeTransportState::CaughtUp)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("initial private intake group should be running");
    let charlie = TestUser::sign_up_with_app(&pair._testnet, app_id("wallet")).await;
    sdk.initiate_link_with_peer(charlie.public_key.clone())
        .await
        .unwrap();

    charlie
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let health = supervisor.health().await;
            if health.peers.contains_key(&charlie.public_key) {
                assert_eq!(health.reserved_streams, 1);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("new link should restart production topology and group loops before healthy refresh");
    supervisor.stop().await;
}

#[tokio::test]
async fn test_post_dequeue_failure_resubscribes_omitted_peer() {
    let pair = linked_two_party().await;
    let attempted_drains = Arc::new(AtomicUsize::new(0));
    let storage = RejectSecondBufferedDrainCommit {
        inner: pair.alice.storage.clone(),
        attempted_drains: Arc::clone(&attempted_drains),
    };
    let sdk = Arc::new(PaykitSdk::new(
        storage,
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let capacity = paykit_sdk::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER;
    publish_private_lists(&pair.bob, &pair.alice, capacity + 1).await;
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let health = supervisor.health().await;
            if health.peers.get(&pair.bob.public_key)
                == Some(&PrivateIntakeTransportState::Backpressured)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("full peer should be omitted before capacity is released");

    let error = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("injected post-dequeue buffered receive failure"));
    assert_eq!(attempted_drains.load(Ordering::SeqCst), 2);
    assert_eq!(
        pair.alice
            .storage
            .snapshot()
            .unwrap()
            .private_stream_items
            .len(),
        1
    );
    wait_for_buffered(&pair.alice.storage, &pair.bob.public_key, capacity).await;
    supervisor.stop().await;
}

#[tokio::test]
async fn test_drained_sse_then_legacy_receive_preserves_generation_for_sse_resume() {
    let pair = linked_two_party().await;
    let sdk = Arc::new(PaykitSdk::new(
        pair.alice.storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    publish_private_lists(&pair.bob, &pair.alice, 1).await;
    wait_for_buffered(&pair.alice.storage, &pair.bob.public_key, 1).await;
    let drained = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await
        .unwrap();
    assert_eq!(drained.report.stream_item_ids.len(), 1);
    assert_eq!(drained.remaining_buffered_packets, 0);
    supervisor.stop().await;

    publish_private_lists(&pair.bob, &pair.alice, 1).await;
    let legacy = sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    assert_eq!(legacy.stream_item_ids.len(), 1);
    let after_legacy = pair.alice.storage.snapshot().unwrap();
    assert_eq!(
        after_legacy.private_intake_peers[&pair.bob.public_key].link_generation,
        after_legacy.encrypted_link_states[&pair.bob.public_key].generation
    );

    let resumed = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    publish_private_lists(&pair.bob, &pair.alice, 1).await;
    wait_for_buffered(&pair.alice.storage, &pair.bob.public_key, 1).await;
    let continued = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &resumed)
        .await
        .unwrap();
    assert_eq!(continued.report.stream_item_ids.len(), 1);
    assert_eq!(continued.remaining_buffered_packets, 0);
    resumed.stop().await;

    pair.bob
        .sdk
        .publish_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .unwrap();
    pair.alice
        .sdk
        .observe_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .unwrap();
    drive_recovery_to_linked(&pair.alice, &pair.bob).await;
    let recovered = pair.alice.storage.snapshot().unwrap();
    assert_eq!(
        recovered.private_intake_peers[&pair.bob.public_key].link_generation,
        recovered.encrypted_link_states[&pair.bob.public_key].generation
    );
}

#[tokio::test]
async fn test_buffered_receive_retention_refusal_keeps_packet_and_checkpoint() {
    let pair = linked_two_party().await;
    publish_private_lists(&pair.bob, &pair.alice, 1).await;
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .unwrap();
    let mut state = pair.alice.storage.snapshot().unwrap();
    let retained = state.private_stream_items[0].clone();
    state.private_stream_items = vec![retained; RETAINED_PRIVATE_STREAM_ITEM_LIMIT];
    state.next_private_stream_item_id = state.private_stream_items.len() as u64;
    let storage = InMemoryStorage::from_state(state);
    publish_private_lists(&pair.bob, &pair.alice, 1).await;

    let sdk = Arc::new(PaykitSdk::new(
        storage.clone(),
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    ));
    let supervisor = sdk
        .start_private_intake(PrivateIntakeConfig::new(1).unwrap())
        .await
        .unwrap();
    wait_for_buffered(&storage, &pair.bob.public_key, 1).await;
    let before = storage.snapshot().unwrap();

    let error = sdk
        .receive_buffered_private_messages(pair.bob.public_key.clone(), &supervisor)
        .await
        .unwrap_err();

    assert!(error.is_retention_limit_reached());
    let after = storage.snapshot().unwrap();
    assert_eq!(after.private_stream_items, before.private_stream_items);
    assert_eq!(
        after.encrypted_link_states[&pair.bob.public_key],
        before.encrypted_link_states[&pair.bob.public_key]
    );
    assert_eq!(
        after.private_intake_peers[&pair.bob.public_key].buffered_packets,
        before.private_intake_peers[&pair.bob.public_key].buffered_packets
    );
    supervisor.stop().await;
}
