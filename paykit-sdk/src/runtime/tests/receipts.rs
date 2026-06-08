use super::*;

#[tokio::test]
async fn test_receipt_access_records_require_initialized_identity() {
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty,
                    "550e8400-e29b-41d4-a716-446655440000",
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert!(views.is_empty());
}

#[tokio::test]
async fn test_receipt_access_records_allow_public_only_identity() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty,
                    "550e8400-e29b-41d4-a716-446655440000",
                ));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert_eq!(views.len(), 1);
}

#[tokio::test]
async fn test_receipt_access_records_hide_conflicted_event_ids() {
    let storage = InMemoryStorage::new();
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
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                tx.save_receipt_access_record(access);
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let views = sdk.receipt_access_records(&counterparty).await.unwrap();

    assert!(views.is_empty());
}

#[tokio::test]
async fn test_retrieve_receipt_reports_conflicted_access_before_missing_public_storage() {
    let storage = InMemoryStorage::new();
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
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                tx.save_receipt_access_record(access);
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_retrieve_receipt_reports_missing_access_before_public_storage() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PublicOnly,
            local_secret_available: false,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
}

#[tokio::test]
async fn test_retrieve_receipt_returns_cached_record_for_public_only_identity() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_receipt_record(receipt_record(counterparty, receipt_id, local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let record = sdk
        .retrieve_receipt(counterparty, receipt_id)
        .await
        .unwrap();

    assert_eq!(record.receipt_id, receipt_id);
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_clean_mismatched_access_for_cached_receipt() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let mut access = receipt_access_record(counterparty.clone(), receipt_id);
                access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                access.stream_item_id = 2;
                access.payment_reference = "other-invoice".into();
                tx.save_receipt_access_record(access);
                tx.save_receipt_record(receipt_record(counterparty, receipt_id, local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty.clone(), receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    let access = storage
        .transaction(|tx| {
            Ok(tx
                .receipt_access_records(&counterparty)
                .into_iter()
                .find(|record| record.receipt_id == receipt_id)
                .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(access.retrieval_status, ReceiptRetrievalStatus::Failed);
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_conflicted_access_for_cached_receipt() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                tx.save_receipt_access_record(access);
                tx.save_receipt_record(receipt_record(counterparty, receipt_id, local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_conflicted_cached_provenance_with_clean_access_present() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receipt_id = "receipt-1";
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let conflicted_access = receipt_access_record(counterparty.clone(), receipt_id);
                let mut clean_access = receipt_access_record(counterparty.clone(), receipt_id);
                clean_access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                clean_access.stream_item_id = 2;
                tx.save_event_dedup_record(conflicted_event_dedup_record(&conflicted_access));
                tx.save_receipt_access_record(conflicted_access);
                tx.save_receipt_access_record(clean_access);
                tx.save_receipt_record(receipt_record(counterparty, receipt_id, local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_receipt_records_filter_recipient_identity() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let wrong_recipient = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let issuer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let issuer = issuer.clone();
            let local_public_key = local_public_key.clone();
            let wrong_recipient = wrong_recipient.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_receipt_record(receipt_record(
                    issuer.clone(),
                    "receipt-1",
                    local_public_key,
                ));
                tx.save_receipt_record(receipt_record(issuer, "receipt-2", wrong_recipient));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let records = sdk.receipt_records(&issuer).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].receipt_id, "receipt-1");
}

#[tokio::test]
async fn test_receipt_records_hide_conflicted_receipt_access_provenance() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let issuer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let issuer = issuer.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                let access = receipt_access_record(issuer.clone(), "receipt-1");
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                tx.save_receipt_record(receipt_record(issuer, "receipt-1", local_public_key));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let records = sdk.receipt_records(&issuer).await.unwrap();

    assert!(records.is_empty());
}

#[tokio::test]
async fn test_retrieve_receipt_requires_public_storage_when_uncached() {
    let storage = InMemoryStorage::new();
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
                    capability: PubkyIdentityCapability::PublicOnly,
                    local_secret_available: false,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_receipt_access_record(receipt_access_record(counterparty, receipt_id));
                Ok(())
            }
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}
