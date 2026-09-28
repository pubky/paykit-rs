use super::*;
use paykit_lib::{PaymentConversion, PaymentRequestTerms};

#[tokio::test]
async fn test_conversion_quote_requires_the_proposing_payee_app() {
    let (storage, peer, request) = setup(PaymentRequestLocalRole::Payer).await;
    let quote = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
    let raw = serialize_payment_request_event(
        &paykit_lib::PaykitAppId::new("other-app").unwrap(),
        &PaymentRequestEvent::ConversionQuote(quote),
    )
    .unwrap();
    persist_messages(&storage, peer.clone(), vec![raw]).await;
    let record = payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(record.conversion_quotes.is_empty());
}

fn recurring_request() -> PaymentRequest {
    let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
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
    let base_terms = request.request();
    let mut endpoints = base_terms.accepted_payment_endpoint_identifiers().to_vec();
    endpoints.push(PaymentEndpointIdentifier::new("usdt-arbitrum-address").unwrap());
    let terms = PaymentRequestTerms::builder(
        base_terms.amount().clone(),
        base_terms.payment_reference().clone(),
        endpoints,
    )
    .proposal_expires_at(base_terms.proposal_expires_at().clone())
    .recurrence(base_terms.recurrence().clone())
    .conversion(Some(PaymentConversion::PerPeriod {}))
    .payment_deadline(base_terms.payment_deadline().cloned())
    .metadata(base_terms.metadata().clone())
    .build()
    .unwrap();
    PaymentRequest::new(
        request.event_id().clone(),
        request.payment_request_id().clone(),
        terms,
    )
}

async fn setup(role: PaymentRequestLocalRole) -> (InMemoryStorage, PubkyPublicKey, PaymentRequest) {
    let storage = registered_storage();
    let peer = counterparty();
    let request = recurring_request();
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
            request.payment_request_id().as_str(),
        )),
        role == PaymentRequestLocalRole::Payer,
    )
    .await;
    (storage, peer, request)
}
fn quote(id: &str, request: &PaymentRequest) -> PaymentConversionQuote {
    PaymentConversionQuote::new(
        EventId::new(id).unwrap(),
        request.payment_request_id().clone(),
        BillingPeriod::new("2026-06-01T00:00:00Z", "2026-07-01T00:00:00Z").unwrap(),
        vec![ConversionRate {
            asset: "usdt".into(),
            value: "50000".into(),
        }],
        "2026-06-01T00:00:00Z".into(),
        "2026-06-02T00:00:00Z".into(),
    )
    .unwrap()
}

fn quoted_proof(request: &PaymentRequest, quote: &PaymentConversionQuote) -> PaymentProof {
    quoted_proof_for_period(request, quote, quote.billing_period().clone())
}

fn quoted_proof_for_period(
    request: &PaymentRequest,
    quote: &PaymentConversionQuote,
    billing_period: BillingPeriod,
) -> PaymentProof {
    PaymentProof::new(
        EventId::new_v4(),
        request.payment_request_id().clone(),
        request.request().payment_reference().clone(),
        Some(billing_period),
        paykit_lib::PaykitAppId::new("bitkit").unwrap(),
        PaymentEndpointIdentifier::new("usdt-arbitrum-address").unwrap(),
        Default::default(),
    )
    .with_conversion_quote_id(quote.event_id().clone())
}

async fn send(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    event: PaymentRequestEvent,
    outbound: bool,
) {
    send_at(storage, peer, event, outbound, timestamp()).await;
}

async fn send_at(
    storage: &InMemoryStorage,
    peer: &PubkyPublicKey,
    event: PaymentRequestEvent,
    outbound: bool,
    recorded_at: DateTime<Utc>,
) {
    let raw =
        serialize_payment_request_event(&paykit_lib::PaykitAppId::new("bitkit").unwrap(), &event)
            .unwrap();
    if outbound {
        enqueue_untyped_private_message(storage, peer.clone(), raw, recorded_at)
            .await
            .unwrap();
    } else {
        persist_messages_at(storage, peer.clone(), vec![raw], recorded_at).await;
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
        let new = PaymentConversionQuote::new(
            EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104").unwrap(),
            request.payment_request_id().clone(),
            old.billing_period().clone(),
            vec![ConversionRate {
                asset: "usdt".into(),
                value: "60000".into(),
            }],
            old.valid_from().into(),
            old.expires_at().into(),
        )
        .unwrap();
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
        let record = payment_request_records(&storage, &peer, timestamp())
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
        assert_eq!(record.conversion_quotes[0].rates, old.rates());
        assert_eq!(
            record.payment_proofs[0].conversion_quote_id.as_deref(),
            Some(old.event_id().as_str())
        );
        // A payer can cancel after reporting an installment. Sender order must
        // survive merging with the payee's quote stream, even at tied timestamps.
        send(
            &storage,
            &peer,
            parsed_event(cancellation_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
                request.payment_request_id().as_str(),
            )),
            role == PaymentRequestLocalRole::Payer,
        )
        .await;
        let canceled = payment_request_records(&storage, &peer, timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(canceled.state, PaymentRequestLifecycleState::Canceled);
        assert_eq!(canceled.conversion_quotes.len(), 2);
        assert_eq!(canceled.payment_proofs.len(), 1);
    }
}

