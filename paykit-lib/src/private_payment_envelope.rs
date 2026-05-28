use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::{
    error::map_error,
    private_message::{
        receive_private_messages, send_private_message, take_latest_pending_message,
        PrivateMessageKind,
    },
    EncryptedLink, PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference, Result,
};

/// Versioned Private Payment Envelope sent over an established Encrypted Link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivatePaymentEnvelope {
    version: u8,
    kind: PrivateMessageKind,
    /// UUID-v4 Payment Reference for this Private Payment Envelope.
    pub reference: PaymentReference,
    /// Complete Payment List carried by this Latest-State Message.
    pub payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

impl PrivatePaymentEnvelope {
    /// Construct a Private Payment Envelope using protocol version 1 and the
    /// `paykit.private_payment_envelope` message kind.
    ///
    /// `payment_endpoints` must be the complete desired Payment List; callers should
    /// include all Payment Endpoints they want the counterparty to see, not
    /// just an incremental patch.
    pub fn new(
        reference: PaymentReference,
        payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PrivatePaymentEnvelope,
            reference,
            payment_endpoints,
        }
    }

    /// Protocol envelope version used for this Private Application Message.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Private Message Kind used by this envelope.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Number of Payment Endpoints in this envelope.
    pub fn len(&self) -> usize {
        self.payment_endpoints.len()
    }

    /// Returns true when this envelope contains no Payment Endpoints.
    pub fn is_empty(&self) -> bool {
        self.payment_endpoints.is_empty()
    }

    /// Look up a Payment Endpoint Payload by Payment Endpoint Identifier.
    pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload> {
        self.payment_endpoints.get(identifier)
    }
}

#[derive(Deserialize)]
struct PrivatePaymentEnvelopeWire {
    version: u8,
    kind: String,
    reference: String,
    payment_endpoints: HashMap<String, String>,
}

#[derive(Serialize)]
struct PrivatePaymentEnvelopeWireRef<'a> {
    version: u8,
    kind: &'static str,
    reference: &'a str,
    payment_endpoints: HashMap<&'a str, &'a str>,
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
                "Private Payment Envelope contains invalid Payment Reference '{}'",
                wire.reference
            ),
            source: Some(err.into()),
        })?;
    let mut payment_endpoints = HashMap::new();
    for (key, value) in wire.payment_endpoints {
        let payment_endpoint_identifier =
            PaymentEndpointIdentifier::new(&key).map_err(|err| PaykitError::InvalidData {
                context: format!(
                    "Private Payment Envelope contains invalid Payment Endpoint Identifier '{key}'"
                ),
                source: Some(err.into()),
            })?;
        payment_endpoints.insert(
            payment_endpoint_identifier,
            PaymentEndpointPayload::new(value),
        );
    }
    Ok(PrivatePaymentEnvelope::new(reference, payment_endpoints))
}

/// Serializes a Private Payment Envelope into its JSON wire representation.
pub(crate) fn serialize_private_payment_envelope_json(
    envelope: &PrivatePaymentEnvelope,
) -> Result<String> {
    let payment_endpoints = envelope
        .payment_endpoints
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wire = PrivatePaymentEnvelopeWireRef {
        version: envelope.version,
        kind: envelope.kind.as_str(),
        reference: envelope.reference.as_str(),
        payment_endpoints,
    };
    serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
        context: format!("failed to serialize Private Payment Envelope JSON: {err}"),
        source: Some(err.into()),
    })
}

/// Encrypts and sends a complete Private Payment Envelope via the established
/// Encrypted Link.
///
/// The caller must pass a [`PrivatePaymentEnvelope`] containing a validated
/// [`PaymentReference`] and the complete Payment List. The
/// caller is still responsible for managing the map contents (adding/removing
/// Payment Endpoints) and should pass the full desired `payment_endpoints` map
/// in `envelope.payment_endpoints`
/// on every update.
///
/// The envelope is serialized as a versioned JSON message before being sent over
/// pubky-noise:
///
/// ```json
/// {
///   "version": 1,
///   "kind": "paykit.private_payment_envelope",
///   "reference": "550e8400-e29b-41d4-a716-446655440000",
///   "payment_endpoints": {
///     "lightning": "ln..."
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
/// # Message size
///
/// The serialized envelope JSON must fit within a single pubky-noise message
/// (`PUBKY_NOISE_MSG_LEN`, currently 1000 bytes). Exceeding this limit
/// returns [`PaykitError::Validation`].
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for encryption and I/O.
/// - `envelope` — the complete Private Payment Envelope, including the
///   required [`PaymentReference`] and complete `payment_endpoints` map.
///
/// # Errors
/// - Returns [`PaykitError::Validation`] if the serialized envelope exceeds
///   the maximum message size.
/// - Returns [`PaykitError::InvalidData`] if the envelope cannot be serialized.
/// - Returns [`PaykitError::Transport`] if `send_message` fails after all
///   retry attempts are exhausted.
#[instrument(skip(link, envelope), fields(count = envelope.payment_endpoints.len()))]
pub async fn set_private_payment_envelope(
    link: &mut EncryptedLink,
    envelope: &PrivatePaymentEnvelope,
) -> Result<()> {
    debug!("sending Private Payment Envelope");
    let json = serialize_private_payment_envelope_json(envelope)
        .map_err(|err| map_error("set_private_payment_envelope", err))?;
    send_private_message(link, json.as_bytes(), "Private Payment Envelope")
        .await
        .map_err(|err| map_error("set_private_payment_envelope", err))
}

