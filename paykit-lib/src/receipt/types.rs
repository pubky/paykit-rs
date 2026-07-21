use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{aead::OsRng, KeyInit, XChaCha20Poly1305};
use serde_json::{Map as JsonMap, Value as JsonValue};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    validation::validate_uuid_v4, BillingPeriod, EventId, PaykitError, PaymentAmount,
    PaymentEndpointIdentifier, PaymentReference, PaymentRequestId, PrivateMessageKind, PublicKey,
    Result,
};

pub(crate) const RECEIPT_ENCRYPTION_ALGORITHM: &str = "XChaCha20Poly1305";

/// UUID-v4 identifier for one stored Receipt artifact.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReceiptId(String);

impl ReceiptId {
    /// Create a Receipt ID from a UUID-v4 string.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        validate_uuid_v4(id.into(), "Receipt ID").map(Self)
    }

    /// Generate a fresh Receipt ID.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the canonical UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReceiptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ReceiptId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Caller-provided receipt fields. [`prepare_receipt`](crate::prepare_receipt)
/// fills in the recipient public key from the established Encrypted Link before
/// encrypting storage.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptDraft {
    /// Optional Receipt ID. [`prepare_receipt`](crate::prepare_receipt)
    /// generates one when this is `None`.
    pub receipt_id: Option<ReceiptId>,
    /// Payment Reference being receipted.
    pub payment_reference: PaymentReference,
    /// Optional Payment Request ID this receipt corresponds to.
    pub payment_request_id: Option<PaymentRequestId>,
    /// Optional Billing Period for recurring Payment Request receipts.
    pub billing_period: Option<BillingPeriod>,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional Payment Amount being receipted.
    pub amount: Option<PaymentAmount>,
    /// Caller-defined Receipt Metadata as a JSON object.
    pub metadata: JsonMap<String, JsonValue>,
}

impl fmt::Debug for ReceiptDraft {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptDraft")
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .finish()
    }
}

/// Canonical receipt plaintext encrypted before storage.
#[derive(Clone, PartialEq, Eq)]
pub struct Receipt {
    /// Receipt artifact identifier.
    pub receipt_id: ReceiptId,
    /// Payment Reference this receipt corresponds to.
    pub payment_reference: PaymentReference,
    /// Optional Payment Request ID this receipt corresponds to.
    pub payment_request_id: Option<PaymentRequestId>,
    /// Optional Billing Period for recurring Payment Request receipts.
    pub billing_period: Option<BillingPeriod>,
    /// Public key of the intended receipt recipient.
    pub recipient_public_key: PublicKey,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional Payment Amount this receipt corresponds to.
    pub amount: Option<PaymentAmount>,
    /// Caller-defined Receipt Metadata as a JSON object.
    pub metadata: JsonMap<String, JsonValue>,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receipt")
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("recipient_public_key", &self.recipient_public_key)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("amount", &self.amount.as_ref().map(|_| "<redacted>"))
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .finish()
    }
}

/// Symmetric key used to decrypt an Encrypted Receipt.
///
/// The key material is intentionally redacted from [`Debug`](std::fmt::Debug)
/// and [`Display`](std::fmt::Display). Use [`as_str`](Self::as_str) only when
/// serializing Receipt Access or storing the key securely.
///
/// The in-memory key bytes are zeroized on drop as defense-in-depth. This is
/// not a complete scrub: the key is serialized verbatim into the Receipt Access
/// wire message, so copies necessarily exist outside this type.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptDecryptionKey(String);

impl ReceiptDecryptionKey {
    /// Generate a fresh 256-bit Receipt Decryption Key encoded as base64url.
    pub fn generate() -> Self {
        let mut key = XChaCha20Poly1305::generate_key(&mut OsRng);
        let encoded = URL_SAFE_NO_PAD.encode(key.as_slice());
        // Scrub the raw key bytes from the transient buffer; the base64url form
        // is the only surviving copy, guarded by this type's Drop impl.
        key.as_mut_slice().zeroize();
        Self(encoded)
    }

