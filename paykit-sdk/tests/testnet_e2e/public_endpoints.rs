use paykit_sdk::{
    load_public_endpoint_records, PaykitAppId, PaykitSdkError,
    PublicPaymentEndpointLoadFailureKind, PublicPaymentResolutionStatus, PublicationStatus,
};

use crate::harness::{build_testnet, public_receiving_detail, TestUser};

/// Payload published for `identifier`, or `None` when the endpoint is absent.
fn payload_of<'a>(list: &'a paykit_lib::PaymentList, identifier: &str) -> Option<&'a str> {
    list.payment_endpoints
        .iter()
        .find(|(id, _)| id.as_str() == identifier)
        .map(|(_, payload)| payload.as_str())
}

#[tokio::test]
async fn test_sync_public_endpoints_publishes_and_removes_managed_endpoints() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter.set_public_details(vec![
        public_receiving_detail("btc-lightning-bolt11", "lnbc-test-invoice"),
        public_receiving_detail("btc-onchain", "bc1q-test-address"),
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
    let list = paykit_lib::get_payment_list(&storage, &payee, &user.app_id)
        .await
        .expect("Payment List fetch should succeed");
    assert_eq!(
        payload_of(&list, "btc-lightning-bolt11"),
        Some("lnbc-test-invoice")
    );
    assert_eq!(payload_of(&list, "btc-onchain"), Some("bc1q-test-address"));

    // Local publication records mirror the remote state.
    let records = load_public_endpoint_records(&user.storage)
        .await
        .expect("loading endpoint records should succeed");
    assert_eq!(records.len(), 2);
    assert!(records
        .iter()
        .all(|record| record.status == PublicationStatus::Published));

    // Shrinking the desired set removes the stale endpoint remotely.
    user.adapter
        .set_public_details(vec![public_receiving_detail(
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

    let list = paykit_lib::get_payment_list(&storage, &payee, &user.app_id)
        .await
        .expect("Payment List fetch should succeed");
    assert_eq!(payload_of(&list, "btc-onchain"), None);
    assert_eq!(
        payload_of(&list, "btc-lightning-bolt11"),
        Some("lnbc-test-invoice")
    );
}

#[tokio::test]
async fn test_sync_public_endpoints_after_sign_out_fails() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter
        .set_public_details(vec![public_receiving_detail(
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

#[tokio::test]
async fn test_public_resolution_isolates_one_app_with_malformed_endpoints() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter
        .set_public_details(vec![public_receiving_detail(
            "btc-lightning-bolt11",
            "lnbc-test-invoice",
        )]);
    user.sdk
        .sync_public_endpoints()
        .await
        .expect("valid endpoint sync should succeed");

    let malformed_app_id = PaykitAppId::new("malformed-app").unwrap();
    user.additional_app(malformed_app_id.clone(), "Malformed App")
        .await;
    let invalid_identifier = "a".repeat(65);
    user.access
        .session
        .storage()
        .put(
            format!(
                "{}apps/{malformed_app_id}/endpoints/{invalid_identifier}",
                paykit_lib::PAYKIT_PATH_PREFIX
            ),
            "malformed-list-entry",
        )
        .await
        .expect("malformed endpoint fixture should be stored");

    let resolution = user
        .sdk
        .resolve_public_contact_payment(user.public_key.clone(), None)
        .await
        .expect("a malformed sibling app must not hide valid endpoints");

    assert_eq!(resolution.status, PublicPaymentResolutionStatus::Payable);
    assert_eq!(resolution.payable_endpoints.len(), 1);
    assert_eq!(resolution.failures.len(), 1);
    assert_eq!(resolution.failures[0].app_id, malformed_app_id);
    assert_eq!(
        resolution.failures[0].kind,
        PublicPaymentEndpointLoadFailureKind::InvalidData
    );
}

#[tokio::test]
async fn test_remove_paykit_app_removes_public_endpoints() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter
        .set_public_details(vec![public_receiving_detail(
            "btc-lightning-bolt11",
            "lnbc-test-invoice",
        )]);
    user.sdk
        .sync_public_endpoints()
        .await
        .expect("public endpoint sync should succeed");

    let registry = user
        .sdk
        .remove_paykit_app()
        .await
        .expect("Paykit app removal should succeed");
    assert!(!registry.apps().contains_key(&user.app_id));

    let storage = user.access.outbox_client.public_storage();
    let owner = user
        .public_key
        .to_public_key()
        .expect("public key conversion should succeed");
    let list = paykit_lib::get_payment_list(&storage, &owner, &user.app_id)
        .await
        .expect("Payment List fetch should succeed");
    assert!(list.payment_endpoints.is_empty());
}

#[tokio::test]
async fn test_remove_paykit_app_resumes_after_registry_entry_is_already_absent() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.adapter
        .set_public_details(vec![public_receiving_detail(
            "btc-lightning-bolt11",
            "lnbc-test-invoice",
        )]);
    user.sdk
        .sync_public_endpoints()
        .await
        .expect("public endpoint sync should succeed");

    let (mut registry, etag) = paykit_lib::get_paykit_app_registry_with_etag(
        &user.access.outbox_client.public_storage(),
        user.access.session.info().public_key(),
    )
    .await
    .expect("App Registry fetch should succeed")
    .expect("App Registry should exist");
    registry.remove_app(&user.app_id);
    paykit_lib::update_paykit_app_registry(&user.access.session, &registry, &etag)
        .await
        .expect("manual registry removal should succeed");

    let removed = user
        .sdk
        .remove_paykit_app()
        .await
        .expect("cleanup should resume after registry removal");

    assert!(!removed.apps().contains_key(&user.app_id));
    let list = paykit_lib::get_payment_list(
        &user.access.outbox_client.public_storage(),
        user.access.session.info().public_key(),
        &user.app_id,
    )
    .await
    .expect("Payment List fetch should succeed");
    assert!(list.payment_endpoints.is_empty());
}
