use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305,
};
use serde::{Deserialize, Serialize};

use crate::{
    transport, PaykitError, PaymentEndpointIdentifier, PaymentReference, PrivateMessageKind,
    PublicKey, Result,
};

/// Caller-provided receipt fields. [`crate::issue_receipt`] fills in the recipient
/// public key from the established encrypted link before encrypting storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptDraft {
    /// Private payment reference being receipted.
    pub reference: PaymentReference,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined receipt metadata.
    pub metadata: HashMap<String, String>,
}

/// Canonical receipt plaintext encrypted before storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Private payment reference this receipt corresponds to.
    pub reference: PaymentReference,
    /// Public key of the intended receipt recipient.
    pub recipient_public_key: PublicKey,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined receipt metadata.
    pub metadata: HashMap<String, String>,
}

/// Symmetric key used to decrypt an encrypted receipt payload.
///
/// The key material is intentionally redacted from [`Debug`](std::fmt::Debug)
/// and [`Display`](std::fmt::Display). Use [`as_str`](Self::as_str) only when
/// serializing receipt access for the intended counterparty or storing the key
/// in caller-managed secure storage.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptDecryptionKey(String);

impl ReceiptDecryptionKey {
    /// Generate a fresh 256-bit receipt decryption key encoded as base64url.
    pub fn generate() -> Self {
        let key = XChaCha20Poly1305::generate_key(&mut OsRng);
        Self(URL_SAFE_NO_PAD.encode(key))
    }

    /// Validate and construct a receipt decryption key from base64url text.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let bytes = URL_SAFE_NO_PAD.decode(&key).map_err(|err| {
            PaykitError::Validation(format!("receipt key must be base64url: {err}"))
        })?;
        if bytes.len() != 32 {
            return Err(PaykitError::Validation(format!(
                "receipt key must decode to 32 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self(key))
    }

    /// Access the raw base64url key material.
    ///
    /// Treat this value as secret; do not log it or include it in telemetry.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn bytes(&self) -> Result<[u8; 32]> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.0)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("receipt key is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| PaykitError::InvalidData {
                context: format!("receipt key must decode to 32 bytes, got {}", bytes.len()),
                source: None,
            })
    }
}

impl AsRef<str> for ReceiptDecryptionKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ReceiptDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReceiptDecryptionKey([redacted])")
    }
}

impl std::fmt::Display for ReceiptDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted receipt decryption key]")
    }
}

/// Receipt access descriptor sent over the existing Noise channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptAccess {
    /// Receipt access envelope version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently always [`PrivateMessageKind::ReceiptAccess`].
    pub kind: PrivateMessageKind,
    /// Private payment reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted receipt payload.
    pub location: String,
    /// Symmetric key needed to decrypt the receipt payload.
    pub key: ReceiptDecryptionKey,
    /// Encryption algorithm. Currently `XChaCha20Poly1305`.
    pub algorithm: String,
}

/// Result returned after issuing and storing an encrypted receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedReceipt {
    /// Private payment reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted receipt payload.
    pub location: String,
    /// Symmetric key needed to decrypt the receipt payload.
    pub key: ReceiptDecryptionKey,
}

#[derive(Serialize, Deserialize)]
struct ReceiptWire {
    version: u8,
    kind: String,
    reference: String,
    recipient_public_key: String,
    payment_endpoint_identifier: Option<String>,
    amount: Option<String>,
    currency: Option<String>,
    metadata: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedReceiptWire {
    version: u8,
    kind: String,
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct ReceiptAccessWire {
    version: u8,
    kind: String,
    reference: String,
    location: String,
    key: String,
    algorithm: String,
}

impl ReceiptAccess {
    /// Return the canonical homeserver storage location for a receipt reference.
    pub fn location_for(reference: &PaymentReference) -> String {
        format!(
            "{}private/receipts/{}",
            transport::pubky::PAYKIT_PATH_PREFIX,
            reference.as_str()
        )
    }

    /// Validate that this access descriptor points at the canonical location for
    /// its payment reference.
    pub fn validate_location(&self) -> Result<()> {
        let expected_location = Self::location_for(&self.reference);
        if self.location != expected_location {
            return Err(PaykitError::InvalidData {
                context: "receipt access location does not match payment reference".into(),
                source: None,
            });
        }
        Ok(())
    }
}

impl From<&Receipt> for ReceiptWire {
    fn from(receipt: &Receipt) -> Self {
        Self {
            version: 1,
            kind: "paykit.receipt".to_string(),
            reference: receipt.reference.as_str().to_string(),
            recipient_public_key: receipt.recipient_public_key.to_string(),
            payment_endpoint_identifier: receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|m| m.as_str().to_string()),
            amount: receipt.amount.clone(),
            currency: receipt.currency.clone(),
            metadata: receipt.metadata.clone(),
        }
    }
}

impl TryFrom<ReceiptWire> for Receipt {
    type Error = PaykitError;

