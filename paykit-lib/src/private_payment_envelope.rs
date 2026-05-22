use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::{
    encrypted_link::send_private_message, map_error, EncryptedLink, PaykitError,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference, PrivateMessageKind,
    Result,
};

/// Versioned Private Payment Envelope sent over an established Noise link.
///
/// Carries private Payment Endpoints for one payment/request disclosure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivatePaymentEnvelope {
    version: u8,
    kind: PrivateMessageKind,
    /// UUID-v4 correlation reference for this Private Payment Envelope.
    pub reference: PaymentReference,
    /// Complete latest-state map of private Payment Endpoints keyed by Payment Endpoint Identifier.
    endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

impl PrivatePaymentEnvelope {
    /// Construct a Private Payment Envelope using protocol version 1 and the
    /// `paykit.private_payment_envelope` message kind.
    ///
    /// `endpoints` must be the complete desired latest-state map; callers should
    /// include all private Payment Endpoints they want the counterparty to see,
    /// not just an incremental patch. Empty endpoint maps are rejected because
    /// a Private Payment Envelope is an explicit payment/request disclosure.
    pub fn new(
        reference: PaymentReference,
        endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Result<Self> {
        if endpoints.is_empty() {
            return Err(PaykitError::Validation(
                "PrivatePaymentEnvelope endpoints must not be empty".into(),
            ));
        }
        Ok(Self {
            version: 1,
            kind: PrivateMessageKind::PrivatePaymentEnvelope,
            reference,
            endpoints,
        })
    }

    /// Protocol envelope version used for private payment messages.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Protocol message kind used for private payment messages.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Number of private Payment Endpoints in this envelope.
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Returns true when this envelope contains no Payment Endpoints.
    ///
    /// This should only be possible for values constructed before validation or
    /// through deserialization bugs; `new` rejects empty endpoint maps.
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// Borrow private Payment Endpoints keyed by Payment Endpoint Identifier.
    pub fn endpoints(&self) -> &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload> {
        &self.endpoints
    }

    /// Consume this envelope and return its private Payment Endpoints.
    pub fn into_endpoints(self) -> HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload> {
        self.endpoints
    }

    /// Look up a Payment Endpoint by Payment Endpoint Identifier.
    pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload> {
        self.endpoints.get(identifier)
    }
}

#[derive(Deserialize)]
struct PrivatePaymentEnvelopeWire {
    version: u8,
    kind: String,
    reference: String,
    endpoints: HashMap<String, String>,
}

#[derive(Serialize)]
struct PrivatePaymentEnvelopeWireRef<'a> {
    version: u8,
    kind: &'static str,
    reference: &'a str,
    endpoints: HashMap<&'a str, &'a str>,
}

/// Deserializes a versioned Private Payment Envelope JSON message.
pub(crate) fn parse_private_payment_envelope_json(json: &str) -> Result<PrivatePaymentEnvelope> {
    let wire: PrivatePaymentEnvelopeWire =
        serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse Private Payment Envelope JSON: {err}"),
            source: Some(err.into()),
        })?;
    if wire.version != 1 {
        return Err(PaykitError::InvalidData {
            context: format!(
                "unsupported Private Payment Envelope version {}",
                wire.version
            ),
            source: None,
        });
    }
    if wire.kind != PrivateMessageKind::PrivatePaymentEnvelope.as_str() {
        return Err(PaykitError::InvalidData {
            context: format!("unsupported Private Payment Envelope kind '{}'", wire.kind),
            source: None,
        });
    }
    let reference =
        PaymentReference::new(&wire.reference).map_err(|err| PaykitError::InvalidData {
            context: format!(
                "Private Payment Envelope contains invalid payment reference '{}'",
                wire.reference
            ),
            source: Some(err.into()),
        })?;
    let mut entries = HashMap::new();
    for (key, value) in wire.endpoints {
        let payment_endpoint_identifier =
            PaymentEndpointIdentifier::new(&key).map_err(|err| PaykitError::InvalidData {
                context: format!(
                    "Private Payment Envelope contains invalid Payment Endpoint Identifier '{key}'"
                ),
                source: Some(err.into()),
            })?;
        entries.insert(
            payment_endpoint_identifier,
            PaymentEndpointPayload::new(value),
        );
    }
    PrivatePaymentEnvelope::new(reference, entries)
}

