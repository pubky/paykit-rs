use super::*;

#[test]
fn test_decrypt_receipt_plaintext_parse_error_redacts_plaintext() {
    // Regression guard: serde_json errors embed verbatim document fragments
    // on type mismatches (`invalid type: string "<value>", expected u8`).
    // The document here is DECRYPTED receipt plaintext and this error's
    // context reaches the FFI (Kotlin/Swift) exception message via
    // PaykitSdkError::Protocol, so neither the context, the source chain,
    // nor Display/Debug output may carry plaintext fragments.
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let sentinel = "SENTINEL_DECRYPTED_PLAINTEXT";
    // A string where ReceiptWire expects `version: u8` makes serde quote
    // the value verbatim in its error message.
    let plaintext = serde_json::to_vec(&json!({
        "version": sentinel,
        "kind": "paykit.receipt",
    }))
    .unwrap();
    let location = ReceiptAccess::location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

    let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();

    let (context, source) = match &err {
        PaykitError::InvalidData { context, source } => (context.clone(), source),
        other => panic!("expected InvalidData error, got {other:?}"),
    };
    assert_eq!(context, "failed to parse receipt plaintext JSON");
    assert!(
        source.is_none(),
        "plaintext parse error must carry no source"
    );
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "decrypted plaintext leaked into error output: {rendered}"
    );
}

#[test]
fn test_decrypt_receipt_wire_validation_error_redacts_plaintext_kind() {
    // Regression guard: a decrypted plaintext that deserializes into
    // ReceiptWire but fails version/kind validation must not echo the
    // offending `kind` value. It is a decrypted field value, and this
    // error's context crosses the FFI boundary as exception text.
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let sentinel = "SENTINEL_PLAINTEXT_KIND";
    let receipt = Receipt {
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
        payment_request_id: None,
        billing_period: None,
        recipient_public_key: Keypair::random().public_key(),
        payment_endpoint_identifier: None,
        amount: None,
        metadata: JsonMap::new(),
    };
    let mut plaintext = serde_json::to_value(ReceiptWire::from(&receipt)).unwrap();
    plaintext["kind"] = JsonValue::String(sentinel.to_string());
    let plaintext = serde_json::to_vec(&plaintext).unwrap();
    let location = ReceiptAccess::location_for(&receipt_id);
    let key = ReceiptDecryptionKey::generate();
    let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

    let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();

    let (context, source) = match &err {
        PaykitError::InvalidData { context, source } => (context.clone(), source),
        other => panic!("expected InvalidData error, got {other:?}"),
    };
    assert_eq!(context, "unsupported Receipt version/kind");
    assert!(
        source.is_none(),
        "wire validation error must carry no source"
    );
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "decrypted kind value leaked into error output: {rendered}"
    );
}

#[test]
fn test_parse_receipt_access_json_parse_error_redacts_plaintext() {
    // Regression guard: Receipt Access JSON is decrypted private-message
    // plaintext carrying the Receipt Decryption Key and Receipt Location.
    // Its parse error can cross the FFI boundary as exception text, so it
    // must stay a static label with no serde detail attached.
    let sentinel = "SENTINEL_RECEIPT_ACCESS_PLAINTEXT";
    let json = format!("{{\"version\":\"{sentinel}\"}}");

    let err = parse_receipt_access_json(&json).unwrap_err();

    let (context, source) = match &err {
        PaykitError::InvalidData { context, source } => (context.clone(), source),
        other => panic!("expected InvalidData error, got {other:?}"),
    };
    assert_eq!(context, "failed to parse Receipt Access JSON");
    assert!(
        source.is_none(),
        "Receipt Access parse error must carry no source"
    );
    let rendered = format!("{err} / {err:?}");
    assert!(
        !rendered.contains(sentinel),
        "decrypted plaintext leaked into error output: {rendered}"
    );
}

#[test]
fn test_store_prepared_receipt_error_redacts_receipt_location() {
    // Regression guard: mirrors the paykit-sdk constructor-seam tests. The
    // Receipt Location is a DH-derived PRIVATE storage path and must never
    // appear in the error context or Display output.
    let location = "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000";

    let err = super::access::store_prepared_receipt_error(
        location,
        anyhow::anyhow!("homeserver rejected put"),
    );

    let context = match &err {
        PaykitError::Transport { context, .. } => context.clone(),
        other => panic!("expected Transport error, got {other:?}"),
    };
    assert_eq!(context, "failed to store encrypted receipt");
    let rendered = err.to_string();
    assert!(
        !rendered.contains(location),
        "Receipt Location leaked into Display: {rendered}"
    );
}

#[test]
fn test_receipt_decryption_key_debug_and_display_are_redacted() {
    let key = ReceiptDecryptionKey::generate();
    let raw_key = key.as_str().to_string();
    let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        event_id: crate::EventId::new_v4(),
        receipt_id: receipt_id.clone(),
        payment_reference: PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
        payment_request_id: None,
        billing_period: None,
        location: ReceiptAccess::location_for(&receipt_id),
        key,
    };

    assert!(!format!("{access:?}").contains(&raw_key));
    assert!(!format!("{:?}", access.key).contains(&raw_key));
    assert!(!format!("{}", access.key).contains(&raw_key));
}

#[test]
fn test_receipt_debug_redacts_plaintext_fields() {
    let prepared = prepared_receipt_for_test();
    let debug = format!("{prepared:?}");

    assert!(!debug.contains("preimage"));
    assert!(!debug.contains("1000"));
    assert!(!debug.contains(&prepared.encrypted_receipt));
    assert!(debug.contains("<redacted:"));
}
