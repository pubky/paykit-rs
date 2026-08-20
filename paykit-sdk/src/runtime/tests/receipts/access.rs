use super::*;

#[tokio::test]
async fn test_receipt_access_records_require_initialized_identity() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                save_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty, "550e8400-e29b-41d4-a716-446655440000"),
                );
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

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert!(views.is_empty());
    assert!(sdk.receipt_access().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_receipt_access_records_allow_public_only_identity() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                save_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty, "550e8400-e29b-41d4-a716-446655440000"),
                );
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

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert_eq!(views.len(), 1);
}

#[tokio::test]
async fn test_receipt_access_records_hide_conflicted_event_ids() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                save_authorized_receipt_access(tx, access);
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

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert!(views.is_empty());
}

#[tokio::test]
async fn test_receipt_access_records_hide_apps_without_receipt_capability() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty.clone(),
                    "550e8400-e29b-41d4-a716-446655440000",
                ));
                tx.save_authorized_receipt_apps(
                    counterparty,
                    vec![paykit_lib::PaykitAppId::new("server")?],
                );
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

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert!(views.is_empty());
}

#[tokio::test]
async fn test_receipt_access_records_preserve_historical_app_authorization() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                save_authorized_receipt_access(
                    tx,
                    receipt_access_record(
                        counterparty.clone(),
                        "550e8400-e29b-41d4-a716-446655440000",
                    ),
                );
                tx.save_authorized_receipt_apps(counterparty, Vec::new());
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

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert_eq!(views.len(), 1);
}

#[tokio::test]
async fn test_retrieve_receipt_reports_conflicted_access_before_missing_public_storage() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                save_authorized_receipt_access(tx, access);
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

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_retrieve_receipt_reports_missing_access_before_public_storage() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
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

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
}

#[tokio::test]
async fn test_retrieve_receipt_ignores_access_from_unauthorized_app() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    initialized_at: FixedClock.now(),
                });
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty.clone(),
                    receipt_id,
                ));
                tx.save_authorized_receipt_apps(
                    counterparty,
                    vec![paykit_lib::PaykitAppId::new("server")?],
                );
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

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(
        result,
        Err(PaykitSdkError::RecoveryRequired { .. })
    ));
}