/// Serializes a Private Payment Envelope into its versioned JSON message.
pub(crate) fn serialize_private_payment_envelope_json(
    payload: &PrivatePaymentEnvelope,
) -> Result<String> {
    let endpoints = payload
        .endpoints
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wire = PrivatePaymentEnvelopeWireRef {
        version: payload.version,
        kind: payload.kind.as_str(),
        reference: payload.reference.as_str(),
        endpoints,
    };
    serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
        context: format!("failed to serialize Private Payment Envelope JSON: {err}"),
        source: Some(err.into()),
    })
}

/// Encrypts and sends a complete Private Payment Envelope via the established
/// encrypted link.
///
/// The caller must pass a [`PrivatePaymentEnvelope`] containing a validated
/// [`PaymentReference`] and the complete map of private Payment Endpoints. The
/// caller is still responsible for managing the map contents (adding/removing
/// entries) and should pass the full desired endpoints map in `payload.endpoints`
/// on every update.
///
/// The payload is serialized as a versioned envelope before being sent over
/// pubky-noise:
///
/// ```json
/// {
///   "version": 1,
///   "kind": "paykit.private_payment_envelope",
///   "reference": "550e8400-e29b-41d4-a716-446655440000",
///   "endpoints": {
///     "btc-lightning-bolt11": "ln..."
///   }
/// }
/// ```
///
/// `reference` is a UUID-v4 [`PaymentReference`] used to correlate the private
/// payment offer with later protocol artifacts such as receipts. This function
/// serializes the envelope to JSON, encrypts it using
/// [`pubky_noise::PubkyNoiseEncryptor::send_message`], and pubky-noise handles
/// file naming and storage location on the homeserver.
///
/// # Automatic retry
///
/// If `send_message` fails because the homeserver write fails, this function
/// automatically retries up to [`EncryptedLink::set_max_send_retries`] times
/// (default: [`crate::DEFAULT_MAX_SEND_RETRIES`]). Transport-phase homeserver write
/// failures do not corrupt the Noise state, so retries are safe without
/// snapshot-based recovery. Deterministic state, counter, nonce, or encryption
/// errors are returned immediately.
///
/// # Payload size
///
/// The serialized envelope JSON must fit within a single pubky-noise message
/// (`PUBKY_NOISE_MSG_LEN`, currently 1000 bytes). Exceeding this limit
/// returns [`PaykitError::Validation`].
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for encryption and I/O.
/// - `payload` — the complete Private Payment Envelope, including the
///   required [`PaymentReference`] and complete endpoints map.
///
/// # Errors
/// - Returns [`PaykitError::Validation`] if the serialized envelope exceeds
///   the maximum message size.
/// - Returns [`PaykitError::InvalidData`] if the envelope cannot be serialized.
/// - Returns [`PaykitError::Transport`] if `send_message` fails after all
///   retry attempts are exhausted.
#[instrument(skip(link, payload), fields(count = payload.endpoints.len()))]
pub async fn set_private_payment_envelope(
    link: &mut EncryptedLink,
    payload: &PrivatePaymentEnvelope,
) -> Result<()> {
    debug!("sending Private Payment Envelope");
    let json = serialize_private_payment_envelope_json(payload)
        .map_err(|err| map_error("set_private_payment_envelope", err))?;
    send_private_message(link, json.as_bytes(), "private payments")
        .await
        .map_err(|err| map_error("set_private_payment_envelope", err))
}

