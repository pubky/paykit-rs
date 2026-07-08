//! Public Paykit receiver marker helpers.
//!
//! A receiver marker is a lightweight public document that makes one
//! app/runtime receiver path discoverable even when it has no public Payment
//! Endpoints. It does not contain payment details.

use serde::{Deserialize, Serialize};

use crate::{
    error::map_error, pubky_routing, validation::invalid_data, PaykitError, PaykitReceiverPath,
    PublicKey, Result,
};

const RECEIVER_MARKER_KIND: &str = "paykit.receiver";

/// Public capabilities advertised by a Paykit receiver marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaykitReceiverCapabilities {
    /// Receiver can participate in private Paykit payment workflows.
    pub private_payments: bool,
    /// Receiver can send or receive Payment Request messages.
    pub payment_requests: bool,
    /// Receiver can issue or retrieve Paykit Receipts.
    pub receipts: bool,
    /// Receiver can execute outgoing payments itself.
    pub outgoing_payments: bool,
}

/// Lightweight public marker for one Paykit receiver path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaykitReceiverMarker {
    /// Receiver path this marker belongs to.
    pub receiver_path: PaykitReceiverPath,
    /// Public receiver capabilities.
    pub capabilities: PaykitReceiverCapabilities,
}

impl PaykitReceiverMarker {
    /// Create a receiver marker for a validated receiver path.
    pub fn new(
        receiver_path: PaykitReceiverPath,
        capabilities: PaykitReceiverCapabilities,
    ) -> Self {
        Self {
            receiver_path,
            capabilities,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiverMarkerWire {
    version: u8,
    kind: String,
    receiver_path: PaykitReceiverPath,
    capabilities: PaykitReceiverCapabilities,
}

impl From<&PaykitReceiverMarker> for ReceiverMarkerWire {
    fn from(marker: &PaykitReceiverMarker) -> Self {
        Self {
            version: 1,
            kind: RECEIVER_MARKER_KIND.into(),
            receiver_path: marker.receiver_path.clone(),
            capabilities: marker.capabilities,
        }
    }
}

impl TryFrom<ReceiverMarkerWire> for PaykitReceiverMarker {
    type Error = PaykitError;

    fn try_from(wire: ReceiverMarkerWire) -> Result<Self> {
        if wire.version != 1 || wire.kind != RECEIVER_MARKER_KIND {
            return Err(invalid_data(
                format!(
                    "unsupported Paykit receiver marker version/kind: {}/{}",
                    wire.version, wire.kind
                ),
                None,
            ));
        }
        Ok(Self::new(wire.receiver_path, wire.capabilities))
    }
}

/// Serialize a Paykit receiver marker to canonical JSON.
pub fn serialize_paykit_receiver_marker(marker: &PaykitReceiverMarker) -> Result<String> {
    serde_json::to_string(&ReceiverMarkerWire::from(marker)).map_err(|err| {
        PaykitError::Validation(format!("failed to serialize Paykit receiver marker: {err}"))
    })
}

/// Parse and validate a Paykit receiver marker JSON payload.
pub fn parse_paykit_receiver_marker_json(
    raw_json: &str,
    expected_receiver_path: &PaykitReceiverPath,
) -> Result<PaykitReceiverMarker> {
    let wire = serde_json::from_str::<ReceiverMarkerWire>(raw_json).map_err(|err| {
        invalid_data(
            format!("Paykit receiver marker JSON is invalid: {err}"),
            Some(err.into()),
        )
    })?;
    let marker = PaykitReceiverMarker::try_from(wire)?;
    if &marker.receiver_path != expected_receiver_path {
        return Err(invalid_data(
            format!(
                "Paykit receiver marker path mismatch: expected {expected_receiver_path}, got {}",
                marker.receiver_path
            ),
            None,
        ));
    }
    Ok(marker)
}

/// Publish a public Paykit receiver marker.
pub async fn publish_paykit_receiver_marker(
    session: &pubky::PubkySession,
    marker: &PaykitReceiverMarker,
) -> Result<()> {
    pubky_routing::upsert_paykit_receiver_marker(session, marker)
        .await
        .map_err(|err| map_error("publish_paykit_receiver_marker", err))
}

/// Remove a public Paykit receiver marker.
pub async fn remove_paykit_receiver_marker(
    session: &pubky::PubkySession,
    receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    pubky_routing::delete_paykit_receiver_marker(session, receiver_path)
        .await
        .map_err(|err| map_error("remove_paykit_receiver_marker", err))
}

/// Fetch a public Paykit receiver marker, if one is present.
pub async fn get_paykit_receiver_marker(
    storage: &pubky::PublicStorage,
    owner: &PublicKey,
    receiver_path: &PaykitReceiverPath,
) -> Result<Option<PaykitReceiverMarker>> {
    pubky_routing::fetch_paykit_receiver_marker(storage, owner, receiver_path)
        .await
        .map_err(|err| map_error("get_paykit_receiver_marker", err))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn receiver_path() -> PaykitReceiverPath {
        PaykitReceiverPath::new("bitkit/server").unwrap()
    }

    fn capabilities() -> PaykitReceiverCapabilities {
        PaykitReceiverCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: false,
        }
    }

    #[test]
    fn test_receiver_marker_json_round_trips() {
        let marker = PaykitReceiverMarker::new(receiver_path(), capabilities());

        let json = serialize_paykit_receiver_marker(&marker).unwrap();
        let parsed = parse_paykit_receiver_marker_json(&json, &receiver_path()).unwrap();

        assert_eq!(parsed, marker);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            json!({
                "version": 1,
                "kind": "paykit.receiver",
                "receiver_path": "bitkit/server",
                "capabilities": {
                    "private_payments": true,
                    "payment_requests": true,
                    "receipts": true,
                    "outgoing_payments": false
                }
            })
        );
    }

    #[test]
    fn test_receiver_marker_rejects_wrong_version_or_kind() {
        let raw = json!({
            "version": 2,
            "kind": "paykit.receiver",
            "receiver_path": "bitkit/server",
            "capabilities": {
                "private_payments": true,
                "payment_requests": true,
                "receipts": true,
                "outgoing_payments": false
            }
        })
        .to_string();
        assert!(matches!(
            parse_paykit_receiver_marker_json(&raw, &receiver_path()),
            Err(PaykitError::InvalidData { .. })
        ));

        let raw = json!({
            "version": 1,
            "kind": "paykit.other",
            "receiver_path": "bitkit/server",
            "capabilities": {
                "private_payments": true,
                "payment_requests": true,
                "receipts": true,
                "outgoing_payments": false
            }
        })
        .to_string();
        assert!(matches!(
            parse_paykit_receiver_marker_json(&raw, &receiver_path()),
            Err(PaykitError::InvalidData { .. })
        ));
    }

    #[test]
    fn test_receiver_marker_rejects_path_mismatch() {
        let marker = PaykitReceiverMarker::new(receiver_path(), capabilities());
        let raw = serialize_paykit_receiver_marker(&marker).unwrap();
        let expected = PaykitReceiverPath::new("bitkit/wallet").unwrap();

        assert!(matches!(
            parse_paykit_receiver_marker_json(&raw, &expected),
            Err(PaykitError::InvalidData { .. })
        ));
    }

    #[test]
    fn test_receiver_marker_rejects_unknown_capability_fields() {
        let raw = json!({
            "version": 1,
            "kind": "paykit.receiver",
            "receiver_path": "bitkit/server",
            "capabilities": {
                "private_payments": true,
                "payment_requests": true,
                "receipts": true,
                "outgoing_payments": false,
                "extra": true
            }
        })
        .to_string();

        assert!(matches!(
            parse_paykit_receiver_marker_json(&raw, &receiver_path()),
            Err(PaykitError::InvalidData { .. })
        ));
    }
}
