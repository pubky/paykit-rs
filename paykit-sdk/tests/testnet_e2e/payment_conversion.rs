//! Exchange conversion terms and evidence over real Encrypted Links; no payments are executed.
use crate::harness::{deliver, linked_two_party};
use chrono::{Duration, SecondsFormat, Utc};
use paykit_lib::{
    BillingPeriod, ConversionRate, EventId, PaymentAmount, PaymentConversion,
    PaymentEndpointIdentifier, PaymentReference, PaymentRequestId, PaymentRequestTerms, Recurrence,
    RecurrenceUnit,
};
use paykit_sdk::{PaykitSdkError, PaymentProofSubmission, PaymentRequestLifecycleState};

#[tokio::test]
async fn test_recurring_quote_issuance_and_proof_delivery() {
    let pair = linked_two_party().await;
    let payer = &pair.alice;
    let payee = &pair.bob;
    let now = Utc::now();
    let text = |value: chrono::DateTime<Utc>| value.to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let period = BillingPeriod {
        starts_at: text(now),
        ends_at: text(now + Duration::days(30)),
    };
    let request = payee
        .sdk
        .propose_payment_request(
            payer.public_key.clone(),
            payer.receiver_path.clone(),
            PaymentRequestTerms {
                amount: PaymentAmount::new("10", "usd").unwrap(),
                payment_reference: PaymentReference::new("monthly-membership").unwrap(),
                proposal_expires_at: None,
                recurrence: Some(Recurrence {
                    every: 1,
                    unit: RecurrenceUnit::Month,
                    starts_at: period.starts_at.clone(),
                    anchor: period.starts_at.clone(),
                    ends_at: None,
                }),
                accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
                    "usdt-arbitrum-address",
                )
                .unwrap()],
                conversion: Some(PaymentConversion::PerPeriod {}),
                payment_deadline: None,
                metadata: Default::default(),
            },
        )
        .await
        .unwrap();
    let id = PaymentRequestId::new(request.payment_request_id).unwrap();
    deliver(payee, payer).await;
    payer
        .sdk
        .accept_payment_request(payee.public_key.clone(), payee.receiver_path.clone(), &id)
        .await
        .unwrap();
    deliver(payer, payee).await;
    let rates = vec![ConversionRate {
        asset: "usdt".into(),
        value: "1".into(),
    }];
    let expires = text(now + Duration::hours(1));
    let wrong_role = payer
        .sdk
        .quote_payment_request(
            payee.public_key.clone(),
            payee.receiver_path.clone(),
            &id,
            period.clone(),
            rates.clone(),
            expires.clone(),
        )
        .await;
    assert!(matches!(wrong_role, Err(PaykitSdkError::Policy { .. })));
    assert!(payee
        .sdk
        .quote_payment_request(
            payer.public_key.clone(),
            payer.receiver_path.clone(),
            &id,
            period.clone(),
            rates.clone(),
            text(now - Duration::seconds(1))
        )
        .await
        .is_err());
    let issued = payee
        .sdk
        .quote_payment_request(
            payer.public_key.clone(),
            payer.receiver_path.clone(),
            &id,
            period.clone(),
            rates.clone(),
            expires.clone(),
        )
        .await
        .unwrap();
    let quote = &issued.conversion_quotes[0];
    assert!(
        chrono::DateTime::parse_from_rfc3339(&quote.valid_from).unwrap()
            >= now - Duration::seconds(1)
    );
    deliver(payee, payer).await;
    let submitted = payer
        .sdk
        .submit_payment_proof_submission(
            payee.public_key.clone(),
            payee.receiver_path.clone(),
            &id,
            PaymentProofSubmission {
                billing_period: Some(period.clone()),
                payment_endpoint_identifier: PaymentEndpointIdentifier::new(
                    "usdt-arbitrum-address",
                )
                .unwrap(),
                conversion_quote_id: Some(EventId::new(&quote.event_id).unwrap()),
                allowance_id: None,
                proof: serde_json::json!({"test_evidence":"quoted-payment"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
        )
        .await
        .unwrap();
    deliver(payer, payee).await;
    let received = payee
        .sdk
        .payment_requests_with(&payer.public_key, &payer.receiver_path)
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        received.state,
        PaymentRequestLifecycleState::ActiveRecurring
    );
    assert_eq!(
        received.payment_proofs[0].event_id,
        submitted.payment_proofs[0].event_id
    );
    assert_eq!(
        received.payment_proofs[0].conversion_quote_id.as_deref(),
        Some(quote.event_id.as_str())
    );
    payee
        .sdk
        .cancel_payment_request(
            payer.public_key.clone(),
            payer.receiver_path.clone(),
            &id,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        payee
            .sdk
            .quote_payment_request(
                payer.public_key.clone(),
                payer.receiver_path.clone(),
                &id,
                period,
                rates,
                expires
            )
            .await,
        Err(PaykitSdkError::Policy { .. })
    ));
}
