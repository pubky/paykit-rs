use chrono::{SecondsFormat, Utc};
use paykit_lib::{
    PaymentAmount, PaymentEndpointIdentifier, PaymentReference, PaymentRequestId,
    PaymentRequestTerms, Recurrence, RecurrenceUnit,
};
use paykit_sdk::{
    PaykitApp, PaykitAppCapabilities, PaykitSdk, PaykitSdkConfig, PaykitSdkError,
    PaymentRequestLifecycleState, PrivatePaymentEndpointReservation, PrivateReceivingDetail,
    PubkyIdentityCapability, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionBootstrap,
    PubkySharedStateStorage, StorageAdapter, PAYKIT_SESSION_CAPABILITIES,
};
use serde_json::Map as JsonMap;

use crate::harness::{
    app_id, build_testnet, linked_two_party, TestnetPaymentAdapter, TestnetSessionProvider,
};

#[tokio::test]
async fn test_pubky_shared_state_is_visible_to_independent_apps_and_survives_sign_out() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap())
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;

    let mut public_only_access = access.clone();
    public_only_access.local_secret_key = None;
    let public_only_storage =
        PubkySharedStateStorage::new(TestnetSessionProvider::new(public_only_access));
    let error = public_only_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        PaykitSdkError::Identity { context, .. }
            if context.contains("requires the local identity secret")
    ));

    let bitkit_provider = TestnetSessionProvider::new(access.clone());
    let bitkit = PaykitSdk::new(
        PubkySharedStateStorage::new(bitkit_provider.clone()),
        bitkit_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    bitkit.initialize().await.unwrap();
    bitkit.publish_paykit_app(test_app("Bitkit")).await.unwrap();

    let server_provider = TestnetSessionProvider::new(access);
    let server_storage = PubkySharedStateStorage::new(server_provider.clone());
    let server = PaykitSdk::new(
        server_storage.clone(),
        server_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("paykit-server").unwrap(),
    );
    server.initialize().await.unwrap();
    server
        .publish_paykit_app(test_app("Paykit Server"))
        .await
        .unwrap();

    let before_sign_out = server_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(before_sign_out.registered_paykit_apps.len(), 2);

    bitkit.sign_out().await.unwrap();

    let after_sign_out = server_storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap();
    assert_eq!(after_sign_out, before_sign_out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pubky_shared_state_rejects_a_stale_writer() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap())
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let first_provider = TestnetSessionProvider::new(access.clone());
    let first = PubkySharedStateStorage::new(first_provider.clone());
    let second = PubkySharedStateStorage::new(TestnetSessionProvider::new(access));
    let sdk = PaykitSdk::new(
        first.clone(),
        first_provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();

    let (loaded_tx, loaded_rx) = std::sync::mpsc::sync_channel(0);
    let (continue_tx, continue_rx) = std::sync::mpsc::sync_channel(0);
    let stale_write = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(first.transaction(move |tx| {
                loaded_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                let mut identity = tx.load_identity_state().unwrap();
                identity.initialized_at += chrono::Duration::seconds(2);
                tx.save_identity_state(identity);
                Ok(())
            }))
    });
    loaded_rx.recv().unwrap();
    second
        .transaction(|tx| {
            let mut identity = tx.load_identity_state().unwrap();
            identity.initialized_at += chrono::Duration::seconds(1);
            tx.save_identity_state(identity);
            Ok(())
        })
        .await
        .unwrap();
    continue_tx.send(()).unwrap();

    let error = stale_write.join().unwrap().unwrap_err();
    assert!(matches!(
        error,
        PaykitSdkError::Storage { context, .. }
            if context.contains("changed during transaction")
    ));
}

#[tokio::test]
async fn test_pubky_shared_state_rejects_a_missing_previously_observed_resource() {
    let testnet = build_testnet().await;
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let access = PubkySessionBootstrap::with_pubky(testnet.sdk().unwrap())
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap()
        .access;
    let provider = TestnetSessionProvider::new(access.clone());
    let storage = PubkySharedStateStorage::new(provider.clone());
    let sdk = PaykitSdk::new(
        storage.clone(),
        provider,
        TestnetPaymentAdapter::default(),
        PaykitSdkConfig::new("bitkit").unwrap(),
    );
    sdk.initialize().await.unwrap();
    let observer = PubkySharedStateStorage::new(TestnetSessionProvider::new(access.clone()));
    let callback_error = observer
        .transaction(|_| -> paykit_sdk::Result<()> {
            Err(PaykitSdkError::Policy {
                context: "test callback failure".into(),
                source: None,
            })
        })
        .await
        .unwrap_err();
    assert!(matches!(callback_error, PaykitSdkError::Policy { .. }));
    access
        .session
        .storage()
        .delete(paykit_lib::PAYKIT_SHARED_STATE_PATH)
        .await
        .unwrap();

    let error = observer
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        PaykitSdkError::Storage { context, .. }
            if context.contains("previously observed Pubky shared state is missing")
    ));
}

fn test_app(name: &str) -> PaykitApp {
    PaykitApp::new(
        name,
        PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    )
    .unwrap()
}

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
