use std::{collections::HashMap, fmt};

use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

use crate::{
    error::map_error, validation::invalid_data, EncryptedLink, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PrivateMessageKind, Result,
};

/// Versioned Private Payment List sent over an established Encrypted Link.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivatePaymentList {
    version: u8,
    kind: PrivateMessageKind,
    /// Complete Payment List carried by this Latest-State Message.
    pub payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

impl fmt::Debug for PrivatePaymentList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentList")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field(
                "payment_endpoints",
                &format_args!("<redacted:{} endpoints>", self.payment_endpoints.len()),
            )
            .finish()
    }
}

impl PrivatePaymentList {
    /// Construct a Private Payment List using protocol version 1 and the
    /// `paykit.private_payment_list` message kind.
    ///
    /// `payment_endpoints` is the complete list to share, not a patch.
    pub fn new(
        payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PrivatePaymentList,
            payment_endpoints,
        }
    }

    /// Protocol version used for this Private Application Message.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Private Message Kind used by this list message.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Number of Payment Endpoints in this private list message.
    pub fn len(&self) -> usize {
        self.payment_endpoints.len()
    }

    /// Returns true when this private list message contains no Payment Endpoints.
    pub fn is_empty(&self) -> bool {
        self.payment_endpoints.is_empty()
    }

    /// Look up a Payment Endpoint Payload by Payment Endpoint Identifier.
    pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload> {
        self.payment_endpoints.get(identifier)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrivatePaymentListWire {
    version: u8,
    kind: String,
    payment_endpoints: HashMap<String, String>,
}

#[derive(Serialize)]
struct PrivatePaymentListWireRef<'a> {
    version: u8,
    kind: &'static str,
    payment_endpoints: HashMap<&'a str, &'a str>,
}

/// Parse a versioned Private Payment List JSON message.
pub fn parse_private_payment_list_json(json: &str) -> Result<PrivatePaymentList> {
    let wire: PrivatePaymentListWire = serde_json::from_str(json).map_err(|err| {
        invalid_data(
            format!("failed to parse Private Payment List JSON: {err}"),
            Some(err.into()),
        )
    })?;
    if wire.version != 1 {
        return Err(invalid_data(
            format!("unsupported Private Payment List version {}", wire.version),
            None,
        ));
    }
    if wire.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
        return Err(invalid_data(
            format!("unsupported Private Payment List kind '{}'", wire.kind),
            None,
        ));
    }
    let mut payment_endpoints = HashMap::new();
    for (key, value) in wire.payment_endpoints {
        let payment_endpoint_identifier = PaymentEndpointIdentifier::new(&key).map_err(|err| {
            invalid_data(
                format!(
                    "Private Payment List contains invalid Payment Endpoint Identifier '{key}'"
                ),
                Some(err.into()),
            )
        })?;
        payment_endpoints.insert(
            payment_endpoint_identifier,
            PaymentEndpointPayload::new(value),
        );
    }
    Ok(PrivatePaymentList::new(payment_endpoints))
}

/// Serializes a Private Payment List into its JSON wire representation.
fn serialize_private_payment_list_json(list: &PrivatePaymentList) -> Result<String> {
    let payment_endpoints = list
        .payment_endpoints
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wire = PrivatePaymentListWireRef {
        version: list.version,
        kind: list.kind.as_str(),
        payment_endpoints,
    };
    serde_json::to_string(&wire).map_err(|err| {
        invalid_data(
            format!("failed to serialize Private Payment List JSON: {err}"),
            Some(err.into()),
        )
    })
}