    /// Validate and construct a Receipt Decryption Key from base64url text.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        // AUDITOR NOTE (defense-in-depth for candidate key material): both the
        // input candidate `String` and the decode buffer are scrubbed on every
        // return path, not just on success. `Self` is only constructed on
        // success, so this type's Drop-based zeroization does not cover the
        // error paths; we scrub `key` explicitly before each early return. The
        // decoded bytes live in a `Zeroizing` buffer, so any partial prefix a
        // failing `decode_vec` writes before erroring is scrubbed on drop.
        let mut key = key.into();
        let mut decoded: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());
        // The base64 DecodeError can render an offending byte of the candidate
        // key text; this is key material, so the error carries no decode
        // detail at all, only static messages.
        if URL_SAFE_NO_PAD.decode_vec(&key, &mut decoded).is_err() {
            key.zeroize();
            return Err(PaykitError::Validation(
                "Receipt Decryption Key must be base64url".into(),
            ));
        }
        if decoded.len() != 32 {
            key.zeroize();
            return Err(PaykitError::Validation(
                "Receipt Decryption Key must decode to 32 bytes".into(),
            ));
        }
        // Move the validated base64url form into the type; its Drop zeroizes it
        // later. `decoded` scrubs its 32 bytes on drop at end of scope.
        Ok(Self(key))
    }

    /// Access the raw base64url key material.
    ///
    /// Treat this value as secret; do not log it or include it in telemetry.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(super) fn bytes(&self) -> Result<[u8; 32]> {
        // The base64 DecodeError can render an offending byte of the stored
        // key text; this is key material, so drop the detail entirely rather
        // than retaining it in `source`.
        let mut decoded =
            URL_SAFE_NO_PAD
                .decode(&self.0)
                .map_err(|_| PaykitError::InvalidData {
                    context: "Receipt Decryption Key is not valid base64url".into(),
                    source: None,
                })?;
        let key_bytes =
            <[u8; 32]>::try_from(decoded.as_slice()).map_err(|_| PaykitError::InvalidData {
                context: "Receipt Decryption Key must decode to 32 bytes".into(),
                source: None,
            });
        // Scrub the transient decoded buffer; the returned array is the caller's
        // responsibility and is scrubbed after cipher construction in crypto.rs.
        decoded.zeroize();
        key_bytes
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

// AUDITOR NOTE (defense-in-depth, not a complete scrub): the Receipt Decryption
// Key is serialized verbatim into the Receipt Access wire JSON (see
// `receipt/wire.rs`), so copies of this secret necessarily exist outside this
// type (network buffers, serialized messages, homeserver storage). This Drop
// only clears the in-memory base64url `String` owned here; clones each zeroize
// independently. It does not, and cannot, erase the key's full footprint.
impl Drop for ReceiptDecryptionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Prepared receipt artifacts ready to store and share.
///
/// Contains the plaintext Receipt, Encrypted Receipt payload, and Receipt
/// Access descriptor.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedReceipt {
    /// Canonical plaintext receipt before encryption.
    pub receipt: Receipt,
    /// Encrypted Receipt JSON to store at [`ReceiptAccess::location`].
    pub encrypted_receipt: String,
    /// Receipt Access descriptor to send to the counterparty.
    pub access: ReceiptAccess,
}

impl fmt::Debug for PreparedReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedReceipt")
            .field("receipt", &self.receipt)
            .field(
                "encrypted_receipt",
                &format_args!("<redacted:{} bytes>", self.encrypted_receipt.len()),
            )
            .field("access", &self.access)
            .finish()
    }
}

