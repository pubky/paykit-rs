use super::super::*;

#[tokio::test]
async fn test_payment_request_records_preserve_outbound_fifo_when_clock_moves_backward() {
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
        timestamp() + ChronoDuration::minutes(5),
    )
    .await
    .unwrap();
    let PaymentRequestEvent::Proof(proof) = parsed_event(proof_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
        "invoice-2026-0001",
    )) else {
        panic!("expected proof event");
    };
    enqueue_payment_proof(
        &storage,
        counterparty.clone(),
        &app_id(),
        &proof,
        timestamp() + ChronoDuration::minutes(1),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::ProofSubmitted
    );
    assert_eq!(records[0].payment_proofs.len(), 1);
}

#[tokio::test]
async fn test_payment_request_records_apply_proposal_before_cross_source_acceptance() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
        timestamp() + ChronoDuration::minutes(5),
    )
    .await
    .unwrap();
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )],
    )
    .await;

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payee));
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
}

#[tokio::test]
async fn test_payment_request_records_flag_later_acceptance_after_local_cancellation() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
    let PaymentRequestEvent::Cancellation(cancellation) = parsed_event(cancellation_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
    )) else {
        panic!("expected cancellation event");
    };
    enqueue_payment_request_cancellation(
        &storage,
        counterparty.clone(),
        &app_id(),
        &cancellation,
        timestamp() + ChronoDuration::minutes(1),
    )
    .await
    .unwrap();
    persist_messages_at(
        &storage,
        counterparty.clone(),
        vec![acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )],
        timestamp() + ChronoDuration::minutes(2),
    )
    .await;

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
        .is_some_and(|reason| reason.contains("acceptance arrived after transition")));
}

#[tokio::test]
async fn test_payment_request_records_apply_later_cancellation_after_acceptance() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
    persist_messages_at(
        &storage,
        counterparty.clone(),
        vec![acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )],
        timestamp() + ChronoDuration::minutes(1),
    )
    .await;
    let PaymentRequestEvent::Cancellation(cancellation) = parsed_event(cancellation_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
    )) else {
        panic!("expected cancellation event");
    };
    enqueue_payment_request_cancellation(
        &storage,
        counterparty.clone(),
        &app_id(),
        &cancellation,
        timestamp() + ChronoDuration::minutes(2),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Canceled);
    assert_eq!(
        records[0].accepted_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
    );
    assert_eq!(
        records[0].canceled_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103")
    );
}

#[tokio::test]
async fn test_payment_request_records_allow_open_request_proof_from_another_app() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
    persist_messages_at(
        &storage,
        counterparty.clone(),
        vec![
            acceptance_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                request_id,
                "first-payer",
            ),
            acceptance_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "competing-payer",
            ),
            rejection_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
                request_id,
                "competing-payer",
            ),
            cancellation_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
                request_id,
                "competing-payer",
            ),
            proof_raw_for_apps(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d106",
                request_id,
                "invoice-2026-0001",
                "competing-payer",
                "bitkit",
            ),
        ],
        timestamp() + ChronoDuration::minutes(1),
    )
    .await;

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::ProofSubmitted
    );
    assert_eq!(
        records[0]
            .payer_app_id
            .as_ref()
            .map(|app_id| app_id.as_str()),
        Some("competing-payer")
    );
    assert_eq!(
        records[0].accepted_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
    );
    assert!(records[0].rejected_event_id.is_none());
    assert!(records[0].canceled_event_id.is_none());
    assert_eq!(records[0].payment_proofs.len(), 1);
    assert_eq!(
        records[0].payment_proofs[0].payment_app_id.as_str(),
        "bitkit"
    );
    assert!(records[0].invalid_reason.is_none());
}

#[tokio::test]
async fn test_payment_request_records_preserve_first_payer_app_rejection() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
    persist_messages_at(
        &storage,
        counterparty.clone(),
        vec![
            rejection_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                request_id,
                "first-payer",
            ),
            acceptance_raw_for_app(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "competing-payer",
            ),
        ],
        timestamp() + ChronoDuration::minutes(1),
    )
    .await;

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Rejected);
    assert_eq!(
        records[0]
            .payer_app_id
            .as_ref()
            .map(|app_id| app_id.as_str()),
        Some("first-payer")
    );
    assert_eq!(
        records[0].rejected_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
    );
    assert!(records[0].accepted_event_id.is_none());
    assert!(records[0].invalid_reason.is_none());
}

#[tokio::test]
async fn test_queue_payment_request_cancellation_claims_first_local_transition() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Cancellation(cancellation) = parsed_event(cancellation_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        request_id,
    )) else {
        panic!("expected cancellation event");
    };
    enqueue_payment_request_cancellation(
        &storage,
        counterparty.clone(),
        &app_id(),
        &cancellation,
        timestamp(),
    )
    .await
    .unwrap();
    let other_app = paykit_lib::PaykitAppId::new("other-app").unwrap();
    let result = enqueue_payment_request_cancellation(
        &storage,
        counterparty,
        &other_app,
        &cancellation,
        timestamp() + ChronoDuration::seconds(1),
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_payment_request_records_surface_recovery_required_outbound_event() {
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
    storage
        .transaction(|tx| {
            let mut outbound = tx.outbound_private_messages(&counterparty)[0].clone();
            outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
            outbound.updated_at = timestamp() + ChronoDuration::minutes(1);
            outbound.last_error = Some("Encrypted Link recovery is required".into());
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::RecoveryRequired
    );
    assert_eq!(
        records[0].accepted_outbound_status,
        Some(OutboundPrivateMessageStatus::RecoveryRequired)
    );
    assert_eq!(
        records[0].last_outbound_status,
        Some(OutboundPrivateMessageStatus::RecoveryRequired)
    );
}

#[tokio::test]
async fn test_payment_request_records_preserve_inbound_fifo_with_same_timestamp() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
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
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![
            proof_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                request_id,
                "invoice-2026-0001",
            ),
            acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", request_id),
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
    assert!(records[0]
        .invalid_reason
        .as_ref()
        .is_some_and(|reason| reason.contains("before acceptance")));
}