/// Receives and decrypts the latest Private Payment Envelope from the remote
/// peer via the established encrypted link.
///
/// Returns `Ok(Some(payload))` when a private payments message is available.
/// The caller can access the correlation reference at `payload.reference` and
/// look up Payment Endpoints from `payload.endpoints` or via
/// [`PrivatePaymentEnvelope::get`].
///
/// Returns `Ok(None)` when no private payments message is currently available.
/// `None` means "no message yet"; it is distinct from receiving a payload whose
/// `endpoints` map is empty.
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for decryption and I/O.
///
/// # Semantics
/// - Receives and buffers all currently available application messages from the
///   shared Noise stream before selecting private payments by message kind.
/// - Returns `Ok(None)` when no private payments messages are available.
/// - Returns the latest queued [`PrivatePaymentEnvelope`]. Intermediate queued
///   private payment updates are consumed because private payments are
///   latest-state data.
/// - Messages with other supported `kind` values are left buffered on the
///   [`EncryptedLink`] for their own typed receivers. They are not parsed as
///   private payments and are not discarded just because this function was called.
/// - Syntactically valid messages with unsupported `kind` values are logged and
///   dropped by the shared dispatcher; they are not buffered indefinitely.
/// - The returned payload is the full versioned Private Payment Envelope,
///   including its required [`PaymentReference`] and complete endpoints map.
/// - Returns `Err(PaykitError::InvalidData)` when the selected private
///   payments payload cannot be parsed as a Private Payment Envelope.
/// - Malformed unrelated private application messages are ignored with
///   diagnostics so one bad message does not prevent later valid messages from
///   being dispatched.
/// - Returns `Err(PaykitError::Transport)` for decryption, counter/nonce, or
///   I/O failures.
#[instrument(skip(link))]
pub async fn get_private_payment_envelope(
    link: &mut EncryptedLink,
) -> Result<Option<PrivatePaymentEnvelope>> {
    debug!("receiving Private Payment Envelope");

    let stats = link
        .private_messages
        .receive_available(&mut link.encryptor)
        .await?;
    let Some(raw) = link
        .private_messages
        .take_latest(PrivateMessageKind::PrivatePaymentEnvelope)
    else {
        debug!(
            received = stats.received,
            "no private payments messages available"
        );
        return Ok(None);
    };

    let payload = parse_private_payment_envelope_json(raw.plaintext())?;
    debug!(
        count = payload.endpoints.len(),
        received = stats.received,
        pending = link.private_messages.len(),
        "Private Payment Envelope received"
    );
    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_private_payment_envelope_json_uses_versioned_envelope() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut entries = HashMap::new();
        entries.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        let payload = PrivatePaymentEnvelope::new(reference.clone(), entries).unwrap();
        let json = serialize_private_payment_envelope_json(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["kind"], "paykit.private_payment_envelope");
        assert_eq!(value["reference"], reference.as_str());
        assert_eq!(value["endpoints"]["btc-lightning-bolt11"], "ln...");
    }

    #[test]
    fn test_parse_private_payment_envelope_json_requires_versioned_envelope() {
        let err = parse_private_payment_envelope_json(r#"{"btc-lightning-bolt11": "ln..."}"#)
            .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Private Payment Envelope"))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_unsupported_version() {
        let err = parse_private_payment_envelope_json(r#"{"version":2,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope version 2")),
            "expected unsupported version error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_unsupported_kind() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.receipt","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope kind")),
            "expected unsupported kind error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_invalid_reference() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"not-a-uuid","endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid payment reference")),
            "expected invalid reference error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_empty_string() {
        let err = parse_private_payment_envelope_json("").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData parse error for empty string, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_truncated_object() {
        let err = parse_private_payment_envelope_json("{").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for truncated JSON, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_array_instead_of_object() {
        let err =
            parse_private_payment_envelope_json(r#"["btc-lightning-bolt11","btc-bitcoin-p2tr"]"#)
                .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON array, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_plain_string() {
        let err = parse_private_payment_envelope_json(r#""just a string""#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for plain JSON string, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_number() {
        let err = parse_private_payment_envelope_json("42").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON number, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_non_string_values() {
        let err = parse_private_payment_envelope_json(
            r#"{"btc-lightning-bolt11": 123, "btc-bitcoin-p2tr": true}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for non-string values, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_trailing_comma() {
        let err = parse_private_payment_envelope_json(r#"{"btc-lightning-bolt11": "ln...",}"#)
            .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for trailing comma, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_null() {
        let err = parse_private_payment_envelope_json("null").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON null, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_empty_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for empty key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_path_traversal_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"..":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for path-traversal key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_slash_in_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"foo/bar":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for key with slash, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_reserved_private_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"private":"secret..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for reserved 'private' key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_oversized_key() {
        let long_key = "a".repeat(65);
        let json = format!(
            r#"{{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{{"{long_key}":"ln..."}}}}"#
        );
        let err = parse_private_payment_envelope_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for oversized key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_one_valid_one_invalid_key() {
        let err =
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"btc-lightning-bolt11":"ln...","":"bc1..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData when one key is invalid, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_valid_single_entry() {
        let result = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"btc-lightning-bolt11":"ln..."}}"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_valid_multiple_entries() {
        let result =
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{"btc-lightning-bolt11":"ln...","btc-bitcoin-p2tr":"bc1..."}}"#).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap()),
            Some(&PaymentEndpointPayload::new("bc1..."))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_empty_object() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("endpoints must not be empty")),
            "expected validation error for empty endpoints, got: {err}"
        );
    }
}
