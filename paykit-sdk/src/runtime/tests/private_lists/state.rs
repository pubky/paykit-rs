use super::*;

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
            tx.save_authorized_private_apps(
                counterparty.clone(),
                vec![paykit_lib::PaykitAppId::new("bitkit").unwrap()],
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
