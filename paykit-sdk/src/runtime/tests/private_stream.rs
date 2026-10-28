use super::*;

#[tokio::test]
async fn test_receive_private_messages_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 1,
                    checkpointed_at: FixedClock.now(),
                });
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = sdk.receive_private_messages(counterparty.clone()).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
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
async fn test_legacy_receive_conflicts_with_active_private_intake_ownership() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut peer =
                    crate::domain::linked_peers::default_linked_peer(counterparty.clone());
                peer.state = crate::LinkedPeerState::Linked;
                tx.save_linked_peer(peer);
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty,
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 1,
                    checkpointed_at: FixedClock.now(),
                });
                tx.claim_private_intake_supervisor(
                    FixedClock.now(),
                    FixedClock.now() + chrono::Duration::minutes(1),
                    1,
                    HashMap::new(),
                )?
                .unwrap();
                Ok(())
            }
        })
        .await
        .unwrap();

    assert!(matches!(
        sdk.receive_private_messages(counterparty.clone()).await,
        Err(PaykitSdkError::PrivateIntakeModeConflict { counterparty: peer })
            if peer == counterparty
    ));
}

#[tokio::test]
async fn test_legacy_receive_conflicts_while_buffered_packet_remains() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: Some(vec![1, 2, 3]),
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: 1,
                    checkpointed_at: FixedClock.now(),
                });
                tx.buffer_private_intake_packet(crate::storage::BufferedPrivatePacketRecord {
                    counterparty,
                    link_generation: 1,
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

    assert!(matches!(
        sdk.receive_private_messages(counterparty.clone()).await,
        Err(PaykitSdkError::PrivateIntakeModeConflict { counterparty: peer })
            if peer == counterparty
    ));
}

#[tokio::test]
async fn test_bulk_legacy_receive_reports_conflict_without_hiding_other_peer() {
    let storage = InMemoryStorage::new();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let owned = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let polling = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let owned = owned.clone();
            let polling = polling.clone();
            move |tx| {
                for counterparty in [&owned, &polling] {
                    let mut peer =
                        crate::domain::linked_peers::default_linked_peer(counterparty.clone());
                    peer.state = crate::LinkedPeerState::Linked;
                    tx.save_linked_peer(peer);
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                        link_snapshot: Some(vec![1, 2, 3]),
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 1,
                        checkpointed_at: FixedClock.now(),
                    });
                }
                tx.buffer_private_intake_packet(crate::storage::BufferedPrivatePacketRecord {
                    counterparty: owned,
                    link_generation: 1,
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

    let reports = sdk
        .receive_private_messages_from_linked_peers()
        .await
        .unwrap();

    assert_eq!(reports.len(), 2);
    assert!(reports
        .iter()
        .find(|report| report.counterparty == owned)
        .unwrap()
        .error
        .as_deref()
        .unwrap()
        .contains("private intake mode conflict"));
    assert!(reports
        .iter()
        .find(|report| report.counterparty == polling)
        .unwrap()
        .error
        .as_deref()
        .unwrap()
        .contains("identity error"));
}
