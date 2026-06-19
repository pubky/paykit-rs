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
            location: ReceiptAccess::location_for(&receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };

        let encrypted = receipt.encrypt(&access.key).unwrap();
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
        let encrypted_receipt = receipt.encrypt(&key).unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            event_id: EventId::new_v4(),
            receipt_id,
            payment_reference: reference,
            payment_request_id: None,
            billing_period: None,
            location: ReceiptAccess::location_for(&receipt.receipt_id),
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
        prepared.encrypted_receipt = other_receipt.encrypt(&prepared.access.key).unwrap();

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
            location: ReceiptAccess::location_for(&other_receipt_id),
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
            location: ReceiptAccess::location_for(&receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let mut value: serde_json::Value =
            serde_json::from_str(&wire::serialize_receipt_access_json(&access).unwrap()).unwrap();
        value["unexpected"] = serde_json::Value::String("not allowed".to_string());
        let json = serde_json::to_string(&value).unwrap();

        let err = wire::parse_receipt_access_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field") && context.contains("unexpected")),
            "expected unknown field error, got: {err}"
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
            serde_json::from_str(&wire::serialize_receipt_access_json(&access).unwrap()).unwrap();
        value["payment_request_id"] = serde_json::Value::Null;
        let json = serde_json::to_string(&value).unwrap();

        let err = wire::parse_receipt_access_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid type: null")),
            "expected null Payment Request ID parse error, got: {err}"
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
}
