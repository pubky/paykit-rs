use super::super::*;

#[tokio::test]
async fn test_payment_request_records_ignore_duplicate_outbound_event() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        )],
    )
    .await;
    let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    )) else {
        panic!("expected acceptance event");
    };
    enqueue_payment_request_acceptance(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_payment_request_acceptance(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert_eq!(
        records[0].accepted_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
    );
}

#[tokio::test]
async fn test_payment_request_records_flag_outbound_event_id_conflict() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        )],
    )
    .await;
    let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    )) else {
        panic!("expected acceptance event");
    };
    let PaymentRequestEvent::Rejection(rejection) = parsed_event(rejection_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
        request_id,
    )) else {
        panic!("expected rejection event");
    };
    enqueue_payment_request_acceptance(
        &storage,
        counterparty.clone(),
        &app_id(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_payment_request_rejection(
        &storage,
        counterparty.clone(),
        &app_id(),
        &rejection,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("Event ID")));
    assert_eq!(records[0].last_outbound_message_id, Some(1));
    assert!(records[0].last_stream_item_id.is_none());
}

#[tokio::test]
async fn test_payment_request_records_flag_outbound_reuse_of_tainted_event_id() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                inbound_request_id,
                "invoice-2026-0001",
                None,
                None,
            ),
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                inbound_request_id,
                "invoice-2026-0002",
                None,
                None,
            ),
        ],
    )
    .await;
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        outbound_request_id,
        "invoice-2026-0003",
        None,
        None,
    )) else {
        panic!("expected request event");
    };
    enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();
    let outbound = records
        .iter()
        .find(|record| record.payment_request_id == outbound_request_id)
        .unwrap();

    assert_eq!(
        outbound.state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(outbound
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("Event ID")));
}

#[tokio::test]
async fn test_payment_request_records_flag_cross_direction_duplicate_event_id() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let raw = request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
        None,
        None,
    );
    let PaymentRequestEvent::Request(request) = parsed_event(raw.clone()) else {
        panic!("expected request event");
    };
    enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();
    persist_messages(&storage, counterparty.clone(), vec![raw]).await;

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("Event ID")));
}

#[tokio::test]
async fn test_payment_request_records_keep_preinvalid_position_during_later_conflict() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                inbound_request_id,
                "invoice-2026-0001",
                None,
                None,
            ),
            malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", inbound_request_id),
        ],
    )
    .await;
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        outbound_request_id,
        "invoice-2026-0002",
        None,
        None,
    )) else {
        panic!("expected request event");
    };
    enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();
    let inbound = records
        .iter()
        .find(|record| record.payment_request_id == inbound_request_id)
        .unwrap();
    let outbound = records
        .iter()
        .find(|record| record.payment_request_id == outbound_request_id)
        .unwrap();

    assert_eq!(inbound.state, PaymentRequestLifecycleState::InvalidConflict);
    assert_eq!(inbound.last_stream_item_id, Some(1));
    assert_eq!(
        outbound.state,
        PaymentRequestLifecycleState::InvalidConflict
    );
}

#[tokio::test]
async fn test_payment_request_records_flag_outbound_reuse_of_malformed_event_id() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![malformed_cancellation_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            inbound_request_id,
        )],
    )
    .await;
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
        outbound_request_id,
        "invoice-2026-0002",
        None,
        None,
    )) else {
        panic!("expected request event");
    };
    enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();
    let outbound = records
        .iter()
        .find(|record| record.payment_request_id == outbound_request_id)
        .unwrap();

    assert_eq!(
        outbound.state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(outbound
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("Event ID")));
}

#[tokio::test]
async fn test_payment_request_records_taint_event_id_without_request_id() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![malformed_missing_request_id_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
        )],
    )
    .await;
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
        outbound_request_id,
        "invoice-2026-0002",
        None,
        None,
    )) else {
        panic!("expected request event");
    };
    enqueue_payment_request(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("Event ID")));
}

#[tokio::test]
async fn test_payment_request_records_keep_malformed_inbound_audit_position() {
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
            malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
        ],
    )
    .await;

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert_eq!(records[0].last_stream_item_id, Some(1));
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("reason must be a string")));
}
