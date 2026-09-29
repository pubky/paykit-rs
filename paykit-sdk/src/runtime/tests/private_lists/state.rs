use super::*;

#[tokio::test]
async fn test_app_removal_retires_unattempted_private_lists_atomically() {
    struct ValidatingStorage(InMemoryStorage);

    #[async_trait]
    impl StorageAdapter for ValidatingStorage {
        async fn transaction_erased<'a>(
            &self,
            f: crate::storage::StorageTransactionCallback<'a>,
        ) -> Result<Box<dyn std::any::Any + Send>> {
            self.0
                .transaction_erased(Box::new(move |tx| {
                    let result = f(tx)?;
                    crate::validate_storage_state(&tx.export_storage_state())?;
                    Ok(result)
                }))
                .await
        }
    }

    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .save_identity_state(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: FixedClock.now(),
        })
        .await
        .unwrap();
    let storage = ValidatingStorage(storage);
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "ln-private".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();

    let blockers = crate::runtime::app_removal::begin_paykit_app_removal(
        &storage,
        &app_id(),
        FixedClock.now(),
    )
    .await
    .unwrap();

    assert!(blockers.is_empty());
    let state = storage.0.snapshot().unwrap();
    assert!(state.retired_paykit_apps.contains(&app_id()));
    assert_eq!(
        state.outbound_private_messages[0].status,
        OutboundPrivateMessageStatus::Superseded
    );
    assert_eq!(state.payment_endpoint_reservations.len(), 1);
}

#[tokio::test]
async fn test_enqueue_private_payment_list_keeps_existing_reservation_on_error() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "existing-reservation".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "existing".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        MixedExistingReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let result = sdk
        .enqueue_private_payment_list_from_receiving_details(counterparty)
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Protocol { .. })));
    assert_eq!(
        *canceled.lock().unwrap(),
        vec!["conflicting-reservation".to_string()]
    );
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        1
    );
}

#[tokio::test]
async fn test_current_private_payment_list_reads_cached_view_for_public_only_identity() {
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    storage
        .transaction(|tx| {
            tx.save_identity_state(IdentityState {
                public_key: Some(PubkyPublicKey::from_public_key(
                    &pubky::Keypair::random().public_key(),
                )),
                initialized_at: FixedClock.now(),
            });
            save_authorized_paykit_app(
                tx,
                counterparty.clone(),
                paykit_lib::PaykitAppId::new("bitkit").unwrap(),
                private_app_capabilities(),
            );
            Ok(())
        })
        .await
        .unwrap();
    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_list_message("ln-private")],
        None,
        FixedClock.now(),
    )
    .await
    .unwrap();
    let sdk = PaykitSdk::with_clock(
        storage,
        TestPubkySessionProvider { session: None },
        TestPaymentAdapter,
        PaykitSdkConfig::new("bitkit").unwrap(),
        FixedClock,
    );

    let views = sdk
        .current_private_payment_lists(&counterparty)
        .await
        .unwrap();
    let view = views
        .iter()
        .find(|view| view.app_id.as_str() == "bitkit")
        .unwrap();

    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-private");
}
