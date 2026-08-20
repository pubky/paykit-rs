use super::super::*;

#[tokio::test]
async fn test_process_outbound_private_messages_preserves_superseded_reservations_without_session()
{
    let storage = registered_test_storage();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-1".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "one".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    let latest = queue_private_payment_list_with_reservations(
        &storage,
        &counterparty,
        app_id(),
        vec![PrivatePaymentEndpointReservation {
            reservation_id: "reservation-2".into(),
            receiving_detail: PrivateReceivingDetail {
                identifier: "btc-lightning-bolt11".into(),
                payload: "two".into(),
            },
            expires_at: None,
            attribution: HashMap::new(),
        }],
        FixedClock.now(),
    )
    .await
    .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let mut sent = tx
                    .outbound_private_messages(&counterparty)
                    .into_iter()
                    .find(|message| message.outbound_message_id == latest.outbound_message_id)
                    .unwrap();
                sent.status = crate::OutboundPrivateMessageStatus::Sent;
                tx.save_outbound_private_message(sent)?;
                Ok(())
            }
        })
        .await
        .unwrap();
    let canceled = Arc::new(Mutex::new(Vec::new()));
    let sdk = PaykitSdk::with_clock(
        storage.clone(),
        TestPubkySessionProvider { session: None },
        InvalidReservedPrivateListPaymentAdapter {
            canceled: canceled.clone(),
        },
        PaykitSdkConfig::new("test-app").unwrap(),
        FixedClock,
    );

    let result = sdk
        .process_outbound_private_messages(counterparty.clone())
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    assert!(canceled.lock().unwrap().is_empty());
    assert_eq!(
        storage
            .snapshot()
            .unwrap()
            .payment_endpoint_reservations
            .len(),
        2
    );
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
