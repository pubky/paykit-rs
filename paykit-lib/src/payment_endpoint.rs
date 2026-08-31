use std::collections::HashMap;

use tracing::{debug, instrument};

use crate::{error::map_error, pubky_routing, PaykitAppId, PaykitError, PublicKey, Result};

/// Machine-readable identifier for a Payment Endpoint.
///
/// A `PaymentEndpointIdentifier` is a single, safe path segment stored in an
/// app-owned endpoint directory. It is validated at construction time to
/// prevent path injection attacks.
///
/// # Allowed characters
/// ASCII alphanumeric (`a-z`, `A-Z`, `0-9`), hyphens (`-`), underscores (`_`),
/// and dots (`.`) — but the value must not consist solely of dots (i.e. `"."` and
/// `".."` are rejected).
///
/// # Limits
/// - Must not be empty.
/// - Must not exceed 64 characters.
/// - Must not be a reserved Paykit storage value.
///
/// # Examples
/// ```
/// # use paykit_lib::PaymentEndpointIdentifier;
/// let id = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
/// assert_eq!(id.as_str(), "btc-lightning-bolt11");
///
/// // Path traversal is rejected:
/// assert!(PaymentEndpointIdentifier::new("../etc/passwd").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentEndpointIdentifier(String);

/// Maximum length (in bytes) of a [`PaymentEndpointIdentifier`] value.
const PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN: usize = 64;
/// Reserved [`PaymentEndpointIdentifier`] value used by private Paykit storage.
const PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE: &str = "private";

impl PaymentEndpointIdentifier {
    /// Create a new `PaymentEndpointIdentifier` after validating the identifier.
    ///
    /// Returns `Err(PaykitError::Validation)` if the value is empty, too long,
    /// contains forbidden characters, resembles a path-traversal component,
    /// or collides with a reserved identifier.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();

        if id.is_empty() {
            return Err(PaykitError::Validation(
                "PaymentEndpointIdentifier must not be empty".into(),
            ));
        }

        if id.len() > PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN {
            return Err(PaykitError::Validation(format!(
                "PaymentEndpointIdentifier must not exceed {PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN} characters, got {}",
                id.chars().count()
            )));
        }

        if id == PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE {
            return Err(PaykitError::Validation(format!(
                "PaymentEndpointIdentifier '{id}' is reserved for Paykit storage"
            )));
        }

        // Every character must be ASCII alphanumeric, hyphen, underscore, or dot.
        if let Some((pos, ch)) = id
            .char_indices()
            .find(|&(_, ch)| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'))
        {
            return Err(PaykitError::Validation(format!(
                "PaymentEndpointIdentifier contains forbidden character '{}' at byte {pos} in \"{id}\"",
                ch
            )));
        }

        // Reject pure-dot names that are path-traversal components.
        if id.bytes().all(|b| b == b'.') {
            return Err(PaykitError::Validation(format!(
                "PaymentEndpointIdentifier must not be a path-traversal component: \"{id}\""
            )));
        }

        Ok(Self(id))
    }

    /// Access the inner identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaymentEndpointIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaymentEndpointIdentifier {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Serialized Payment Endpoint Payload served by a Payment Endpoint.
///
/// The payload is UTF-8 text such as JSON, lnurl, or another payment-specific
/// descriptor. If you need to transmit binary payloads, encode them (for
/// example base64) before wrapping in `PaymentEndpointPayload`.
/// Public storage helpers reject payloads larger than
/// [`PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentEndpointPayload(String);

/// Maximum encoded size of one public Payment Endpoint payload.
pub const PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES: usize = 64 * 1024;
/// Maximum number of public Payment Endpoints fetched for one Paykit App.
pub const PAYMENT_LIST_MAX_ENDPOINTS: usize = 256;

impl PaymentEndpointPayload {
    /// Wrap a UTF-8 string as a Payment Endpoint Payload.
    pub fn new(data: impl Into<String>) -> Self {
        Self(data.into())
    }

