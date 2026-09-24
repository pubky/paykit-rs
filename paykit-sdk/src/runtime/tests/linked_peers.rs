use super::*;

#[tokio::test]
async fn test_linked_peers_lists_tracked_peers() {
    let storage = InMemoryStorage::new();
    let first = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let first = first.clone();
            let second = second.clone();
            move |tx| {
                for counterparty in [second, first] {
                    tx.save_linked_peer(LinkedPeerRecord {
                        counterparty,
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
                }
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

    let peers = sdk.linked_peers().await.unwrap();

    assert_eq!(peers.len(), 2);
    assert!(peers[0].counterparty.as_str() <= peers[1].counterparty.as_str());
}
