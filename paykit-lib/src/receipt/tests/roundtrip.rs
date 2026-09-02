use super::*;

#[test]
fn test_receipt_location_uses_receipt_id() {
    let receipt_id = ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    assert_eq!(
        ReceiptAccess::location_for(&receipt_id),
        "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000"
    );
}

#[test]
fn test_receipt_location_rejects_non_uuid_receipt_id() {
    let err = ReceiptId::new("invoice 2026/0001").unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_encrypt_receipt_roundtrip_binds_location() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
    let recipient_public_key = Keypair::random().public_key();
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: reference.clone(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key,
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("1000", "sats").unwrap()),
        metadata: metadata(json!({"preimage": "abc", "details": {"confirmations": 3}})),
    };
    let location = ReceiptAccess::location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();

    let encrypted = receipt.encrypt(&key).unwrap();
    let decrypted = decrypt_receipt(&encrypted, &key, &location).unwrap();
    assert_eq!(decrypted, receipt);

    let wrong_location = "/pub/paykit/v0/private/receipts/650e8400-e29b-41d4-a716-446655440000";
    let err = decrypt_receipt(&encrypted, &key, wrong_location).unwrap_err();
    assert!(matches!(err, PaykitError::InvalidData { .. }));
}

#[test]
fn test_encrypt_receipt_rejects_oversized_envelope() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let receipt = Receipt {
        receipt_id,
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: Keypair::random().public_key(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: metadata(json!({
            "oversized": "x".repeat(ENCRYPTED_RECEIPT_MAX_BYTES)
        })),
    };

    let err = receipt
        .encrypt(&ReceiptDecryptionKey::generate())
        .unwrap_err();

    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_decrypt_receipt_rejects_oversized_envelope_before_parsing() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let location = ReceiptAccess::location_for(&receipt_id);
    let oversized = "x".repeat(ENCRYPTED_RECEIPT_MAX_BYTES + 1);

    let err =
        decrypt_receipt(&oversized, &ReceiptDecryptionKey::generate(), &location).unwrap_err();

    assert!(matches!(err, PaykitError::InvalidData { .. }));
}

#[test]
fn test_encrypt_receipt_rejects_invalid_amount() {
    let receipt = Receipt {
        receipt_id: ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: Keypair::random().public_key(),
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount {
            value: "ten".to_string(),
            asset: "sats".to_string(),
        }),
        metadata: JsonMap::new(),
    };

    let err = receipt
        .encrypt(&ReceiptDecryptionKey::generate())
        .unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("decimal string")),
        "expected Receipt amount validation error, got: {err}"
    );
}

#[test]
fn test_receipt_roundtrip_preserves_payment_request_context() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let payment_request_id = PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
    let billing_period = BillingPeriod {
        starts_at: "2026-06-01T00:00:00Z".to_string(),
        ends_at: "2026-07-01T00:00:00Z".to_string(),
    };
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("subscription-2026-0001").unwrap(),
        payment_request_id: Some(payment_request_id.clone()),
        billing_period: Some(billing_period.clone()),
        recipient_public_key: Keypair::random().public_key(),
        payment_endpoint_identifier: Some(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        ),
        amount: Some(PaymentAmount::new("0.001", "btc").unwrap()),
        metadata: JsonMap::new(),
    };
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: receipt.payment_reference.clone(),
        payment_request_id: Some(payment_request_id.clone()),
        billing_period: Some(billing_period.clone()),
        location: ReceiptAccess::location_for(&receipt_id),
        key: ReceiptDecryptionKey::generate(),
    };

    let encrypted = receipt.encrypt(&access.key).unwrap();
    let decrypted = decrypt_receipt(&encrypted, &access.key, &access.location).unwrap();
    let parsed_access = wire::parse_receipt_access_json(
        &wire::serialize_receipt_access_json(&app_id(), &access).unwrap(),
    )
    .unwrap();

    assert_eq!(decrypted.payment_request_id, Some(payment_request_id));
    assert_eq!(decrypted.billing_period, Some(billing_period));
    assert_eq!(parsed_access, access);
}

#[test]
fn test_validate_prepared_receipt_rejects_mismatched_encrypted_payload() {
    let mut prepared = prepared_receipt_for_test();
    let mut other_receipt = prepared.receipt.clone();
    other_receipt.metadata.insert(
        "different".to_string(),
        JsonValue::String("value".to_string()),
    );
    prepared.encrypted_receipt = other_receipt.encrypt(&prepared.access.key).unwrap();

    let err = access::validate_prepared_receipt(&prepared).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match plaintext")),
        "expected Prepared Receipt mismatch validation error, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_rejects_plaintext_receipt_id_that_does_not_match_location() {
    let location_receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let plaintext_receipt_id = ReceiptId::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
    let reference = PaymentReference::new("invoice-2026-0001").unwrap();
    let recipient_public_key = Keypair::random().public_key();
    let receipt = Receipt {
        receipt_id: plaintext_receipt_id,
        payment_reference: reference,
        payment_request_id: None,
        billing_period: None,
        recipient_public_key,
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("1000", "sats").unwrap()),
        metadata: JsonMap::new(),
    };
    let location = ReceiptAccess::location_for(&location_receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let encrypted = encrypt_receipt_for_test_location(&receipt, &key, &location);

    let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt ID does not match Receipt Location")),
        "expected Receipt/Receipt Location mismatch error, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_rejects_null_request_context_fields() {
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: Keypair::random().public_key(),
        payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
        amount: Some(PaymentAmount::new("1000", "sats").unwrap()),
        metadata: JsonMap::new(),
    };
    let mut plaintext = serde_json::to_value(ReceiptWire::from(&receipt)).unwrap();
    plaintext["payment_request_id"] = JsonValue::Null;
    let plaintext = serde_json::to_vec(&plaintext).unwrap();
    let location = ReceiptAccess::location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

    let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse receipt plaintext JSON")),
        "expected Receipt plaintext parse error, got: {err}"
    );
}

// Re-serialize an Encrypted Receipt envelope after mutating one field.
// The envelope field set is preserved (the wire type uses
// `deny_unknown_fields`) so the tampered JSON still parses into the
// envelope and reaches the decrypt check under test.
