use super::*;

#[tokio::test]
async fn test_peer_operation_contention_preserves_lease_until_release() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let lease = sdk.claim_peer_link_operation(&counterparty).await.unwrap();
    assert!(sdk
        .claim_peer_link_operation(&counterparty)
        .await
        .unwrap_err()
        .is_concurrent_update());
    storage
        .transaction(|tx| {
            assert_eq!(
                tx.peer_link_operation_lease(&counterparty),
                Some(lease.clone())
            );
            Ok(())
        })
        .await
        .unwrap();
    sdk.release_peer_link_operation(&lease).await.unwrap();
    let next = sdk.claim_peer_link_operation(&counterparty).await.unwrap();
    assert_ne!(lease.lease_id, next.lease_id);
}

#[tokio::test]
async fn test_peer_lease_cleanup_preserves_operation_result() {
    struct FailingCleanupStorage;
    #[async_trait]
    impl StorageAdapter for FailingCleanupStorage {
        async fn transaction_erased<'a>(
            &self,
            _: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            Err(PaykitSdkError::Storage {
                context: "cleanup unavailable".into(),
                source: None,
            })
        }
    }
    let sdk = PaykitSdk::with_clock(
        FailingCleanupStorage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    let lease = PeerLinkOperationLease {
        counterparty: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
        lease_id: 1,
        claimed_at: FixedClock.now(),
        expires_at: FixedClock.now() + ChronoDuration::seconds(60),
    };
    assert_eq!(
        sdk.finish_peer_link_operation(lease.clone(), Ok(7))
            .await
            .unwrap(),
        7
    );
    let error = sdk
        .finish_peer_link_operation::<()>(
            lease,
            Err(PaykitSdkError::Transport {
                context: "publication failed".into(),
                source: None,
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Transport { .. }));
}

#[tokio::test]
async fn test_block_and_relink_preserve_attempted_private_list_reservations() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let reservation = |id: &str| PrivatePaymentEndpointReservation {
        reservation_id: id.into(),
        receiving_detail: PrivateReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: id.into(),
        },
        expires_at: None,
        attribution: HashMap::new(),
    };
    let queued = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("first")],
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction(|tx| {
            let mut sending = queued.clone();
            sending.status = OutboundPrivateMessageStatus::Failed;
            sending.attempt_count = 1;
            sending.last_attempt_at = Some(FixedClock.now());
            sending.last_error = Some("publication response lost".into());
            sending.prepared_send = Some(PreparedOutboundPrivateSend {
                destination_path: "/pub/paykit/v0/private/old/0".into(),
                ciphertext: vec![1; pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN],
            });
            tx.save_outbound_private_message(sending)
        })
        .await
        .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    sdk.block_peer(counterparty.clone()).await.unwrap();
    let blocked = storage.snapshot().unwrap();
    assert_eq!(
        blocked.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    assert!(blocked.outbound_private_messages[0].prepared_send.is_none());
    sdk.unblock_peer(counterparty.clone()).await.unwrap();
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::RecoveryRequired
    );
    crate::domain::linked_peers::save_linked_peer_link_state(
        &storage,
        counterparty.clone(),
        vec![4, 5, 6],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let recovered = &storage.snapshot().unwrap().outbound_private_messages[0];
    assert_eq!(recovered.status, OutboundPrivateMessageStatus::Pending);
    assert_eq!(recovered.attempt_count, 1);
    assert_eq!(recovered.last_attempt_at, Some(FixedClock.now()));
    assert!(recovered.prepared_send.is_none());

    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![reservation("second")],
        FixedClock.now(),
    )
    .await
    .unwrap();
    assert!(sdk
        .cancel_unattempted_superseded_reservations(&counterparty, None, None)
        .await
        .is_empty());
    assert!(canceled.lock().unwrap().is_empty());
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        2
    );
}

#[test]
fn test_link_identity_must_differ_from_local_identity() {
    let local = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    let result = crate::runtime::encrypted_links::require_distinct_link_identity(&local, &local);

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_initiate_link_with_peer_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.initiate_link_with_peer(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_initiate_link_with_peer_requires_session_before_using_stored_link() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_link(&storage, counterparty.clone()).await;
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.initiate_link_with_peer(counterparty).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(snapshot.encrypted_link_states.len(), 1);
}

#[tokio::test]
async fn test_initiate_link_with_peer_preserves_untrusted_linking_state_without_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::linked_peers::save_link_handshake_state(
        &storage,
        counterparty.clone(),
        EncryptedLinkHandshakeRole::Initiator,
        vec![1, 2, 3],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.initiate_link_with_peer(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_private_queue_readiness_allows_linking_peer_with_handshake() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    seed_private_capable_identity_and_handshake(&storage, counterparty.clone()).await;
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let readiness = sdk.private_queue_readiness(&counterparty).await.unwrap();

    assert_eq!(readiness, PrivateQueueReadiness::PendingHandshake);
}

#[tokio::test]
async fn test_private_queue_readiness_rejects_linking_peer_without_handshake_role() {
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
                    handshake_role: None,
                    generation: 0,
                    checkpointed_at: FixedClock.now(),
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

    let result = sdk.private_queue_readiness(&counterparty).await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
}

#[tokio::test]
async fn test_recovery_required_peer_allows_relink_attempt() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::domain::linked_peers::save_linked_peer_state(
        &storage,
        counterparty.clone(),
        LinkedPeerState::RecoveryRequired,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + chrono::Duration::seconds(60),
                    )?
                    .unwrap())
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

    let result = sdk
        .start_link_handshake_with_claim(counterparty, EncryptedLinkHandshakeRole::Initiator, lease)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

#[tokio::test]
async fn test_ensure_link_recovery_required_ignores_stale_link_snapshot() {
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
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 4,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + chrono::Duration::seconds(60),
                    )?
                    .unwrap())
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
        .ensure_link_with_peer_with_claim(
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            0,
            lease,
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.linked_peers[&counterparty].state,
        LinkedPeerState::RecoveryRequired
    );
    assert_eq!(
        snapshot.encrypted_link_states[&counterparty].link_snapshot,
        Some(vec![1, 2, 3])
    );
}

#[tokio::test]
async fn test_ensure_link_recovery_required_ignores_stale_handshake_snapshot() {
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
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Responder),
                    generation: 4,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();
    let lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        FixedClock.now(),
                        FixedClock.now() + chrono::Duration::seconds(60),
                    )?
                    .unwrap())
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
        .ensure_link_with_peer_with_claim(
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Responder,
            0,
            lease,
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let snapshot = storage.snapshot().unwrap();
    assert_eq!(
        snapshot.linked_peers[&counterparty].state,
        LinkedPeerState::RecoveryRequired
    );
    assert_eq!(
        snapshot.encrypted_link_states[&counterparty].handshake_snapshot,
        Some(vec![1, 2, 3])
    );
}

#[tokio::test]
async fn test_advance_link_handshake_preserves_recovery_state_without_session() {
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
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 4,
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

    let result = sdk.advance_link_handshake(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert_eq!(
        crate::load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap()
            .generation,
        4
    );
}

#[tokio::test]
async fn test_advance_link_handshake_preserves_unusable_link_state_without_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 0,
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

    let result = sdk.advance_link_handshake(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_advance_link_handshake_preserves_unusable_handshake_snapshot_without_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
                    generation: 0,
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

    let result = sdk.advance_link_handshake(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_advance_link_handshake_preserves_unusable_handshake_metadata_without_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: None,
                    handshake_snapshot: Some(vec![1, 2, 3]),
                    handshake_role: None,
                    generation: 0,
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

    let result = sdk.advance_link_handshake(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .is_some());
}
