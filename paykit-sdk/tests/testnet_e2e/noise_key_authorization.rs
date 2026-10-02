use crate::harness::{build_testnet, session_bootstrap, TestUser};
use paykit_lib::{
    get_paykit_noise_key_authorization, PaykitNoiseKeyAuthorization,
    PAYKIT_NOISE_KEY_AUTHORIZATION_PATH,
};
use paykit_sdk::{
    PaykitSdk, PaykitSdkConfig, PaykitSdkError, StorageAdapter, PAYKIT_SESSION_CAPABILITIES,
};

#[tokio::test]
async fn test_public_only_contact_does_not_require_noise_authorization() {
    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    let bob = TestUser::sign_up(&testnet).await;
    let (_, revision) = paykit_lib::get_paykit_app_registry_with_revision(
        &bob.access.outbox_client.public_storage(),
        &bob.public_key.to_public_key().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let registry = paykit_lib::PaykitAppRegistry::new(None);
    paykit_lib::update_paykit_app_registry(&bob.access.session, &registry, &revision)
        .await
        .unwrap();
    let remote = bob.access.session.storage();
    remote
        .delete(PAYKIT_NOISE_KEY_AUTHORIZATION_PATH)
        .await
        .unwrap();

    assert!(alice
        .sdk
        .current_private_payment_lists(&bob.public_key)
        .await
        .unwrap()
        .is_empty());
    let report = alice
        .sdk
        .observe_encrypted_link_recovery_marker(bob.public_key.clone())
        .await
        .unwrap();
    assert!(!report.remote_marker_changed);
    assert!(report.remote_attempt_id.is_none());
    assert!(matches!(
        alice
            .sdk
            .paykit_noise_key_authorization(bob.public_key.clone())
            .await,
        Err(PaykitSdkError::NotFound { .. })
    ));
    assert!(matches!(
        alice
            .sdk
            .ensure_link_with_peer(bob.public_key.clone(), 0)
            .await,
        Err(PaykitSdkError::NotFound { .. })
    ));

    remote
        .put(PAYKIT_NOISE_KEY_AUTHORIZATION_PATH, "invalid")
        .await
        .unwrap();
    assert!(matches!(
        alice
            .sdk
            .current_private_payment_lists(&bob.public_key)
            .await,
        Err(PaykitSdkError::Protocol { .. })
    ));
    assert!(alice
        .sdk
        .observe_encrypted_link_recovery_marker(bob.public_key.clone())
        .await
        .is_err());
}

#[tokio::test]
async fn test_delegated_access_cannot_replace_authorization_or_rotate_keys() {
    let testnet = build_testnet().await;
    let authorizer = TestUser::sign_up(&testnet).await;
    let mut access = session_bootstrap(&testnet, "delegated.test")
        .sign_in(
            authorizer.access.local_secret_key.as_ref().unwrap(),
            PAYKIT_SESSION_CAPABILITIES,
        )
        .await
        .unwrap()
        .access;
    let current = access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(1)
        .unwrap();
    access.local_secret_key = None;
    access.paykit_identity_secret_key = Some(current);
    let storage = access.session.storage();
    assert!(storage
        .put(PAYKIT_NOISE_KEY_AUTHORIZATION_PATH, "untrusted")
        .await
        .is_err());
    assert!(storage
        .delete(PAYKIT_NOISE_KEY_AUTHORIZATION_PATH)
        .await
        .is_err());
    let sdk = PaykitSdk::new(
        authorizer.storage.clone(),
        crate::harness::TestnetSessionProvider::new(access),
        crate::harness::TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("server").unwrap(),
    );
    let before = authorizer.storage.snapshot().unwrap();
    assert!(sdk.publish_paykit_noise_key_authorization().await.is_err());
    assert!(sdk
        .rotate_paykit_identity_key(
            authorizer
                .access
                .local_secret_key
                .as_ref()
                .unwrap()
                .derive_paykit_identity_secret_key(2)
                .unwrap()
        )
        .await
        .is_err());
    assert_eq!(before, authorizer.storage.snapshot().unwrap());
    let record = sdk
        .paykit_noise_key_authorization(authorizer.public_key.clone())
        .await
        .unwrap();
    assert_eq!(record.key_generation(), 1);
}

#[tokio::test]
async fn test_links_require_authorization_and_pin_verified_generations() {
    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    let bob = TestUser::sign_up(&testnet).await;
    let public_storage = bob.access.outbox_client.public_storage();
    let first = get_paykit_noise_key_authorization(
        &public_storage,
        &bob.public_key.to_public_key().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    bob.access
        .session
        .storage()
        .delete(PAYKIT_NOISE_KEY_AUTHORIZATION_PATH)
        .await
        .unwrap();
    assert!(matches!(
        alice
            .sdk
            .ensure_link_with_peer(bob.public_key.clone(), 0)
            .await,
        Err(PaykitSdkError::NotFound { .. })
    ));
    assert!(matches!(
        bob.sdk
            .ensure_link_with_peer(alice.public_key.clone(), 0)
            .await,
        Err(PaykitSdkError::Identity { .. })
    ));
    bob.sdk
        .publish_paykit_noise_key_authorization()
        .await
        .unwrap();
    alice
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .unwrap();
    let pinned = alice
        .storage
        .transaction(|tx| {
            Ok(tx
                .linked_peer(&bob.public_key)
                .unwrap()
                .noise_key_authorization)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pinned, first);

    // The App Registry is discovery metadata, not authority for a peer's key.
    let (mut registry, revision) = paykit_lib::get_paykit_app_registry_with_revision(
        &public_storage,
        &bob.public_key.to_public_key().unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    registry
        .rotate_noise_public_key(pubky::Keypair::random().public_key(), 2)
        .unwrap();
    paykit_lib::update_paykit_app_registry(&bob.access.session, &registry, &revision)
        .await
        .unwrap();
    assert_eq!(
        alice
            .sdk
            .paykit_noise_key_authorization(bob.public_key.clone())
            .await
            .unwrap(),
        first
    );
    alice
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .unwrap();

    // An identity-signed but conflicting generation must not replace a pin.
    let forged = PaykitNoiseKeyAuthorization::sign(
        &pubky::Keypair::from_secret(bob.access.local_secret_key.as_ref().unwrap().as_bytes()),
        pubky::Keypair::random().public_key(),
        1,
    )
    .unwrap();
    bob.access
        .session
        .storage()
        .put(
            PAYKIT_NOISE_KEY_AUTHORIZATION_PATH,
            serde_json::to_vec(&forged).unwrap(),
        )
        .await
        .unwrap();
    assert!(alice
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .is_err());
    let after = alice
        .storage
        .transaction(|tx| {
            Ok(tx
                .linked_peer(&bob.public_key)
                .unwrap()
                .noise_key_authorization)
        })
        .await
        .unwrap();
    assert_eq!(after, Some(first));
}

#[tokio::test]
async fn test_rotation_retries_authorization_publication_and_rejects_peer_rollback() {
    let testnet = build_testnet().await;
    let alice = TestUser::sign_up(&testnet).await;
    let bob = TestUser::sign_up(&testnet).await;
    let first = alice
        .sdk
        .paykit_noise_key_authorization(bob.public_key.clone())
        .await
        .unwrap();
    alice
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .unwrap();
    let replacement = bob
        .access
        .local_secret_key
        .as_ref()
        .unwrap()
        .derive_paykit_identity_secret_key(2)
        .unwrap();
    let remote = bob.access.session.storage();
    let lock = remote
        .lock(
            PAYKIT_NOISE_KEY_AUTHORIZATION_PATH,
            std::time::Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert!(bob
        .sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .is_err());
    assert_eq!(
        alice
            .sdk
            .paykit_noise_key_authorization(bob.public_key.clone())
            .await
            .unwrap(),
        first
    );
    let committed = bob.storage.snapshot().unwrap();
    remote.unlock(&lock).await.unwrap();
    bob.sdk
        .rotate_paykit_identity_key(replacement.clone())
        .await
        .unwrap();
    assert_eq!(bob.storage.snapshot().unwrap(), committed);
    bob.sdk
        .rotate_paykit_identity_key(replacement)
        .await
        .unwrap();

    alice
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .unwrap();
    let backup = alice.sdk.export_backup_state().await.unwrap();
    assert_eq!(
        backup
            .linked_peers
            .iter()
            .find(|peer| peer.counterparty == bob.public_key)
            .unwrap()
            .noise_key_authorization
            .as_ref()
            .unwrap()
            .key_generation(),
        2
    );
    assert!(
        bob.sdk
            .ensure_link_with_peer(alice.public_key.clone(), 0)
            .await
            .is_err(),
        "old local keys must stop working"
    );

    remote
        .put(
            PAYKIT_NOISE_KEY_AUTHORIZATION_PATH,
            serde_json::to_vec(&first).unwrap(),
        )
        .await
        .unwrap();
    let restarted = alice.restart_with_storage(alice.storage.clone()).await;
    assert!(restarted
        .sdk
        .ensure_link_with_peer(bob.public_key.clone(), 0)
        .await
        .is_err());
    assert_eq!(
        alice
            .storage
            .transaction(|tx| Ok(tx
                .linked_peer(&bob.public_key)
                .unwrap()
                .noise_key_authorization
                .unwrap()
                .key_generation()))
            .await
            .unwrap(),
        2
    );
}
