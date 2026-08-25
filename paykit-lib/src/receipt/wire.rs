use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    shared_wire::{deserialize_optional_no_null, BillingPeriodWire, PaymentAmountWire},
    validation::{
        invalid_data, json_error_category, private_message_parse_error, validate_wire_version_kind,
        validate_wire_version_kind_str,
    },
    BillingPeriod, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier,
    PaymentReference, PaymentRequestId, PrivateApplicationMessage, PrivateMessageKind,
    PrivateMessageParseCategory, PrivateMessageParseError, PublicKey, Result,
};

use super::{Receipt, ReceiptAccess, ReceiptAccessEventMessage, ReceiptDecryptionKey, ReceiptId};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) receipt_id: String,
    pub(super) payment_reference: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) payment_request_id: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) billing_period: Option<BillingPeriodWire>,
    pub(super) recipient_public_key: String,
    pub(super) payment_endpoint_identifier: Option<String>,
    pub(super) amount: Option<PaymentAmountWire>,
    pub(super) metadata: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EncryptedReceiptWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) algorithm: String,
    pub(super) nonce: String,
    pub(super) ciphertext: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReceiptAccessWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) event_id: String,
    pub(super) receipt_id: String,
    pub(super) payment_reference: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) payment_request_id: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) billing_period: Option<BillingPeriodWire>,
    pub(super) location: String,
    pub(super) key: String,
}

impl From<&Receipt> for ReceiptWire {
    fn from(receipt: &Receipt) -> Self {
        Self {
            version: 1,
            kind: "paykit.receipt".to_string(),
            receipt_id: receipt.receipt_id.as_str().to_string(),
            payment_reference: receipt.payment_reference.as_str().to_string(),
            payment_request_id: receipt
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_string()),
            billing_period: receipt.billing_period.as_ref().map(BillingPeriodWire::from),
            recipient_public_key: receipt.recipient_public_key.to_string(),
            payment_endpoint_identifier: receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|identifier| identifier.as_str().to_string()),
            amount: receipt.amount.as_ref().map(PaymentAmountWire::from),
            metadata: receipt.metadata.clone(),
        }
    }
}

impl TryFrom<ReceiptWire> for Receipt {
    type Error = PaykitError;

    fn try_from(wire: ReceiptWire) -> Result<Self> {
        validate_wire_version_kind_str(wire.version, &wire.kind, "paykit.receipt", "Receipt")?;
        // SECURITY / REDACTION: this wire is parsed from DECRYPTED receipt
        // plaintext, and the field validators quote the offending value in
        // their error Display. Each dropped inner error is replaced by the
        // typed redacted category so no decrypted value survives in the error
        // chain (Debug, logs, `source()` walkers); only the static contexts
        // may cross the FFI boundary as exception text.
        let receipt_id = ReceiptId::new(wire.receipt_id).map_err(|_| PaykitError::InvalidData {
            context: "Receipt contains invalid Receipt ID".into(),
            source: receipt_invalid_structure(),
        })?;
        let payment_reference = PaymentReference::new(wire.payment_reference).map_err(|_| {
            PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Reference".into(),
                source: receipt_invalid_structure(),
            }
        })?;
        let payment_request_id = wire
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Request ID".into(),
                source: receipt_invalid_structure(),
            })?;
        let billing_period = wire.billing_period.map(BillingPeriod::from);
        if let Some(period) = &billing_period {
            period
                .validate_with_label("Receipt Billing Period")
                .map_err(|_| PaykitError::InvalidData {
                    context: "Receipt contains invalid Billing Period".into(),
                    source: receipt_invalid_structure(),
                })?;
        }
        // The pkarr parse error's Debug output can echo the offending decrypted
        // field value, so it is dropped like the validator errors above.
        let recipient_public_key = PublicKey::try_from(wire.recipient_public_key.as_str())
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt contains invalid recipient public key".into(),
                source: receipt_invalid_structure(),
            })?;
        let payment_endpoint_identifier = wire
            .payment_endpoint_identifier
            .map(PaymentEndpointIdentifier::new)
            .transpose()
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Endpoint Identifier".into(),
                source: receipt_invalid_structure(),
            })?;
        let amount = wire.amount.map(PaymentAmount::from);
        if let Some(amount) = &amount {
            amount
                .validate_with_label("Receipt amount")
                .map_err(|_| PaykitError::InvalidData {
                    context: "Receipt contains invalid Payment Amount".into(),
                    source: receipt_invalid_structure(),
                })?;
        }
        let receipt = Self {
            receipt_id,
            payment_reference,
            payment_request_id,
            billing_period,
            recipient_public_key,
            payment_endpoint_identifier,
            amount,
            metadata: wire.metadata,
        };
        receipt
            .validate_request_context()
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Request context".into(),
                source: receipt_invalid_structure(),
            })?;
        Ok(receipt)
    }
}

impl From<&ReceiptAccess> for ReceiptAccessWire {
    fn from(access: &ReceiptAccess) -> Self {
        Self {
            version: access.version,
            kind: access.kind.as_str().to_string(),
            event_id: access.event_id.as_str().to_string(),
            receipt_id: access.receipt_id.as_str().to_string(),
            payment_reference: access.payment_reference.as_str().to_string(),
            payment_request_id: access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_string()),
            billing_period: access.billing_period.as_ref().map(BillingPeriodWire::from),
            location: access.location.clone(),
            key: access.key.as_str().to_string(),
        }
    }
}

