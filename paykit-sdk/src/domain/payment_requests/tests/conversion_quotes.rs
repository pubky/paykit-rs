use super::*;
use paykit_lib::PaymentConversion;

async fn setup(role: PaymentRequestLocalRole) -> (InMemoryStorage, PubkyPublicKey, PaymentRequest) {
    let storage = InMemoryStorage::new();
    let peer = counterparty();
    let PaymentRequestEvent::Request(mut request) = parsed_event(request_raw(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
        "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        "monthly",
        None,
        Some(
            r#"{"every":1,"unit":"month","starts_at":"2026-06-01T00:00:00Z","anchor":"2026-06-01T00:00:00Z","ends_at":null}"#,
        ),
    )) else {
        panic!()
    };
    request.request.conversion = Some(PaymentConversion::PerPeriod {});
    request
        .request
        .accepted_payment_endpoint_identifiers
        .push(PaymentEndpointIdentifier::new("usdt-arbitrum-address").unwrap());
    send(
        &storage,
        &peer,
        PaymentRequestEvent::Request(request.clone()),
        role == PaymentRequestLocalRole::Payee,
    )
    .await;
    send(
        &storage,
        &peer,
        parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request.payment_request_id.as_str(),
        )),
        role == PaymentRequestLocalRole::Payer,
    )
    .await;
    (storage, peer, request)
}
fn quote(id: &str, request: &PaymentRequest) -> PaymentConversionQuote {
    PaymentConversionQuote::new(
        EventId::new(id).unwrap(),
        request.payment_request_id.clone(),
        BillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".into(),
            ends_at: "2026-07-01T00:00:00Z".into(),
        },
        vec![ConversionRate {
            asset: "usdt".into(),
            value: "50000".into(),
        }],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
}

fn quoted_proof(request: &PaymentRequest, quote: &PaymentConversionQuote) -> PaymentProof {
    PaymentProof::new(
        EventId::new_v4(),
        request.payment_request_id.clone(),
        request.request.payment_reference.clone(),
        Some(quote.billing_period.clone()),
        PaymentEndpointIdentifier::new("usdt-arbitrum-address").unwrap(),
        Default::default(),
    )
    .with_conversion_quote_id(quote.event_id.clone())
}

async fn send(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    event: PaymentRequestEvent,
    outbound: bool,
) {
    let raw = serialize_payment_request_event(&event).unwrap();
    if outbound {
        enqueue_untyped_private_message(storage, peer.clone(), receiver_path(), raw, timestamp())
            .await
            .unwrap();
    } else {
        persist_messages(storage, peer.clone(), vec![raw]).await;
    }
}

#[tokio::test]
async fn test_conversion_quotes_keep_old_rates_for_delayed_proofs_and_replay() {
    for role in [
        PaymentRequestLocalRole::Payer,
        PaymentRequestLocalRole::Payee,
    ] {
        let (storage, peer, request) = setup(role).await;
        let old = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
        let mut new = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", &request);
        new.rates[0].value = "60000".into();
        for quote in [old.clone(), old.clone(), new] {
            send(
                &storage,
                &peer,
                PaymentRequestEvent::ConversionQuote(quote),
                role == PaymentRequestLocalRole::Payee,
            )
            .await;
        }
        let proof = quoted_proof(&request, &old);
        send(
            &storage,
            &peer,
            PaymentRequestEvent::Proof(proof),
            role == PaymentRequestLocalRole::Payer,
        )
        .await;
        let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            record.state,
            PaymentRequestLifecycleState::ActiveRecurring,
            "{:?}",
            record.invalid_reason
        );
        assert_eq!(record.conversion_quotes.len(), 2);
        assert_eq!(record.conversion_quotes[0].rates, old.rates);
        assert_eq!(
            record.payment_proofs[0].conversion_quote_id.as_deref(),
            Some(old.event_id.as_str())
        );
        // A payer can cancel after reporting an installment. Sender order must
        // survive merging with the payee's quote stream, even at tied timestamps.
        send(
            &storage,
            &peer,
            parsed_event(cancellation_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
                request.payment_request_id.as_str(),
            )),
            role == PaymentRequestLocalRole::Payer,
        )
        .await;
        let canceled = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(canceled.state, PaymentRequestLifecycleState::Canceled);
        assert_eq!(canceled.conversion_quotes.len(), 2);
        assert_eq!(canceled.payment_proofs.len(), 1);
    }
}

#[tokio::test]
async fn test_conversion_quote_rejects_payer_issuance_and_conflicting_identity() {
    for wrong_side in [true, false] {
        let (storage, peer, request) = setup(PaymentRequestLocalRole::Payer).await;
        let mut quote = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
        send(
            &storage,
            &peer,
            PaymentRequestEvent::ConversionQuote(quote.clone()),
            wrong_side,
        )
        .await;
        if !wrong_side {
            quote.rates[0].value = "60000".into();
            send(
                &storage,
                &peer,
                PaymentRequestEvent::ConversionQuote(quote),
                false,
            )
            .await;
        }
        let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    }
}

