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
async fn test_issued_receipt_is_readable_by_another_app_on_the_identity() {
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
    let wallet = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("first-app").unwrap(),
        FixedClock,
    );
    let merchant = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("other-app").unwrap(),
        FixedClock,
    );
    let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
    wallet
        .prepare_receipt_issuance(counterparty.clone(), receipt_draft(receipt_id))
        .await
        .unwrap();

    let receipt = merchant
        .issued_receipt(&counterparty, receipt_id)
        .await
        .unwrap();

    assert_eq!(
        receipt,
        wallet
            .issued_receipt(&counterparty, receipt_id)
            .await
            .unwrap()
    );
    assert_eq!(receipt.receipt_id.as_str(), receipt_id);
    assert_eq!(receipt.metadata, receipt_draft(receipt_id).metadata);
}

#[tokio::test]
async fn test_issued_receipt_reports_unknown_receipt_as_not_found() {
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

    let missing = sdk
        .issued_receipt(&counterparty, "550e8400-e29b-41d4-a716-446655440000")
        .await;

    assert!(matches!(missing, Err(PaykitSdkError::NotFound { .. })));
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

#[tokio::test]
async fn test_receipt_access_enqueue_is_idempotent_for_stale_record() {
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
    let prepared_view = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();
    let prepared = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = prepared_view.receipt_id.clone();
            move |tx| {
                Ok(tx
                    .receipt_issuance_record(&counterparty, &receipt_id)
                    .unwrap())
            }
        })
        .await
        .unwrap();

    let stored_at = FixedClock.now() + chrono::Duration::minutes(5);
    storage
        .transaction(|tx| {
            tx.save_receipt_issuance_record(prepared.mark_stored(stored_at));
            Ok(())
        })
        .await
        .unwrap();
    let first = crate::domain::receipts::enqueue_receipt_access_for_issuance(
        &storage,
        prepared.clone(),
        FixedClock.now(),
    )
    .await
    .unwrap();
    let second = crate::domain::receipts::enqueue_receipt_access_for_issuance(
        &storage,
        prepared,
        FixedClock.now(),
    )
    .await
    .unwrap();

    assert_eq!(first.outbound_message_id, second.outbound_message_id);
    assert_eq!(first.updated_at, stored_at);
    assert_eq!(first.access_queued_at, Some(stored_at));
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages[0].created_at,
        stored_at
    );
    assert_eq!(
        storage.snapshot().unwrap().outbound_private_messages.len(),
        1
    );
}

