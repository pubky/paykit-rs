use super::*;

#[tokio::test]
async fn test_prepare_receipt_issuance_persists_pending_record() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty_keypair = pubky::Keypair::random();
    let counterparty = PubkyPublicKey::from_public_key(&counterparty_keypair.public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let record = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();

    assert_eq!(record.counterparty, counterparty);
    assert_eq!(record.status, ReceiptIssuanceStatus::PendingStorage);
    assert!(record.stored_at.is_none());
    assert!(record.outbound_message_id.is_none());

    let stored = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = record.receipt_id.clone();
            move |tx| Ok(tx.receipt_issuance_record(&counterparty, &receipt_id))
        })
        .await
        .unwrap()
        .unwrap();
    let access = paykit_lib::parse_receipt_access_json(&stored.access_json).unwrap();
    let receipt =
        paykit_lib::decrypt_receipt(&stored.encrypted_receipt, &access.key, &access.location)
            .unwrap();
    assert_eq!(receipt.receipt_id.as_str(), record.receipt_id);
    assert_eq!(
        receipt.recipient_public_key,
        counterparty_keypair.public_key()
    );

    let records = sdk.receipt_issuance_records(&counterparty).await.unwrap();
    assert_eq!(records, vec![record]);
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_retired_app() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(move |tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                initialized_at: FixedClock.now(),
            });
            tx.retire_paykit_app(paykit_lib::PaykitAppId::new("test-app")?);
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(storage
        .transaction(move |tx| Ok(tx.receipt_issuance_records(&counterparty).is_empty()))
        .await
        .unwrap());
}

#[tokio::test]
async fn test_prepare_receipt_issuance_requires_receipt_capability() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(move |tx| {
            let app_id = paykit_lib::PaykitAppId::new("test-app")?;
            tx.save_identity_state(IdentityState {
                public_key: Some(local_public_key),
                initialized_at: FixedClock.now(),
            });
            tx.save_paykit_app_capabilities(
                &app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: false,
                    outgoing_payments: true,
                },
            );
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(storage
        .transaction(move |tx| Ok(tx.receipt_issuance_records(&counterparty).is_empty()))
        .await
        .unwrap());
}

#[tokio::test]
async fn test_receipt_listing_helpers_match_record_views() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                save_authorized_receipt_access(
                    tx,
                    receipt_access_record(
                        counterparty.clone(),
                        "550e8400-e29b-41d4-a716-446655440000",
                    ),
                );
                tx.save_receipt_record(receipt_record(
                    counterparty,
                    "550e8400-e29b-41d4-a716-446655440000",
                    local_public_key,
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    assert_eq!(
        sdk.receipt_access_from(&counterparty).await.unwrap(),
        sdk.receipt_access_records(&counterparty).await.unwrap()
    );
    assert_eq!(
        sdk.receipt_access().await.unwrap(),
        sdk.receipt_access_from(&counterparty).await.unwrap()
    );
    assert_eq!(
        sdk.receipts_from(&counterparty).await.unwrap(),
        sdk.receipt_records(&counterparty).await.unwrap()
    );
    assert_eq!(
        sdk.receipts().await.unwrap(),
        sdk.receipts_from(&counterparty).await.unwrap()
    );

    let issuance = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("650e8400-e29b-41d4-a716-446655440001"),
        )
        .await
        .unwrap();

    assert_eq!(
        sdk.issued_receipts_to(&counterparty).await.unwrap(),
        sdk.receipt_issuance_records(&counterparty).await.unwrap()
    );
    assert_eq!(sdk.issued_receipts().await.unwrap(), vec![issuance]);
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_conflicting_reused_receipt_id() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let first = sdk
        .prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await
        .unwrap();
    let second = sdk
        .prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await
        .unwrap();
    let mut conflicting = receipt_draft(receipt_id);
    conflicting.payment_reference = paykit_lib::PaymentReference::new("invoice-2026-0002").unwrap();

    let result = sdk
        .prepare_receipt_issuance(counterparty, conflicting)
        .await;

    assert_eq!(first, second);
    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_receipt_issuance_cannot_be_reused_or_processed_by_another_app() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let first_app = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("first-app").unwrap(),
        FixedClock,
    );
    let other_app = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("other-app").unwrap(),
        FixedClock,
    );
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    first_app
        .prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await
        .unwrap();

    let reuse = other_app
        .prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await;
    let process = other_app
        .process_receipt_issuance(counterparty, receipt_id)
        .await;

    assert!(matches!(reuse, Err(PaykitSdkError::Protocol { .. })));
    assert!(matches!(process, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_reused_receipt_id_for_other_counterparty() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let first_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    sdk.prepare_receipt_issuance(first_counterparty, receipt_draft(receipt_id))
        .await
        .unwrap();

    let result = sdk
        .prepare_receipt_issuance(second_counterparty, receipt_draft(receipt_id))
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_prepare_receipt_issuance_reuses_idempotent_record() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    sdk.prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await
        .unwrap();

    let result = sdk
        .prepare_receipt_issuance(counterparty, receipt_draft(receipt_id))
        .await
        .unwrap();

    assert_eq!(result.receipt_id, receipt_id);
}

#[tokio::test]
async fn test_issue_receipt_requires_retry_safe_receipt_id() {
    let sdk = PaykitSdk::with_clock(
        registered_test_storage(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let mut draft = receipt_draft("550e8400-e29b-41d4-a716-446655440000");
    draft.receipt_id = None;

    let result = sdk.issue_receipt(counterparty, draft).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_process_receipt_issuance_without_session_preserves_prepared_record() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );
    let record = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();

    let result = sdk
        .process_receipt_issuance(counterparty.clone(), &record.receipt_id)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let records = sdk.receipt_issuance_records(&counterparty).await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, ReceiptIssuanceStatus::PendingStorage);
    let stored = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = record.receipt_id.clone();
            move |tx| Ok(tx.receipt_issuance_record(&counterparty, &receipt_id))
        })
        .await
        .unwrap()
        .unwrap();
    assert!(stored.last_error.is_none());
}
