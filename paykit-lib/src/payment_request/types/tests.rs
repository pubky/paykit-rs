use super::*;

fn app_id() -> crate::PaykitAppId {
    crate::PaykitAppId::new("test-app").unwrap()
}

fn request_terms() -> PaymentRequestTerms {
    PaymentRequestTerms {
        amount: PaymentAmount {
            value: "0.001".to_string(),
            asset: "btc".to_string(),
        },
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
        recurrence: None,
        accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
            "btc-lightning-bolt11",
        )
        .unwrap()],
        required_app_id: None,
        metadata: JsonMap::new(),
    }
}

#[test]
fn payment_request_terms_reject_empty_endpoint_list() {
    let mut terms = request_terms();
    terms.accepted_payment_endpoint_identifiers.clear();
    let err = terms.validate().unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("must not be empty")));
}

#[test]
fn recurrence_rejects_non_utc_timestamp() {
    let recurrence = Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00+01:00".to_string(),
        anchor: "2026-06-01T00:00:00Z".to_string(),
        ends_at: None,
    };
    let err = recurrence.validate().unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("Z suffix")));
}

fn recurrence_with_ends_at(ends_at: Option<&str>) -> Recurrence {
    Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-07-01T00:00:00Z".to_string(),
        anchor: "2026-07-01T00:00:00Z".to_string(),
        ends_at: ends_at.map(str::to_string),
    }
}

#[test]
fn recurrence_rejects_ends_at_before_starts_at() {
    // specs/payment-requests.md: non-null ends_at MUST be after starts_at.
    let recurrence = recurrence_with_ends_at(Some("2026-06-01T00:00:00Z"));
    let err = recurrence.validate().unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn recurrence_rejects_ends_at_equal_to_starts_at() {
    let recurrence = recurrence_with_ends_at(Some("2026-07-01T00:00:00Z"));
    let err = recurrence.validate().unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn recurrence_accepts_ends_at_after_starts_at() {
    let recurrence = recurrence_with_ends_at(Some("2026-08-01T00:00:00Z"));
    recurrence.validate().unwrap();
}

#[test]
fn payment_reference_is_not_required_to_be_uuid() {
    let terms = request_terms();
    assert_eq!(terms.payment_reference.as_str(), "invoice-2026-0001");
}

#[test]
fn payment_request_event_debug_redacts_private_payloads() {
    let request = PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        PaymentRequestTerms {
            metadata: JsonMap::from_iter([(
                "note".to_string(),
                JsonValue::String("private request note".to_string()),
            )]),
            ..request_terms()
        },
    );
    let proof = PaymentProof::new(
        EventId::new_v4(),
        request.payment_request_id.clone(),
        request.request.payment_reference.clone(),
        None,
        app_id(),
        request.request.accepted_payment_endpoint_identifiers[0].clone(),
        JsonMap::from_iter([(
            "preimage".to_string(),
            JsonValue::String("private proof secret".to_string()),
        )]),
    );
    let message = PaymentRequestEventMessage {
        kind: PrivateMessageKind::PaymentProof,
        app_id: Some(app_id()),
        event_id: Some(proof.event_id.clone()),
        payment_request_id: Some(proof.payment_request_id.clone()),
        raw_json: r#"{"proof":{"preimage":"raw proof secret"}}"#.to_string(),
        event: Ok(PaymentRequestEvent::Proof(proof.clone())),
    };

    assert!(!format!("{request:?}").contains("invoice-2026-0001"));
    assert!(!format!("{proof:?}").contains("invoice-2026-0001"));
    assert!(!format!("{request:?}").contains("private request note"));
    assert!(!format!("{proof:?}").contains("private proof secret"));
    let debug = format!("{message:?}");
    assert!(!debug.contains("raw proof secret"));
    assert!(!debug.contains("private proof secret"));
    assert!(debug.contains("<redacted:"));
}

fn payment_request() -> PaymentRequest {
    PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        request_terms(),
    )
}

fn payment_proof_for(request: &PaymentRequest) -> PaymentProof {
    PaymentProof::new(
        EventId::new_v4(),
        request.payment_request_id.clone(),
        request.request.payment_reference.clone(),
        None,
        app_id(),
        request.request.accepted_payment_endpoint_identifiers[0].clone(),
        JsonMap::new(),
    )
}

#[test]
fn payment_proof_validates_for_matching_one_time_request() {
    let request = payment_request();
    let proof = payment_proof_for(&request);
    proof.validate_for_request(&request).unwrap();
}

#[test]
fn payment_proof_rejects_mismatched_reference() {
    let request = payment_request();
    let mut proof = payment_proof_for(&request);
    proof.payment_reference = PaymentReference::new("other-reference").unwrap();
    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("payment_reference")));
}

#[test]
fn payment_proof_rejects_mismatched_request_id() {
    let request = payment_request();
    let mut proof = payment_proof_for(&request);
    proof.payment_request_id = PaymentRequestId::new_v4();
    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("payment_request_id")));
}

#[test]
fn payment_proof_rejects_unaccepted_endpoint() {
    let request = payment_request();
    let mut proof = payment_proof_for(&request);
    proof.payment_endpoint_identifier = PaymentEndpointIdentifier::new("btc-onchain").unwrap();
    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("not accepted")));
}

#[test]
fn payment_proof_rejects_billing_period_for_one_time_request() {
    let request = payment_request();
    let mut proof = payment_proof_for(&request);
    proof.billing_period = Some(BillingPeriod {
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        ends_at: "2026-07-01T00:00:00Z".to_string(),
    });
    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("one-time")));
}

#[test]
fn payment_proof_rejects_invalid_billing_period_shape() {
    let mut request = payment_request();
    request.request.recurrence = Some(Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        anchor: "2026-06-01T00:00:00Z".to_string(),
        ends_at: None,
    });
    let mut proof = payment_proof_for(&request);
    proof.billing_period = Some(BillingPeriod {
        starts_at: "2026-07-01T00:00:00Z".to_string(),
        ends_at: "2026-06-01T00:00:00Z".to_string(),
    });

    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn payment_proof_requires_billing_period_for_recurring_request() {
    let mut request = payment_request();
    request.request.recurrence = Some(Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        anchor: "2026-06-01T00:00:00Z".to_string(),
        ends_at: None,
    });
    let proof = payment_proof_for(&request);
    let err = proof.validate_for_request(&request).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("required")));
}

#[test]
fn payment_proof_validates_for_recurring_request_with_billing_period() {
    let mut request = payment_request();
    request.request.recurrence = Some(Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        anchor: "2026-06-01T00:00:00Z".to_string(),
        ends_at: None,
    });
    let mut proof = payment_proof_for(&request);
    proof.billing_period = Some(BillingPeriod {
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        ends_at: "2026-07-01T00:00:00Z".to_string(),
    });
    proof.validate_for_request(&request).unwrap();
}