/// Encrypts and sends a complete Private Payment List via the established
/// Encrypted Link.
///
/// The list must contain the full desired Payment List. Homeserver write
/// failures are retried according to [`EncryptedLink::set_max_send_retries`].
/// Oversized messages return [`crate::PaykitError::Validation`].
#[instrument(skip(link, list), fields(count = list.payment_endpoints.len()))]
pub async fn set_private_payment_list(
    link: &mut EncryptedLink,
    list: &PrivatePaymentList,
) -> Result<()> {
    debug!("sending Private Payment List");
    let json = serialize_private_payment_list_json(list)
        .map_err(|err| map_error("set_private_payment_list", err))?;
    link.send_private_payment_list_message(json.as_bytes())
        .await
        .map_err(|err| map_error("set_private_payment_list", err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PaykitError, PaymentEndpointIdentifier, PaymentEndpointPayload};
    use std::collections::HashMap;

    #[test]
    fn test_serialize_private_payment_list_json_uses_versioned_message() {
        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("lightning").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        let payload = PrivatePaymentList::new(payment_endpoints);
        let json = serialize_private_payment_list_json(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["kind"], "paykit.private_payment_list");
        assert!(value.get("reference").is_none());
        assert_eq!(value["payment_endpoints"]["lightning"], "ln...");
    }

    #[test]
    fn test_private_payment_list_debug_redacts_payloads() {
        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new("lightning").unwrap(),
            PaymentEndpointPayload::new("ln-secret"),
        );
        let list = PrivatePaymentList::new(payment_endpoints);
        let debug = format!("{list:?}");

        assert!(!debug.contains("ln-secret"));
        assert!(debug.contains("<redacted:"));
    }

    #[test]
    fn test_parse_private_payment_list_json_requires_versioned_message() {
        let err = parse_private_payment_list_json(r#"{"lightning": "ln..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Private Payment List"))
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_rejects_unsupported_version() {
        let err = parse_private_payment_list_json(
            r#"{"version":2,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment List version 2")),
            "expected unsupported version error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_rejects_unsupported_kind() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.receipt","payment_endpoints":{}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment List kind")),
            "expected unsupported kind error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_rejects_removed_reference_field() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","reference":"invoice-2026-0001","payment_endpoints":{}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unknown field") && context.contains("reference")),
            "expected unknown reference field error, got: {err}"
        );
    }

    // ── parse_private_payment_list_json tests ───────────────────────────────

    #[test]
    fn test_parse_private_payment_list_json_empty_string() {
        let err = parse_private_payment_list_json("").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData parse error for empty string, got: {err}"
        );
    }

    // ── Malformed JSON ──────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payment_list_json_truncated_object() {
        let err = parse_private_payment_list_json("{").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for truncated JSON, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_array_instead_of_object() {
        let err = parse_private_payment_list_json(r#"["lightning","onchain"]"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON array, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_plain_string() {
        let err = parse_private_payment_list_json(r#""just a string""#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for plain JSON string, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_number() {
        let err = parse_private_payment_list_json("42").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON number, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_non_string_values() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"lightning":123,"onchain":true}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for non-string values, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_trailing_comma() {
        let err = parse_private_payment_list_json(r#"{"lightning": "ln...",}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for trailing comma, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_null() {
        let err = parse_private_payment_list_json("null").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON null, got: {err}"
        );
    }

    // ── Invalid Payment Endpoint Identifiers inside valid JSON ────────────────────────────

    #[test]
    fn test_parse_private_payment_list_json_empty_key() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"":"ln..."}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for empty key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_path_traversal_key() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"..":"ln..."}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for path-traversal key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_slash_in_key() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"foo/bar":"ln..."}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for key with slash, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_reserved_private_key() {
        let err = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"private":"secret..."}}"#,
        )
        .unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for reserved 'private' key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_oversized_key() {
        let long_key = "a".repeat(65);
        let json = format!(
            r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"{long_key}":"ln..."}}}}"#
        );
        let err = parse_private_payment_list_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for oversized key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_one_valid_one_invalid_key() {
        // The valid key should not mask the invalid one.
        let err =
            parse_private_payment_list_json(r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"lightning":"ln...","":"bc1..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData when one key is invalid, got: {err}"
        );
    }

    // ── Happy path ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payment_list_json_valid_single_payment_endpoint() {
        let result = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"lightning":"ln..."}}"#,
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("lightning").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
    }

    #[test]
    fn test_parse_private_payment_list_json_valid_multiple_payment_endpoints() {
        let result =
            parse_private_payment_list_json(r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"lightning":"ln...","onchain":"bc1..."}}"#).unwrap();
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
    fn test_parse_private_payment_list_json_empty_object() {
        let result = parse_private_payment_list_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#,
        )
        .unwrap();
        assert!(result.is_empty());
    }
}
