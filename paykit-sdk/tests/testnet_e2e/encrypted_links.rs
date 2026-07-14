use paykit_sdk::{
    load_encrypted_link_state, EncryptedLinkHandshakeRole, LinkedPeerState, PaykitSdkError,
    PubkyPublicKey,
};
use pubky_testnet::pubky::Keypair;

use crate::harness::{build_testnet, drive_link_to_linked, receiver_path, two_party, TestUser};

#[tokio::test]
async fn test_link_handshake_two_party_reaches_linked() {
    let pair = two_party().await;

    let initiated = pair
        .alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
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
        .accept_link_with_peer(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
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
    let link_state = load_encrypted_link_state(
        &pair.alice.storage,
        &pair.bob.public_key,
        &pair.bob.receiver_path,
    )
    .await
    .expect("loading link state should succeed")
    .expect("link state should exist after handshake completion");
    assert!(link_state.link_snapshot.is_some());
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
}

#[tokio::test]
async fn test_advance_link_handshake_without_started_handshake_fails() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    let stranger = PubkyPublicKey::from_public_key(&Keypair::random().public_key());

    let err = user
        .sdk
        .advance_link_handshake(stranger, receiver_path("other/wallet"))
        .await
        .expect_err("advancing without stored handshake state must fail");
    assert!(
        matches!(err, PaykitSdkError::RecoveryRequired(_)),
        "unexpected error: {err:?}"
    );
}