    /// Access the inner payload string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper and return the inner string.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for PaymentEndpointPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaymentEndpointPayload {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Collection of Payment Endpoints keyed by Payment Endpoint Identifiers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaymentList {
    /// Map of Payment Endpoint Identifier to Payment Endpoint Payload.
    pub payment_endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

/// Stores or updates a public payment endpoint in the authenticated Pubky session.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, PaykitAppId, PaymentEndpointIdentifier, PaymentEndpointPayload};
/// # async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
/// let app_id = PaykitAppId::new("bitkit")?;
/// let identifier = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
/// let payload = PaymentEndpointPayload::new("ln...");
/// set_payment_endpoint(session, &app_id, identifier, payload).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(session, payload), fields(identifier = %identifier))]
pub async fn set_payment_endpoint(
    session: &pubky::PubkySession,
    app_id: &PaykitAppId,
    identifier: PaymentEndpointIdentifier,
    payload: PaymentEndpointPayload,
) -> Result<()> {
    debug!("storing payment endpoint");
    pubky_routing::upsert_payment_endpoint(session, app_id, &identifier, &payload)
        .await
        .map_err(|err| map_error("set_payment_endpoint", err))
}

/// Removes a public payment endpoint from the authenticated Pubky session.
#[instrument(skip(session), fields(identifier = %identifier))]
pub async fn remove_payment_endpoint(
    session: &pubky::PubkySession,
    app_id: &PaykitAppId,
    identifier: PaymentEndpointIdentifier,
) -> Result<()> {
    debug!("removing payment endpoint");
    pubky_routing::delete_payment_endpoint(session, app_id, &identifier)
        .await
        .map_err(|err| map_error("remove_payment_endpoint", err))
}

/// Retrieves the public Payment List for the given payee.
///
/// # Semantics
/// - Returns an empty map when the payee has not published any endpoints or their
///   storage directory is missing.
/// - Returns `Err(PaykitError::InvalidData)` when a resource path is unparseable or
///   an endpoint payload contains invalid UTF-8.
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # #![recursion_limit = "256"]
/// # use paykit_lib::{get_payment_list, PaykitAppId};
/// # async fn demo(storage: &pubky::PublicStorage, pk: &paykit_lib::PublicKey) -> paykit_lib::Result<()> {
/// let app_id = PaykitAppId::new("bitkit")?;
/// let payments = get_payment_list(storage, pk, &app_id).await?;
/// if payments.payment_endpoints.is_empty() {
///     println!("payee published no endpoints yet");
/// } else {
///     for (identifier, payload) in &payments.payment_endpoints {
///         println!(
///             "identifier={} payload={}",
///             identifier.as_str(),
///             payload.as_str()
///         );
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(storage))]
pub async fn get_payment_list(
    storage: &pubky::PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
) -> Result<PaymentList> {
    get_payment_list_with_limits(
        storage,
        payee,
        app_id,
        PAYMENT_LIST_MAX_ENDPOINTS,
        PAYMENT_LIST_MAX_ENDPOINTS * PAYMENT_ENDPOINT_PAYLOAD_MAX_BYTES,
    )
    .await
}

/// Fetch an app's public Payment Endpoints within caller-supplied aggregate limits.
///
/// The fetch fails if the app publishes more endpoints or total payload bytes
/// than the supplied limits. Per-endpoint protocol limits still apply.
#[instrument(skip(storage))]
pub async fn get_payment_list_with_limits(
    storage: &pubky::PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    max_endpoints: usize,
    max_total_payload_bytes: usize,
) -> Result<PaymentList> {
    debug!("fetching Payment List");
    let result = pubky_routing::fetch_payment_list_with_limits(
        storage,
        payee,
        app_id,
        max_endpoints.min(PAYMENT_LIST_MAX_ENDPOINTS),
        max_total_payload_bytes,
    )
    .await
    .map_err(|err| map_error("get_payment_list", err))?;
    debug!(
        count = result.payment_endpoints.len(),
        "Payment List retrieved"
    );
    Ok(result)
}

/// Return whether a Payment List fetch failed because it exceeded caller-supplied limits.
pub fn is_payment_list_limit_exceeded(error: &PaykitError) -> bool {
    pubky_routing::is_payment_list_limit_exceeded(error)
}

