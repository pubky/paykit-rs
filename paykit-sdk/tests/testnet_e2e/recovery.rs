use chrono::Utc;
use paykit_sdk::{LinkedPeerState, PaykitSdkError, PubkyPublicKey, StorageAdapter};
use std::time::{Duration, Instant};

use crate::harness::{linked_two_party, private_receiving_detail, two_party, TestUser};

#[tokio::test]
async fn test_recovery_marker_publish_observe_remove_roundtrip() {
    let pair = linked_two_party().await;
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
    let bob_pubky_secret_key = pair
        .bob
        .access
        .local_secret_key
        .as_ref()
        .expect("bob's session should retain a local secret key")
        .as_bytes();
    let bob_noise_secret_key = paykit_lib::derive_paykit_noise_secret_key(bob_pubky_secret_key);
    let alice_public_key = pair
        .alice
        .public_key
        .to_public_key()
        .expect("public key conversion should succeed");
    let alice_noise_public_key = paykit_lib::derive_paykit_noise_public_key(
        pair.alice
            .access
            .local_secret_key
            .as_ref()
            .expect("alice's session should retain a local secret key")
            .as_bytes(),
    );
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        &bob_noise_secret_key,
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
