use super::*;

#[tokio::test]
async fn test_remote_recovery_marker_observation_rejects_active_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + ChronoDuration::seconds(10),
                )
                .expect("lease should be available");
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() + ChronoDuration::seconds(1),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_ignores_stale_marker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() - ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_ignores_same_second_marker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now() + ChronoDuration::milliseconds(500),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now(),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_ignores_marker_before_private_receive() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now() + ChronoDuration::seconds(4)),
                    last_private_receive_at: Some(FixedClock.now() + ChronoDuration::seconds(4)),
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now() + ChronoDuration::seconds(1),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() + ChronoDuration::seconds(3),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_preserves_newer_handshake() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() - ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.handshake_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(
        link_state.handshake_role,
        Some(EncryptedLinkHandshakeRole::Initiator)
    );
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_preserves_in_progress_handshake() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() + ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.handshake_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(
        link_state.handshake_role,
        Some(EncryptedLinkHandshakeRole::Initiator)
    );
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_accepts_newer_marker_after_stale_handshake() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now() - ChronoDuration::seconds(120)),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 7,
                    checkpointed_at: FixedClock.now() - ChronoDuration::seconds(120),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now(),
        )
        .await
        .unwrap();

    assert!(changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(
        peer.remote_recovery_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.handshake_snapshot.is_none());
    assert!(link_state.handshake_role.is_none());
    assert_eq!(link_state.generation, 8);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_preserves_in_progress_handshake_with_active_lease()
{
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Responder),
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + ChronoDuration::seconds(10),
                )
                .expect("lease should be available");
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() + ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    assert!(peer.remote_recovery_attempt_id.is_none());
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_some());
}
