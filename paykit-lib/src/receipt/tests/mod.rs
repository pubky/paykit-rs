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

mod access_messages;
mod envelope_validation;
mod redaction;
mod roundtrip;

fn app_id() -> crate::PaykitAppId {
    crate::PaykitAppId::new("test-app").unwrap()
}
use serde_json::{json, Map as JsonMap, Value as JsonValue};

fn metadata(value: JsonValue) -> JsonMap<String, JsonValue> {
    value.as_object().cloned().expect("metadata object")
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
