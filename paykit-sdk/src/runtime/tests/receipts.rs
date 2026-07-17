use super::*;

#[tokio::test]
async fn test_prepare_receipt_issuance_persists_pending_record() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty_keypair = pubky::Keypair::random();
    let counterparty = PubkyPublicKey::from_public_key(&counterparty_keypair.public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
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

    let record = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receiver_path(),
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
            move |tx| Ok(tx.receipt_issuance_record(&counterparty, &receiver_path(), &receipt_id))
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

    let records = sdk
        .receipt_issuance_records(&counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(records, vec![record]);
}

#[tokio::test]
async fn test_receipt_listing_helpers_match_record_views() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let counterparty = counterparty.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
                    initialized_at: FixedClock.now(),
                    sign_out_generation: 0,
                });
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty.clone(),
                    "550e8400-e29b-41d4-a716-446655440000",
                ));
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
        PaykitSdkConfig::default(),
        FixedClock,
    );

    assert_eq!(
        sdk.receipt_access_from(&counterparty, &receiver_path())
            .await
            .unwrap(),
        sdk.receipt_access_records(&counterparty, &receiver_path())
            .await
            .unwrap()
    );
    assert_eq!(
        sdk.receipt_access().await.unwrap(),
        sdk.receipt_access_from(&counterparty, &receiver_path())
            .await
            .unwrap()
    );
    assert_eq!(
        sdk.receipts_from(&counterparty, &receiver_path())
            .await
            .unwrap(),
        sdk.receipt_records(&counterparty, &receiver_path())
            .await
            .unwrap()
    );
    assert_eq!(
        sdk.receipts().await.unwrap(),
        sdk.receipts_from(&counterparty, &receiver_path())
            .await
            .unwrap()
    );

    let issuance = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receiver_path(),
            receipt_draft("650e8400-e29b-41d4-a716-446655440001"),
        )
        .await
        .unwrap();

    assert_eq!(
        sdk.issued_receipts_to(&counterparty, &receiver_path())
            .await
            .unwrap(),
        sdk.receipt_issuance_records(&counterparty, &receiver_path())
            .await
            .unwrap()
    );
    assert_eq!(sdk.issued_receipts().await.unwrap(), vec![issuance]);
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_conflicting_reused_receipt_id() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    let first = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receiver_path(),
            receipt_draft(receipt_id),
        )
        .await
        .unwrap();
    let second = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receiver_path(),
            receipt_draft(receipt_id),
        )
        .await
        .unwrap();
    let mut conflicting = receipt_draft(receipt_id);
    conflicting.payment_reference = paykit_lib::PaymentReference::new("invoice-2026-0002").unwrap();

    let result = sdk
        .prepare_receipt_issuance(counterparty, receiver_path(), conflicting)
        .await;

    assert_eq!(first, second);
    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_reused_receipt_id_for_other_counterparty() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let first_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    sdk.prepare_receipt_issuance(
        first_counterparty,
        receiver_path(),
        receipt_draft(receipt_id),
    )
    .await
    .unwrap();

    let result = sdk
        .prepare_receipt_issuance(
            second_counterparty,
            receiver_path(),
            receipt_draft(receipt_id),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_prepare_receipt_issuance_rejects_reused_receipt_id_for_other_receiver() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    sdk.prepare_receipt_issuance(
        counterparty.clone(),
        receiver_path(),
        receipt_draft(receipt_id),
    )
    .await
    .unwrap();

    let result = sdk
        .prepare_receipt_issuance(
            counterparty,
            other_receiver_path(),
            receipt_draft(receipt_id),
        )
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_issue_receipt_requires_retry_safe_receipt_id() {
    let sdk = PaykitSdk::with_clock(
        InMemoryStorage::new(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::default(),
        FixedClock,
    );
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let mut draft = receipt_draft("550e8400-e29b-41d4-a716-446655440000");
    draft.receipt_id = None;

    let result = sdk
        .issue_receipt(counterparty, receiver_path(), draft)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
}

#[tokio::test]
async fn test_process_receipt_issuance_without_session_preserves_prepared_record() {
    let storage = InMemoryStorage::new();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            initialized_at: FixedClock.now(),
            sign_out_generation: 0,
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
    let record = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receiver_path(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();

    let result = sdk
        .process_receipt_issuance(counterparty.clone(), receiver_path(), &record.receipt_id)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    let records = sdk
        .receipt_issuance_records(&counterparty, &receiver_path())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, ReceiptIssuanceStatus::PendingStorage);
    let stored = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = record.receipt_id.clone();
            move |tx| Ok(tx.receipt_issuance_record(&counterparty, &receiver_path(), &receipt_id))
        })
        .await
        .unwrap()
        .unwrap();
    assert!(stored.last_error.is_none());
}

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

    let views = sdk
        .receipt_access_records(&counterparty, &receiver_path())
        .await
        .unwrap();

    assert!(views.is_empty());
}

#[tokio::test]
async fn test_receipt_access_records_allow_identity_without_live_session() {
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let views = sdk
        .receipt_access_records(&counterparty, &receiver_path())
        .await
        .unwrap();

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let views = sdk
        .receipt_access_records(&counterparty, &receiver_path())
        .await
        .unwrap();

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
        .await;

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
            capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::RecoveryRequired(_))));
}

#[tokio::test]
async fn test_retrieve_receipt_returns_cached_record_without_live_session() {
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty.clone(), receiver_path(), receipt_id)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    let access = storage
        .transaction(|tx| {
            Ok(tx
                .receipt_access_records(&counterparty, &receiver_path())
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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
        .await;

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
        .await;

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let records = sdk
        .receipt_records(&issuer, &receiver_path())
        .await
        .unwrap();

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let records = sdk
        .receipt_records(&issuer, &receiver_path())
        .await
        .unwrap();

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
                    capability: PubkyIdentityCapability::PrivateLinkCapable,
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

    let result = sdk
        .retrieve_receipt(counterparty, receiver_path(), receipt_id)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}

fn receipt_draft(receipt_id: &str) -> ReceiptDraft {
    ReceiptDraft {
        receipt_id: Some(paykit_lib::ReceiptId::new(receipt_id).unwrap()),
        payment_reference: paykit_lib::PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        payment_endpoint_identifier: Some(
            paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(paykit_lib::PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: serde_json::json!({"settlement_id": "abc-123"})
            .as_object()
            .cloned()
            .unwrap(),
    }
}
