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
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: None,
                    local_recovery_marker_created_at: None,
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                    noise_key_authorization: None,
                });
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![4, 5, 6]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 2,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + ChronoDuration::seconds(10),
                )?
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

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, Some(1))
        .await
        .unwrap();
    assert!(matches!(recovery_update, RecoveryRequiredUpdate::Skipped));

    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_some());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let lease = tx
                    .peer_link_operation_lease(&counterparty)
                    .expect("test lease should still be active");
                tx.release_peer_link_operation(&counterparty, lease.lease_id);
                Ok(())
            }
        })
        .await
        .unwrap();

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, Some(2))
        .await
        .unwrap();
    assert!(matches!(recovery_update, RecoveryRequiredUpdate::Marked));

    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert!(link_state.link_snapshot.is_none());
    assert_eq!(link_state.generation, 3);
    assert!(storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
        })
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_recovery_discards_buffered_private_intake_and_allows_relink() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut peer =
                    crate::domain::linked_peers::default_linked_peer(counterparty.clone());
                peer.state = LinkedPeerState::Linked;
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                tx.buffer_private_intake_packet(crate::storage::BufferedPrivatePacketRecord {
                    counterparty,
                    link_generation: 7,
                    receive_counter: 0,
                    ciphertext: vec![0; crate::storage::PRIVATE_INTAKE_PACKET_BYTES],
                    source_cursor: None,
                    buffered_at: FixedClock.now(),
                })?;
                Ok(())
            }
        })
        .await
        .unwrap();

    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                crate::domain::linked_peers::mark_recovery_required_in_transaction(
                    tx,
                    &counterparty,
                    FixedClock.now(),
                )
            }
        })
        .await
        .unwrap();

    let recovered = storage.snapshot().unwrap();
    assert_eq!(recovered.encrypted_link_states[&counterparty].generation, 8);
    assert_eq!(
        recovered.private_intake_peers[&counterparty].link_generation,
        8
    );
    assert!(recovered.private_intake_peers[&counterparty]
        .buffered_packets
        .is_empty());

    let relinked = crate::domain::linked_peers::save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![4, 5, 6],
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert_eq!(relinked.generation, 9);
    assert_eq!(
        storage.snapshot().unwrap().private_intake_peers[&counterparty].link_generation,
        9
    );
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
                    state: LinkedPeerState::RecoveryRequired,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 1,
                    local_recovery_attempt_id: Some("650e8400-e29b-41d4-a716-446655440000".into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: Some("550e8400-e29b-41d4-a716-446655440000".into()),
                    remote_recovery_marker_observed_at: Some(FixedClock.now()),
                    noise_key_authorization: None,
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty)
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
                    initialized_at: FixedClock.now(),
                });
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
                    noise_key_authorization: None,
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

    let result = sdk
        .publish_encrypted_link_recovery_marker(counterparty.clone())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.state, LinkedPeerState::Linked);
    let link_state = crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(link_state.link_snapshot, Some(vec![1, 2, 3]));
    assert_eq!(link_state.generation, 7);
}
