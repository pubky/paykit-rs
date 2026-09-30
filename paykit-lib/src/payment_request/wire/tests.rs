use super::*;
use crate::PaymentAmount;

fn app_id() -> PaykitAppId {
    PaykitAppId::new("test-app").unwrap()
}

#[test]
fn test_private_event_parse_errors_redact_plaintext() {
    let sentinel = "SENTINEL_PRIVATE_EVENT";
    let json = format!(r#"{{"version":"{sentinel}","app_id":"test-app"}}"#);
    let errors = [
        parse_payment_request_json(&json).unwrap_err(),
        parse_acceptance_json(&json).unwrap_err(),
        parse_rejection_json(&json).unwrap_err(),
        parse_cancellation_json(&json).unwrap_err(),
        parse_payment_proof_json(&json).unwrap_err(),
    ];

    for error in errors {
        assert!(matches!(
            error,
            PaykitError::InvalidData { source: None, .. }
        ));
        assert!(!format!("{error:?}").contains(sentinel));
        assert!(!error.to_string().contains(sentinel));
    }
}

fn request_terms() -> PaymentRequestTerms {
    PaymentRequestTerms::builder(
        PaymentAmount::new("0.001".to_string(), "btc".to_string()).unwrap(),
        PaymentReference::new("invoice-2026-0001").unwrap(),
        vec![PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()],
    )
    .proposal_expires_at(Some("2026-06-01T00:00:00Z".to_string()))
    .recurrence(None)
    .metadata(JsonMap::new())
    .build()
    .unwrap()
}

#[test]
fn test_payment_endpoints_wire_round_trip_and_absence() {
    let mut event = PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        request_terms(),
    );
    let raw = serialize_payment_request_json(&app_id(), &event).unwrap();
    let value: JsonValue = serde_json::from_str(&raw).unwrap();
    assert!(value["request"].get("payment_endpoints").is_none());
    assert_eq!(parse_payment_request_json(&raw).unwrap(), event);

    event.request.required_app_id = Some(app_id());
    event.request.payment_endpoints = Some(HashMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("private-invoice"),
    )]));
    let raw = serialize_payment_request_json(&app_id(), &event).unwrap();
    let value: JsonValue = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(
        value["request"]["payment_endpoints"]["btc-lightning-bolt11"],
        "private-invoice"
    );
    assert_eq!(parse_payment_request_json(&raw).unwrap(), event);
}

#[test]
fn test_payment_endpoints_wire_rejects_malformed_bindings() {
    let event = PaymentRequest::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        request_terms(),
    );
    let raw = serialize_payment_request_json(&app_id(), &event).unwrap();
    let mut value: JsonValue = serde_json::from_str(&raw).unwrap();
    value["request"]["required_app_id"] = serde_json::json!("test-app");
    for endpoints in [
        JsonValue::Null,
        serde_json::json!([]),
        serde_json::json!({}),
        serde_json::json!({"btc-lightning-bolt11": ""}),
        serde_json::json!({"btc-lightning-bolt11": 1}),
        serde_json::json!({"btc-lightning-bolt11": null}),
        serde_json::json!({"private": "private-invoice"}),
        serde_json::json!({"../btc": "private-invoice"}),
        serde_json::json!({"eur-sepa-iban": "private-iban"}),
    ] {
        value["request"]["payment_endpoints"] = endpoints;
        let error = parse_payment_request_json(&value.to_string()).unwrap_err();
        assert!(matches!(error, PaykitError::InvalidData { .. }));
        assert!(!format!("{error:?}").contains("private-invoice"));
    }
    value["request"]["payment_endpoints"] =
        serde_json::json!({"btc-lightning-bolt11": "private-invoice"});
    value["request"]["required_app_id"] = JsonValue::Null;
    assert!(matches!(
        parse_payment_request_json(&value.to_string()),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_payment_endpoints_wire_order_is_stable() {
    let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let onchain = PaymentEndpointIdentifier::new("btc-onchain").unwrap();
    let terms = PaymentRequestTerms::builder(
        PaymentAmount::new("0.001", "btc").unwrap(),
        PaymentReference::new("invoice-1").unwrap(),
        vec![lightning.clone(), onchain.clone()],
    )
    .required_app_id(Some(app_id()))
    .payment_endpoints(Some(HashMap::from([
        (onchain, PaymentEndpointPayload::new("private-address")),
        (lightning, PaymentEndpointPayload::new("private-invoice")),
    ])))
    .build()
    .unwrap();
    let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);
    let raw = serialize_payment_request_json(&app_id(), &event).unwrap();
    assert!(raw.contains(r#""payment_endpoints":{"btc-lightning-bolt11":"private-invoice","btc-onchain":"private-address"}"#));
    for _ in 0..16 {
        let parsed = parse_payment_request_json(&raw).unwrap();
        assert_eq!(
            serialize_payment_request_json(&app_id(), &parsed).unwrap(),
            raw
        );
    }
}

#[test]
fn event_header_ids_are_parsed_independently() {
    let json = r#"{
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
        }"#;

    let (_, event_id, payment_request_id) = parse_event_header(json);

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

    let (_, event_id, payment_request_id) = parse_event_header(json);

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

    let (_, event_id, payment_request_id) = parse_event_header(json);

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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Proof JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request JSON")
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
    // Internal fixture intentionally bypasses construction to test the
    // defensive serialization boundary against invalid domain state.
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request Acceptance JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request Rejection JSON")
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
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Payment Request Cancellation JSON")
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
            conversion: None,
            payment_deadline: None,
            proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
            recurrence: Some(
                Recurrence::try_from(crate::RecurrenceConfig {
                    every: 3,
                    unit: RecurrenceUnit::Week,
                    starts_at: "2026-06-01T00:00:00Z".to_string(),
                    anchor: "2026-06-01T00:00:00Z".to_string(),
                    ends_at: Some("2026-12-01T00:00:00Z".to_string()),
                })
                .unwrap(),
            ),
            ..request_terms()
        },
    );

    let json = serialize_payment_request_json(&app_id(), &event).unwrap();
    let parsed = parse_payment_request_json(&json).unwrap();
    assert_eq!(parsed, event);
}