/// Receipt Access descriptor sent over the existing Noise channel.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptAccess {
    /// Receipt Access message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently always [`PrivateMessageKind::ReceiptAccess`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Receipt artifact identifier used to derive the Receipt Location path.
    pub receipt_id: ReceiptId,
    /// Payment Reference for the receipt.
    pub payment_reference: PaymentReference,
    /// Optional Payment Request ID this Receipt Access corresponds to.
    pub payment_request_id: Option<PaymentRequestId>,
    /// Optional Billing Period for recurring Payment Request receipts.
    pub billing_period: Option<BillingPeriod>,
    /// Homeserver path of the Encrypted Receipt.
    pub location: String,
    /// Symmetric key needed to decrypt the Receipt.
    pub key: ReceiptDecryptionKey,
}

impl fmt::Debug for ReceiptAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptAccess")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field("payment_reference", &"<redacted>")
            .field("payment_request_id", &self.payment_request_id)
            .field("billing_period", &self.billing_period)
            .field("location", &"<redacted>")
            .field("key", &"<redacted>")
            .finish()
    }
}

/// A recognized Receipt Access Event Message plus the raw JSON payload received
/// from the Encrypted Link.
#[derive(Clone, PartialEq)]
pub struct ReceiptAccessEventMessage {
    /// Private message kind selected from the message header.
    pub kind: PrivateMessageKind,
    /// Parsed top-level Event ID when present and valid.
    pub event_id: Option<EventId>,
    /// Parsed top-level Receipt ID when present and valid.
    pub receipt_id: Option<ReceiptId>,
    /// Raw JSON plaintext as sent over the Encrypted Link.
    pub raw_json: String,
    /// Parsed Receipt Access, or an error string explaining why this recognized
    /// message failed structural validation.
    pub access: std::result::Result<ReceiptAccess, String>,
}

impl fmt::Debug for ReceiptAccessEventMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceiptAccessEventMessage")
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("receipt_id", &self.receipt_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("parsed", &self.access.is_ok())
            .field("validation_error", &self.validation_error())
            .finish()
    }
}

impl ReceiptAccessEventMessage {
    /// Return the Private Message Kind for this event message.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Whether the recognized event message parsed successfully.
    pub fn is_valid(&self) -> bool {
        self.access.is_ok()
    }

    /// Access the parsed Receipt Access when structural validation succeeded.
    pub fn parsed_access(&self) -> Option<&ReceiptAccess> {
        self.access.as_ref().ok()
    }

    /// Access the validation error when structural validation failed.
    pub fn validation_error(&self) -> Option<&str> {
        self.access.as_ref().err().map(String::as_str)
    }

    /// Access the Event ID.
    ///
    /// Returns `None` when the recognized message is malformed and the Event ID
    /// could not be parsed as a valid Event ID.
    pub fn event_id(&self) -> Option<&EventId> {
        self.event_id.as_ref()
    }

    /// Access the Receipt ID carried by this Receipt Access event.
    ///
    /// Returns `None` when the recognized message is malformed and the Receipt
    /// ID could not be parsed as a valid Receipt ID.
    pub fn receipt_id(&self) -> Option<&ReceiptId> {
        self.receipt_id.as_ref()
    }
}

fn validate_request_context(
    payment_request_id: Option<&PaymentRequestId>,
    billing_period: Option<&BillingPeriod>,
    label: &str,
) -> Result<()> {
    if payment_request_id.is_none() && billing_period.is_some() {
        return Err(PaykitError::Validation(format!(
            "{label} billing_period requires payment_request_id"
        )));
    }
    if let Some(period) = billing_period {
        period.validate_with_label(label)?;
    }
    Ok(())
}

impl ReceiptDraft {
    pub(crate) fn validate_request_context(&self) -> Result<()> {
        validate_request_context(
            self.payment_request_id.as_ref(),
            self.billing_period.as_ref(),
            "Receipt Draft",
        )
    }
}

impl Receipt {
    pub(crate) fn validate_request_context(&self) -> Result<()> {
        validate_request_context(
            self.payment_request_id.as_ref(),
            self.billing_period.as_ref(),
            "Receipt",
        )
    }
}

