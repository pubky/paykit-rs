use chrono::{SecondsFormat, Utc};
use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{
    PaykitSdkError, PaymentRequestLifecycleState, PrivatePaymentEndpointReservation,
    PrivateReceivingDetail, PubkyIdentityCapability, StorageAdapter,
};
use serde_json::Map as JsonMap;

use crate::harness::{app_id, linked_two_party};

#[tokio::test]
async fn test_two_apps_concurrently_claim_one_payment_request_response() {
    let pair = linked_two_party().await;
    let alice_server = pair
        .alice
        .additional_app(app_id("paykit-server"), "Paykit Server")
        .await;
    let request = pair
        .bob
        .sdk
        .propose_payment_request(pair.alice.public_key.clone(), recurring_request_terms())
        .await
        .expect("request proposal should queue");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .expect("request proposal should send");
    pair.alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("request proposal should be received");

    let request_id = request_id(&request);
    let (bitkit_result, server_result) = tokio::join!(
        pair.alice
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id),
        alice_server
            .sdk
            .accept_payment_request(pair.bob.public_key.clone(), &request_id),
    );

    assert_ne!(bitkit_result.is_ok(), server_result.is_ok());
    let winner = bitkit_result
        .as_ref()
        .ok()
        .and_then(|record| record.payer_app_id.clone())
        .or_else(|| {
            server_result
                .as_ref()
                .ok()
                .and_then(|record| record.payer_app_id.clone())
        })
        .expect("one application should own the accepted request");
    let record = request_with_id(
        pair.alice
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("shared request state should remain readable"),
        request_id.as_str(),
    );
    assert_eq!(record.payer_app_id, Some(winner));

    let snapshot = pair.alice.storage.snapshot().unwrap();
    let acceptance_count = snapshot
        .outbound_private_messages
        .iter()
        .filter(|message| message.kind == "paykit.payment_request_acceptance")
        .count();
    assert_eq!(acceptance_count, 1);
}

#[tokio::test]
async fn test_two_apps_share_private_request_state_and_app_lifecycle() {
    let pair = linked_two_party().await;
    let alice_server = pair
        .alice
        .additional_app(app_id("paykit-server"), "Paykit Server")
        .await;

    let request = pair
        .bob
        .sdk
        .propose_payment_request(pair.alice.public_key.clone(), recurring_request_terms())
        .await
        .expect("request proposal should queue");
    pair.bob
        .sdk
        .process_outbound_private_messages(pair.alice.public_key.clone())
        .await
        .expect("request proposal should send");

    let intake = pair
        .alice
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the first application should receive the request");
    assert_eq!(intake.stream_item_ids.len(), 1);

    let bitkit_request = request_with_id(
        pair.alice
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("Bitkit should read shared request state"),
        &request.payment_request_id,
    );
    let server_request = request_with_id(
        alice_server
            .sdk
            .payment_requests_with(&pair.bob.public_key)
            .await
            .expect("Paykit Server should read shared request state"),
        &request.payment_request_id,
    );
    assert_eq!(bitkit_request, server_request);
    assert_eq!(server_request.payer_app_id, None);

    let second_intake = alice_server
        .sdk
        .receive_private_messages(pair.bob.public_key.clone())
        .await
        .expect("the second application should resume the shared receive checkpoint");
    assert!(second_intake.stream_item_ids.is_empty());

    let signed_out = pair
        .alice
        .sdk
        .sign_out()
        .await
        .expect("signing out one application should succeed");
    assert_eq!(signed_out.capability, PubkyIdentityCapability::SignedOut);

    let accepted = alice_server
        .sdk
        .accept_payment_request(pair.bob.public_key.clone(), &request_id(&request))
        .await
        .expect("the remaining application should claim the payer response");
    assert_eq!(
        accepted.state,
        PaymentRequestLifecycleState::ActiveRecurring
    );
    assert_eq!(accepted.payer_app_id.as_ref(), Some(&alice_server.app_id));

    let other_app_cancel = pair
        .alice
        .sdk
        .cancel_payment_request(pair.bob.public_key.clone(), &request_id(&request), None)
        .await;
    assert!(matches!(
        other_app_cancel,
        Err(PaykitSdkError::Policy { .. })
    ));

    alice_server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("acceptance should send from the owning application");
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("the payee should receive the acceptance");

    let removal = alice_server.sdk.remove_paykit_app().await;
    assert!(matches!(removal, Err(PaykitSdkError::Policy { .. })));

    alice_server
        .sdk
        .cancel_payment_request(
            pair.bob.public_key.clone(),
            &request_id(&request),
            Some("application removal".into()),
        )
        .await
        .expect("the owning application should cancel its request state");
    let undelivered_removal = alice_server.sdk.remove_paykit_app().await;
    assert!(matches!(
        undelivered_removal,
        Err(PaykitSdkError::Policy { .. })
    ));

    alice_server
        .sdk
        .process_outbound_private_messages(pair.bob.public_key.clone())
        .await
        .expect("cancellation should send before application removal");
    pair.bob
        .sdk
        .receive_private_messages(pair.alice.public_key.clone())
        .await
        .expect("the payee should receive the cancellation");

    let registry = alice_server
        .sdk
        .remove_paykit_app()
        .await
        .expect("application removal should succeed after owned work is complete");
    assert!(!registry.apps().contains_key(&alice_server.app_id));
    assert!(registry.apps().contains_key(&pair.alice.app_id));
}

