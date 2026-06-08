use super::*;

#[tokio::test]
async fn test_initiate_link_with_peer_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
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
        PaykitSdkConfig::default(),
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
    crate::linked_peers::save_link_handshake_state(
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
        PaykitSdkConfig::default(),
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
async fn test_recovery_required_peer_allows_relink_attempt() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    crate::linked_peers::save_linked_peer_state(
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
                    )
                    .unwrap())
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

    let result = sdk
        .start_link_handshake_with_claim(counterparty, EncryptedLinkHandshakeRole::Initiator, lease)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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
        PaykitSdkConfig::default(),
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
        PaykitSdkConfig::default(),
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.advance_link_handshake(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(crate::load_encrypted_link_state(&storage, &counterparty)
        .await
        .unwrap()
        .is_some());
}