/// Typed redacted source for Receipt and Receipt Access invariant failures.
///
/// SECURITY / REDACTION: both wires are parsed from decrypted plaintext, and
/// the dropped inner Validation message can echo decrypted field values
/// (identifier, timestamp, and UUID validators quote the offending value), so
/// only this typed category may travel as `source`.
fn receipt_invalid_structure() -> Option<anyhow::Error> {
    Some(anyhow::Error::new(PrivateMessageParseError::new(
        PrivateMessageParseCategory::InvalidStructure,
    )))
}

impl TryFrom<ReceiptAccessWire> for ReceiptAccess {
    type Error = PaykitError;

    fn try_from(wire: ReceiptAccessWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::ReceiptAccess,
            "Receipt Access",
        )?;
        let event_id = EventId::new(wire.event_id).map_err(|_| PaykitError::InvalidData {
            context: "Receipt Access contains invalid Event ID".into(),
            source: receipt_invalid_structure(),
        })?;
        let receipt_id = ReceiptId::new(wire.receipt_id).map_err(|_| PaykitError::InvalidData {
            context: "Receipt Access contains invalid Receipt ID".into(),
            source: receipt_invalid_structure(),
        })?;
        let payment_reference = PaymentReference::new(wire.payment_reference).map_err(|_| {
            PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Reference".into(),
                source: receipt_invalid_structure(),
            }
        })?;
        let payment_request_id = wire
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Request ID".into(),
                source: receipt_invalid_structure(),
            })?;
        let billing_period = wire.billing_period.map(BillingPeriod::from);
        if let Some(period) = &billing_period {
            period
                .validate_with_label("Receipt Access Billing Period")
                .map_err(|_| PaykitError::InvalidData {
                    context: "Receipt Access contains invalid Billing Period".into(),
                    source: receipt_invalid_structure(),
                })?;
        }
        let access = Self {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            event_id,
            receipt_id,
            payment_reference,
            payment_request_id,
            billing_period,
            location: wire.location,
            key: ReceiptDecryptionKey::new(wire.key).map_err(|_| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Receipt Decryption Key".into(),
                source: receipt_invalid_structure(),
            })?,
        };
        access
            .validate_request_context()
            .map_err(|_| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Request context".into(),
                source: receipt_invalid_structure(),
            })?;
        access.validate_wire_location()?;
        Ok(access)
    }
}

pub fn serialize_receipt_access_json(access: &ReceiptAccess) -> Result<String> {
    // Outbound serialize of locally constructed data: keep the serde source,
    // keep the context static.
    serde_json::to_string(&ReceiptAccessWire::from(access))
        .map_err(|err| invalid_data("failed to serialize Receipt Access JSON", Some(err.into())))
}

/// Parse a Receipt Access JSON message.
///
/// Use [`parse_receipt_access_event_message`] when parsing from the raw private
/// stream.
pub fn parse_receipt_access_json(json: &str) -> Result<ReceiptAccess> {
    // SECURITY / REDACTION: `json` is decrypted private-message plaintext
    // carrying the Receipt Decryption Key and Receipt Location. The serde
    // error's Display embeds field values on type mismatches, so it must not
    // be folded into the context or kept as `source` -- this error can cross
    // the FFI boundary as exception text. Only the typed redacted category
    // travels as `source`.
    let wire: ReceiptAccessWire = serde_json::from_str(json).map_err(|err| {
        private_message_parse_error(
            "failed to parse Receipt Access JSON",
            json_error_category(&err),
        )
    })?;
    ReceiptAccess::try_from(wire)
}

fn parse_receipt_access_header_ids(raw: &str) -> (Option<EventId>, Option<ReceiptId>) {
    let Ok(value) = serde_json::from_str::<JsonValue>(raw) else {
        return (None, None);
    };
    let event_id = value
        .get("event_id")
        .and_then(JsonValue::as_str)
        .and_then(|id| EventId::new(id).ok());
    let receipt_id = value
        .get("receipt_id")
        .and_then(JsonValue::as_str)
        .and_then(|id| ReceiptId::new(id).ok());
    (event_id, receipt_id)
}

/// Parse a raw Private Application Message as a Receipt Access Event Message.
///
/// Returns `None` when the message kind is not `paykit.receipt_access`.
/// Recognized but malformed Receipt Access events return `Some` with
/// [`ReceiptAccessEventMessage::is_valid`] set to `false`.
pub fn parse_receipt_access_event_message(
    message: &PrivateApplicationMessage,
) -> Option<ReceiptAccessEventMessage> {
    let kind = message.known_kind()?;
    (kind == PrivateMessageKind::ReceiptAccess).then(|| {
        let (event_id, receipt_id) = parse_receipt_access_header_ids(&message.raw_json);
        // SECURITY / REDACTION: the stored validation error is exactly a
        // stable redacted category string (persisted by SDK callers and
        // byte-compared on backup restore), never free-form error text.
        let access = parse_receipt_access_json(&message.raw_json).map_err(|err| {
            err.private_message_parse_category()
                .unwrap_or(PrivateMessageParseCategory::InvalidStructure)
                .as_str()
                .to_owned()
        });
        ReceiptAccessEventMessage {
            kind,
            event_id,
            receipt_id,
            raw_json: message.raw_json.clone(),
            access,
        }
    })
}
