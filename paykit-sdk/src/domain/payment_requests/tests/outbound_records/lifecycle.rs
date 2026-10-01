use super::super::*;

#[tokio::test]
async fn test_payment_request_records_merge_outbound_acceptance() {
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

    let records = payment_request_records(&storage, &counterparty, timestamp())
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
        &app_id(),
        &rejection,
        timestamp(),
    )
    .await
    .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
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
    let updated_at = timestamp() + ChronoDuration::minutes(5);
    storage
        .transaction(|tx| {
            let mut outbound = tx.outbound_private_messages(&counterparty)[0].clone();
            outbound.status = OutboundPrivateMessageStatus::Sent;
            outbound.updated_at = updated_at;
            outbound.sent_at = Some(updated_at);
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();

    let records = payment_request_records(&storage, &counterparty, timestamp())
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
    let updated_at = timestamp() + ChronoDuration::minutes(5);
    storage
        .transaction(|tx| {
            let mut outbound = tx.outbound_private_messages(&counterparty)[0].clone();
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

    let records = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap();

    assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
    assert_eq!(records[0].last_event_at, Some(updated_at));
}

#[tokio::test]
async fn test_payment_request_records_merge_sent_request_acceptance() {
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
    assert!(records[0].proposal_outbound_message_id.is_some());
}

#[tokio::test]
async fn test_payment_request_records_do_not_compare_independent_source_ids() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    enqueue_untyped_private_message(
        &storage,
        counterparty.clone(),
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#.into(),
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
        &app_id(),
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

    let records = payment_request_records(&storage, &counterparty, timestamp())
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
        timestamp(),
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
