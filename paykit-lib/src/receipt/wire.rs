use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::{
    PaykitError, PaymentEndpointIdentifier, PaymentReference, PrivateMessageKind, PublicKey, Result,
};

use super::{Receipt, ReceiptAccess, ReceiptDecryptionKey};

#[derive(Serialize, Deserialize)]
pub(super) struct ReceiptWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) reference: String,
    pub(super) recipient_public_key: String,
    pub(super) payment_endpoint_identifier: Option<String>,
    pub(super) amount: Option<String>,
    pub(super) currency: Option<String>,
    pub(super) metadata: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct EncryptedReceiptWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) algorithm: String,
    pub(super) nonce: String,
    pub(super) ciphertext: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct ReceiptAccessWire {
    pub(super) version: u8,
    pub(super) kind: String,
    pub(super) reference: String,
    pub(super) location: String,
    pub(super) key: String,
    pub(super) algorithm: String,
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
                .map(|identifier| identifier.as_str().to_string()),
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
                    "unsupported Receipt version/kind: {}/{}",
                    wire.version, wire.kind
                ),
                source: None,
            });
        }
        let reference =
            PaymentReference::new(wire.reference).map_err(|err| PaykitError::InvalidData {
                context: "Receipt contains invalid Payment Reference".into(),
                source: Some(err.into()),
            })?;
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
                    "unsupported Receipt Access version/kind/algorithm: {}/{}/{}",
                    wire.version, wire.kind, wire.algorithm
                ),
                source: None,
            });
        }
        let reference =
            PaymentReference::new(wire.reference).map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Payment Reference".into(),
                source: Some(err.into()),
            })?;
        let access = Self {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference,
            location: wire.location,
            key: ReceiptDecryptionKey::new(wire.key).map_err(|err| PaykitError::InvalidData {
                context: "Receipt Access contains invalid Receipt Decryption Key".into(),
                source: Some(err.into()),
            })?,
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        access.validate_location()?;
        Ok(access)
    }
}

pub(super) fn serialize_receipt_access_json(access: &ReceiptAccess) -> Result<String> {
    serde_json::to_string(&ReceiptAccessWire::from(access)).map_err(|err| {
        PaykitError::InvalidData {
            context: format!("failed to serialize Receipt Access JSON: {err}"),
            source: Some(err.into()),
        }
    })
}

pub(super) fn parse_receipt_access_json(json: &str) -> Result<ReceiptAccess> {
    let wire: ReceiptAccessWire =
        serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse Receipt Access JSON: {err}"),
            source: Some(err.into()),
        })?;
    ReceiptAccess::try_from(wire)
}
