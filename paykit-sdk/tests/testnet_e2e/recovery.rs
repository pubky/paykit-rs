use paykit_sdk::{LinkedPeerState, PaykitSdkError};

use crate::harness::{linked_two_party, receiving_detail, two_party};

#[tokio::test]
async fn test_recovery_marker_publish_observe_remove_roundtrip() {
    let pair = linked_two_party().await;

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
        .set_private_details(vec![receiving_detail(
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
        matches!(err, PaykitSdkError::RecoveryRequired(_)),
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

    // Removal clears the local marker; a later observe sees no new marker.
    let removed = pair
        .alice
        .sdk
        .remove_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect("removing the recovery marker should succeed");
    assert!(removed.local_attempt_id.is_none());

    let observed_again = pair
        .bob
        .sdk
        .observe_encrypted_link_recovery_marker(pair.alice.public_key.clone())
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
        .publish_encrypted_link_recovery_marker(pair.bob.public_key.clone())
        .await
        .expect_err("publishing a marker without private link state must fail");
    assert!(
        matches!(err, PaykitSdkError::Policy(_)),
        "unexpected error: {err:?}"
    );
}