#[tokio::test]
async fn test_conversion_proof_cannot_select_another_period_or_unknown_quote() {
    for unknown in [true, false] {
        let (storage, peer, request) = setup(PaymentRequestLocalRole::Payee).await;
        let quote = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
        send(
            &storage,
            &peer,
            PaymentRequestEvent::ConversionQuote(quote.clone()),
            true,
        )
        .await;
        let mut proof = quoted_proof(&request, &quote);
        if unknown {
            proof.conversion_quote_id = Some(EventId::new_v4());
        } else {
            proof.billing_period.as_mut().unwrap().starts_at = "2026-06-02T00:00:00Z".into();
        }
        send(&storage, &peer, PaymentRequestEvent::Proof(proof), false).await;
        let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
        assert!(record.payment_proofs.is_empty());
    }
}

#[tokio::test]
async fn test_quote_delivery_recovery_preserves_prior_payment_evidence() {
    let (storage, peer, request) = setup(PaymentRequestLocalRole::Payee).await;
    let old = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
    send(
        &storage,
        &peer,
        PaymentRequestEvent::ConversionQuote(old.clone()),
        true,
    )
    .await;
    let proof = quoted_proof(&request, &old);
    send(&storage, &peer, PaymentRequestEvent::Proof(proof), false).await;
    let before = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(before.payment_proofs.len(), 1);
    let mut later = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", &request);
    later.billing_period.starts_at = "2026-07-01T00:00:00Z".into();
    later.billing_period.ends_at = "2026-08-01T00:00:00Z".into();
    later.expires_at = "2026-07-02T00:00:00Z".into();
    send(
        &storage,
        &peer,
        PaymentRequestEvent::ConversionQuote(later),
        true,
    )
    .await;
    storage
        .transaction(|tx| {
            let mut outbound = tx
                .outbound_private_messages(&peer, &receiver_path())
                .last()
                .unwrap()
                .clone();
            outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
            outbound.last_error = Some("Encrypted Link recovery is required".into());
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();
    let after = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(after.state, PaymentRequestLifecycleState::ActiveRecurring);
    assert_eq!(after.payment_proofs, before.payment_proofs);
}

#[tokio::test]
async fn test_cancellation_delivery_recovery_preserves_prior_payment_evidence() {
    for role in [
        PaymentRequestLocalRole::Payee,
        PaymentRequestLocalRole::Payer,
    ] {
        let storage = InMemoryStorage::new();
        let peer = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let proof_id = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103";
        let events = [
            (
                PaymentRequestLocalRole::Payee,
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice",
                    None,
                    None,
                ),
            ),
            (
                PaymentRequestLocalRole::Payer,
                acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", request_id),
            ),
            (
                PaymentRequestLocalRole::Payer,
                proof_raw(proof_id, request_id, "invoice"),
            ),
            (
                PaymentRequestLocalRole::Payee,
                cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
            ),
        ];
        for (index, (sender, raw)) in events.into_iter().enumerate() {
            let at = timestamp() + ChronoDuration::minutes(index as i64);
            if sender == role {
                enqueue_untyped_private_message(&storage, peer.clone(), receiver_path(), raw, at)
                    .await
                    .unwrap();
            } else {
                persist_messages_at(&storage, peer.clone(), vec![raw], at).await;
            }
        }
        // The payee's cancellation or payer's proof can require delivery recovery.
        storage
            .transaction(|tx| {
                let mut outbound = tx
                    .outbound_private_messages(&peer, &receiver_path())
                    .last()
                    .unwrap()
                    .clone();
                outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
                outbound.last_error = Some("Encrypted Link recovery is required".into());
                tx.save_outbound_private_message(outbound)?;
                Ok(())
            })
            .await
            .unwrap();
        let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            record.state,
            PaymentRequestLifecycleState::RecoveryRequired,
            "{role:?}"
        );
        assert_eq!(record.payment_proofs.len(), 1, "{role:?}");
        assert_eq!(record.payment_proofs[0].event_id, proof_id, "{role:?}");
    }
}

#[tokio::test]
async fn test_quote_admission_respects_acceptance_and_payee_cancellation() {
    let (storage, peer, request) = setup(PaymentRequestLocalRole::Payee).await;
    send(
        &storage,
        &peer,
        parsed_event(cancellation_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            request.payment_request_id.as_str(),
        )),
        true,
    )
    .await;
    send(
        &storage,
        &peer,
        PaymentRequestEvent::ConversionQuote(quote(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            &request,
        )),
        true,
    )
    .await;
    let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(record
        .invalid_reason
        .unwrap()
        .contains("payee's cancellation"));
    assert!(record.conversion_quotes.is_empty());

    let (storage, peer, request) = setup(PaymentRequestLocalRole::Payer).await;
    let raw = serialize_payment_request_event(&PaymentRequestEvent::ConversionQuote(quote(
        "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
        &request,
    )))
    .unwrap();
    persist_messages_at(
        &storage,
        peer.clone(),
        vec![raw],
        timestamp() - ChronoDuration::seconds(1),
    )
    .await;
    let record = payment_request_records(&storage, &peer, &receiver_path(), timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(record.invalid_reason.unwrap().contains("before acceptance"));
    let inspection =
        received_payment_request_records(&storage, &peer, &receiver_path(), timestamp())
            .await
            .unwrap()
            .remove(0);
    assert!(inspection.conversion_quotes.is_empty());
}
