use super::*;

type ObservationSdk =
    PaykitSdk<InMemoryStorage, TestPubkySessionProvider, TestPaymentAdapter, FixedClock>;

#[tokio::test]
async fn test_observation_requires_live_access_without_reading_private_state() {
    let sdk = ObservationSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    assert!(matches!(
        sdk.observe_state().await,
        Err(PaykitSdkError::Identity { .. })
    ));
}

#[tokio::test]
async fn test_observation_preserves_read_filters_and_does_not_mutate_state() {
    let storage = registered_test_storage();
    let owner = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let noise_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let peer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let blocked = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(owner.clone()),
                initialized_at: FixedClock.now(),
            });
            tx.save_paykit_noise_public_key(noise_key.clone());
            for counterparty in [&peer, &blocked] {
                let mut record = default_linked_peer(counterparty.clone());
                record.state = if counterparty == &blocked {
                    LinkedPeerState::Blocked
                } else {
                    LinkedPeerState::Linked
                };
                tx.save_linked_peer(record);
                save_retrieved_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty.clone(), "receipt-1"),
                );
                tx.save_receipt_record(receipt_record(
                    counterparty.clone(),
                    "receipt-1",
                    owner.clone(),
                ));
            }
            tx.save_receipt_record(receipt_record(
                peer.clone(),
                "without-access",
                owner.clone(),
            ));
            Ok(())
        })
        .await
        .unwrap();
    for counterparty in [&peer, &blocked] {
        persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            vec![payment_request_message(
                "750e8400-e29b-41d4-a716-446655440000",
                "550e8400-e29b-41d4-a716-446655440000",
                None,
            )],
            None,
            FixedClock.now(),
        )
        .await
        .unwrap();
    }
    let sdk = ObservationSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    sdk.save_contact(ContactUpdate {
        public_key: peer.clone(),
        label: Some("Alice".into()),
    })
    .await
    .unwrap();
    let before = storage.snapshot().unwrap();
    let observation = storage
        .transaction(|tx| {
            ObservationSdk::state_observation_in_transaction(
                tx,
                &owner,
                &noise_key,
                1,
                FixedClock.now(),
            )
        })
        .await
        .unwrap();
    assert_eq!(observation.public_key, owner);
    assert_eq!(observation.key_generation, 1);
    assert_eq!(observation.observed_at, FixedClock.now());
    assert_eq!(observation.contacts, sdk.contact_records().await.unwrap());
    assert_eq!(observation.linked_peers, sdk.linked_peers().await.unwrap());
    assert_eq!(
        observation.payment_requests,
        sdk.payment_requests().await.unwrap()
    );
    assert_eq!(observation.payment_requests.len(), 1);
    assert_eq!(observation.receipts, sdk.receipts().await.unwrap());
    assert_eq!(observation.receipts.len(), 1);
    assert_eq!(storage.snapshot().unwrap(), before);
}

#[tokio::test]
async fn test_observation_rejects_identity_or_noise_key_mismatch() {
    let storage = registered_test_storage();
    let owner = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let other = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(owner.clone()),
                initialized_at: FixedClock.now(),
            });
            tx.save_paykit_noise_public_key(key.clone());
            Ok(())
        })
        .await
        .unwrap();
    let before = storage.snapshot().unwrap();
    for (observed_owner, observed_key) in [(&other, &key), (&owner, &other)] {
        let result = storage
            .transaction(|tx| {
                ObservationSdk::state_observation_in_transaction(
                    tx,
                    observed_owner,
                    observed_key,
                    1,
                    FixedClock.now(),
                )
            })
            .await;
        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }
    assert_eq!(storage.snapshot().unwrap(), before);
}
