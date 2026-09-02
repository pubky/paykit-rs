use super::*;

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

    let update = sdk
        .mark_private_recovery_pending(&counterparty, Some(7))
        .await
        .unwrap();

    assert!(matches!(update, RecoveryRequiredUpdate::Skipped));
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
async fn test_automatic_recovery_marker_publish_records_missing_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let lease = sdk.claim_peer_link_operation(&counterparty).await.unwrap();

    sdk.publish_local_recovery_marker_if_possible(&counterparty, &lease, true)
        .await;
    sdk.release_peer_link_operation(&lease).await.unwrap();

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty)
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let lease = sdk.claim_peer_link_operation(&counterparty).await.unwrap();

    sdk.remove_local_recovery_marker_if_recorded(&counterparty, &lease)
        .await
        .unwrap();
    sdk.release_peer_link_operation(&lease).await.unwrap();

    let status = sdk
        .encrypted_link_recovery_marker_status(&counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status.local_marker_last_error.as_deref(),
        Some("no Pubky session available")
    );
}

#[tokio::test]
async fn test_replaced_lease_cannot_record_recovery_marker_publish_error() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
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
                    counterparty: counterparty.clone(),
                    link_snapshot: None,
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 7,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now() - ChronoDuration::seconds(2),
                        FixedClock.now() - ChronoDuration::seconds(1),
                    )?
                    .expect("stale lease should be available"))
            }
        })
        .await
        .unwrap();
    let replacement_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + ChronoDuration::seconds(10),
                    )?
                    .expect("expired lease should be replaceable"))
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

    sdk.publish_local_recovery_marker_if_possible(&counterparty, &stale_lease, true)
        .await;

    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.local_recovery_marker_last_error, None);
    assert_eq!(
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
            })
            .await
            .unwrap(),
        Some(replacement_lease)
    );
}

#[tokio::test]
async fn test_replaced_lease_cannot_clear_recovery_marker() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let attempt_id = "650e8400-e29b-41d4-a716-446655440000";
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_linked_peer(LinkedPeerRecord {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    last_sync_at: Some(FixedClock.now()),
                    last_private_receive_at: None,
                    failure_count: 0,
                    local_recovery_attempt_id: Some(attempt_id.into()),
                    local_recovery_marker_created_at: Some(FixedClock.now()),
                    local_recovery_marker_last_error: None,
                    remote_recovery_attempt_id: None,
                    remote_recovery_marker_observed_at: None,
                });
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now() - ChronoDuration::seconds(2),
                        FixedClock.now() - ChronoDuration::seconds(1),
                    )?
                    .expect("stale lease should be available"))
            }
        })
        .await
        .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.claim_peer_link_operation(
                    &counterparty,
                    FixedClock.now(),
                    FixedClock.now() + ChronoDuration::seconds(10),
                )?
                .expect("expired lease should be replaceable");
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

    let err = sdk
        .remove_local_recovery_marker_if_recorded(&counterparty, &stale_lease)
        .await
        .unwrap_err();

    assert!(matches!(err, PaykitSdkError::Policy { .. }));
    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(peer.local_recovery_attempt_id.as_deref(), Some(attempt_id));
    assert_eq!(peer.local_recovery_marker_last_error, None);
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, Some(2))
        .await
        .unwrap();
    assert!(matches!(
        recovery_update,
        RecoveryRequiredUpdate::Marked { new_episode: true }
    ));

    let peer = crate::load_linked_peer(&storage, &counterparty)
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let recovery_update = sdk
        .mark_private_recovery_pending(&counterparty, Some(2))
        .await
        .unwrap();
    assert!(matches!(
        recovery_update,
        RecoveryRequiredUpdate::Marked { new_episode: false }
    ));

    let peer = crate::load_linked_peer(&storage, &counterparty)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        peer.local_recovery_attempt_id.as_deref(),
        Some("650e8400-e29b-41d4-a716-446655440000")
    );
}
