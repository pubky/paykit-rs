use super::*;

#[tokio::test]
async fn test_payment_request_records_merge_outbound_acceptance() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payer));
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert_eq!(
        records[0].accepted_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
    );
    assert_eq!(
        records[0].accepted_outbound_status,
        Some(OutboundPrivateMessageStatus::Pending)
    );
    assert_eq!(
        records[0].last_outbound_status,
        Some(OutboundPrivateMessageStatus::Pending)
    );
}

#[tokio::test]
async fn test_payment_request_records_allow_rejection_after_proposal_expiry() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            Some("2026-06-03T11:59:59Z"),
            None,
        )],
    )
    .await;
    let PaymentRequestEvent::Rejection(rejection) = parsed_event(rejection_raw(
        "8a0d8b4c-913f-4e31-bfc9-2a6f5bb4d102",
        request_id,
    )) else {
        panic!("expected rejection event");
    };
    enqueue_payment_request_rejection(
        &storage,
        counterparty.clone(),
        receiver_path(),
        &rejection,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Rejected);
    assert!(records[0].invalid_reason.is_none());
    assert_eq!(
        records[0].rejected_event_id.as_deref(),
        Some("8a0d8b4c-913f-4e31-bfc9-2a6f5bb4d102")
    );
}

#[tokio::test]
async fn test_payment_request_records_use_outbound_update_time_for_freshness() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    let updated_at = timestamp() + ChronoDuration::minutes(5);
    storage
        .transaction(|tx| {
            let mut outbound =
                tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
            outbound.status = OutboundPrivateMessageStatus::Sent;
            outbound.updated_at = updated_at;
            outbound.sent_at = Some(updated_at);
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].last_event_at, Some(updated_at));
    assert_eq!(
        records[0].last_outbound_status,
        Some(OutboundPrivateMessageStatus::Sent)
    );
}

#[tokio::test]
async fn test_payment_request_records_keep_latest_freshness_after_later_applied_inbound() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();
    let updated_at = timestamp() + ChronoDuration::minutes(5);
    storage
        .transaction(|tx| {
            let mut outbound =
                tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
            outbound.status = OutboundPrivateMessageStatus::Sent;
            outbound.updated_at = updated_at;
            outbound.sent_at = Some(updated_at);
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert_eq!(records[0].last_event_at, Some(updated_at));
}

#[tokio::test]
async fn test_payment_request_records_merge_sent_request_acceptance() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payee));
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert!(records[0].proposal_outbound_message_id.is_some());
}

#[tokio::test]
async fn test_payment_request_records_do_not_compare_independent_source_ids() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    enqueue_untyped_private_message(
        &storage,
        counterparty.clone(),
        receiver_path(),
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into(),
        timestamp(),
    )
    .await
    .unwrap();
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
        receiver_path(),
        &request,
        timestamp(),
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert_eq!(records[0].proposal_outbound_message_id, Some(1));
    assert_eq!(
        records[0].proposal_outbound_status,
        Some(OutboundPrivateMessageStatus::Pending)
    );
    assert_eq!(records[0].last_stream_item_id, Some(0));
}

#[tokio::test]
async fn test_payment_request_records_merge_outbound_proof() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
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
        receiver_path(),
        &proof,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(
        records[0].state,
        PaymentRequestLifecycleState::ProofSubmitted
    );
    assert_eq!(records[0].payment_proofs.len(), 1);
    assert_eq!(
        records[0].payment_proofs[0].outbound_status,
        Some(OutboundPrivateMessageStatus::Pending)
    );
    assert_eq!(
        records[0].last_outbound_status,
        Some(OutboundPrivateMessageStatus::Pending)
    );
    assert!(!format!("{:?}", records[0].payment_proofs[0]).contains("secret"));
}

#[tokio::test]
async fn test_payment_request_records_preserve_outbound_fifo_when_clock_moves_backward() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
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
        receiver_path(),
        &proof,
        timestamp() + ChronoDuration::minutes(1),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payee));
    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
}

#[tokio::test]
async fn test_payment_request_records_flag_later_acceptance_after_local_cancellation() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
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
        receiver_path(),
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
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
        receiver_path(),
        &cancellation,
        timestamp() + ChronoDuration::minutes(2),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
async fn test_payment_request_records_surface_recovery_required_outbound_event() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    storage
        .transaction(|tx| {
            let mut outbound =
                tx.outbound_private_messages(&counterparty, &receiver_path())[0].clone();
            outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
            outbound.updated_at = timestamp() + ChronoDuration::minutes(1);
            outbound.last_error = Some("Encrypted Link recovery is required".into());
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
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

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
async fn test_payment_request_records_ignore_duplicate_outbound_event() {
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_payment_request_acceptance(
        &storage,
        counterparty.clone(),
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &acceptance,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_payment_request_rejection(
        &storage,
        counterparty.clone(),
        receiver_path(),
        &rejection,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();
    persist_messages(&storage, counterparty.clone(), vec![raw]).await;

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
    let storage = InMemoryStorage::new();
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
        receiver_path(),
        &request,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
            malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
        ],
    )
    .await;

    let records = payment_request_records(&storage, &counterparty, &receiver_path(), timestamp())
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
