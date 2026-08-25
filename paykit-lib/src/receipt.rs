mod access;
mod crypto;
mod types;
mod wire;

pub use access::{
    prepare_receipt, prepare_receipt_for_recipient, send_receipt_access, store_prepared_receipt,
};
pub use crypto::decrypt_receipt;
pub(crate) use types::RECEIPT_ENCRYPTION_ALGORITHM;
pub use types::{
    PreparedReceipt, Receipt, ReceiptAccess, ReceiptAccessEventMessage, ReceiptDecryptionKey,
    ReceiptDraft, ReceiptId,
};
pub use wire::{
    parse_receipt_access_event_message, parse_receipt_access_json, serialize_receipt_access_json,
};

#[cfg(test)]
use wire::{EncryptedReceiptWire, ReceiptWire};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BillingPeriod, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier,
        PaymentReference, PaymentRequestId, PrivateApplicationMessage, PrivateMessageKind,
        PrivateMessageParseCategory,
    };
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use chacha20poly1305::{
        aead::{Aead, AeadCore, KeyInit, OsRng},
        XChaCha20Poly1305,
    };
    use pubky_testnet::pubky::Keypair;
    use serde_json::{json, Map as JsonMap, Value as JsonValue};

    fn metadata(value: JsonValue) -> JsonMap<String, JsonValue> {
        value.as_object().cloned().expect("metadata object")
    }

    fn receiver_path() -> crate::PaykitReceiverPath {
        crate::PaykitReceiverPath::new("bitkit/wallet").unwrap()
    }

    #[test]
    fn test_receipt_location_uses_receipt_id() {
        let receipt_id = ReceiptId::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            ReceiptAccess::location(&receiver_path(), &receipt_id),
            "/pub/paykit/v0/private/bitkit/wallet/receipts/550e8400-e29b-41d4-a716-446655440000"
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
        let location = ReceiptAccess::location(&receiver_path(), &receipt_id);
        let key = ReceiptDecryptionKey::generate();

        let encrypted = receipt.encrypt(&receiver_path(), &key).unwrap();
        let decrypted = decrypt_receipt(&encrypted, &key, &location).unwrap();
        assert_eq!(decrypted, receipt);

        let wrong_location = "/pub/paykit/v0/private/receipts/650e8400-e29b-41d4-a716-446655440000";
        let err = decrypt_receipt(&encrypted, &key, wrong_location).unwrap_err();
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
            .encrypt(&receiver_path(), &ReceiptDecryptionKey::generate())
            .unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("decimal string")),
            "expected Receipt amount validation error, got: {err}"
        );
    }

    #[test]
    fn test_receipt_roundtrip_preserves_payment_request_context() {
        let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
        let payment_request_id =
            PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap();
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
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };

        let encrypted = receipt.encrypt(&receiver_path(), &access.key).unwrap();
        let decrypted = decrypt_receipt(&encrypted, &access.key, &access.location).unwrap();
        let parsed_access =
            wire::parse_receipt_access_json(&wire::serialize_receipt_access_json(&access).unwrap())
                .unwrap();

        assert_eq!(decrypted.payment_request_id, Some(payment_request_id));
        assert_eq!(decrypted.billing_period, Some(billing_period));
        assert_eq!(parsed_access, access);
    }

    fn prepared_receipt_for_test() -> PreparedReceipt {
        let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
        let reference = PaymentReference::new("invoice-2026-0001").unwrap();
        let key = ReceiptDecryptionKey::generate();
        let receipt = Receipt {
            receipt_id: receipt_id.clone(),
            payment_reference: reference.clone(),
            payment_request_id: None,
            billing_period: None,
            recipient_public_key: Keypair::random().public_key(),
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
            amount: Some(PaymentAmount::new("1000", "sats").unwrap()),
            metadata: metadata(json!({"preimage": "abc", "details": {"confirmations": 3}})),
        };
        let encrypted_receipt = receipt.encrypt(&receiver_path(), &key).unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            event_id: EventId::new_v4(),
            receipt_id,
            payment_reference: reference,
            payment_request_id: None,
            billing_period: None,
            location: ReceiptAccess::location(&receiver_path(), &receipt.receipt_id),
            key,
        };

        PreparedReceipt {
            receipt,
            encrypted_receipt,
            access,
        }
    }

    #[test]
    fn test_validate_prepared_receipt_rejects_mismatched_encrypted_payload() {
        let mut prepared = prepared_receipt_for_test();
        let mut other_receipt = prepared.receipt.clone();
        other_receipt.metadata.insert(
            "different".to_string(),
            JsonValue::String("value".to_string()),
        );
        prepared.encrypted_receipt = other_receipt
            .encrypt(&receiver_path(), &prepared.access.key)
            .unwrap();

        let err = access::validate_prepared_receipt(&prepared).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match plaintext")),
            "expected Prepared Receipt mismatch validation error, got: {err}"
        );
    }

    fn encrypt_receipt_for_test_location(
        receipt: &Receipt,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> String {
        let plaintext = serde_json::to_vec(&ReceiptWire::from(receipt)).unwrap();
        encrypt_receipt_plaintext_for_test_location(&plaintext, key, location)
    }

    fn encrypt_receipt_plaintext_for_test_location(
        plaintext: &[u8],
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> String {
        let key_bytes = key.bytes().unwrap();
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: plaintext,
                    aad: Receipt::aad_for_location(location).as_bytes(),
                },
            )
            .unwrap();
        serde_json::to_string(&EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: "XChaCha20Poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        })
        .unwrap()
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
        let location = ReceiptAccess::location(&receiver_path(), &location_receipt_id);
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
        let location = ReceiptAccess::location(&receiver_path(), &receipt_id);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse receipt plaintext JSON")),
            "expected Receipt plaintext parse error, got: {err}"
        );
    }

    #[test]
    fn test_decrypt_receipt_redacts_invalid_payment_endpoint_identifier() {
        // SECURITY / REDACTION: field validators quote the offending decrypted
        // value in their error Display, so a Receipt field-validation failure
        // must carry only the typed redacted category as `source`. The
        // sentinel must not survive into Display or Debug (Debug covers the
        // full source chain).
        const SENTINEL: &str = "SENTINEL-9f4c-DO-NOT-PRINT";
        let receipt_id = ReceiptId::new("450e8400-e29b-41d4-a716-446655440000").unwrap();
        let plaintext = serde_json::to_vec(&json!({
            "version": 1,
            "kind": "paykit.receipt",
            "receipt_id": receipt_id.as_str(),
            "payment_reference": "invoice-2026-0001",
            "recipient_public_key": Keypair::random().public_key().to_string(),
            // The trailing "!" makes the identifier invalid while keeping the
            // sentinel recognizable.
            "payment_endpoint_identifier": format!("{SENTINEL}!"),
            "amount": null,
            "metadata": {}
        }))
        .unwrap();
        let location = ReceiptAccess::location(&receiver_path(), &receipt_id);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();

        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains(SENTINEL),
            "sentinel leaked into Display: {display}"
        );
        assert!(
            !debug.contains(SENTINEL),
            "sentinel leaked into Debug: {debug}"
        );
        assert_eq!(
            err.private_message_parse_category(),
            Some(PrivateMessageParseCategory::InvalidStructure),
            "decrypted receipt field-validation failure must carry the typed category"
        );
        assert!(
            matches!(
                &err,
                PaykitError::InvalidData { context, .. }
                    if context == "Receipt contains invalid Payment Endpoint Identifier"
            ),
            "unexpected error: {err:?}"
        );
    }

    // Re-serialize an Encrypted Receipt envelope after mutating one field.
    // The envelope field set is preserved (the wire type uses
    // `deny_unknown_fields`) so the tampered JSON still parses into the
    // envelope and reaches the decrypt check under test.
    fn tamper_encrypted_envelope(
        encrypted: &str,
        mutate: impl FnOnce(&mut JsonMap<String, JsonValue>),
    ) -> String {
        let mut value: JsonValue = serde_json::from_str(encrypted).unwrap();
        mutate(
            value
                .as_object_mut()
                .expect("encrypted receipt envelope object"),
        );
        serde_json::to_string(&value).unwrap()
    }

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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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

        let err = decrypt_receipt(&tampered, &prepared.access.key, &prepared.access.location)
            .unwrap_err();
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
        let location = ReceiptAccess::location(&receiver_path(), &receipt_id);
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
        let location = ReceiptAccess::location(&receiver_path(), &receipt_id);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_plaintext_for_test_location(&plaintext, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();

        let context = match &err {
            PaykitError::InvalidData { context, .. } => context.clone(),
            other => panic!("expected InvalidData error, got {other:?}"),
        };
        assert_eq!(context, "unsupported Receipt kind");
        // The only source is the typed redacted category; the offending kind
        // value is deliberately dropped.
        assert_eq!(
            err.private_message_parse_category(),
            Some(crate::PrivateMessageParseCategory::WrongKind)
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

        let context = match &err {
            PaykitError::InvalidData { context, .. } => context.clone(),
            other => panic!("expected InvalidData error, got {other:?}"),
        };
        assert_eq!(context, "failed to parse Receipt Access JSON");
        // The only source is the typed redacted category; the serde error is
        // deliberately dropped. A string where u8 is expected is a
        // Data-category serde error, so it classifies as InvalidStructure.
        assert_eq!(
            err.private_message_parse_category(),
            Some(crate::PrivateMessageParseCategory::InvalidStructure)
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
        let location =
            "/pub/paykit/v0/private/bitkit/wallet/receipts/550e8400-e29b-41d4-a716-446655440000";

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
            location: ReceiptAccess::location(&receiver_path(), &other_receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let json = wire::serialize_receipt_access_json(&access).unwrap();

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
            location: ReceiptAccess::location(&receiver_path(), &other_receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let raw_json = wire::serialize_receipt_access_json(&access).unwrap();
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::ReceiptAccess.as_str().to_string()),
            raw_json: raw_json.clone(),
        };

        let event_message = wire::parse_receipt_access_event_message(&message).unwrap();
        assert_eq!(event_message.kind(), PrivateMessageKind::ReceiptAccess);
        assert_eq!(event_message.event_id(), Some(&event_id));
        assert_eq!(event_message.receipt_id(), Some(&receipt_id));
        assert_eq!(event_message.raw_json, raw_json);
        assert!(!event_message.is_valid());
        assert!(event_message.parsed_access().is_none());
        // The stored validation error is exactly the stable redacted category
        // string; the location-mismatch detail stays out of the wrapper.
        assert_eq!(
            event_message.validation_error(),
            Some(crate::PrivateMessageParseCategory::InvalidStructure.as_str())
        );
        assert_eq!(
            event_message.parse_category(),
            Some(crate::PrivateMessageParseCategory::InvalidStructure)
        );
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
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let raw_json = wire::serialize_receipt_access_json(&access).unwrap();
        let stale_message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::PaymentRequest.as_str().to_string()),
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
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let mut value: serde_json::Value =
            serde_json::from_str(&wire::serialize_receipt_access_json(&access).unwrap()).unwrap();
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
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let mut value: serde_json::Value =
            serde_json::from_str(&wire::serialize_receipt_access_json(&access).unwrap()).unwrap();
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
            location: ReceiptAccess::location(&receiver_path(), &other_receipt_id),
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
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
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
            payment_reference: PaymentReference::new("550e8400-e29b-41d4-a716-446655440000")
                .unwrap(),
            payment_request_id: None,
            billing_period: None,
            location: ReceiptAccess::location(&receiver_path(), &receipt_id),
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
}
