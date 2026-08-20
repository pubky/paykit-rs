use super::*;

#[test]
fn test_parse_receipt_access_json_rejects_location_that_does_not_match_receipt_id() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let other_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id,
        payment_reference: reference.clone(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&other_receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };
    let json = wire::serialize_receipt_access_json(&app_id(), &access).unwrap();

    let err = wire::parse_receipt_access_json(&json).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt Access location does not match Receipt ID")),
        "expected mismatched location error, got: {err}"
    );
}

#[test]
fn test_receipt_access_event_message_preserves_raw_and_ids_when_body_is_invalid() {
    let event_id = EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d109").unwrap();
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let other_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: event_id.clone(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&other_receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };
    let raw_json = wire::serialize_receipt_access_json(&app_id(), &access).unwrap();
    let message = PrivateApplicationMessage {
        version: Some(1),
        kind: Some(PrivateMessageKind::ReceiptAccess.as_str().to_string()),
        app_id: Some(app_id().as_str().to_string()),
        raw_json: raw_json.clone(),
    };

    let event_message = wire::parse_receipt_access_event_message(&message).unwrap();
    assert_eq!(event_message.kind(), PrivateMessageKind::ReceiptAccess);
    assert_eq!(event_message.event_id(), Some(&event_id));
    assert_eq!(event_message.receipt_id(), Some(&receipt_id));
    assert_eq!(event_message.raw_json, raw_json);
    assert!(!event_message.is_valid());
    assert!(event_message.parsed_access().is_none());
    assert!(event_message
        .validation_error()
        .is_some_and(|err| err.contains("Receipt Access location")));
}

#[test]
fn test_receipt_access_event_parser_uses_raw_json_kind() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };
    let raw_json = wire::serialize_receipt_access_json(&app_id(), &access).unwrap();
    let stale_message = PrivateApplicationMessage {
        version: Some(1),
        kind: Some(PrivateMessageKind::PaymentRequest.as_str().to_string()),
        app_id: Some(app_id().as_str().to_string()),
        raw_json,
    };

    let event_message = wire::parse_receipt_access_event_message(&stale_message)
        .expect("raw JSON kind should route to Receipt Access parser");

    assert_eq!(event_message.kind(), PrivateMessageKind::ReceiptAccess);
    assert_eq!(event_message.parsed_access(), Some(&access));
}

#[test]
fn test_parse_receipt_access_json_rejects_unknown_fields() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&wire::serialize_receipt_access_json(&app_id(), &access).unwrap())
            .unwrap();
    value["unexpected"] = serde_json::Value::String("not allowed".to_string());
    let json = serde_json::to_string(&value).unwrap();

    let err = wire::parse_receipt_access_json(&json).unwrap_err();
    // The serde detail ("unknown field `unexpected`") is deliberately
    // redacted: the parse error must stay a static label because the input
    // is decrypted plaintext. Rejection itself is what this test pins.
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Receipt Access JSON"),
        "expected Receipt Access parse error, got: {err}"
    );
}

#[test]
fn test_parse_receipt_access_json_rejects_null_request_context_fields() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };
    let mut value: serde_json::Value =
        serde_json::from_str(&wire::serialize_receipt_access_json(&app_id(), &access).unwrap())
            .unwrap();
    value["payment_request_id"] = serde_json::Value::Null;
    let json = serde_json::to_string(&value).unwrap();

    let err = wire::parse_receipt_access_json(&json).unwrap_err();
    // The serde detail ("invalid type: null") is deliberately redacted:
    // the parse error must stay a static label because the input is
    // decrypted plaintext. Rejection itself is what this test pins.
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "failed to parse Receipt Access JSON"),
        "expected Receipt Access parse error, got: {err}"
    );
}

#[test]
fn test_receipt_access_validate_location_uses_validation_for_caller_data() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let other_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id,
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&other_receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };

    let err = access.validate_location().unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_receipt_access_validate_rejects_invalid_outgoing_header() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 2,
        kind: PrivateMessageKind::PrivatePaymentList,
        event_id: crate::EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };

    let err = access.validate().unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref reason) if reason.contains("version")),
        "expected version validation error, got: {err}"
    );

    let mut access = access;
    access.version = 1;
    let err = access.validate().unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref reason) if reason.contains("kind")),
        "expected kind validation error, got: {err}"
    );
}
