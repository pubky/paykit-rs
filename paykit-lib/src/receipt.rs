mod access;
mod crypto;
mod types;
mod wire;

pub use access::{get_receipt_access, issue_receipt};
pub use crypto::decrypt_receipt;
pub use types::{IssuedReceipt, Receipt, ReceiptAccess, ReceiptDecryptionKey, ReceiptDraft};

#[cfg(test)]
use wire::{EncryptedReceiptWire, ReceiptWire};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaykitError, PaymentEndpointIdentifier, PaymentReference, PrivateMessageKind};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use chacha20poly1305::{
        aead::{Aead, AeadCore, KeyInit, OsRng},
        XChaCha20Poly1305,
    };
    use pubky_testnet::pubky::Keypair;
    use std::collections::HashMap;

    #[test]
    fn test_receipt_location_uses_payment_reference() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            ReceiptAccess::location_for(&reference),
            "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_encrypt_receipt_roundtrip_binds_location() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let recipient_public_key = Keypair::random().public_key();
        let receipt = Receipt {
            reference: reference.clone(),
            recipient_public_key,
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::from([("preimage".to_string(), "abc".to_string())]),
        };
        let location = ReceiptAccess::location_for(&reference);
        let key = ReceiptDecryptionKey::generate();

        let encrypted = receipt.encrypt(&key).unwrap();
        let decrypted = decrypt_receipt(&encrypted, &key, &location).unwrap();
        assert_eq!(decrypted, receipt);

        let wrong_location = "/pub/paykit/v0/private/receipts/650e8400-e29b-41d4-a716-446655440000";
        let err = decrypt_receipt(&encrypted, &key, wrong_location).unwrap_err();
        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }

    fn encrypt_receipt_for_test_location(
        receipt: &Receipt,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> String {
        let key_bytes = key.bytes().unwrap();
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plaintext = serde_json::to_vec(&ReceiptWire::from(receipt)).unwrap();
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &plaintext,
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
    fn test_decrypt_receipt_rejects_plaintext_reference_that_does_not_match_location() {
        let location_reference =
            PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let plaintext_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let recipient_public_key = Keypair::random().public_key();
        let receipt = Receipt {
            reference: plaintext_reference,
            recipient_public_key,
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::new(),
        };
        let location = ReceiptAccess::location_for(&location_reference);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_for_test_location(&receipt, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt Payment Reference does not match Receipt Location")),
            "expected Receipt/Receipt Location mismatch error, got: {err}"
        );
    }

    #[test]
    fn test_parse_receipt_access_json_rejects_location_that_does_not_match_reference() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let other_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference: reference.clone(),
            location: ReceiptAccess::location_for(&other_reference),
            key: ReceiptDecryptionKey::generate(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let json = wire::serialize_receipt_access_json(&access).unwrap();

        let err = wire::parse_receipt_access_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt Access location does not match Payment Reference")),
            "expected mismatched location error, got: {err}"
        );
    }

    #[test]
    fn test_receipt_decryption_key_debug_and_display_are_redacted() {
        let key = ReceiptDecryptionKey::generate();
        let raw_key = key.as_str().to_string();
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference: reference.clone(),
            location: ReceiptAccess::location_for(&reference),
            key: key.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let issued = IssuedReceipt {
            reference,
            location: access.location.clone(),
            key,
        };

        assert!(!format!("{issued:?}").contains(&raw_key));
        assert!(!format!("{access:?}").contains(&raw_key));
        assert!(!format!("{:?}", access.key).contains(&raw_key));
        assert!(!format!("{}", access.key).contains(&raw_key));
    }
}
