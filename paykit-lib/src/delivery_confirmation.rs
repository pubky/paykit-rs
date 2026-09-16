//! Stateless Delivery Confirmation wire support.

use serde::{Deserialize, Serialize};

use crate::{
    validation::{invalid_plaintext_json, validate_wire_version_kind},
    EventId, PaykitAppId, PaykitError, PrivateMessageKind, Result,
};

/// Confirmation that a counterparty durably received one Event Message.
///
/// This is not an Event Message: `event_id` identifies the original event,
/// not this confirmation. Delivery Confirmations must never be confirmed.
/// They do not indicate payment acceptance, execution, or settlement.
///
/// The caller owns durable receipt and sends this over the authenticated
/// Encrypted Link to the original sender only after persisting the event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryConfirmation {
    app_id: PaykitAppId,
    event_id: EventId,
    payload_hash: String,
}

impl DeliveryConfirmation {
    /// Create a version-1 confirmation for an existing Event Message.
    ///
    /// `app_id` is the local confirming App, and `event_id` is the original
    /// Event ID. `payload_hash` must be SHA-256 of the exact raw UTF-8 original
    /// JSON, including whitespace, formatted as `sha256:<64 lowercase hex>`.
    /// This validates the hash's format; the caller computes it without
    /// reserializing the original JSON and confirms only Event Message kinds.
    pub fn new(app_id: PaykitAppId, event_id: EventId, payload_hash: String) -> Result<Self> {
        let valid_hash = payload_hash.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
        if !valid_hash {
            return Err(PaykitError::Validation(
                "Delivery Confirmation payload_hash must be sha256: followed by 64 lowercase hex digits"
                    .into(),
            ));
        }
        Ok(Self {
            app_id,
            event_id,
            payload_hash,
        })
    }

    /// Local App confirming durable receipt of the original Event Message.
    pub fn app_id(&self) -> &PaykitAppId {
        &self.app_id
    }

    /// Original Event ID; no fresh Event ID is allocated for this confirmation.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }

    /// SHA-256 of the exact raw UTF-8 original JSON, prefixed with `sha256:`.
    pub fn payload_hash(&self) -> &str {
        &self.payload_hash
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryConfirmationWire {
    version: u8,
    kind: String,
    app_id: String,
    event_id: String,
    payload_hash: String,
}

/// Parse a version-1 Delivery Confirmation, rejecting malformed wire data.
///
/// A valid confirmation must still be matched to its authenticated sender,
/// original Event ID, and exact payload hash by the caller before use.
pub fn parse_delivery_confirmation_json(json: &str) -> Result<DeliveryConfirmation> {
    // Decrypted private plaintext must not enter error chains, including
    // validation errors for the App ID, Event ID, and payload hash.
    let invalid = || invalid_plaintext_json("invalid Delivery Confirmation JSON");
    let wire: DeliveryConfirmationWire = serde_json::from_str(json).map_err(|_| invalid())?;
    validate_wire_version_kind(
        wire.version,
        &wire.kind,
        PrivateMessageKind::DeliveryConfirmation,
        "Delivery Confirmation",
    )?;
    DeliveryConfirmation::new(
        PaykitAppId::new(wire.app_id).map_err(|_| invalid())?,
        EventId::new(wire.event_id).map_err(|_| invalid())?,
        wire.payload_hash,
    )
    .map_err(|_| invalid())
}

/// Serialize a Delivery Confirmation into its compact JSON wire representation.
pub fn serialize_delivery_confirmation(confirmation: &DeliveryConfirmation) -> Result<String> {
    let wire = DeliveryConfirmationWire {
        version: 1,
        kind: PrivateMessageKind::DeliveryConfirmation.as_str().to_owned(),
        app_id: confirmation.app_id.as_str().to_owned(),
        event_id: confirmation.event_id.as_str().to_owned(),
        payload_hash: confirmation.payload_hash.clone(),
    };
    serde_json::to_string(&wire)
        .map_err(|_| invalid_plaintext_json("failed to serialize Delivery Confirmation JSON"))
}

#[cfg(test)]
mod tests;
