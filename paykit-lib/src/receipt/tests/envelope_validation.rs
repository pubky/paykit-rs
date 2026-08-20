use super::*;

#[test]
fn test_decrypt_receipt_wrong_key_rejected() {
    // Pins key binding: a Receipt encrypted under key A must never decrypt
    // under an unrelated key B.
    let prepared = prepared_receipt_for_test();
    let wrong_key = ReceiptDecryptionKey::generate();

    let err = decrypt_receipt(
        &prepared.encrypted_receipt,
        &wrong_key,
        &prepared.access.location,
    )
    .unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to decrypt receipt")),
        "expected AEAD decrypt failure under wrong key, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_tampered_ciphertext_rejected() {
    // Pins AEAD integrity: flipping a single ciphertext byte must fail the
    // Poly1305 tag check rather than decrypt to altered plaintext.
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        let mut ciphertext = URL_SAFE_NO_PAD
            .decode(obj.get("ciphertext").and_then(JsonValue::as_str).unwrap())
            .unwrap();
        ciphertext[0] ^= 0x01;
        obj.insert(
            "ciphertext".to_string(),
            JsonValue::String(URL_SAFE_NO_PAD.encode(&ciphertext)),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to decrypt receipt")),
        "expected AEAD integrity failure on tampered ciphertext, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_short_nonce_rejected() {
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        let mut nonce = URL_SAFE_NO_PAD
            .decode(obj.get("nonce").and_then(JsonValue::as_str).unwrap())
            .unwrap();
        nonce.truncate(12);
        obj.insert(
            "nonce".to_string(),
            JsonValue::String(URL_SAFE_NO_PAD.encode(&nonce)),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "encrypted receipt nonce must be 24 bytes"),
        "expected short-nonce rejection, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_overlong_nonce_rejected() {
    // Pins the other side of the `nonce.len() != 24` guard: a 25-byte nonce
    // must be rejected before the fixed-size XNonce conversion, which would
    // otherwise panic. A `< 24` guard would let this case through.
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        let mut nonce = URL_SAFE_NO_PAD
            .decode(obj.get("nonce").and_then(JsonValue::as_str).unwrap())
            .unwrap();
        nonce.push(0);
        obj.insert(
            "nonce".to_string(),
            JsonValue::String(URL_SAFE_NO_PAD.encode(&nonce)),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "encrypted receipt nonce must be 24 bytes"),
        "expected overlong-nonce rejection, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_non_base64url_ciphertext_rejected() {
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        obj.insert(
            "ciphertext".to_string(),
            JsonValue::String("not*base64url".to_string()),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "encrypted receipt ciphertext is not valid base64url"),
        "expected non-base64url ciphertext rejection, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_non_base64url_nonce_rejected() {
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        obj.insert(
            "nonce".to_string(),
            JsonValue::String("not*base64url".to_string()),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context == "encrypted receipt nonce is not valid base64url"),
        "expected non-base64url nonce rejection, got: {err}"
    );
}

#[test]
fn test_receipt_decryption_key_validation_messages_are_static() {
    // The base64 DecodeError names the offending byte of the candidate key
    // text; these messages are key-material adjacent and must stay static.
    let err = ReceiptDecryptionKey::new("not*base64url").unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg == "Receipt Decryption Key must be base64url"),
        "expected static base64 validation message, got: {err}"
    );
    let err = ReceiptDecryptionKey::new(URL_SAFE_NO_PAD.encode([0u8; 16])).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg == "Receipt Decryption Key must decode to 32 bytes"),
        "expected static length validation message, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_unsupported_envelope_version_rejected() {
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        obj.insert("version".to_string(), JsonValue::from(2u8));
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported encrypted receipt envelope")),
        "expected unsupported-envelope rejection, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_wrong_kind_rejected() {
    // Pins the `kind` half of the combined envelope guard: a mislabeled
    // envelope must be rejected even when its version and algorithm match.
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        obj.insert(
            "kind".to_string(),
            JsonValue::String("paykit.receipt.plaintext".to_string()),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported encrypted receipt envelope")),
        "expected unsupported-envelope rejection on wrong kind, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_wrong_algorithm_rejected() {
    // Pins the `algorithm` half of the combined envelope guard: an envelope
    // claiming a different cipher must be rejected even when its version and
    // kind match.
    let prepared = prepared_receipt_for_test();
    let tampered = tamper_encrypted_envelope(&prepared.encrypted_receipt, |obj| {
        obj.insert(
            "algorithm".to_string(),
            JsonValue::String("AES-256-GCM".to_string()),
        );
    });

    let err =
        decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location).unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported encrypted receipt envelope")),
        "expected unsupported-envelope rejection on wrong algorithm, got: {err}"
    );
}

#[test]
fn test_decrypt_receipt_malformed_envelope_json_rejected() {
    let prepared = prepared_receipt_for_test();

    let err = decrypt_receipt(
        "{\"version\": 1",
        &prepared.access.key,
        &prepared.access.location,
    )
    .unwrap_err();
    assert!(
        matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse encrypted receipt JSON")),
        "expected malformed-envelope parse rejection, got: {err}"
    );
}
