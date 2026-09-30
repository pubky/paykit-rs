use std::time::Duration;

use paykit_sdk::{
    storage::PublicEndpointRecord, InMemoryStorage, PaykitApp, PaykitAppCapabilities, PaykitSdk,
    PaykitSdkConfig, PaykitSdkError, PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey,
    PubkySharedStateStorage, StorageAdapter, PAYKIT_SESSION_CAPABILITIES,
};

use crate::harness::{
    build_testnet, build_testnet_with_admin, session_bootstrap, TestnetPaymentAdapter,
    TestnetSessionProvider,
};

#[tokio::test]
async fn test_shared_state_bootstraps_after_public_only_registry_but_rejects_lost_state() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = session_bootstrap(&testnet, "storage-loading.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let mut public_access = access.clone();
    public_access.local_secret_key = None;
    public_access.paykit_identity_secret_key = None;
    let public_sdk = PaykitSdk::new(
        InMemoryStorage::new(),
        TestnetSessionProvider::new(public_access),
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("public-app").unwrap(),
    );
    public_sdk.initialize().await.unwrap();
    let app = PaykitApp::new(
        "Public App",
        PaykitAppCapabilities {
            private_payments: false,
            payment_requests: false,
            receipts: false,
            outgoing_payments: false,
        },
    )
    .unwrap();
    let registry = public_sdk.publish_paykit_app(app.clone()).await.unwrap();
    assert!(registry.noise_public_key().is_none());

    let provider = TestnetSessionProvider::new(access.clone());
    let storage = PubkySharedStateStorage::new(provider.clone());
    let private_sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("private-app").unwrap(),
    );
    private_sdk.initialize().await.unwrap();
    let registry = private_sdk.publish_paykit_app(app).await.unwrap();
    assert!(registry.noise_public_key().is_some());
    assert_eq!(registry.apps().len(), 2);

    access
        .session
        .storage()
        .delete(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap();
    let fresh_storage = PubkySharedStateStorage::new(TestnetSessionProvider::new(access));
    for adapter in [storage, fresh_storage] {
        assert!(matches!(
            adapter
                .transaction(|tx| Ok(tx.export_storage_state()))
                .await,
            Err(PaykitSdkError::Storage { .. })
        ));
    }
    let status = private_sdk.forget_session_access().await.unwrap();
    assert_eq!(status.public_key.as_ref(), Some(&secret.public_key()));
    assert_eq!(
        private_sdk.initialize().await.unwrap().capability,
        PubkyIdentityCapability::SignedOut
    );
    let status = private_sdk.identity_status().await.unwrap().unwrap();
    assert_eq!(status.public_key.as_ref(), Some(&secret.public_key()));
    assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
}

#[tokio::test]
async fn test_shared_state_identity_status_remains_available_after_sign_out() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let result = session_bootstrap(&testnet, "paykit-sdk.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap();
    let session_secret = result.export_session_secret().await.unwrap().into_inner();
    let provider = TestnetSessionProvider::with_session_secret(result.access, session_secret);
    let sdk = PaykitSdk::new(
        PubkySharedStateStorage::new(provider.clone()),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    sdk.sign_out().await.unwrap();
    assert_eq!(
        sdk.initialize().await.unwrap().capability,
        PubkyIdentityCapability::SignedOut
    );
    let status = sdk.identity_status().await.unwrap().unwrap();
    assert_eq!(status.public_key.as_ref(), Some(&secret.public_key()));
    assert_eq!(status.capability, PubkyIdentityCapability::SignedOut);
}

#[tokio::test]
async fn test_shared_state_quota_rejection_keeps_previous_state_without_cooldown() {
    let testnet = build_testnet_with_admin().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = session_bootstrap(&testnet, "storage-loading.test")
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let provider = TestnetSessionProvider::new(access.clone());
    let storage = PubkySharedStateStorage::new(provider.clone());
    let sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    let original = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    let admin = testnet.homeserver_app().admin_server().unwrap();
    reqwest::Client::new()
        .patch(format!(
            "http://{}/users/{}/quota",
            admin.listen_socket(),
            secret.public_key()
        ))
        .header(
            "X-Admin-Password",
            pubky_testnet::pubky_homeserver::ConfigToml::default_test_config()
                .admin
                .admin_password,
        )
        .json(&serde_json::json!({ "storage_quota_mb": 1 }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let error = storage
        .transaction(|tx| {
            let now = chrono::Utc::now();
            tx.save_public_endpoint_record(PublicEndpointRecord {
                app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
                identifier: "large-endpoint".into(),
                payload: Some("x".repeat(2 * 1024 * 1024)),
                updated_at: now,
                status: paykit_sdk::PublicationStatus::PendingPublication,
                last_error: None,
            });
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(error, PaykitSdkError::Transport { context, .. }
        if context == "Pubky shared-state write rejected by storage quota"));
    let restored = tokio::time::timeout(
        Duration::from_secs(10),
        storage.transaction(|tx| Ok(tx.export_storage_state())),
    )
    .await
    .expect("quota rejection must not cause an uncertain-write cooldown")
    .unwrap();
    assert_eq!(restored, original);
}
