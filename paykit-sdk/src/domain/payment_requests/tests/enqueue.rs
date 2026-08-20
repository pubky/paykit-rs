use super::*;

#[tokio::test]
async fn test_enqueue_payment_request_event_stores_canonical_payload() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let event = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "invoice-2026-0001",
        None,
        None,
    ));

    let record = enqueue_payment_request_event(
        &storage,
        counterparty.clone(),
        &app_id(),
        &event,
        timestamp(),
    )
    .await
    .unwrap();
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();

    assert_eq!(queued, vec![record.clone()]);
    assert_eq!(record.kind, "paykit.payment_request");
    assert_eq!(
        record.raw_json,
        serialize_payment_request_event(&app_id(), &event).unwrap()
    );
}

#[tokio::test]
async fn test_enqueue_payment_request_acceptance_sets_kind() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let PaymentRequestEvent::Acceptance(event) = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
    )) else {
        panic!("expected acceptance event");
    };

    let record =
        enqueue_payment_request_acceptance(&storage, counterparty, &app_id(), &event, timestamp())
            .await
            .unwrap();

    assert_eq!(record.kind, "paykit.payment_request_acceptance");
}

#[tokio::test]
async fn test_enqueue_payment_request_response_allows_only_first_app() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    ));
    let rejection = parsed_event(rejection_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
    ));

    let first = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    let err = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &paykit_lib::PaykitAppId::new("server").unwrap(),
        &rejection,
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, crate::PaykitSdkError::Policy { .. }));
    assert_eq!(
        queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap(),
        vec![first]
    );
}

#[tokio::test]
async fn test_concurrent_payment_request_responses_claim_only_one_app() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let bitkit = app_id();
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    storage
        .transaction({
            let bitkit = bitkit.clone();
            let server = server.clone();
            move |tx| {
                tx.activate_paykit_app(&bitkit);
                tx.activate_paykit_app(&server);
                Ok(())
            }
        })
        .await
        .unwrap();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    ));
    let rejection = parsed_event(rejection_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
    ));

    let accept = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &bitkit,
        &acceptance,
        timestamp(),
    );
    let reject = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &server,
        &rejection,
        timestamp(),
    );
    let (accept, reject) = tokio::join!(accept, reject);

    assert_ne!(accept.is_ok(), reject.is_ok());
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    let expected_owner = if accept.is_ok() { bitkit } else { server };
    assert_eq!(queued[0].app_id, expected_owner);
}

#[tokio::test]
async fn test_enqueue_payment_request_allows_same_app_cancellation_after_acceptance() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    ));
    let PaymentRequestEvent::Cancellation(cancellation) = parsed_event(cancellation_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
        request_id,
    )) else {
        panic!("expected cancellation event");
    };

    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    let cancellation = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &PaymentRequestEvent::Cancellation(cancellation),
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(cancellation.kind, "paykit.payment_request_cancellation");
    assert_eq!(
        queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn test_enqueue_payment_request_rejects_other_app_cancellation_after_acceptance() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    ));
    let PaymentRequestEvent::Cancellation(cancellation) = parsed_event(cancellation_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
        request_id,
    )) else {
        panic!("expected cancellation event");
    };

    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    let error = enqueue_checked_payment_request_action(
        &storage,
        counterparty,
        &paykit_lib::PaykitAppId::new("server").unwrap(),
        &PaymentRequestEvent::Cancellation(cancellation),
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, crate::PaykitSdkError::Policy { .. }));
}

#[tokio::test]
async fn test_enqueue_payment_request_rejects_invalid_terms() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let PaymentRequestEvent::Request(mut event) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "invoice-2026-0001",
        None,
        None,
    )) else {
        panic!("expected request event");
    };
    event.request.accepted_payment_endpoint_identifiers.clear();

    let err = enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &event,
        timestamp(),
    )
    .await
    .unwrap_err();
    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();

    assert!(matches!(err, crate::PaykitSdkError::Protocol { .. }));
    assert!(queued.is_empty());
}

#[tokio::test]
async fn test_checked_payment_request_action_rejects_newer_inbound_cancellation() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            ),
            cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
        ],
    )
    .await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_authorized_payment_request_apps(counterparty, vec![app_id()]);
                Ok(())
            }
        })
        .await
        .unwrap();
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    ));

    let error = enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Policy { .. }));
    assert!(queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_checked_payment_request_action_requires_app_capability() {
    let storage = InMemoryStorage::new();
    storage
        .transaction(|tx| {
            let app_id = app_id();
            tx.save_paykit_app_capabilities(
                &app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: false,
                    receipts: true,
                    outgoing_payments: true,
                },
            );
            tx.activate_paykit_app(&app_id);
            Ok(())
        })
        .await
        .unwrap();
    let acceptance = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
    ));

    let error = enqueue_checked_payment_request_action(
        &storage,
        counterparty(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Policy { .. }));
}
