use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    shared_wire::{deserialize_optional_no_null, BillingPeriodWire, PaymentAmountWire},
    validation::{invalid_data, validate_wire_version_kind, validate_wire_version_kind_str},
    BillingPeriod, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier,
    PaymentReference, PaymentRequestId, PrivateApplicationMessage, PrivateMessageKind, PublicKey,
    Result,
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
        let receipt_id =
            ReceiptId::new(wire.receipt_id).map_err(|err| PaykitError::InvalidData {
                context: "Receipt contains invalid Receipt ID".into(),
                source: Some(err.into()),
            })?;
        let payment_reference = PaymentReference::new(wire.payment_reference).map_err(|err| {
            PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Reference".into(),
                source: Some(err.into()),
            }
        })?;
        let payment_request_id = wire
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()
            .map_err(|err| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Request ID".into(),
                source: Some(err.into()),
            })?;
        let billing_period = wire.billing_period.map(BillingPeriod::from);
        if let Some(period) = &billing_period {
            period
                .validate_with_label("Receipt Billing Period")
                .map_err(|err| PaykitError::InvalidData {
                    context: "Receipt contains invalid Billing Period".into(),
                    source: Some(err.into()),
                })?;
        }
        let recipient_public_key = PublicKey::try_from(wire.recipient_public_key.as_str())
            .map_err(|err| PaykitError::InvalidData {
                context: format!("Receipt contains invalid recipient public key: {err:?}"),
                source: anyhow::anyhow!("invalid recipient public key: {err:?}").into(),
            })?;
        let payment_endpoint_identifier = wire
            .payment_endpoint_identifier
            .map(PaymentEndpointIdentifier::new)
            .transpose()
            .map_err(|err| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Endpoint Identifier".into(),
                source: Some(err.into()),
            })?;
        let amount = wire.amount.map(PaymentAmount::from);
        if let Some(amount) = &amount {
            amount
                .validate_with_label("Receipt amount")
                .map_err(|err| PaykitError::InvalidData {
                    context: "Receipt contains invalid Payment Amount".into(),
                    source: Some(err.into()),
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
            .map_err(|err| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Request context".into(),
                source: Some(err.into()),
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

impl TryFrom<ReceiptAccessWire> for ReceiptAccess {
    type Error = PaykitError;

    fn try_from(wire: ReceiptAccessWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::ReceiptAccess,
            "Receipt Access",
        )?;
        let event_id = EventId::new(wire.event_id).map_err(|err| PaykitError::InvalidData {
            context: "Receipt Access contains invalid Event ID".into(),
            source: Some(err.into()),
        })?;
        let receipt_id =
            ReceiptId::new(wire.receipt_id).map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Receipt ID".into(),
                source: Some(err.into()),
            })?;
        let payment_reference = PaymentReference::new(wire.payment_reference).map_err(|err| {
            PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Reference".into(),
                source: Some(err.into()),
            }
        })?;
        let payment_request_id = wire
            .payment_request_id
            .map(PaymentRequestId::new)
            .transpose()
            .map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Request ID".into(),
                source: Some(err.into()),
            })?;
        let billing_period = wire.billing_period.map(BillingPeriod::from);
        if let Some(period) = &billing_period {
            period
                .validate_with_label("Receipt Access Billing Period")
                .map_err(|err| PaykitError::InvalidData {
                    context: "Receipt Access contains invalid Billing Period".into(),
                    source: Some(err.into()),
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
            key: ReceiptDecryptionKey::new(wire.key).map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Receipt Decryption Key".into(),
                source: Some(err.into()),
            })?,
        };
        access
            .validate_request_context()
            .map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Request context".into(),
                source: Some(err.into()),
            })?;
        access.validate_wire_location()?;
        Ok(access)
    }
}

pub fn serialize_receipt_access_json(access: &ReceiptAccess) -> Result<String> {
    serde_json::to_string(&ReceiptAccessWire::from(access)).map_err(|err| {
        invalid_data(
            format!("failed to serialize Receipt Access JSON: {err}"),
            Some(err.into()),
        )
    })
}

/// Parse a Receipt Access JSON message.
///
/// Use [`parse_receipt_access_event_message`] when parsing from the raw private
/// stream.
pub fn parse_receipt_access_json(json: &str) -> Result<ReceiptAccess> {
    let wire: ReceiptAccessWire = serde_json::from_str(json).map_err(|err| {
        invalid_data(
            format!("failed to parse Receipt Access JSON: {err}"),
            Some(err.into()),
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
        let access = parse_receipt_access_json(&message.raw_json).map_err(|err| err.to_string());
        ReceiptAccessEventMessage {
            kind,
            event_id,
            receipt_id,
            raw_json: message.raw_json.clone(),
            access,
        }
    })
}