#[tokio::test]
async fn test_failed_app_removal_is_isolated_and_retryable() {
    let pair = linked_two_party().await;
    let alice_server = pair
        .alice
        .additional_app(app_id("paykit-server"), "Paykit Server")
        .await;
    alice_server
        .sdk
        .enqueue_private_payment_list_with_reservations(
            pair.bob.public_key.clone(),
            vec![PrivatePaymentEndpointReservation {
                reservation_id: "server-reservation".into(),
                receiving_detail: PrivateReceivingDetail {
                    identifier: "btc-lightning-bolt11".into(),
                    payload: "ln-server".into(),
                },
                expires_at: None,
                attribution: std::collections::HashMap::new(),
            }],
        )
        .await
        .expect("server reservation should queue");
    let before = alice_server.storage.snapshot().unwrap();
    alice_server.adapter.set_fail_reservation_cancellation(true);

    let failed = alice_server.sdk.remove_paykit_app().await;

    assert!(matches!(failed, Err(PaykitSdkError::Policy { .. })));
    let after_failure = alice_server.storage.snapshot().unwrap();
    assert!(after_failure
        .registered_paykit_apps
        .contains(&pair.alice.app_id));
    assert!(!after_failure
        .retired_paykit_apps
        .contains(&pair.alice.app_id));
    assert_eq!(
        before
            .public_endpoint_records
            .iter()
            .filter(|((app_id, _), _)| app_id == &pair.alice.app_id)
            .collect::<Vec<_>>(),
        after_failure
            .public_endpoint_records
            .iter()
            .filter(|((app_id, _), _)| app_id == &pair.alice.app_id)
            .collect::<Vec<_>>()
    );

    alice_server
        .adapter
        .set_fail_reservation_cancellation(false);
    alice_server
        .storage
        .transaction({
            let counterparty = pair.bob.public_key.clone();
            let app_id = alice_server.app_id.clone();
            move |tx| {
                let mut reservation = tx
                    .payment_endpoint_reservation(&counterparty, &app_id, "server-reservation")
                    .expect("failed cleanup should preserve reservation");
                reservation.cancellation_started_at =
                    Some(Utc::now() - chrono::Duration::seconds(61));
                tx.save_payment_endpoint_reservation(reservation);
                Ok(())
            }
        })
        .await
        .unwrap();

    let registry = alice_server
        .sdk
        .remove_paykit_app()
        .await
        .expect("removal should succeed when cleanup can be retried");
    assert!(!registry.apps().contains_key(&alice_server.app_id));
    assert!(registry.apps().contains_key(&pair.alice.app_id));
}

fn recurring_request_terms() -> PaymentRequestTerms {
    let starts_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    PaymentRequestTerms {
        amount: PaymentAmount {
            value: "0.001".into(),
            asset: "btc".into(),
        },
        payment_reference: PaymentReference::new("shared-identity-subscription").unwrap(),
        proposal_expires_at: None,
        recurrence: Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: starts_at.clone(),
            anchor: starts_at,
            ends_at: None,
        }),
        accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
            "btc-lightning-bolt11",
        )
        .unwrap()],
        required_app_id: None,
        metadata: JsonMap::new(),
    }
}

fn request_id(record: &paykit_sdk::PaymentRequestRecord) -> PaymentRequestId {
    PaymentRequestId::new(record.payment_request_id.clone()).unwrap()
}

fn request_with_id(
    records: Vec<paykit_sdk::PaymentRequestRecord>,
    payment_request_id: &str,
) -> paykit_sdk::PaymentRequestRecord {
    records
        .into_iter()
        .find(|record| record.payment_request_id == payment_request_id)
        .expect("shared Payment Request should exist")
}
