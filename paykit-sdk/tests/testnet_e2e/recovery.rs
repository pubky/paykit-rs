use chrono::Utc;
use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms,
};
use paykit_sdk::{
    InMemoryStorage, LinkedPeerState, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentRequestLifecycleState, PrivatePaymentListReservationUpdate, PubkyPublicKey,
    StorageAdapter,
};
use std::time::{Duration, Instant};

use crate::harness::{
    drive_link_to_linked, linked_two_party, private_receiving_detail, two_party, TestUser,
    TestnetSessionProvider,
};

#[tokio::test]
async fn test_published_events_survive_relink_and_lost_confirmations() {
    let mut pair = linked_two_party().await;
    let request = pair
        .alice
        .sdk
        .propose_payment_request(
            pair.bob.public_key.clone(),
            PaymentRequestTerms {
                amount: PaymentAmount {
                    value: "0.001".into(),
                    asset: "btc".into(),
                },
                payment_reference: PaymentReference::new("reliable-delivery").unwrap(),
                proposal_expires_at: None,
                recurrence: None,
                accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
                    "btc-lightning-bolt11",
                )
                .unwrap()],
                required_app_id: None,
                metadata: Default::default(),
            },
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
        // The first recovery discards unread events; the second discards unread confirmations.
        wait_until_marker_is_newer_than_observer_checkpoint(&pair.bob, &pair.alice.public_key)
            .await;
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
    wait_until_marker_is_newer_than_observer_checkpoint(&pair.bob, &pair.alice.public_key).await;

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

    // Removal clears the local marker; a later observe sees no new marker.
    let removed = pair
        .alice
        .sdk
        .remove_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("removing the recovery marker should succeed");
    assert!(removed.local_attempt_id.is_none());

    // `remote_marker_changed` stays false both when the marker is gone and
    // when the already-observed marker is still present, so assert remote
    // deletion directly.
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        &bob_noise_secret_key,
        pair.bob.access.session.info().public_key(),
        &alice_public_key,
        &alice_noise_public_key,
    )
    .await
    .expect("direct marker fetch after removal should succeed");
    assert!(
        marker.is_none(),
        "the recovery marker must be deleted from the homeserver"
    );

    let observed_again = pair
        .bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .expect("re-observing after removal should succeed");
    assert!(!observed_again.remote_marker_changed);

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
}

async fn wait_until_marker_is_newer_than_observer_checkpoint(
    observer: &TestUser,
    counterparty: &PubkyPublicKey,
) {
    let cutoff = observer
        .storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let link_checkpoint = tx.encrypted_link_state(&counterparty).and_then(|state| {
                    (state.link_snapshot.is_some() || state.handshake_snapshot.is_some())
                        .then_some(state.checkpointed_at)
                });
                let receive_checkpoint = tx
                    .linked_peer(&counterparty)
                    .and_then(|peer| peer.last_private_receive_at);
                Ok(link_checkpoint.max(receive_checkpoint))
            }
        })
        .await
        .expect("observer checkpoint lookup should succeed");

    let Some(cutoff) = cutoff else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Utc::now().timestamp() <= cutoff.timestamp() {
        assert!(
            Instant::now() < deadline,
            "test clock did not advance past observer checkpoint"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
async fn test_mutual_recovery_markers_do_not_block_relink() {
    let pair = linked_two_party().await;

    pair.alice
        .sdk
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("alice should publish a recovery marker");
    pair.bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .expect("bob should observe alice's marker");
    pair.bob
        .sdk
        .publish_encrypted_link_recovery_marker(pair.alice.public_key.clone())
        .await
        .expect("bob should publish a recovery marker");
    pair.alice
        .sdk
        .observe_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("alice should observe bob's marker");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alice_state = LinkedPeerState::RecoveryRequired;
    let mut bob_state = LinkedPeerState::RecoveryRequired;
    while alice_state != LinkedPeerState::Linked || bob_state != LinkedPeerState::Linked {
        assert!(Instant::now() < deadline, "relink timed out");

        pair.alice
            .sdk
            .observe_encrypted_link_recovery_marker(pair.bob.public_key.clone())
            .await
            .expect("alice marker observation should not reset an in-progress relink");
        pair.bob
            .sdk
            .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
            .await
            .expect("bob marker observation should not reset an in-progress relink");

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
