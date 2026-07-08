use paykit_sdk::{LinkedPeerState, PaykitSdkError};

use crate::harness::{linked_two_party, receiving_detail, two_party};

#[tokio::test]
async fn test_recovery_marker_publish_observe_remove_roundtrip() {
    let pair = linked_two_party().await;

    let published = pair
        .alice
        .sdk
        .publish_encrypted_link_recovery_marker(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
        )
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
        .set_private_details(vec![receiving_detail(
            "btc-lightning-bolt11",
            "ln-private-alice",
        )]);
    let err = pair
        .alice
        .sdk
        .enqueue_private_payment_list(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
        .await
        .expect_err("private automation must be blocked during recovery");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired(_)),
        "unexpected error: {err:?}"
    );

    // The counterparty observes the marker through public storage.
    let observed = pair
        .bob
        .sdk
        .observe_encrypted_link_recovery_marker(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
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
    let bob_secret_key = pair
        .bob
        .access
        .local_secret_key
        .as_ref()
        .expect("bob's session should retain a local secret key")
        .as_bytes();
    let alice_public_key = pair
        .alice
        .public_key
        .to_public_key()
        .expect("public key conversion should succeed");
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        bob_secret_key,
        &alice_public_key,
        &pair.bob.receiver_path,
        &pair.alice.receiver_path,
    )
    .await
    .expect("direct marker fetch should succeed")
    .expect("the published marker should be present on the homeserver");
    assert_eq!(marker.attempt_id(), attempt_id.as_str());

    // Removal clears the local marker; a later observe sees no new marker.
    let removed = pair
        .alice
        .sdk
        .remove_encrypted_link_recovery_marker(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
        )
        .await
        .expect("removing the recovery marker should succeed");
    assert!(removed.local_attempt_id.is_none());

    // `remote_marker_changed` stays false both when the marker is gone and
    // when the already-observed marker is still present, so assert remote
    // deletion directly.
    let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
        &storage,
        bob_secret_key,
        &alice_public_key,
        &pair.bob.receiver_path,
        &pair.alice.receiver_path,
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
        .observe_encrypted_link_recovery_marker(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
        .await
        .expect("re-observing after removal should succeed");
    assert!(!observed_again.remote_marker_changed);
}

#[tokio::test]
async fn test_publish_recovery_marker_without_private_link_state_fails() {
    let pair = two_party().await;

    let err = pair
        .alice
        .sdk
        .publish_encrypted_link_recovery_marker(
            pair.bob.public_key.clone(),
            pair.bob.receiver_path.clone(),
        )
        .await
        .expect_err("publishing a marker without private link state must fail");
    assert!(
        matches!(err, PaykitSdkError::Policy(_)),
        "unexpected error: {err:?}"
    );
}
