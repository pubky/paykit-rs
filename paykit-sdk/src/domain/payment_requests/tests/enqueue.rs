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
    claim_execution(&storage, counterparty.clone(), request_id, app_id()).await;

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
async fn test_concurrent_payment_request_claims_allow_only_one_app() {
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
                tx.save_paykit_app_capabilities(
                    &server,
                    paykit_lib::PaykitAppCapabilities {
                        private_payments: true,
                        payment_requests: true,
                        receipts: true,
                        outgoing_payments: true,
                    },
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    let bitkit_claim = claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    );
    let server_claim = claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &server,
        &request_id,
        timestamp(),
    );
    let (bitkit_claim, server_claim) = tokio::join!(bitkit_claim, server_claim);

    assert_ne!(bitkit_claim.is_ok(), server_claim.is_ok());
    let claim = storage
        .transaction(move |tx| {
            Ok(tx.payment_request_execution_claim(&counterparty, request_id.as_str()))
        })
        .await
        .unwrap()
        .unwrap();
    let expected_owner = if bitkit_claim.is_ok() { bitkit } else { server };
    assert_eq!(claim.app_id, expected_owner);
}

#[tokio::test]
async fn test_open_accepted_request_can_be_released_and_claimed_by_another_app() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let bitkit = app_id();
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    register_execution_app(&storage, server.clone()).await;
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &bitkit,
        &parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id.as_str(),
        )),
        timestamp(),
    )
    .await
    .unwrap();

    let released = release_payment_request_execution_claim(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    let claimed =
        claim_payment_request_execution(&storage, counterparty, &server, &request_id, timestamp())
            .await
            .unwrap();

    assert_eq!(released.state, PaymentRequestLifecycleState::Accepted);
    assert!(released.execution_claim_app_id.is_none());
    assert_eq!(claimed.execution_claim_app_id, Some(server));
}

#[tokio::test]
async fn test_one_time_proof_finishes_execution_claim() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_authorized_request(&storage, counterparty.clone(), request_id).await;
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id.as_str(),
        )),
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &app_id(),
        &parsed_event(proof_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            request_id.as_str(),
            "invoice-2026-0001",
        )),
        timestamp(),
    )
    .await
    .unwrap();

    let record = payment_request_records(&storage, &counterparty, timestamp())
        .await
        .unwrap()
        .remove(0);
    let release = release_payment_request_execution_claim(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request_id,
        timestamp(),
    )
    .await;
    let reclaim = claim_payment_request_execution(
        &storage,
        counterparty,
        &app_id(),
        &request_id,
        timestamp(),
    )
    .await;

    assert_eq!(record.state, PaymentRequestLifecycleState::ProofSubmitted);
    assert!(record.execution_claim_app_id.is_none());
    assert!(matches!(release, Err(PaykitSdkError::Policy { .. })));
    assert!(matches!(reclaim, Err(PaykitSdkError::Policy { .. })));
}

#[tokio::test]
async fn test_expired_request_execution_claim_can_be_released() {
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
            Some("2026-06-03T12:01:00Z"),
            None,
        )],
    )
    .await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_authorized_paykit_apps(
                    counterparty,
                    HashMap::from([(app_id(), payment_request_capabilities())]),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &app_id(),
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();

    let released = release_payment_request_execution_claim(
        &storage,
        counterparty,
        &app_id(),
        &request_id,
        timestamp() + chrono::Duration::minutes(2),
    )
    .await
    .unwrap();

    assert_eq!(
        released.state,
        PaymentRequestLifecycleState::ProposalExpired
    );
    assert!(released.execution_claim_app_id.is_none());
}

#[tokio::test]
async fn test_targeted_request_can_be_claimed_by_another_payer_app_after_release() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let bitkit = app_id();
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    register_execution_app(&storage, server.clone()).await;
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    let targeted = request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        request_id,
        "invoice-2026-0001",
        None,
        None,
    )
    .replace(r#""required_app_id":null"#, r#""required_app_id":"bitkit""#);
    persist_messages(&storage, counterparty.clone(), vec![targeted]).await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_authorized_paykit_apps(
                    counterparty,
                    HashMap::from([(app_id(), payment_request_capabilities())]),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    release_payment_request_execution_claim(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();

    let claimed =
        claim_payment_request_execution(&storage, counterparty, &server, &request_id, timestamp())
            .await
            .unwrap();

    assert_eq!(claimed.execution_claim_app_id, Some(server));
}

#[tokio::test]
async fn test_recurring_claim_handoff_preserves_completed_period() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let bitkit = app_id();
    let server = paykit_lib::PaykitAppId::new("server").unwrap();
    register_execution_app(&storage, server.clone()).await;
    let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
    persist_messages(
        &storage,
        counterparty.clone(),
        vec![request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            Some(
                r#"{"every":1,"unit":"month","starts_at":"2026-06-01T00:00:00Z","anchor":"2026-06-01T00:00:00Z","ends_at":null}"#,
            ),
        )],
    )
    .await;
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                tx.save_authorized_paykit_apps(
                    counterparty,
                    HashMap::from([(app_id(), payment_request_capabilities())]),
                );
                Ok(())
            }
        })
        .await
        .unwrap();
    let request_id = paykit_lib::PaymentRequestId::new(request_id).unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &bitkit,
        &parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id.as_str(),
        )),
        timestamp(),
    )
    .await
    .unwrap();
    enqueue_checked_payment_request_action(
        &storage,
        counterparty.clone(),
        &bitkit,
        &recurring_proof_event(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            request_id.as_str(),
            "bitkit",
        ),
        timestamp(),
    )
    .await
    .unwrap();
    release_payment_request_execution_claim(
        &storage,
        counterparty.clone(),
        &bitkit,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();
    claim_payment_request_execution(
        &storage,
        counterparty.clone(),
        &server,
        &request_id,
        timestamp(),
    )
    .await
    .unwrap();

    let duplicate = enqueue_checked_payment_request_action(
        &storage,
        counterparty,
        &server,
        &recurring_proof_event(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            request_id.as_str(),
            "server",
        ),
        timestamp(),
    )
    .await
    .unwrap_err();

    assert!(matches!(duplicate, PaykitSdkError::Policy { .. }));
}

async fn register_execution_app(storage: &InMemoryStorage, app_id: paykit_lib::PaykitAppId) {
    storage
        .transaction(move |tx| {
            tx.activate_paykit_app(&app_id);
            tx.save_paykit_app_capabilities(
                &app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            );
            Ok(())
        })
        .await
        .unwrap();
}

fn recurring_proof_event(event_id: &str, request_id: &str, app_id: &str) -> PaymentRequestEvent {
    parsed_event(format!(
        r#"{{"version":1,"kind":"paykit.payment_proof","app_id":"{app_id}","event_id":"{event_id}","payment_request_id":"{request_id}","payment_reference":"invoice-2026-0001","billing_period":{{"starts_at":"2026-06-01T00:00:00Z","ends_at":"2026-07-01T00:00:00Z"}},"payment_endpoint_identifier":"btc-lightning-bolt11","payment_app_id":"{app_id}","proof":{{"txid":"secret"}}}}"#
    ))
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
    claim_execution(&storage, counterparty.clone(), request_id, app_id()).await;

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
    claim_execution(&storage, counterparty.clone(), request_id, app_id()).await;

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
                tx.save_authorized_paykit_apps(
                    counterparty,
                    HashMap::from([(app_id(), payment_request_capabilities())]),
                );
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