fn payment_proof() -> PaymentProof {
    PaymentProof::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        PaymentReference::new("invoice-2026-0001").unwrap(),
        None,
        crate::PaykitAppId::new("bitkit").unwrap(),
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        JsonMap::new(),
    )
}

fn erc20_payment_proof(receipt_log_index: JsonValue) -> PaymentProof {
    let mut proof = JsonMap::new();
    proof.insert(
        "type".into(),
        JsonValue::String("erc20-transfer-eip712".into()),
    );
    proof.insert("receipt_log_index".into(), receipt_log_index);
    PaymentProof::new(
        EventId::new_v4(),
        PaymentRequestId::new_v4(),
        PaymentReference::new("invoice-2026-0001").unwrap(),
        None,
        crate::PaykitAppId::new("bitkit").unwrap(),
        PaymentEndpointIdentifier::new("usdt-arbitrum-address").unwrap(),
        proof,
    )
}

#[test]
fn test_payment_proof_round_trips_optional_allowance_id() {
    let proof = payment_proof();
    let raw =
        serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &proof).unwrap();
    assert!(!raw.contains("allowance_id"));
    assert_eq!(parse_payment_proof_json(&raw).unwrap(), proof);

    let allowance_id = AllowanceId::new_v4();
    let proof = proof.with_allowance_id(allowance_id.clone());
    let raw =
        serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &proof).unwrap();
    let parsed = parse_payment_proof_json(&raw).unwrap();
    assert_eq!(parsed.allowance_id(), Some(&allowance_id));
    assert_eq!(parsed, proof);
}

#[test]
fn test_payment_proof_rejects_noncanonical_allowance_ids_and_null() {
    let raw = serialize_payment_proof_json(
        &crate::PaykitAppId::new("bitkit").unwrap(),
        &payment_proof(),
    )
    .unwrap();
    let mut value: JsonValue = serde_json::from_str(&raw).unwrap();
    for id in [
        JsonValue::Null,
        JsonValue::from(1),
        JsonValue::from("not-a-uuid"),
        JsonValue::from("B7F9C2A1-6D43-4B0E-A8D4-0FE2C712AB44"),
        JsonValue::from("b7f9c2a16d434b0ea8d40fe2c712ab44"),
        JsonValue::from("b7f9c2a1-6d43-1b0e-a8d4-0fe2c712ab44"),
    ] {
        value["allowance_id"] = id;
        let raw = value.to_string();
        assert!(matches!(
            parse_payment_proof_json(&raw),
            Err(PaykitError::InvalidData { .. })
        ));
        let message = crate::PrivateApplicationMessage {
            app_id: Some("bitkit".into()),

            version: Some(1),
            kind: Some("paykit.payment_proof".into()),
            raw_json: raw.clone(),
        };
        let parsed = crate::parse_payment_request_event_message(&message).unwrap();
        assert!(!parsed.is_valid());
        assert_eq!(parsed.raw_json, raw);
    }
}

#[test]
fn test_payment_proof_rejects_duplicate_allowance_id() {
    let id = AllowanceId::new_v4();
    let proof = payment_proof().with_allowance_id(id.clone());
    let raw =
        serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &proof).unwrap();
    let duplicate = raw.replacen('{', &format!("{{\"allowance_id\":\"{id}\","), 1);
    assert!(matches!(
        parse_payment_proof_json(&duplicate),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_erc20_receipt_log_index_round_trips_uint256_max() {
    let maximum = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
    let proof = erc20_payment_proof(JsonValue::String(maximum.into()));

    let raw =
        serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &proof).unwrap();
    let parsed = parse_payment_proof_json(&raw).unwrap();

    assert_eq!(
        parsed
            .proof()
            .get("receipt_log_index")
            .and_then(JsonValue::as_str),
        Some(maximum)
    );
    assert_eq!(parsed, proof);
}

#[test]
fn test_erc20_receipt_log_index_rejects_noncanonical_or_out_of_range_values() {
    let valid = erc20_payment_proof(JsonValue::String("0".into()));
    let raw =
        serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &valid).unwrap();
    for invalid in [
        JsonValue::from(0),
        JsonValue::String(String::new()),
        JsonValue::String("00".into()),
        JsonValue::String("+1".into()),
        JsonValue::String(
            "115792089237316195423570985008687907853269984665640564039457584007913129639936".into(),
        ),
    ] {
        let proof = erc20_payment_proof(invalid.clone());
        assert!(matches!(
            serialize_payment_proof_json(&crate::PaykitAppId::new("bitkit").unwrap(), &proof),
            Err(PaykitError::Validation(_))
        ));

        let mut value: JsonValue = serde_json::from_str(&raw).unwrap();
        value["proof"]["receipt_log_index"] = invalid;
        assert!(matches!(
            parse_payment_proof_json(&value.to_string()),
            Err(PaykitError::InvalidData { .. })
        ));
    }
}
