use super::*;

#[tokio::test]
async fn test_enqueue_payment_request_event_stores_canonical_payload() {
    let storage = InMemoryStorage::new();
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
        receiver_id(),
        &event,
        timestamp(),
    )
    .await
    .unwrap();
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_id())
        .await
        .unwrap();

    assert_eq!(queued, vec![record.clone()]);
    assert_eq!(record.kind, "paykit.payment_request");
    assert_eq!(
        record.raw_json,
        serialize_payment_request_event(&event).unwrap()
    );
}

#[tokio::test]
async fn test_enqueue_payment_request_acceptance_sets_kind() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let PaymentRequestEvent::Acceptance(event) = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
    )) else {
        panic!("expected acceptance event");
    };

    let record = enqueue_payment_request_acceptance(
        &storage,
        counterparty,
        receiver_id(),
        &event,
        timestamp(),
    )
    .await
    .unwrap();

    assert_eq!(record.kind, "paykit.payment_request_acceptance");
}

#[tokio::test]
async fn test_enqueue_payment_request_rejects_invalid_terms() {
    let storage = InMemoryStorage::new();
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
        receiver_id(),
        &event,
        timestamp(),
    )
    .await
    .unwrap_err();
    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_id())
        .await
        .unwrap();

    assert!(matches!(err, crate::PaykitSdkError::Protocol(_)));
    assert!(queued.is_empty());
}
