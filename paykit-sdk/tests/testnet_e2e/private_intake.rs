use std::{
    any::Any,
    sync::{
        atomic::{AtomicBool, Ordering},
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

use crate::harness::{linked_two_party, two_party, TestUser, TestnetSessionProvider, TwoParty};

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
    tokio::time::timeout(Duration::from_secs(10), async {
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
    .await
    .expect("private intake should durably buffer expected packets");
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
    assert!(pair
        .alice
        .access
        .outbox_client
        .public_storage()
        .exists(format!(
            "pubky://{}{}",
            pair.alice.public_key, old_packet_path
        ))
        .await
        .unwrap());
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

    drive_handshake_from_restarted_intake(&pair.alice, &pair.bob).await;
    assert_eq!(
        pair.alice.storage.snapshot().unwrap().linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::Linked
    );
    assert!(pair
        .alice
        .access
        .outbox_client
        .public_storage()
        .exists(format!(
            "pubky://{}{}",
            pair.alice.public_key, old_packet_path
        ))
        .await
        .unwrap());
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
async fn test_buffered_receive_is_bounded_and_drains_while_transport_stale() {
    let TwoParty {
        _testnet,
        alice,
        bob,
    } = linked_two_party().await;
    let receive_limit = paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT;
    let count = paykit_sdk::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER
        .min(receive_limit.saturating_add(1));
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
