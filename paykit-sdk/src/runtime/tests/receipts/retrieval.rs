use super::*;

#[tokio::test]
async fn test_retrieve_receipt_returns_cached_record_for_public_only_identity() {
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
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                save_retrieved_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty.clone(), receipt_id),
                );
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let record = sdk
        .retrieve_receipt(counterparty, receipt_id)
        .await
        .unwrap();

    assert_eq!(record.receipt_id, receipt_id);
}

#[tokio::test]
async fn test_cached_receipt_requires_authorized_retrieved_access() {
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
                tx.save_receipt_access_record(receipt_access_record(
                    counterparty.clone(),
                    "receipt-1",
                ));
                tx.save_receipt_record(receipt_record(counterparty, "receipt-1", local_public_key));
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

    let retrieval = sdk
        .retrieve_receipt(counterparty.clone(), "receipt-1")
        .await;
    let listed = sdk.receipt_records(&counterparty).await.unwrap();

    assert!(matches!(retrieval, Err(PaykitSdkError::Protocol { .. })));
    assert!(listed.is_empty());
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_clean_mismatched_access_for_cached_receipt() {
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
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                let mut access = receipt_access_record(counterparty.clone(), receipt_id);
                access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                access.stream_item_id = 2;
                access.payment_reference = "other-invoice".into();
                save_authorized_receipt_access(tx, access);
                save_retrieved_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty.clone(), receipt_id),
                );
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty.clone(), receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    let access = storage
        .transaction(|tx| {
            Ok(tx
                .receipt_access_records(&counterparty)
                .into_iter()
                .find(|record| record.event_id == "750e8400-e29b-41d4-a716-446655440000")
                .unwrap())
        })
        .await
        .unwrap();
    assert_eq!(access.retrieval_status, ReceiptRetrievalStatus::Failed);
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_conflicted_access_for_cached_receipt() {
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
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                let access = receipt_access_record(counterparty.clone(), receipt_id);
                tx.save_event_dedup_record(conflicted_event_dedup_record(&access));
                save_authorized_receipt_access(tx, access);
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_retrieve_receipt_rejects_conflicted_cached_provenance_with_clean_access_present() {
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
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                let conflicted_access = receipt_access_record(counterparty.clone(), receipt_id);
                let mut clean_access = receipt_access_record(counterparty.clone(), receipt_id);
                clean_access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                clean_access.stream_item_id = 2;
                tx.save_event_dedup_record(conflicted_event_dedup_record(&conflicted_access));
                save_authorized_receipt_access(tx, conflicted_access);
                save_authorized_receipt_access(tx, clean_access);
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk.retrieve_receipt(counterparty, receipt_id).await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
}

#[tokio::test]
async fn test_retrieve_receipt_ignores_unauthorized_conflicting_access_for_cached_receipt() {
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
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
                });
                let mut unauthorized = receipt_access_record(counterparty.clone(), receipt_id);
                unauthorized.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                unauthorized.stream_item_id = 2;
                unauthorized.payment_reference = "other-invoice".into();
                tx.save_event_dedup_record(conflicted_event_dedup_record(&unauthorized));
                tx.save_receipt_access_record(unauthorized);
                save_retrieved_authorized_receipt_access(
                    tx,
                    receipt_access_record(counterparty.clone(), receipt_id),
                );
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let record = sdk
        .retrieve_receipt(counterparty, receipt_id)
        .await
        .unwrap();

    assert_eq!(record.receipt_id, receipt_id);
}

#[tokio::test]
async fn test_receipt_records_filter_recipient_identity() {
    let storage = registered_test_storage();
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
                    initialized_at: FixedClock.now(),
                });
                save_retrieved_authorized_receipt_access(
                    tx,
                    receipt_access_record(issuer.clone(), "receipt-1"),
                );
                tx.save_receipt_record(receipt_record(
                    issuer.clone(),
                    "receipt-1",
                    local_public_key,
                ));
                let mut second_access = receipt_access_record(issuer.clone(), "receipt-2");
                second_access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                second_access.stream_item_id = 2;
                save_retrieved_authorized_receipt_access(tx, second_access);
                let mut second_record = receipt_record(issuer, "receipt-2", wrong_recipient);
                second_record.receipt_access_event_id =
                    "750e8400-e29b-41d4-a716-446655440000".into();
                tx.save_receipt_record(second_record);
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

    let records = sdk.receipt_records(&issuer).await.unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].receipt_id, "receipt-1");
}

#[tokio::test]
async fn test_receipt_records_hide_conflicted_receipt_access_provenance() {
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let issuer = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction({
            let issuer = issuer.clone();
            let local_public_key = local_public_key.clone();
            move |tx| {
                tx.save_identity_state(IdentityState {
                    public_key: Some(local_public_key.clone()),
                    initialized_at: FixedClock.now(),
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
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let records = sdk.receipt_records(&issuer).await.unwrap();

    assert!(records.is_empty());
}

#[tokio::test]
async fn test_retrieve_receipt_requires_public_storage_when_uncached() {
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
                save_authorized_receipt_access(tx, receipt_access_record(counterparty, receipt_id));
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

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
}
