use super::*;

fn app_id() -> PaykitAppId {
    PaykitAppId::new("test-app").unwrap()
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
fn event_header_ids_are_parsed_independently() {
    let json = r#"{
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
        }"#;

    let (event_id, payment_request_id) = parse_event_header_ids(json);

    assert_eq!(
        event_id.as_ref().map(EventId::as_str),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
    );
    assert_eq!(
        payment_request_id.as_ref().map(PaymentRequestId::as_str),
        Some("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
    );
}

#[test]
fn event_header_ids_keep_payment_request_id_when_event_id_is_missing() {
    let json = r#"{
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
        }"#;

    let (event_id, payment_request_id) = parse_event_header_ids(json);

    assert!(event_id.is_none());
    assert_eq!(
        payment_request_id.as_ref().map(PaymentRequestId::as_str),
        Some("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
    );
}

#[test]
fn event_header_ids_keep_event_id_when_payment_request_id_is_missing() {
    let json = r#"{
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101"
        }"#;

    let (event_id, payment_request_id) = parse_event_header_ids(json);

    assert_eq!(
        event_id.as_ref().map(EventId::as_str),
        Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
    );
    assert!(payment_request_id.is_none());
}

#[test]
fn payment_request_requires_explicit_nullable_fields() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("missing field"))
    );
}

#[test]
fn payment_request_rejects_unknown_top_level_field() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": {}
            },
            "ignored_extra_field": true
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field"))
    );
}

#[test]
fn payment_request_rejects_unknown_request_field() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": {},
                "unexpected": true
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field"))
    );
}

#[test]
fn payment_request_rejects_unknown_amount_field() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc", "currency": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": {}
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field"))
    );
}

#[test]
fn payment_request_rejects_non_object_metadata() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": "not-an-object"
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(matches!(err, PaykitError::InvalidData { .. }));
}

#[test]
fn payment_request_defaults_omitted_metadata_to_empty_object() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null
            }
        }"#;

    let request = parse_payment_request_json(json).unwrap();
    assert!(request.request.metadata.is_empty());
}

#[test]
fn payment_proof_requires_explicit_billing_period() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "payment_app_id": "test-app",
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": {}
        }"#;

    let err = parse_payment_proof_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("missing field"))
    );
}

#[test]
fn payment_proof_rejects_invalid_billing_period_order() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "billing_period": {
                "starts_at": "2026-07-01T00:00:00Z",
                "ends_at": "2026-06-01T00:00:00Z"
            },
            "payment_app_id": "test-app",
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": {}
        }"#;

    let err = parse_payment_proof_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn payment_proof_rejects_non_object_proof() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "billing_period": null,
            "payment_app_id": "test-app",
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": "not-an-object"
        }"#;

    let err = parse_payment_proof_json(json).unwrap_err();
    assert!(matches!(err, PaykitError::InvalidData { .. }));
}

#[test]
fn payment_request_recurrence_requires_explicit_ends_at() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": {
                    "every": 1,
                    "unit": "month",
                    "starts_at": "2026-06-01T00:00:00Z",
                    "anchor": "2026-06-01T00:00:00Z"
                },
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("missing field"))
    );
}

#[test]
fn payment_request_rejects_invalid_recurrence_window_order() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": {
                    "every": 1,
                    "unit": "month",
                    "starts_at": "2026-07-01T00:00:00Z",
                    "anchor": "2026-07-01T00:00:00Z",
                    "ends_at": "2026-06-01T00:00:00Z"
                },
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": {}
            }
        }"#;

    let err = parse_payment_request_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("ends_at must be after starts_at"))
    );
}

fn recurrence_with_ends_at(ends_at: &str) -> Recurrence {
    Recurrence {
        every: 1,
        unit: RecurrenceUnit::Month,
        starts_at: "2026-07-01T00:00:00Z".to_string(),
        anchor: "2026-07-01T00:00:00Z".to_string(),
        ends_at: Some(ends_at.to_string()),
    }
}

#[test]
fn payment_request_rejects_outgoing_recurrence_ends_at_before_starts_at() {
    let mut terms = request_terms();
    terms.recurrence = Some(recurrence_with_ends_at("2026-06-01T00:00:00Z"));
    let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);

    let err = serialize_payment_request_json(&app_id(), &event).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn payment_request_rejects_outgoing_recurrence_ends_at_equal_to_starts_at() {
    let mut terms = request_terms();
    terms.recurrence = Some(recurrence_with_ends_at("2026-07-01T00:00:00Z"));
    let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);

    let err = serialize_payment_request_json(&app_id(), &event).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
    );
}

