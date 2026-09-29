use paykit_sdk::{
    load_public_endpoint_records, PaykitAppId, PaykitSdkError,
    PublicPaymentEndpointLoadFailureKind, PublicPaymentResolutionStatus, PublicationStatus,
    StorageAdapter,
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
async fn test_managed_endpoint_removal_uses_published_payload_after_failed_update() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.sdk
        .sync_public_endpoints_with_receiving_details(vec![public_receiving_detail(
            "btc-onchain",
            "published-address",
        )])
        .await
        .unwrap();
    user.storage
        .transaction(|tx| {
            let mut record = tx.public_endpoint_records().remove(0);
            record.payload = Some("unpublished-address".into());
            record.status = PublicationStatus::Failed;
            record.last_error = Some("publication failed".into());
            tx.save_public_endpoint_record(record);
            Ok(())
        })
        .await
        .unwrap();
    let unmanaged = paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    paykit_lib::set_payment_endpoint(
        &user.access.session,
        &user.app_id,
        unmanaged.clone(),
        paykit_lib::PaymentEndpointPayload::new("unmanaged-invoice"),
    )
    .await
    .unwrap();

    let report = user
        .sdk
        .sync_public_endpoints_with_receiving_details(Vec::new())
        .await
        .unwrap();

    assert!(report.failed.is_empty());
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].identifier, "btc-onchain");
    let list = paykit_lib::get_payment_list(
        &user.access.outbox_client.public_storage(),
        &user.public_key.to_public_key().unwrap(),
        &user.app_id,
    )
    .await
    .unwrap();
    assert_eq!(payload_of(&list, "btc-onchain"), None);
    assert_eq!(
        payload_of(&list, unmanaged.as_str()),
        Some("unmanaged-invoice")
    );
    assert_eq!(
        load_public_endpoint_records(&user.storage).await.unwrap()[0].status,
        PublicationStatus::Removed
    );
}

#[tokio::test]
async fn test_managed_endpoint_read_failure_does_not_block_other_changes() {
    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.sdk
        .sync_public_endpoints_with_receiving_details(vec![
            public_receiving_detail("btc-onchain", "address"),
            public_receiving_detail("btc-lightning-bolt11", "invoice"),
        ])
        .await
        .unwrap();
    user.access
        .session
        .storage()
        .put(
            format!(
                "{}apps/{}/endpoints/btc-onchain",
                paykit_lib::PAYKIT_PATH_PREFIX,
                user.app_id
            ),
            vec![0xff],
        )
        .await
        .unwrap();

    let report = user
        .sdk
        .sync_public_endpoints_with_receiving_details(vec![public_receiving_detail(
            "eur-sepa-iban",
            "new-account",
        )])
        .await
        .unwrap();

    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].identifier, "btc-onchain");
    assert!(report.failed[0].error.is_some());
    assert_eq!(report.published.len(), 1);
    assert_eq!(report.published[0].identifier, "eur-sepa-iban");
    assert_eq!(report.removed.len(), 1);
    assert_eq!(report.removed[0].identifier, "btc-lightning-bolt11");
    let records = load_public_endpoint_records(&user.storage).await.unwrap();
    let failed = records
        .iter()
        .find(|record| record.identifier == "btc-onchain")
        .unwrap();
    assert_eq!(failed.status, PublicationStatus::Failed);
    assert_eq!(failed.last_error, report.failed[0].error);
    let public_storage = user.access.outbox_client.public_storage();
    for (identifier, expected) in [
        ("eur-sepa-iban", Some("new-account")),
        ("btc-lightning-bolt11", None),
    ] {
        let payload = paykit_lib::get_payment_endpoint(
            &public_storage,
            user.access.session.info().public_key(),
            &user.app_id,
            &paykit_lib::PaymentEndpointIdentifier::new(identifier).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(payload.as_ref().map(|payload| payload.as_str()), expected);
    }
}

#[tokio::test]
async fn test_managed_empty_endpoint_removal_preserves_concurrent_replacement() {
    struct ReplaceOnRemoval {
        inner: paykit_sdk::InMemoryStorage,
        session: pubky::PubkySession,
        endpoint_path: String,
        replaced: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl StorageAdapter for ReplaceOnRemoval {
        async fn transaction_erased<'a>(
            &self,
            f: paykit_sdk::storage::StorageTransactionCallback<'a>,
        ) -> paykit_sdk::Result<Box<dyn std::any::Any + Send>> {
            let result = self.inner.transaction_erased(f).await?;
            if self
                .inner
                .snapshot()?
                .public_endpoint_records
                .values()
                .any(|record| record.status == PublicationStatus::PendingRemoval)
                && !self
                    .replaced
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // Publish after the removal snapshot is captured, before its conditional DELETE.
                self.session
                    .storage()
                    .put(&self.endpoint_path, "replacement-address")
                    .await
                    .unwrap();
            }
            Ok(result)
        }
    }

    let testnet = build_testnet().await;
    let user = TestUser::sign_up(&testnet).await;
    user.sdk
        .sync_public_endpoints_with_receiving_details(vec![public_receiving_detail(
            "btc-onchain",
            "",
        )])
        .await
        .unwrap();
    let sdk = paykit_sdk::PaykitSdk::new(
        ReplaceOnRemoval {
            inner: user.storage.clone(),
            session: user.access.session.clone(),
            endpoint_path: format!(
                "{}apps/{}/endpoints/btc-onchain",
                paykit_lib::PAYKIT_PATH_PREFIX,
                user.app_id
            ),
            replaced: false.into(),
        },
        crate::harness::TestnetSessionProvider::new(user.access.clone()),
        user.adapter.clone(),
        paykit_sdk::PaykitSdkConfig::new(user.app_id.clone()).unwrap(),
    );

    let report = sdk
        .sync_public_endpoints_with_receiving_details(Vec::new())
        .await
        .unwrap();

    assert!(report.removed.is_empty());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].identifier, "btc-onchain");
    let records = load_public_endpoint_records(&user.storage).await.unwrap();
    assert_eq!(records[0].status, PublicationStatus::Failed);
    assert_eq!(records[0].last_error, report.failed[0].error);
    let public_storage = user.access.outbox_client.public_storage();
    let identifier = paykit_lib::PaymentEndpointIdentifier::new("btc-onchain").unwrap();
    let payload = paykit_lib::get_payment_endpoint(
        &public_storage,
        user.access.session.info().public_key(),
        &user.app_id,
        &identifier,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(payload.as_str(), "replacement-address");

    let retry = user
        .sdk
        .sync_public_endpoints_with_receiving_details(Vec::new())
        .await
        .unwrap();
    assert!(retry.failed.is_empty());
    assert_eq!(retry.removed.len(), 1);
    assert!(paykit_lib::get_payment_endpoint_with_revision(
        &public_storage,
        user.access.session.info().public_key(),
        &user.app_id,
        &identifier,
    )
    .await
    .unwrap()
    .is_none());
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
    user.additional_app(&testnet, malformed_app_id.clone(), "Malformed App")
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

    let (mut registry, revision) = paykit_lib::get_paykit_app_registry_with_revision(
        &user.access.outbox_client.public_storage(),
        user.access.session.info().public_key(),
    )
    .await
    .expect("App Registry fetch should succeed")
    .expect("App Registry should exist");
    registry.remove_app(&user.app_id);
    paykit_lib::update_paykit_app_registry(&user.access.session, &registry, &revision)
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