    fn try_from(wire: ReceiptWire) -> Result<Self> {
        if wire.version != 1 || wire.kind != "paykit.receipt" {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "unsupported receipt payload version/kind: {}/{}",
                    wire.version, wire.kind
                ),
                source: None,
            });
        }
        let reference =
            PaymentReference::new(wire.reference).map_err(|err| PaykitError::InvalidData {
                context: "receipt contains invalid payment reference".into(),
                source: Some(err.into()),
            })?;
        let recipient_public_key = PublicKey::try_from(wire.recipient_public_key.as_str())
            .map_err(|err| PaykitError::InvalidData {
                context: format!("receipt contains invalid recipient public key: {err:?}"),
                source: anyhow::anyhow!("invalid recipient public key: {err:?}").into(),
            })?;
        let payment_endpoint_identifier = wire
            .payment_endpoint_identifier
            .map(PaymentEndpointIdentifier::new)
            .transpose()
            .map_err(|err| PaykitError::InvalidData {
                context: "receipt contains invalid payment method".into(),
                source: Some(err.into()),
            })?;
        Ok(Self {
            reference,
            recipient_public_key,
            payment_endpoint_identifier,
            amount: wire.amount,
            currency: wire.currency,
            metadata: wire.metadata,
        })
    }
}

impl Receipt {
    fn aad_for_location(location: &str) -> String {
        format!("paykit.receipt.v1:{location}")
    }

    /// Encrypt this receipt for storage at its canonical location using `key`.
    ///
    /// The location is derived from the receipt reference and authenticated as
    /// AEAD associated data; callers must use that same canonical location when
    /// decrypting.
    pub fn encrypt(&self, key: &ReceiptDecryptionKey) -> Result<String> {
        let location = ReceiptAccess::location_for(&self.reference);
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plaintext = serde_json::to_vec(&ReceiptWire::from(self)).map_err(|err| {
            PaykitError::InvalidData {
                context: format!("failed to serialize receipt JSON: {err}"),
                source: Some(err.into()),
            }
        })?;
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &plaintext,
                    aad: Self::aad_for_location(&location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to encrypt receipt: {err}"),
                source: None,
            })?;
        let wire = EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: "XChaCha20Poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to serialize encrypted receipt JSON: {err}"),
            source: Some(err.into()),
        })
    }

    /// Decrypt an encrypted receipt payload fetched from a homeserver.
    ///
    /// `key` and `location` normally come from a [`ReceiptAccess`] message. The
    /// location is authenticated as AEAD associated data and the decrypted
    /// receipt reference must match the canonical location.
    pub fn decrypt(
        encrypted_json: &str,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> Result<Self> {
        let wire: EncryptedReceiptWire =
            serde_json::from_str(encrypted_json).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse encrypted receipt JSON: {err}"),
                source: Some(err.into()),
            })?;
        if wire.version != 1
            || wire.kind != "paykit.receipt.encrypted"
            || wire.algorithm != "XChaCha20Poly1305"
        {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "unsupported encrypted receipt envelope version/kind/algorithm: {}/{}/{}",
                    wire.version, wire.kind, wire.algorithm
                ),
                source: None,
            });
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(wire.nonce)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("encrypted receipt nonce is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        let ciphertext =
            URL_SAFE_NO_PAD
                .decode(wire.ciphertext)
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("encrypted receipt ciphertext is not valid base64url: {err}"),
                    source: Some(err.into()),
                })?;
        if nonce.len() != 24 {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "encrypted receipt nonce must be 24 bytes, got {}",
                    nonce.len()
                ),
                source: None,
            });
        }
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let plaintext = cipher
            .decrypt(
                nonce.as_slice().into(),
                chacha20poly1305::aead::Payload {
                    msg: &ciphertext,
                    aad: Self::aad_for_location(location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to decrypt receipt: {err}"),
                source: None,
            })?;
        let receipt_wire: ReceiptWire =
            serde_json::from_slice(&plaintext).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse receipt plaintext JSON: {err}"),
                source: Some(err.into()),
            })?;
        let receipt = Self::try_from(receipt_wire)?;
        if ReceiptAccess::location_for(&receipt.reference) != location {
            return Err(PaykitError::InvalidData {
                context: "receipt reference does not match receipt location".into(),
                source: None,
            });
        }
        Ok(receipt)
    }
}