#[test]
fn acceptance_reason_is_invalid_when_present() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_acceptance",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": "accepted"
        }"#;

    let err = parse_acceptance_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("must not include reason"))
    );
}

#[test]
fn acceptance_rejects_wrong_kind_payment_reference_field() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_acceptance",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001"
        }"#;

    let err = parse_acceptance_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field"))
    );
}

#[test]
fn rejection_reason_null_is_invalid_when_present() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_rejection",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": null
        }"#;

    let err = parse_rejection_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("reason must be a string"))
    );
}

#[test]
fn cancellation_reason_null_is_invalid_when_present() {
    let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_cancellation",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": null
        }"#;

    let err = parse_cancellation_json(json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("reason must be a string"))
    );
}

#[test]
fn payment_request_rejects_mutated_outgoing_kind() {
    let mut event = PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        request_terms(),
    );
    event.kind = PrivateMessageKind::PaymentProof;

    let err = serialize_payment_request_json(&app_id(), &event).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("kind")));
}

/// Build Payment Request JSON that is valid except for the caller-chosen
/// `proposal_expires_at` and `recurrence` JSON fragments, so each
/// value-level test below varies exactly one field.
fn payment_request_json_with(proposal_expires_at: &str, recurrence: &str) -> String {
    format!(
        r#"{{
            "version": 1,
            "kind": "paykit.payment_request",
            "app_id": "test-app",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {{
                "amount": {{ "value": "0.001", "asset": "btc" }},
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": {proposal_expires_at},
                "recurrence": {recurrence},
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "required_app_id": null,
                "metadata": {{}}
            }}
        }}"#
    )
}

// The four tests below pin the value-level Validation -> InvalidData remap
// (`invalid_wire`) for network-delivered Payment Request JSON: each payload
// is structurally valid JSON whose failure is in a field value, and the
// parser must surface `PaykitError::InvalidData` because the data arrived
// from the network (see CLAUDE.md, Error Handling).

#[test]
fn test_payment_request_rejects_zero_recurrence_every() {
    let json = payment_request_json_with(
        "null",
        r#"{
                "every": 0,
                "unit": "month",
                "starts_at": "2026-06-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
    );

    let err = parse_payment_request_json(&json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Recurrence every must be a positive integer"))
    );
}

#[test]
fn test_payment_request_rejects_unsupported_recurrence_unit() {
    let json = payment_request_json_with(
        "null",
        r#"{
                "every": 1,
                "unit": "fortnight",
                "starts_at": "2026-06-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
    );

    let err = parse_payment_request_json(&json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Recurrence unit"))
    );
}

#[test]
fn test_payment_request_rejects_unparseable_recurrence_timestamp() {
    // Z-suffixed so it passes the UTC-suffix gate and fails inside the
    // chrono RFC3339 parse (month 13 is out of range).
    let json = payment_request_json_with(
        "null",
        r#"{
                "every": 1,
                "unit": "month",
                "starts_at": "2026-13-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
    );

    let err = parse_payment_request_json(&json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Recurrence starts_at must be a valid RFC3339 timestamp"))
    );
}

#[test]
fn test_payment_request_rejects_malformed_proposal_expires_at() {
    // Exercises the missing-Z branch of parse_utc_timestamp; the
    // unparseable-recurrence test above covers the chrono branch.
    let json = payment_request_json_with(r#""not-a-timestamp""#, "null");

    let err = parse_payment_request_json(&json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("proposal_expires_at must be an RFC3339 UTC timestamp"))
    );
}

/// Deterministic wire-level positive guard so the value-level rejection
/// tests above cannot pass vacuously. Complements the probabilistic
/// `valid_event_round_trips` proptest and the networked
/// `recurring_payment_request_and_proof_with_billing_period_round_trip`
/// test, neither of which pins this seam deterministically offline.
#[test]
fn test_payment_request_round_trips_fully_valid_recurrence() {
    let event = PaymentRequest::new(
        EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101").unwrap(),
        PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
        PaymentRequestTerms {
            proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
            recurrence: Some(Recurrence {
                every: 3,
                unit: RecurrenceUnit::Week,
                starts_at: "2026-06-01T00:00:00Z".to_string(),
                anchor: "2026-06-01T00:00:00Z".to_string(),
                ends_at: Some("2026-12-01T00:00:00Z".to_string()),
            }),
            ..request_terms()
        },
    );

    let json = serialize_payment_request_json(&app_id(), &event).unwrap();
    let parsed = parse_payment_request_json(&json).unwrap();
    assert_eq!(parsed, event);
}