/// Retrieves a specific Payment Endpoint for `payee` and `identifier`.
///
/// # Semantics
/// - Returns `Ok(None)` when the endpoint file is missing or empty.
/// - Returns `Err(PaykitError::InvalidData)` when the endpoint payload contains invalid UTF-8.
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # use paykit_lib::{get_payment_endpoint, PaykitAppId, PaymentEndpointIdentifier, PublicKey};
/// # async fn inspect(storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
/// let app_id = PaykitAppId::new("bitkit")?;
/// let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
/// if let Some(endpoint) = get_payment_endpoint(storage, pk, &app_id, &lightning).await? {
///     println!("lightning endpoint: {}", endpoint.as_str());
/// } else {
///     println!("no lightning endpoint published");
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(storage), fields(identifier = %identifier))]
pub async fn get_payment_endpoint(
    storage: &pubky::PublicStorage,
    payee: &PublicKey,
    app_id: &PaykitAppId,
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    debug!("fetching payment endpoint");
    let result = pubky_routing::fetch_payment_endpoint(storage, payee, app_id, identifier)
        .await
        .map_err(|err| map_error("get_payment_endpoint", err))?;
    debug!(found = result.is_some(), "payment endpoint lookup complete");
    Ok(result)
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    // ── PaymentEndpointIdentifier: valid inputs ──────────────────────────────────────────

    #[test]
    fn test_payment_endpoint_identifier_valid_simple_names() {
        for name in [
            "btc-lightning-bolt11",
            "btc-lightning-bolt12",
            "btc-bitcoin-p2tr",
        ] {
            assert!(
                PaymentEndpointIdentifier::new(name).is_ok(),
                "expected '{name}' to be valid"
            );
        }
    }

    #[test]
    fn test_payment_endpoint_identifier_valid_with_dots() {
        let m = PaymentEndpointIdentifier::new("method.v2").unwrap();
        assert_eq!(m.as_str(), "method.v2");
    }

    #[test]
    fn test_payment_endpoint_identifier_valid_with_underscores() {
        let m = PaymentEndpointIdentifier::new("my_method").unwrap();
        assert_eq!(m.as_str(), "my_method");
    }

    #[test]
    fn test_payment_endpoint_identifier_valid_mixed_case() {
        let m = PaymentEndpointIdentifier::new("LnUrl-Pay").unwrap();
        assert_eq!(m.as_str(), "LnUrl-Pay");
    }

    #[test]
    fn test_payment_endpoint_identifier_valid_max_length() {
        let name = "a".repeat(PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN);
        assert!(PaymentEndpointIdentifier::new(&name).is_ok());
    }

    #[test]
    fn test_payment_endpoint_identifier_valid_single_char() {
        assert!(PaymentEndpointIdentifier::new("x").is_ok());
    }

    #[test]
    fn test_payment_endpoint_identifier_display() {
        let m = PaymentEndpointIdentifier::new("lightning").unwrap();
        assert_eq!(format!("{m}"), "lightning");
    }

    #[test]
    fn test_payment_endpoint_identifier_as_ref() {
        let m = PaymentEndpointIdentifier::new("onchain").unwrap();
        let s: &str = m.as_ref();
        assert_eq!(s, "onchain");
    }

    // ── PaymentEndpointIdentifier: invalid inputs ────────────────────────────────────────

    #[test]
    fn test_payment_endpoint_identifier_reject_empty() {
        let err = PaymentEndpointIdentifier::new("").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("empty")));
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_path_traversal_dotdot() {
        assert!(PaymentEndpointIdentifier::new("..").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_path_traversal_dot() {
        assert!(PaymentEndpointIdentifier::new(".").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_path_traversal_sequence() {
        // Slashes are rejected by the character allowlist, but verify the
        // specific traversal pattern is caught.
        assert!(PaymentEndpointIdentifier::new("../etc/passwd").is_err());
        assert!(PaymentEndpointIdentifier::new("../../foo").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_forward_slash() {
        assert!(PaymentEndpointIdentifier::new("foo/bar").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_backslash() {
        assert!(PaymentEndpointIdentifier::new("a\\b").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_null_byte() {
        assert!(PaymentEndpointIdentifier::new("foo\0bar").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_too_long() {
        let name = "a".repeat(PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN + 1);
        let err = PaymentEndpointIdentifier::new(&name).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("exceed")));
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_space() {
        assert!(PaymentEndpointIdentifier::new("foo bar").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_special_chars() {
        for bad in ["foo@bar", "foo:bar", "foo?bar", "foo#bar", "foo=bar"] {
            assert!(
                PaymentEndpointIdentifier::new(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_unicode() {
        assert!(PaymentEndpointIdentifier::new("⚡lightning").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_triple_dots() {
        assert!(PaymentEndpointIdentifier::new("...").is_err());
    }

    #[test]
    fn test_payment_endpoint_identifier_reject_reserved_values() {
        let err = PaymentEndpointIdentifier::new("private").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("reserved")));
    }

    // ── PaymentEndpointPayload: basic accessors ───────────────────────────────────

    #[test]
    fn test_payment_endpoint_payload_new_and_accessors() {
        let d = PaymentEndpointPayload::new("ln...");
        assert_eq!(d.as_str(), "ln...");
        assert_eq!(format!("{d}"), "ln...");
    }

    #[test]
    fn test_payment_endpoint_payload_into_inner() {
        let d = PaymentEndpointPayload::new("payload");
        assert_eq!(d.into_inner(), "payload");
    }

    #[test]
    fn test_payment_endpoint_payload_as_ref() {
        let d = PaymentEndpointPayload::new("data");
        let s: &str = d.as_ref();
        assert_eq!(s, "data");
    }
}
