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
                save_authorized_paykit_app(
                    tx,
                    counterparty,
                    paykit_lib::PaykitAppId::new("server")?,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: false,
                        payment_requests: false,
                        receipts: true,
                        outgoing_payments: false,
                    },
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
                tx.save_authorized_paykit_apps(counterparty, HashMap::new());
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
                save_authorized_paykit_app(
                    tx,
                    counterparty,
                    paykit_lib::PaykitAppId::new("server")?,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: false,
                        payment_requests: false,
                        receipts: true,
                        outgoing_payments: false,
                    },
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

async fn receipt_outbound_conflict_fixture() -> (InMemoryStorage, ReceiptAccessRecord, ReceiptRecord)
{
    let storage = registered_test_storage();
    let local_public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(local_public_key.clone()),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let mut access = receipt_access_record(counterparty.clone(), "receipt-1");
    access.app_authorized = true;
    let mut receipt = receipt_record(counterparty, "receipt-1", local_public_key);
    receipt.receipt_access_key_hash = crate::domain::receipts::receipt_access_key_hash(&access.key);
    (storage, access, receipt)
}

async fn queue_allowance_with_receipt_event_id(
    storage: &InMemoryStorage,
    access: &ReceiptAccessRecord,
    status: OutboundPrivateMessageStatus,
) {
    let event = paykit_lib::AllowanceEvent::Proposal(paykit_lib::AllowanceProposal::new(
        paykit_lib::EventId::new(&access.event_id).unwrap(),
        paykit_lib::AllowanceId::new_v4(),
        paykit_lib::AllowanceRole::Allower,
        paykit_lib::AllowanceTerms::builder("btc")
            .lifetime_amount_limit("1")
            .build()
            .unwrap(),
    ));
    storage
        .transaction(|tx| {
            let mut message =
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    access.counterparty.clone(),
                    access.app_id.clone(),
                    event.kind().as_str().to_owned(),
                    paykit_lib::serialize_allowance_event(
                        &paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                        &event,
                    )
                    .unwrap(),
                    FixedClock.now(),
                ))?;
            message.status = status;
            tx.save_outbound_private_message(message)
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_receipt_outbound_conflict_blocks_uncached_retrieval_and_access_listings() {
    for status in [
        OutboundPrivateMessageStatus::Pending,
        OutboundPrivateMessageStatus::Sending,
        OutboundPrivateMessageStatus::Sent,
        OutboundPrivateMessageStatus::Failed,
        OutboundPrivateMessageStatus::RecoveryRequired,
    ] {
        let (storage, access, _) = receipt_outbound_conflict_fixture().await;
        queue_allowance_with_receipt_event_id(&storage, &access, status).await;
        storage
            .transaction(|tx| {
                // This is the first inbound carrier, so inbound-only dedupe
                // cannot detect its collision with the local Allowance event.
                let mut dedupe = conflicted_event_dedup_record(&access);
                dedupe.conflicting_stream_item_ids.clear();
                tx.save_event_dedup_record(dedupe);
                tx.save_receipt_access_record(access.clone());
                Ok(())
            })
            .await
            .unwrap();
        let sdk = PaykitSdk::with_clock(
            storage.clone(),
            TestPubkySessionProvider { session: None },
            TestPaymentAdapter,
            PaykitSdkConfig::new("bitkit").unwrap(),
            FixedClock,
        );

        let result = sdk
            .retrieve_receipt(access.counterparty.clone(), &access.receipt_id)
            .await;
        assert!(
            matches!(result, Err(PaykitSdkError::Protocol { context, .. })
            if context.contains("conflicting Event ID"))
        );
        assert!(sdk
            .receipt_access_records(&access.counterparty)
            .await
            .unwrap()
            .is_empty());
        assert!(sdk.receipt_access().await.unwrap().is_empty());
        let retained = storage
            .transaction(|tx| Ok(tx.receipt_access_records(&access.counterparty)))
            .await
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(
            retained[0].retrieval_status,
            ReceiptRetrievalStatus::Pending
        );
        assert!(retained[0].retrieval_attempted_at.is_none());
    }
}

#[tokio::test]
async fn test_receipt_outbound_conflict_blocks_cached_provenance_and_receipt_listings() {
    let (storage, access, receipt) = receipt_outbound_conflict_fixture().await;
    storage
        .transaction(|tx| {
            tx.save_receipt_record(receipt.clone());
            tx.save_receipt_access_record(access.clone().mark_retrieved(FixedClock.now()));
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    assert!(sdk
        .retrieve_receipt(access.counterparty.clone(), &access.receipt_id)
        .await
        .is_ok());
    queue_allowance_with_receipt_event_id(&storage, &access, OutboundPrivateMessageStatus::Sent)
        .await;

    // A separate clean descriptor must not rehabilitate conflicted cached provenance.
    for add_clean_access in [false, true] {
        if add_clean_access {
            storage
                .transaction(|tx| {
                    let mut clean_access = access.clone();
                    clean_access.event_id = "750e8400-e29b-41d4-a716-446655440000".into();
                    tx.save_receipt_access_record(clean_access);
                    Ok(())
                })
                .await
                .unwrap();
        }
        let result = sdk
            .retrieve_receipt(access.counterparty.clone(), &access.receipt_id)
            .await;
        assert!(
            matches!(result, Err(PaykitSdkError::Protocol { context, .. })
            if context.contains("conflicting Event ID"))
        );
        assert!(sdk
            .receipt_records(&access.counterparty)
            .await
            .unwrap()
            .is_empty());
        assert!(sdk.receipts().await.unwrap().is_empty());
    }
    assert_eq!(sdk.receipt_access().await.unwrap().len(), 1);
    assert!(storage
        .transaction(|tx| Ok(tx.receipt_record(&access.counterparty, &access.receipt_id)))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_receipt_outbound_conflict_ignores_other_links_and_non_carriers() {
    let (storage, access, receipt) = receipt_outbound_conflict_fixture().await;
    let mut other_receiver = access.clone();
    other_receiver.counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let mut other_counterparty = access.clone();
    other_counterparty.counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    for (carrier, status) in [
        (&other_receiver, OutboundPrivateMessageStatus::Sent),
        (&other_counterparty, OutboundPrivateMessageStatus::Sent),
        (&access, OutboundPrivateMessageStatus::Invalid),
        (&access, OutboundPrivateMessageStatus::Superseded),
    ] {
        queue_allowance_with_receipt_event_id(&storage, carrier, status).await;
    }
    storage
        .transaction(|tx| {
            tx.save_receipt_access_record(access.clone());
            tx.save_receipt_record(receipt.clone());
            Ok(())
        })
        .await
        .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );
    assert!(sdk
        .retrieve_receipt(access.counterparty.clone(), &access.receipt_id)
        .await
        .is_ok());
    assert_eq!(
        sdk.receipt_access_records(&access.counterparty)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sdk.receipt_access().await.unwrap().len(), 1);
    assert_eq!(
        sdk.receipt_records(&access.counterparty)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(sdk.receipts().await.unwrap().len(), 1);
}
