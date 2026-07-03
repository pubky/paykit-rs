use super::*;

#[tokio::test]
async fn test_mark_private_recovery_pending_skips_newer_link_generation() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: Some(vec![4, 5, 6]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 2,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, &receiver_id(), Some(1))
        .await
        .unwrap();
    assert!(matches!(recovery_update, RecoveryRequiredUpdate::Skipped));

    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
        })
        .await
        .unwrap()
        .is_some());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let lease = tx
                    .peer_link_operation_lease(&counterparty, &receiver_id())
                    .expect("test lease should still be active");
                tx.release_peer_link_operation(&counterparty, &receiver_id(), lease.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, &receiver_id(), Some(2))
        .await
        .unwrap();
    assert!(matches!(
        recovery_update,
        RecoveryRequiredUpdate::Marked { new_episode: true }
    ));

    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.link_snapshot.is_none());
    assert_eq!(link_state.generation, 3);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_encrypted_link_recovery_marker_status_reports_peer_fields() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty,
                    counterparty_receiver_id: receiver_id(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
                    remote_recovery_marker_observed_at: Some(FixedClock.now()),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(status.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(
        status.local_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
    assert_eq!(
        status.remote_attempt_id.as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
    assert!(!status.remote_marker_changed);
}

#[tokio::test]
async fn test_publish_recovery_marker_disabled_does_not_mutate_link_state() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
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
        PaykitSdkConfig {
            encrypted_link_recovery_markers: EncryptedLinkRecoveryMarkerPolicy::Disabled,
            ..PaykitSdkConfig::default()
        },
        FixedClock,
    );

    let result = sdk
        .publish_encrypted_link_recovery_marker(counterparty.clone(), receiver_id())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_publish_recovery_marker_public_only_does_not_mutate_link_state() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(PubkyPublicKey::from_public_key(
                        &pubky::Keypair::random().public_key(),
                    )),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .publish_encrypted_link_recovery_marker(counterparty.clone(), receiver_id())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

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
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            &receiver_id(),
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() + ChronoDuration::seconds(1),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
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
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            &receiver_id(),
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() - ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}

#[tokio::test]
async fn test_remote_recovery_marker_observation_accepts_same_second_marker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            &receiver_id(),
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now(),
        )
        .await
        .unwrap();

    assert!(changed);
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(
        peer.remote_recovery_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.link_snapshot.is_none());
    assert_eq!(link_state.generation, 8);
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
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let changed = sdk
        .mark_remote_recovery_marker_observed_if_needed(
            &counterparty,
            &receiver_id(),
            "650e8400-e29b-41d4-a716-446655440000",
            FixedClock.now() - ChronoDuration::seconds(1),
        )
        .await
        .unwrap();

    assert!(!changed);
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linking);
    assert!(peer.remote_recovery_attempt_id.is_none());
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
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
async fn test_mark_private_recovery_pending_skips_active_peer_lease() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
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
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    &receiver_id(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let update = sdk
        .mark_private_recovery_pending(&counterparty, &receiver_id(), Some(7))
        .await
        .unwrap();

    assert!(matches!(update, RecoveryRequiredUpdate::Skipped));
    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty, &receiver_id()))
        })
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_automatic_recovery_marker_publish_records_missing_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: None,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    sdk.publish_local_recovery_marker_if_possible(&counterparty, &receiver_id(), true)
        .await;

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.local_marker_last_error.as_deref(),
        Some("no Pubky session available")
    );
}

#[tokio::test]
async fn test_automatic_recovery_marker_remove_records_missing_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    sdk.remove_local_recovery_marker_if_recorded(&counterparty, &receiver_id())
        .await
        .unwrap();

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.local_marker_last_error.as_deref(),
        Some("no Pubky session available")
    );
}

#[tokio::test]
async fn test_mark_private_recovery_pending_preserves_marker_until_publish() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
                    remote_recovery_marker_observed_at: Some(FixedClock.now()),
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: Some(vec![4, 5, 6]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 2,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, &receiver_id(), Some(2))
        .await
        .unwrap();
    assert!(matches!(
        recovery_update,
        RecoveryRequiredUpdate::Marked { new_episode: true }
    ));

    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    assert_eq!(
        peer.local_recovery_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
    assert_eq!(
        peer.remote_recovery_attempt_id.as_deref(),
        Some("550e8400-e29b-41d4-a716-446655440000")
    );
}

#[test]
fn test_local_recovery_marker_must_belong_to_current_episode() {
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let recovery_started_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();
    let previous_episode_marker_at = Utc.with_ymd_and_hms(2026, 6, 3, 11, 59, 0).unwrap();
    let current_episode_marker_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 1, 0).unwrap();
    let mut peer = LinkedPeerRecord {
        counterparty,
        counterparty_receiver_id: receiver_id(),
        state: LinkedPeerState::RecoveryRequired,
        last_sync_at: Some(recovery_started_at),
        last_private_receive_at: None,
        failure_count: 1,
        local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
        local_recovery_marker_created_at: Some(previous_episode_marker_at),
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    };

    assert!(!local_recovery_marker_belongs_to_current_episode(&peer));

    peer.local_recovery_marker_created_at = Some(current_episode_marker_at);

    assert!(local_recovery_marker_belongs_to_current_episode(&peer));
}

#[tokio::test]
async fn test_mark_private_recovery_pending_preserves_ongoing_local_marker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_id: receiver_id(),
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    counterparty_receiver_id: receiver_id(),
                    link_snapshot: Some(vec![4, 5, 6]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 2,
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, &receiver_id(), Some(2))
        .await
        .unwrap();
    assert!(matches!(
        recovery_update,
        RecoveryRequiredUpdate::Marked { new_episode: false }
    ));

    let peer = crate::load_linked_peer(&storage, &counterparty, &receiver_id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        peer.local_recovery_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
}
