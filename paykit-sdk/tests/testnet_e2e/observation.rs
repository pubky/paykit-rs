use std::time::Instant;

use paykit_sdk::{
    ContactUpdate, PaykitSdk, PaykitSdkConfig, PaykitSdkError, PubkyLocalSecretKey, PubkyPublicKey,
    PubkySharedStateStorage, StorageAdapter, PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
};

use crate::harness::{
    build_testnet, session_bootstrap, TestnetPaymentAdapter, TestnetSessionProvider,
};

#[tokio::test]
async fn test_observation_matches_shared_read_views_and_requires_active_session() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = session_bootstrap(&testnet, "observation.test")
        .sign_up(
            &secret,
            &homeserver,
            None,
            PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
        )
        .await
        .unwrap()
        .access;
    let provider = TestnetSessionProvider::new(access);
    let storage = PubkySharedStateStorage::new(provider.clone());
    let sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    sdk.save_contacts(
        (0..32)
            .map(|index| ContactUpdate {
                public_key: PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key()),
                label: Some(format!("Contact {index}")),
            })
            .collect(),
    )
    .await
    .unwrap();
    let before = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    let contacts = sdk.contact_records().await.unwrap();
    let peers = sdk.linked_peers().await.unwrap();
    let requests = sdk.payment_requests().await.unwrap();
    let receipts = sdk.receipts().await.unwrap();
    let view = sdk.observe_state().await.unwrap();
    assert_eq!(view.public_key, secret.public_key());
    assert_eq!(view.key_generation, 1);
    assert_eq!(view.contacts, contacts);
    assert_eq!(view.linked_peers, peers);
    assert_eq!(view.payment_requests, requests);
    assert_eq!(view.receipts, receipts);
    assert_eq!(
        storage
            .transaction(|tx| Ok(tx.export_storage_state()))
            .await
            .unwrap(),
        before
    );

    // Alternating paths share the same initialized, local testnet and payload.
    // These timings measure observation reads, not linking or payment execution.
    for pair in 0..10 {
        for consolidated in if pair % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let started = Instant::now();
            if consolidated {
                assert_eq!(sdk.observe_state().await.unwrap().contacts.len(), 32);
            } else {
                assert_eq!(sdk.contact_records().await.unwrap().len(), 32);
                sdk.linked_peers().await.unwrap();
                sdk.payment_requests().await.unwrap();
                sdk.receipts().await.unwrap();
            }
            println!(
                "observation_read pair={pair} consolidated={consolidated} elapsed_us={}",
                started.elapsed().as_micros()
            );
        }
    }

    sdk.forget_session_access().await.unwrap();
    assert!(matches!(
        sdk.observe_state().await,
        Err(PaykitSdkError::Identity { .. })
    ));
}
