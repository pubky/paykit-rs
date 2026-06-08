use super::*;

#[tokio::test]
async fn test_sync_public_endpoints_requires_pubky_session() {
    let storage = InMemoryStorage::new();
    let pubky = TestPubkySessionProvider { session: None };
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        pubky,
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.sync_public_endpoints().await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}