#[tokio::test]
async fn test_receipt_issuance_failure_respects_storage_and_queue_progress() {
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
    let prepared_view = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();
    let prepared = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = prepared_view.receipt_id.clone();
            move |tx| {
                Ok(tx
                    .receipt_issuance_record(&counterparty, &receipt_id)
                    .unwrap())
            }
        })
        .await
        .unwrap();
    let stored = prepared.mark_stored(FixedClock.now());
    storage
        .transaction(|tx| {
            tx.save_receipt_issuance_record(stored.clone());
            Ok(())
        })
        .await
        .unwrap();

    sdk.save_receipt_issuance_failure(
        &counterparty,
        &prepared,
        FixedClock.now(),
        "stale storage failure".into(),
    )
    .await
    .unwrap();
    assert_eq!(
        storage
            .transaction(|tx| Ok(tx
                .receipt_issuance_record(&counterparty, &prepared.receipt_id)
                .unwrap()))
            .await
            .unwrap(),
        stored,
    );
    sdk.save_receipt_issuance_failure(
        &counterparty,
        &stored,
        FixedClock.now(),
        "queue failure".into(),
    )
    .await
    .unwrap();
    let failed = storage
        .transaction(|tx| {
            Ok(tx
                .receipt_issuance_record(&counterparty, &prepared.receipt_id)
                .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(failed.status, ReceiptIssuanceStatus::Failed);
    assert_eq!(failed.stored_at, stored.stored_at);
    assert_eq!(failed.last_error.as_deref(), Some("queue failure"));
    crate::domain::receipts::enqueue_receipt_access_for_issuance(
        &storage,
        failed,
        FixedClock.now(),
    )
    .await
    .unwrap();
    for attempted in [&prepared, &stored] {
        sdk.save_receipt_issuance_failure(
            &counterparty,
            attempted,
            FixedClock.now(),
            "stale failure".into(),
        )
        .await
        .unwrap();
    }

    let stored = storage
        .transaction(move |tx| {
            Ok(tx
                .receipt_issuance_record(&counterparty, &prepared.receipt_id)
                .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(stored.status, ReceiptIssuanceStatus::AccessQueued);
    assert!(stored.last_error.is_none());
}

#[tokio::test]
async fn test_issued_receipts_report_receipt_access_delivery() {
    use crate::domain::outbound_private::{
        mark_outbound_failed, mark_outbound_invalid, mark_outbound_recovery_required,
        mark_outbound_sent,
    };

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
    let prepared = sdk
        .prepare_receipt_issuance(
            counterparty.clone(),
            receipt_draft("550e8400-e29b-41d4-a716-446655440000"),
        )
        .await
        .unwrap();
    let stored = storage
        .transaction({
            let counterparty = counterparty.clone();
            let receipt_id = prepared.receipt_id.clone();
            move |tx| {
                let stored = tx
                    .receipt_issuance_record(&counterparty, &receipt_id)
                    .unwrap()
                    .mark_stored(FixedClock.now());
                tx.save_receipt_issuance_record(stored.clone());
                Ok(stored)
            }
        })
        .await
        .unwrap();
    let queued = crate::domain::receipts::enqueue_receipt_access_for_issuance(
        &storage,
        stored,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let outbound_message_id = queued.outbound_message_id.unwrap();

    let set_outbound = |update: Box<
        dyn FnOnce(OutboundPrivateMessageRecord) -> OutboundPrivateMessageRecord + Send,
    >| {
        let storage = storage.clone();
        let counterparty = counterparty.clone();
        async move {
            storage
                .transaction(move |tx| {
                    let message = tx
                        .outbound_private_messages(&counterparty)
                        .into_iter()
                        .find(|message| message.outbound_message_id == outbound_message_id)
                        .unwrap();
                    tx.save_outbound_private_message(update(message))
                })
                .await
                .unwrap();
        }
    };
    // Both list paths must agree on every derived field.
    let views = || async {
        let all = sdk.issued_receipts().await.unwrap();
        let to = sdk.issued_receipts_to(&counterparty).await.unwrap();
        assert_eq!(all, to);
        assert_eq!(all.len(), 1);
        all.into_iter().next().unwrap()
    };
    // Each outbound change happens at its own time after queueing.
    let queued_at = FixedClock.now();
    let at = |minutes| queued_at + chrono::Duration::minutes(minutes);

    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessQueued);
    assert_eq!(view.outbound_message_id, Some(outbound_message_id));
    assert_eq!(view.updated_at, queued_at);
    assert!(view.access_sent_at.is_none() && view.last_error.is_none());

    let recover = |now| {
        let storage = storage.clone();
        let counterparty = counterparty.clone();
        async move {
            storage
                .transaction(move |tx| {
                    crate::domain::linked_peers::requeue_recovery_required_outbound_messages(
                        tx,
                        &counterparty,
                        now,
                    )
                })
                .await
                .unwrap();
        }
    };

    // A failed attempt is retried, so the receipt stays queued with the error.
    set_outbound(Box::new(move |message| {
        mark_outbound_failed(message, "timeout".into(), at(1))
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessQueued);
    assert_eq!(view.last_error.as_deref(), Some("timeout"));
    assert_eq!(view.updated_at, at(1));

    set_outbound(Box::new(move |message| {
        mark_outbound_invalid(message, "bad payload".into(), at(2))
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessFailed);
    assert_eq!(view.last_error.as_deref(), Some("bad payload"));
    assert_eq!(view.updated_at, at(2));

    set_outbound(Box::new(move |message| {
        mark_outbound_recovery_required(message, "link lost".into(), at(3))
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessFailed);
    assert_eq!(view.last_error.as_deref(), Some("link lost"));
    assert_eq!(view.updated_at, at(3));

    // Encrypted Link recovery queues the access again, at a later time.
    recover(at(4)).await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessQueued);
    assert!(view.last_error.is_none());
    assert_eq!(view.updated_at, at(4));

    // A Delivery Confirmation after an ambiguous publication proves delivery
    // without a known first publication time.
    set_outbound(Box::new(move |mut message| {
        message.status = OutboundPrivateMessageStatus::Failed;
        message.confirmed_at = Some(at(5));
        message
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessSent);
    assert!(view.access_sent_at.is_none());
    assert_eq!(view.updated_at, at(5));

    // Recovery completes the confirmed message and records `sent_at` at
    // recovery time; that is not a publication time.
    set_outbound(Box::new(move |message| {
        mark_outbound_recovery_required(message, "rotated".into(), at(6))
    }))
    .await;
    recover(at(7)).await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessSent);
    assert!(view.access_sent_at.is_none());
    assert_eq!(view.updated_at, at(5));

    set_outbound(Box::new(move |mut message| {
        message.confirmed_at = None;
        message.sent_at = None;
        mark_outbound_sent(message, at(8))
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessSent);
    assert_eq!(view.access_sent_at, Some(at(8)));
    assert_eq!(view.updated_at, at(8));
    assert!(view.last_error.is_none());

    // A failed retry of the unconfirmed event does not undo the publication
    // or move its time.
    set_outbound(Box::new(move |message| {
        mark_outbound_failed(message, "retry".into(), at(9))
    }))
    .await;
    let view = views().await;
    assert_eq!(view.status, ReceiptIssuanceStatus::AccessSent);
    assert_eq!(view.access_sent_at, Some(at(8)));
    assert_eq!(view.updated_at, at(8));
    assert!(view.last_error.is_none());

    // Repeating issuance returns the same delivery status.
    let repeated = sdk
        .process_receipt_issuance(counterparty.clone(), &prepared.receipt_id)
        .await
        .unwrap();
    assert_eq!(repeated.status, ReceiptIssuanceStatus::AccessSent);

    // The stored record keeps `AccessQueued`; delivery is derived on read.
    let stored = storage
        .transaction(move |tx| {
            Ok(tx
                .receipt_issuance_record(&counterparty, &prepared.receipt_id)
                .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(stored.status, ReceiptIssuanceStatus::AccessQueued);
}
