use crate::{
    BillingPeriod, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier, PaymentProof,
    PaymentReference, PaymentRequest, PaymentRequestEvent, PaymentRequestId, PaymentRequestTerms,
    Recurrence, RecurrenceConfig, RecurrenceUnit,
};

fn recurrence_config() -> RecurrenceConfig {
    RecurrenceConfig {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-02-01T00:00:00Z".into(),
        anchor: "2026-03-31T00:00:00Z".into(),
        ends_at: None,
    }
}

fn terms_builder() -> crate::PaymentRequestTermsBuilder {
    PaymentRequestTerms::builder(
        PaymentAmount::new(".5", "BTC").unwrap(),
        PaymentReference::new("invoice-1").unwrap(),
        vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
    )
}

#[test]
fn test_validated_terms_preserve_existing_wire_values() {
    let endpoint = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let terms = PaymentRequestTerms::builder(
        PaymentAmount::new("000.500", "BTC").unwrap(),
        PaymentReference::new("invoice-1").unwrap(),
        vec![endpoint.clone(), endpoint],
    )
    .proposal_expires_at(Some("2000-01-01T00:00:00Z".into()))
    .recurrence(Some(Recurrence::try_from(recurrence_config()).unwrap()))
    .build()
    .unwrap();
    assert_eq!(terms.amount().value(), "000.500");
    assert_eq!(terms.amount().asset(), "BTC");
    assert_eq!(terms.accepted_payment_endpoint_identifiers().len(), 2);
    assert_eq!(
        terms.recurrence().as_ref().unwrap().anchor(),
        "2026-03-31T00:00:00Z"
    );
    let event = PaymentRequestEvent::Request(PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        terms,
    ));
    let raw = crate::serialize_payment_request_event(&event).unwrap();
    let message = crate::PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".into()),
        raw_json: raw,
    };
    assert_eq!(
        crate::parse_payment_request_event_message(&message)
            .unwrap()
            .parsed_event(),
        Some(&event)
    );
}

#[test]
fn test_terms_builder_rejects_invalid_inputs() {
    assert!(matches!(
        terms_builder()
            .proposal_expires_at(Some("invalid".into()))
            .build(),
        Err(PaykitError::Validation(_))
    ));
    assert!(PaymentRequestTerms::builder(
        PaymentAmount::new("0", "btc").unwrap(),
        PaymentReference::new("invoice-1").unwrap(),
        Vec::new()
    )
    .build()
    .is_err());
}

#[test]
fn test_recurrence_constructor_checks_complete_configuration() {
    let mut config = recurrence_config();
    config.every = 0;
    assert!(Recurrence::try_from(config).is_err());
    for end in ["invalid", "2026-02-01T00:00:00Z", "2026-01-01T00:00:00Z"] {
        let mut config = recurrence_config();
        config.ends_at = Some(end.into());
        assert!(Recurrence::try_from(config).is_err());
    }
    let mut config = recurrence_config();
    config.anchor = "2026-01-01T00:00:00+00:00".into();
    assert!(Recurrence::try_from(config).is_err());
    assert!(Recurrence::try_from(recurrence_config()).is_ok());
}

#[test]
fn test_billing_period_constructor_checks_order_and_utc() {
    assert!(BillingPeriod::new("2026-02-01T00:00:00Z", "2026-03-01T00:00:00Z").is_ok());
    for end in [
        "invalid",
        "2026-02-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
        "2026-03-01T00:00:00+00:00",
    ] {
        assert!(matches!(
            BillingPeriod::new("2026-02-01T00:00:00Z", end),
            Err(PaykitError::Validation(_))
        ));
    }
}

#[test]
fn test_valid_components_still_require_proof_request_correlation() {
    let request = PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        terms_builder().build().unwrap(),
    );
    let proof = PaymentProof::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        request.request().payment_reference().clone(),
        None,
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        serde_json::Map::new(),
    );
    assert!(proof.validate_for_request(&request).is_err());
}
