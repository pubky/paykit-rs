use paykit_sdk::{load_public_endpoint_records, PaykitSdkError, PublicationStatus};

use crate::harness::{build_testnet, receiving_detail, TestUser};

#[tokio::test]
async fn test_sync_public_endpoints_publishes_and_removes_managed_endpoints() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter.set_public_details(vec![
        receiving_detail("btc-lightning-bolt11", "lnbc-test-invoice"),
        receiving_detail("btc-onchain", "bc1q-test-address"),
    ]);

    let report = user
        .sdk
        .sync_public_endpoints()
        .await
        .expect("public endpoint sync should succeed");
    assert_eq!(report.published.len(), 2);
    assert!(report.removed.is_empty());
    assert!(report.failed.is_empty());
    for change in &report.published {
        assert_eq!(change.status, PublicationStatus::Published);
        assert!(change.error.is_none());
    }

    // Remote state: both endpoints readable through unauthenticated storage.
    let storage = user.access.outbox_client.public_storage();
    let payee = user
        .public_key
        .to_public_key()
        .expect("public key conversion should succeed");
    let list = paykit_lib::get_payment_list(&storage, &payee)
        .await
        .expect("Payment List fetch should succeed");
    assert!(list
        .payment_endpoints
        .keys()
        .any(|id| id.as_str() == "btc-lightning-bolt11"));
    assert!(list
        .payment_endpoints
        .keys()
        .any(|id| id.as_str() == "btc-onchain"));

    // Local publication records mirror the remote state.
    let records = load_public_endpoint_records(&user.storage)
        .await
        .expect("loading endpoint records should succeed");
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.status == PublicationStatus::Published));

    // Shrinking the desired set removes the stale endpoint remotely.
    user.adapter.set_public_details(vec![receiving_detail(
        "btc-lightning-bolt11",
        "lnbc-test-invoice",
    )]);
    let report = user
        .sdk
        .sync_public_endpoints()
        .await
        .expect("second public endpoint sync should succeed");
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].identifier, "btc-onchain");
    assert_eq!(report.removed[0].status, PublicationStatus::Removed);
    assert!(report.failed.is_empty());

    let list = paykit_lib::get_payment_list(&storage, &payee)
        .await
        .expect("Payment List fetch should succeed");
    assert!(!list
        .payment_endpoints
        .keys()
        .any(|id| id.as_str() == "btc-onchain"));
    assert!(list
        .payment_endpoints
        .keys()
        .any(|id| id.as_str() == "btc-lightning-bolt11"));
}

#[tokio::test]
async fn test_sync_public_endpoints_after_sign_out_fails() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter.set_public_details(vec![receiving_detail(
        "btc-lightning-bolt11",
        "lnbc-test-invoice",
    )]);

    user.sdk.sign_out().await.expect("sign-out should succeed");

    let err = user
        .sdk
        .sync_public_endpoints()
        .await
        .expect_err("sync without a session must fail");
    assert!(
        matches!(err, PaykitSdkError::Identity { .. }),
        "unexpected error: {err:?}"
    );
}
