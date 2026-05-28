use std::collections::HashMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{aead::OsRng, KeyInit, XChaCha20Poly1305};

use crate::{
    PaykitError, PaymentEndpointIdentifier, PaymentReference, PrivateMessageKind, PublicKey, Result,
};

/// Caller-provided receipt fields. [`issue_receipt`](crate::issue_receipt)
/// fills in the recipient public key from the established Encrypted Link before
/// encrypting storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptDraft {
    /// Payment Reference being receipted.
    pub reference: PaymentReference,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined Receipt Metadata.
    pub metadata: HashMap<String, String>,
}

/// Canonical receipt plaintext encrypted before storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Payment Reference this receipt corresponds to.
    pub reference: PaymentReference,
    /// Public key of the intended receipt recipient.
    pub recipient_public_key: PublicKey,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined Receipt Metadata.
    pub metadata: HashMap<String, String>,
}

/// Symmetric key used to decrypt an encrypted Receipt.
///
/// The key material is intentionally redacted from [`Debug`](std::fmt::Debug)
/// and [`Display`](std::fmt::Display). Use [`as_str`](Self::as_str) only when
/// serializing Receipt Access for the intended counterparty or storing the key
/// in caller-managed secure storage.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptDecryptionKey(String);

impl ReceiptDecryptionKey {
    /// Generate a fresh 256-bit Receipt Decryption Key encoded as base64url.
    pub fn generate() -> Self {
        let key = XChaCha20Poly1305::generate_key(&mut OsRng);
        Self(URL_SAFE_NO_PAD.encode(key))
    }

    /// Validate and construct a Receipt Decryption Key from base64url text.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let bytes = URL_SAFE_NO_PAD.decode(&key).map_err(|err| {
            PaykitError::Validation(format!("Receipt Decryption Key must be base64url: {err}"))
        })?;
        if bytes.len() != 32 {
            return Err(PaykitError::Validation(format!(
                "Receipt Decryption Key must decode to 32 bytes, got {}",
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

    pub(crate) fn bytes(&self) -> Result<[u8; 32]> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.0)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("Receipt Decryption Key is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| PaykitError::InvalidData {
                context: format!(
                    "Receipt Decryption Key must decode to 32 bytes, got {}",
                    bytes.len()
                ),
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
        f.write_str("[redacted Receipt Decryption Key]")
    }
}

/// Receipt Access descriptor sent over the existing Noise channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptAccess {
    /// Receipt Access envelope version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently always [`PrivateMessageKind::ReceiptAccess`].
    pub kind: PrivateMessageKind,
    /// Payment Reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted Receipt.
    pub location: String,
    /// Symmetric key needed to decrypt the Receipt.
    pub key: ReceiptDecryptionKey,
    /// Encryption algorithm. Currently `XChaCha20Poly1305`.
    pub algorithm: String,
}

/// Result returned after issuing and storing an encrypted receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedReceipt {
    /// Payment Reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted Receipt.
    pub location: String,
    /// Symmetric key needed to decrypt the Receipt.
    pub key: ReceiptDecryptionKey,
}
