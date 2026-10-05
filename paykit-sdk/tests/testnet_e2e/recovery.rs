use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms,
};
use paykit_sdk::{
    storage::{EncryptedLinkStateRecord, LinkedPeerRecord, StorageTransactionCallback},
    InMemoryStorage, LinkedPeerState, OutboundPrivateMessageStatus, PaykitSdk, PaykitSdkConfig,
    PaykitSdkError, PaymentRequestLifecycleState, PrivatePaymentListReservationUpdate,
    PubkyPublicKey, StorageAdapter,
};
use std::{
    any::Any,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

use crate::harness::{
    deliver, drive_link_to_linked, linked_two_party, private_receiving_detail, two_party,
    TestnetSessionProvider,
};

struct PausedRestoreStorage {
    inner: InMemoryStorage,
    counterparty: PubkyPublicKey,
    receive: bool,
    pause: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

#[async_trait::async_trait]
impl StorageAdapter for PausedRestoreStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> paykit_sdk::Result<Box<dyn Any + Send>> {
        let result = self.inner.transaction_erased(f).await?;
        // These restore-state reads follow authorization and recovery-marker preflight.
        let peer = if self.receive {
            result
                .downcast_ref::<(LinkedPeerRecord, Option<EncryptedLinkStateRecord>)>()
                .map(|(peer, _)| peer)
        } else {
            result
                .downcast_ref::<(Option<LinkedPeerRecord>, Option<EncryptedLinkStateRecord>)>()
                .and_then(|(peer, _)| peer.as_ref())
        };
        let pause = if peer.is_some_and(|peer| peer.counterparty == self.counterparty) {
            self.pause.lock().unwrap().take()
        } else {
            None
        };
        if let Some((ready, resume)) = pause {
            ready.send(()).expect("restore observer should remain live");
            resume.await.expect("restore should be released");
        }
        Ok(result)
    }
}

#[tokio::test]
async fn test_private_send_restore_transport_failure_preserves_state() {
    assert_restore_transport_failure_preserves_state(false).await;
}

#[tokio::test]
async fn test_private_receive_restore_transport_failure_preserves_state() {
    assert_restore_transport_failure_preserves_state(true).await;
}

async fn assert_restore_transport_failure_preserves_state(receive: bool) {
    let pair = linked_two_party().await;
    for (sender, receiver) in [(&pair.alice, &pair.bob), (&pair.bob, &pair.alice)] {
        sender
            .sdk
            .clear_private_payment_list(receiver.public_key.clone())
            .await
            .unwrap();
        deliver(sender, receiver).await;
        sender
            .sdk
            .propose_payment_request(
                receiver.public_key.clone(),
                PaymentRequestTerms::builder(
                    PaymentAmount::new("0.001", "btc").unwrap(),
                    PaymentReference::new("restore-outage").unwrap(),
                    vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
                )
                .build()
                .unwrap(),
            )
            .await
            .unwrap();
    }
    let sent = pair
        .bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .unwrap();
    assert_eq!(sent.sent.len(), 1);
    assert!(sent.failed.is_empty());

    let before = pair.alice.storage.snapshot().unwrap();
    let link = &before.encrypted_link_states[&pair.bob.public_key];
    let bytes = link.link_snapshot.as_ref().unwrap();
    let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(bytes).unwrap();
    let noise = pubky_noise::serializer::PubkyNoiseSessionState::deserialize(
        &bytes[..pubky_noise::serializer::SESSION_STATE_V1_LEN],
    )
    .unwrap();
    assert!(noise.sending_nonce > 0 && noise.receiving_nonce > 0);
    assert!(noise.write_counter > noise.counter && noise.read_counter > noise.counter);
    assert!(link.generation > 0);
    assert_eq!(
        before.linked_peers[&pair.bob.public_key].state,
        LinkedPeerState::Linked
    );
    assert!(before.outbound_private_messages.iter().any(|message| {
        message.kind == paykit_lib::PrivateMessageKind::PaymentRequest.as_str()
            && message.status == OutboundPrivateMessageStatus::Pending
            && message.attempt_count == 0
    }));
    let key = pair
        .alice
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(paykit_sdk::INITIAL_PAYKIT_KEY_GENERATION)
        .unwrap();
    assert!(snapshot
        .has_pending_private_application_message(
            &pair.alice.access.outbox_client.public_storage(),
            pair.alice.access.session.info().public_key(),
            &paykit_lib::derive_paykit_noise_secret_key(key.as_bytes()),
        )
        .await
        .unwrap());

    let (ready, reached) = oneshot::channel();
    let (resume, paused) = oneshot::channel();
    let sdk = PaykitSdk::new(
        PausedRestoreStorage {
            inner: pair.alice.storage.clone(),
            counterparty: pair.bob.public_key.clone(),
            receive,
            pause: Mutex::new(Some((ready, paused))),
        },
        TestnetSessionProvider::new(pair.alice.access.clone()),
        pair.alice.adapter.clone(),
        PaykitSdkConfig::new(pair.alice.app_id.clone()).unwrap(),
    );
    let operation = async {
        if receive {
            sdk.receive_private_messages(pair.bob.public_key.clone())
                .await
                .map(|_| ())
        } else {
            sdk.process_outbound_private_messages(pair.bob.public_key.clone())
                .await
                .map(|_| ())
        }
    };
    tokio::pin!(operation);
    tokio::select! {
        reached = tokio::time::timeout(Duration::from_secs(30), reached) => {
            reached.expect("restore checkpoint timed out").unwrap();
        }
        result = &mut operation => panic!("restore did not pause: {result:?}"),
    }
    assert!(pair
        .alice
        .storage
        .snapshot()
        .unwrap()
        .peer_link_operation_leases
        .contains_key(&pair.bob.public_key));
    let server = pair._testnet.homeserver_app().client_server();
    server.shutdown();
    tokio::time::timeout(Duration::from_secs(10), async {
        for endpoint in [
            server.icann_http_url_string(),
            server.pubky_tls_ip_url_ring(),
        ] {
            let url = url::Url::parse(&endpoint).unwrap();
            let address = (url.host_str().unwrap(), url.port().unwrap());
            while tokio::net::TcpStream::connect(address).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    })
    .await
    .expect("homeserver listeners should close");
    resume.send(()).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .expect("restore failure should be bounded")
        .expect_err("offline transcript restore must fail");
    assert!(
        matches!(&error, PaykitSdkError::Transport { context, .. }
            if context.contains("failed to restore Encrypted Link")),
        "unexpected restore error: {error:?}"
    );

    let after = pair.alice.storage.snapshot().unwrap();
    assert!(after.peer_link_operation_leases.is_empty());
    assert_eq!(after.linked_peers, before.linked_peers);
    assert_eq!(after.encrypted_link_states, before.encrypted_link_states);
    assert_eq!(
        after.outbound_private_messages,
        before.outbound_private_messages
    );
    assert_eq!(after.private_stream_items, before.private_stream_items);
    assert_eq!(after.event_dedup_records, before.event_dedup_records);
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
    }

    #[async_trait::async_trait]
    impl PubkySessionProvider for RecoverDuringAdvance<'_> {
        async fn load_session_access(&self) -> paykit_sdk::Result<Option<PubkySessionAccess>> {
            Ok(Some(self.local.access.clone()))
        }

        async fn load_public_storage(&self) -> paykit_sdk::Result<Option<pubky::PublicStorage>> {
            // Authorization and the initial marker lookup precede advancement.
            // Publish before the marker recheck, before the completed link is saved.
            if self.reads.fetch_add(1, Ordering::SeqCst) == 2 {
                self.remote
                    .sdk
                    .publish_encrypted_link_recovery_marker(self.local.public_key.clone())
                    .await?;
            }
            Ok(Some(self.local.access.outbox_client.public_storage()))
        }

        async fn clear_session_access(&self) -> paykit_sdk::Result<()> {
            unreachable!("this test does not sign out")
        }
    }

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
        },
        pair.bob.adapter.clone(),
        PaykitSdkConfig::new(pair.bob.app_id.clone()).unwrap(),
    );
    let result = sdk
        .advance_link_handshake(pair.alice.public_key.clone())
        .await;
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