impl ReceiptAccess {
    pub(crate) fn validate_request_context(&self) -> Result<()> {
        validate_request_context(
            self.payment_request_id.as_ref(),
            self.billing_period.as_ref(),
            "Receipt Access",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receiver_path() -> crate::PaykitReceiverPath {
        crate::PaykitReceiverPath::new("bitkit/wallet").unwrap()
    }

    #[test]
    fn receipt_debug_redacts_payment_reference() {
        let payment_reference = PaymentReference::new("invoice-secret-123").unwrap();
        let draft = ReceiptDraft {
            receipt_id: Some(ReceiptId::new_v4()),
            payment_reference: payment_reference.clone(),
            payment_request_id: Some(PaymentRequestId::new_v4()),
            billing_period: None,
            payment_endpoint_identifier: Some(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some(PaymentAmount {
                value: "0.001".to_string(),
                asset: "btc".to_string(),
            }),
            metadata: JsonMap::from_iter([(
                "note".to_string(),
                JsonValue::String("private receipt note".to_string()),
            )]),
        };
        let receipt = Receipt {
            receipt_id: draft.receipt_id.clone().unwrap(),
            payment_reference,
            payment_request_id: draft.payment_request_id.clone(),
            billing_period: None,
            recipient_public_key: pubky::Keypair::random().public_key().clone(),
            payment_endpoint_identifier: draft.payment_endpoint_identifier.clone(),
            amount: draft.amount.clone(),
            metadata: draft.metadata.clone(),
        };

        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            event_id: EventId::new_v4(),
            receipt_id: receipt.receipt_id.clone(),
            payment_reference: receipt.payment_reference.clone(),
            payment_request_id: receipt.payment_request_id.clone(),
            billing_period: None,
            location: ReceiptAccess::location(&receiver_path(), &receipt.receipt_id),
            key: ReceiptDecryptionKey::generate(),
        };
        let prepared = PreparedReceipt {
            receipt: receipt.clone(),
            encrypted_receipt: "encrypted-secret".into(),
            access: access.clone(),
        };

        for debug in [
            format!("{draft:?}"),
            format!("{receipt:?}"),
            format!("{access:?}"),
            format!("{prepared:?}"),
        ] {
            assert!(!debug.contains("invoice-secret-123"));
            assert!(!debug.contains("private receipt note"));
            assert!(!debug.contains("encrypted-secret"));
            assert!(!debug.contains(access.key.as_str()));
            assert!(!debug.contains(&access.location));
        }
    }

    #[test]
    fn test_receipt_decryption_key_new_rejects_non_32_byte_material() {
        // Scrubbing the decoded buffer must not disturb length validation.
        let short = URL_SAFE_NO_PAD.encode([7u8; 16]);
        assert!(matches!(
            ReceiptDecryptionKey::new(short).unwrap_err(),
            PaykitError::Validation(_)
        ));
    }

    #[test]
    fn test_receipt_decryption_key_new_rejects_non_base64url() {
        assert!(matches!(
            ReceiptDecryptionKey::new("not valid base64!!").unwrap_err(),
            PaykitError::Validation(_)
        ));
    }

    #[test]
    fn test_receipt_decryption_key_bytes_roundtrip() {
        // Re-encoding the decoded bytes must reproduce the stored form, proving
        // the transient-buffer scrub does not corrupt the key.
        let key = ReceiptDecryptionKey::generate();
        let bytes = key.bytes().unwrap();
        assert_eq!(URL_SAFE_NO_PAD.encode(bytes), key.as_str());
    }

    #[test]
    fn test_receipt_decryption_key_clone_survives_dropped_original() {
        // Clones own independent heap buffers, so the original's Drop-time
        // zeroize must not affect a surviving clone.
        let expected;
        let clone;
        {
            let original = ReceiptDecryptionKey::generate();
            expected = original.as_str().to_string();
            clone = original.clone();
            // `original` is dropped (and zeroized) at the end of this scope.
        }
        assert_eq!(clone.as_str(), expected);
        assert_eq!(clone.bytes().unwrap().len(), 32);
    }
}