#[tokio::test]
async fn test_conversion_quotes_ignore_cross_stream_clock_order() {
    for role in [
        PaymentRequestLocalRole::Payer,
        PaymentRequestLocalRole::Payee,
    ] {
        let (storage, peer, request) = setup(role).await;
        let quote = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
        send_at(
            &storage,
            &peer,
            PaymentRequestEvent::ConversionQuote(quote.clone()),
            role == PaymentRequestLocalRole::Payee,
            timestamp() - ChronoDuration::seconds(1),
        )
        .await;
        send(
            &storage,
            &peer,
            PaymentRequestEvent::Proof(quoted_proof(&request, &quote)),
            role == PaymentRequestLocalRole::Payer,
        )
        .await;

        let record = payment_request_records(&storage, &peer, timestamp())
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            record.state,
            PaymentRequestLifecycleState::ActiveRecurring,
            "{role:?}: {:?}",
            record.invalid_reason
        );
        assert_eq!(record.conversion_quotes.len(), 1, "{role:?}");
        assert_eq!(
            record.payment_proofs[0].conversion_quote_id.as_deref(),
            Some(quote.event_id().as_str()),
            "{role:?}"
        );
    }
}

#[tokio::test]
async fn test_conversion_quote_rejects_payer_issuance_and_conflicting_identity() {
    for wrong_side in [true, false] {
        let (storage, peer, request) = setup(PaymentRequestLocalRole::Payer).await;
        let quote = quote("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103", &request);
        send(
            &storage,
            &peer,
            PaymentRequestEvent::ConversionQuote(quote.clone()),
            wrong_side,
        )
        .await;
        if !wrong_side {
            let quote = PaymentConversionQuote::new(
                quote.event_id().clone(),
                request.payment_request_id().clone(),
                quote.billing_period().clone(),
                vec![ConversionRate {
                    asset: "usdt".into(),
                    value: "60000".into(),
                }],
                quote.valid_from().into(),
                quote.expires_at().into(),
            )
            .unwrap();
            send(
                &storage,
                &peer,
                PaymentRequestEvent::ConversionQuote(quote),
                false,
            )
            .await;
        }
        let record = payment_request_records(&storage, &peer, timestamp())
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
        let proof = if unknown {
            quoted_proof(&request, &quote).with_conversion_quote_id(EventId::new_v4())
        } else {
            quoted_proof_for_period(
                &request,
                &quote,
                BillingPeriod::new("2026-06-02T00:00:00Z", "2026-07-01T00:00:00Z").unwrap(),
            )
        };
        send(&storage, &peer, PaymentRequestEvent::Proof(proof), false).await;
        let record = payment_request_records(&storage, &peer, timestamp())
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
    let before = payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(before.payment_proofs.len(), 1);
    let later = PaymentConversionQuote::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104").unwrap(),
        request.payment_request_id().clone(),
        BillingPeriod::new("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z").unwrap(),
        old.rates().to_vec(),
        old.valid_from().into(),
        "2026-07-02T00:00:00Z".into(),
    )
    .unwrap();
    send(
        &storage,
        &peer,
        PaymentRequestEvent::ConversionQuote(later),
        true,
    )
    .await;
    storage
        .transaction(|tx| {
            let mut outbound = tx.outbound_private_messages(&peer).last().unwrap().clone();
            outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
            outbound.last_error = Some("Encrypted Link recovery is required".into());
            tx.save_outbound_private_message(outbound)?;
            Ok(())
        })
        .await
        .unwrap();
    let after = payment_request_records(&storage, &peer, timestamp())
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
        let storage = registered_storage();
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
                enqueue_untyped_private_message(&storage, peer.clone(), raw, at)
                    .await
                    .unwrap();
            } else {
                persist_messages_at(&storage, peer.clone(), vec![raw], at).await;
            }
        }
        // The payee's cancellation or payer's proof can require delivery recovery.
        storage
            .transaction(|tx| {
                let mut outbound = tx.outbound_private_messages(&peer).last().unwrap().clone();
                outbound.status = OutboundPrivateMessageStatus::RecoveryRequired;
                outbound.last_error = Some("Encrypted Link recovery is required".into());
                tx.save_outbound_private_message(outbound)?;
                Ok(())
            })
            .await
            .unwrap();
        let record = payment_request_records(&storage, &peer, timestamp())
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
async fn test_conversion_quote_rejects_payee_cancellation() {
    let (storage, peer, request) = setup(PaymentRequestLocalRole::Payee).await;
    send(
        &storage,
        &peer,
        parsed_event(cancellation_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            request.payment_request_id().as_str(),
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
    let record = payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(record
        .invalid_reason
        .unwrap()
        .contains("payee's cancellation"));
    assert!(record.conversion_quotes.is_empty());
}

#[tokio::test]
async fn test_conversion_quote_requires_acceptance() {
    let storage = registered_storage();
    let peer = counterparty();
    let request = recurring_request();
    send(
        &storage,
        &peer,
        PaymentRequestEvent::Request(request.clone()),
        false,
    )
    .await;
    send(
        &storage,
        &peer,
        PaymentRequestEvent::ConversionQuote(quote(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            &request,
        )),
        false,
    )
    .await;
    let record = payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap()
        .remove(0);
    assert_eq!(record.state, PaymentRequestLifecycleState::InvalidConflict);
    assert!(record.invalid_reason.unwrap().contains("before acceptance"));
    let inspection = received_payment_request_records(&storage, &peer, timestamp())
        .await
        .unwrap()
        .remove(0);
    assert!(inspection.conversion_quotes.is_empty());
}