/// Receives and decrypts the latest Private Payment Envelope from the
/// counterparty via the established Encrypted Link.
///
/// Returns `Ok(Some(envelope))` when a Private Payment Envelope is available.
/// The caller can access the correlation reference at `envelope.reference` and
/// look up Payment Endpoint Payloads from `envelope.payment_endpoints` or via
/// [`PrivatePaymentEnvelope::get`].
///
/// Returns `Ok(None)` when no Private Payment Envelope is currently available.
/// `None` means "no message yet"; it is distinct from receiving an envelope whose
/// `payment_endpoints` map is empty.
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for decryption and I/O.
///
/// # Semantics
/// - Receives and buffers all currently available application messages from the
///   shared Noise stream before selecting the Private Payment Envelope message kind.
/// - Returns `Ok(None)` when no Private Payment Envelopes are available.
/// - Returns the latest queued [`PrivatePaymentEnvelope`]. Intermediate queued
///   envelopes are consumed because Private Payment Envelope uses
///   Latest-State Message semantics.
/// - Messages with other supported `kind` values are left buffered on the
///   [`EncryptedLink`] for their own typed receivers. They are not parsed as
///   Private Payment Envelopes and are not discarded just because this function was called.
/// - Syntactically valid messages with unsupported `kind` values are logged and
///   dropped by the shared dispatcher; they are not buffered indefinitely.
/// - The returned envelope is the full versioned Private Payment Envelope,
///   including its required [`PaymentReference`] and complete `payment_endpoints` map.
/// - Returns `Err(PaykitError::InvalidData)` when the selected private
///   message cannot be parsed as a Private Payment Envelope.
/// - Malformed unrelated Private Application Messages are ignored with
///   diagnostics so one bad message does not prevent later valid messages from
///   being dispatched.
/// - Returns `Err(PaykitError::Transport)` for decryption, counter/nonce, or
///   I/O failures.
#[instrument(skip(link))]
pub async fn get_private_payment_envelope(
    link: &mut EncryptedLink,
) -> Result<Option<PrivatePaymentEnvelope>> {
    debug!("receiving Private Payment Envelope");

    let received = receive_private_messages(link).await?;
    let Some(raw) = take_latest_pending_message(
        &mut link.pending_private_messages,
        PrivateMessageKind::PrivatePaymentEnvelope,
    ) else {
        debug!(received, "no Private Payment Envelopes available");
        return Ok(None);
    };

    let envelope = parse_private_payment_envelope_json(&raw.plaintext)?;
    debug!(
        count = envelope.payment_endpoints.len(),
        received,
        pending = link.pending_private_messages.len(),
        "Private Payment Envelope received"
    );
    Ok(Some(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload};
    use std::collections::HashMap;

    #[test]
    fn test_serialize_private_payment_envelope_json_uses_versioned_envelope() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("lightning").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        let payload = PrivatePaymentEnvelope::new(reference.clone(), payment_endpoints);
        let json = serialize_private_payment_envelope_json(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["kind"], "paykit.private_payment_envelope");
        assert_eq!(value["reference"], reference.as_str());
        assert_eq!(value["payment_endpoints"]["lightning"], "ln...");
    }

    #[test]
    fn test_parse_private_payment_envelope_json_requires_versioned_envelope() {
        let err = parse_private_payment_envelope_json(r#"{"lightning": "ln..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Private Payment Envelope"))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_unsupported_version() {
        let err = parse_private_payment_envelope_json(r#"{"version":2,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope version 2")),
            "expected unsupported version error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_unsupported_kind() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.receipt","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope kind")),
            "expected unsupported kind error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_invalid_reference() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"not-a-uuid","payment_endpoints":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Reference")),
            "expected invalid reference error, got: {err}"
        );
    }

    // ── parse_private_payment_envelope_json tests ───────────────────────────────

    #[test]
    fn test_parse_private_payment_envelope_json_empty_string() {
        let err = parse_private_payment_envelope_json("").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData parse error for empty string, got: {err}"
        );
    }

    // ── Malformed JSON ──────────────────────────────────────────────────

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
        let err = parse_private_payment_envelope_json(r#"["lightning","onchain"]"#).unwrap_err();
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
        let err = parse_private_payment_envelope_json(r#"{"lightning": 123, "onchain": true}"#)
            .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for non-string values, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_trailing_comma() {
        let err = parse_private_payment_envelope_json(r#"{"lightning": "ln...",}"#).unwrap_err();
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

    // ── Invalid Payment Endpoint Identifiers inside valid JSON ────────────────────────────

    #[test]
    fn test_parse_private_payment_envelope_json_empty_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for empty key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_path_traversal_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"..":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for path-traversal key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_slash_in_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"foo/bar":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for key with slash, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_reserved_private_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"private":"secret..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for reserved 'private' key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_oversized_key() {
        let long_key = "a".repeat(65);
        let json = format!(
            r#"{{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{{"{long_key}":"ln..."}}}}"#
        );
        let err = parse_private_payment_envelope_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for oversized key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_one_valid_one_invalid_key() {
        // The valid key should not mask the invalid one.
        let err =
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"lightning":"ln...","":"bc1..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData when one key is invalid, got: {err}"
        );
    }

    // ── Happy path ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payment_envelope_json_valid_single_payment_endpoint() {
        let result = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"lightning":"ln..."}}"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("lightning").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_valid_multiple_payment_endpoints() {
        let result =
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{"lightning":"ln...","onchain":"bc1..."}}"#).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("lightning").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("onchain").unwrap()),
            Some(&PaymentEndpointPayload::new("bc1..."))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_empty_object() {
        let result = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payment_envelope","reference":"550e8400-e29b-41d4-a716-446655440000","payment_endpoints":{}}"#).unwrap();
        assert!(result.is_empty());
    }
}