/// Decrypts an encrypted receipt payload fetched from a homeserver.
///
/// `encrypted_json` is the public receipt object stored by [`crate::issue_receipt`].
/// `key` and `location` normally come from a [`ReceiptAccess`] message received
/// with [`crate::get_receipt_access`]. The `location` is authenticated as additional
/// data, so decrypting with a different location fails even when the key and
/// ciphertext are otherwise correct.
///
/// Receipt keys are sensitive. [`ReceiptDecryptionKey`] redacts its `Debug` and
/// `Display` output, but callers must still avoid logging values returned by
/// [`ReceiptDecryptionKey::as_str`].
///
/// # Errors
/// - Returns [`PaykitError::InvalidData`] if the encrypted envelope is malformed,
///   uses an unsupported version/kind/algorithm, has invalid base64url fields,
///   fails authenticated decryption, decrypts to malformed receipt JSON, or
///   decrypts to a receipt whose reference does not match the authenticated
///   receipt location.
pub fn decrypt_receipt(
    encrypted_json: &str,
    key: &ReceiptDecryptionKey,
    location: &str,
) -> Result<Receipt> {
    Receipt::decrypt(encrypted_json, key, location)
}

impl From<&ReceiptAccess> for ReceiptAccessWire {
    fn from(access: &ReceiptAccess) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess.as_str().to_string(),
            reference: access.reference.as_str().to_string(),
            location: access.location.clone(),
            key: access.key.as_str().to_string(),
            algorithm: access.algorithm.clone(),
        }
    }
}

impl TryFrom<ReceiptAccessWire> for ReceiptAccess {
    type Error = PaykitError;

    fn try_from(wire: ReceiptAccessWire) -> Result<Self> {
        if wire.version != 1
            || wire.kind != PrivateMessageKind::ReceiptAccess.as_str()
            || wire.algorithm != "XChaCha20Poly1305"
        {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "unsupported receipt access version/kind/algorithm: {}/{}/{}",
                    wire.version, wire.kind, wire.algorithm
                ),
                source: None,
            });
        }
        let reference =
            PaymentReference::new(wire.reference).map_err(|err| PaykitError::InvalidData {
                context: "receipt access contains invalid payment reference".into(),
                source: Some(err.into()),
            })?;
        let access = Self {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference,
            location: wire.location,
            key: ReceiptDecryptionKey::new(wire.key).map_err(|err| PaykitError::InvalidData {
                context: "receipt access contains invalid decryption key".into(),
                source: Some(err.into()),
            })?,
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        access.validate_location()?;
        Ok(access)
    }
}

pub(crate) fn serialize_receipt_access_json(access: &ReceiptAccess) -> Result<String> {
    serde_json::to_string(&ReceiptAccessWire::from(access)).map_err(|err| {
        PaykitError::InvalidData {
            context: format!("failed to serialize receipt access JSON: {err}"),
            source: Some(err.into()),
        }
    })
}

pub(crate) fn parse_receipt_access_json(json: &str) -> Result<ReceiptAccess> {
    let wire: ReceiptAccessWire =
        serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse receipt access JSON: {err}"),
            source: Some(err.into()),
        })?;
    ReceiptAccess::try_from(wire)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let recipient_public_key = pubky::Keypair::random().public_key();
        let receipt = Receipt {
            reference: reference.clone(),
            recipient_public_key,
            payment_endpoint_identifier: Some(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
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
        let recipient_public_key = pubky::Keypair::random().public_key();
        let receipt = Receipt {
            reference: plaintext_reference,
            recipient_public_key,
            payment_endpoint_identifier: Some(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::new(),
        };
        let location = ReceiptAccess::location_for(&location_reference);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_for_test_location(&receipt, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("receipt reference does not match receipt location")),
            "expected receipt reference/location mismatch error, got: {err}"
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
        let json = serialize_receipt_access_json(&access).unwrap();

        let err = parse_receipt_access_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("receipt access location does not match payment reference")),
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
