use super::*;

#[tokio::test]
async fn test_received_payment_request_records_flag_inbound_acceptance_for_received_proposal() {
    let storage = InMemoryStorage::new();
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
            acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", request_id),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert_eq!(
        records[0].terms.as_ref().unwrap().payment_reference,
        "invoice-2026-0001"
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("outbound payer event")));
    assert!(!format!("{:?}", records[0]).contains("private"));
}

#[tokio::test]
async fn test_received_payment_request_records_mark_proposal_expired() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "invoice-2026-0001",
            Some("2026-06-03T11:59:59Z"),
            None,
        )],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::ProposalExpired
    );
}

#[tokio::test]
async fn test_received_payment_request_records_flag_inbound_proof_for_received_proposal() {
    let storage = InMemoryStorage::new();
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
            proof_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "invoice-2026-0001",
            ),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("outbound payer event")));
}

#[tokio::test]
async fn test_received_payment_request_records_flag_event_id_conflict() {
    let storage = InMemoryStorage::new();
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
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0002",
                None,
                None,
            ),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
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
async fn test_received_payment_request_records_flag_invalid_transition() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![proof_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "invoice-2026-0001",
        )],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("before acceptance")));
}

#[tokio::test]
async fn test_received_payment_request_records_preserve_first_invalid_reason() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            proof_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "invoice-2026-0001",
            ),
            malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("before acceptance")));
}

#[tokio::test]
async fn test_received_payment_request_records_keep_invalid_before_later_proposal() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            proof_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "invoice-2026-0001",
            ),
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            ),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert!(records[0].terms.is_none());
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("before acceptance")));
}

#[tokio::test]
async fn test_received_payment_request_records_derive_cancellation() {
    let storage = InMemoryStorage::new();
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

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Canceled);
    assert_eq!(
        records[0].canceled_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104")
    );
}

#[tokio::test]
async fn test_received_payment_request_records_flag_second_cancellation() {
    let storage = InMemoryStorage::new();
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
            cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105", request_id),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::InvalidConflict
    );
    assert_eq!(
        records[0].canceled_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104")
    );
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("terminal state")));
}

#[tokio::test]
async fn test_received_payment_request_records_return_newest_first() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let older_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let newer_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                older_request_id,
                "invoice-2026-0001",
                None,
                None,
            ),
            request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                newer_request_id,
                "invoice-2026-0002",
                None,
                None,
            ),
        ],
    )
    .await;

    let records = received_payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].payment_request_id, newer_request_id);
    assert_eq!(records[1].payment_request_id, older_request_id);
}
