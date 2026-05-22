#![doc = include_str!("../README.md")]

use std::collections::HashMap;

use thiserror::Error;
use tracing::{debug, instrument, warn};

pub use pubky::{PubkySession, PublicKey, PublicStorage};
pub use pubky_noise;

mod encrypted_link;
mod private_message_dispatch;
mod private_payment_envelope;
mod pubky_routing;
mod receipt;

use encrypted_link::send_private_message;
pub use encrypted_link::{
    accept_encrypted_link, advance_handshake, close_encrypted_link, initiate_encrypted_link,
    restore_encrypted_link, restore_encrypted_link_from_config, restore_encrypted_link_handshake,
    restore_encrypted_link_handshake_from_config, EncryptedLink, EncryptedLinkHandshake,
    EncryptedLinkHandshakeSnapshot, EncryptedLinkSnapshot, HandshakeProgress,
    DEFAULT_MAX_RECOVERY_ATTEMPTS, DEFAULT_MAX_SEND_RETRIES,
};
#[cfg(test)]
use encrypted_link::{is_retryable_private_send_error, send_attempts_from_retries};
pub use private_message_dispatch::PrivateMessageKind;
pub use private_payment_envelope::{
    get_private_payment_envelope, set_private_payment_envelope, PrivatePaymentEnvelope,
};
pub use pubky_routing::paths::{PAYKIT_PATH_PREFIX, PAYKIT_PRIVATE_PATH_PREFIX};
pub use receipt::{
    decrypt_receipt, IssuedReceipt, Receipt, ReceiptAccess, ReceiptDecryptionKey, ReceiptDraft,
};
use receipt::{parse_receipt_access_json, serialize_receipt_access_json};

/// Common result alias for Paykit operations.
pub type Result<T> = std::result::Result<T, PaykitError>;

/// Domain-specific error type.
///
/// Variants that wrap an underlying cause carry a `source` field backed by
/// [`anyhow::Error`] so that the standard [`std::error::Error::source`] chain
/// is preserved while keeping the public API decoupled from upstream error
/// types. Callers can downcast the source via [`anyhow::Error::downcast_ref`]
/// when they need the original typed error.
#[derive(Debug, Error)]
pub enum PaykitError {
    /// Wrapper for transport layer failures.
    ///
    /// Most user-facing failures bubble up through this variant, encapsulating
    /// lower-level SDK/network errors (timeouts, connection refused, permission
    /// denied, etc.).
    #[error("transport error: {context}")]
    Transport {
        /// Human-readable description of what went wrong.
        context: String,
        /// The underlying error that caused this failure.
        #[source]
        source: anyhow::Error,
    },

    /// The requested resource does not exist.
    ///
    /// Returned when a Payment Endpoint or other resource is not found (404/GONE).
    #[error("not found: {0}")]
    NotFound(String),

    /// Retrieved data is corrupt or structurally invalid.
    ///
    /// Returned when a resource was successfully fetched from the network but its
    /// content cannot be interpreted — for example invalid UTF-8 bytes or an
    /// unparseable resource path. This is distinct from
    /// [`PaykitError::Transport`] (the network call itself failed).
    #[error("invalid data: {context}")]
    InvalidData {
        /// Human-readable description of the data problem.
        context: String,
        /// The underlying error, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Input failed validation.
    ///
    /// Returned when a caller-supplied value (such as a [`PaymentEndpointIdentifier`]) violates
    /// structural invariants — for example containing path-traversal sequences,
    /// null bytes, or characters outside the allowed set.
    #[error("validation error: {0}")]
    Validation(String),
}

/// Machine-readable identifier for a Payment Endpoint.
///
/// A `PaymentEndpointIdentifier` is a single, safe path segment stored under `/pub/paykit/v0/…`.
/// It identifies a Payment Endpoint type and is validated at
/// construction time to prevent path injection attacks.
///
/// # Allowed characters
/// ASCII alphanumeric (`a-z`, `A-Z`, `0-9`), hyphens (`-`), underscores (`_`),
/// and dots (`.`) — but the value must not consist solely of dots (i.e. `"."` and
/// `".."` are rejected).
///
/// # Limits
/// - Must not be empty.
/// - Must not exceed 64 characters.
/// - Must not be the reserved value `"private"`.
///
/// # Examples
/// ```
/// # use paykit_lib::PaymentEndpointIdentifier;
/// let m = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
/// assert_eq!(m.as_str(), "btc-lightning-bolt11");
///
/// // Path traversal is rejected:
/// assert!(PaymentEndpointIdentifier::new("../etc/passwd").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentEndpointIdentifier(String);

/// Maximum length (in bytes) of a [`PaymentEndpointIdentifier`] value.
const PAYMENT_ENDPOINT_IDENTIFIER_MAX_LEN: usize = 64;
/// Reserved [`PaymentEndpointIdentifier`] value used by private-payment storage.
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
                "PaymentEndpointIdentifier '{PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE}' is reserved for private payments"
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

/// UTF-8 wrapper for a Payment Endpoint Payload.
///
/// This is the payload part of a Payment Endpoint: UTF-8 text such as JSON,
/// LNURL, an address, an invoice, an offer, or another payment-specific handle.
/// If you need to transmit binary payloads, encode them (e.g., base64) before
/// wrapping in `PaymentEndpointPayload`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentEndpointPayload(String);

impl PaymentEndpointPayload {
    /// Wrap a payload string as a Payment Endpoint Payload.
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

/// A whole payee-owned entry in a Payment List.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentEndpoint {
    /// Machine-readable type identifier for this Payment Endpoint.
    pub identifier: PaymentEndpointIdentifier,
    /// Payee-owned receiving payload for this Payment Endpoint.
    pub payload: PaymentEndpointPayload,
}

/// Payee-published or shared list of Payment Endpoints.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaymentList {
    /// Payment Endpoints keyed by Payment Endpoint Identifier.
    pub endpoints: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

/// UUID-v4 correlation reference used to connect private payment offers and receipts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentReference(String);

impl PaymentReference {
    /// Create a payment reference after validating that the input is a UUID v4 string.
    ///
    /// Accepted UUID-v4 inputs are canonicalized to lowercase hyphenated form.
    pub fn new(reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        let uuid = uuid::Uuid::try_parse(&reference).map_err(|err| {
            PaykitError::Validation(format!("payment reference must be a UUID v4 string: {err}"))
        })?;
        if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
            return Err(PaykitError::Validation(
                "payment reference must be an RFC4122 UUID v4 string".into(),
            ));
        }
        Ok(Self(uuid.hyphenated().to_string()))
    }

    /// Generate a fresh random UUID-v4 payment reference.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the inner UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaymentReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaymentReference {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Stores or updates a Payment Endpoint via the authenticated Pubky session.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, PaymentEndpointIdentifier, PaymentEndpointPayload, PubkySession};
/// # async fn demo(client: &PubkySession) -> paykit_lib::Result<()> {
/// let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
/// let data = PaymentEndpointPayload::new("ln...");
/// set_payment_endpoint(client, method, data).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(client, data), fields(method = %method))]
pub async fn set_payment_endpoint(
    client: &PubkySession,
    method: PaymentEndpointIdentifier,
    data: PaymentEndpointPayload,
) -> Result<()> {
    debug!("storing payment endpoint");
    pubky_routing::public_storage::for_session(client)
        .set_payment_endpoint(&method, &data)
        .await
        .map_err(|err| map_error("set_payment_endpoint", err))
}

/// Issues, stores, and shares an encrypted payment receipt with the linked peer.
///
/// The encrypted receipt is written to the caller's homeserver at a deterministic
/// Paykit receipt path derived from `draft.reference`. A fresh symmetric receipt
/// key is generated for each call. The corresponding [`ReceiptAccess`] envelope
/// is then sent over the existing Noise channel so the peer can fetch and decrypt
/// the stored receipt with [`decrypt_receipt`].
///
/// Receipts are event-like private messages: every receipt access message matters.
/// Reissuing the same [`PaymentReference`] stores a new encrypted receipt at the
/// same location with a new key, so older access descriptors for that reference
/// may no longer decrypt after a later successful reissue.
///
/// # Identity binding
///
/// `session` is used for homeserver storage, while `link` is used to send the
/// receipt-access message. Paykit does not currently verify that `session`
/// belongs to the same local identity that established `link`; callers must pass
/// the matching session or they may persist the receipt under the wrong identity
/// while sending access over a different encrypted link.
///
/// # Durability and ordering
///
/// This function stores the encrypted receipt first and sends access second. If
/// the process crashes, or the Noise send fails after storage succeeds, the
/// encrypted receipt may remain on the homeserver without the counterparty ever
/// receiving access. Callers that need stronger delivery guarantees should keep
/// their own durable issuance state and retry or reconcile at the application
/// layer.
///
/// # Secrets
///
/// The returned [`IssuedReceipt::key`] is sensitive decryption material. Paykit
/// redacts it from `Debug` and `Display`, but callers must not log or persist the
/// raw [`ReceiptDecryptionKey::as_str`] value outside secure storage.
///
/// # Errors
/// - Returns [`PaykitError::InvalidData`] if receipt serialization or encryption
///   fails.
/// - Returns [`PaykitError::Transport`] if storing the encrypted receipt fails or
///   the receipt-access Noise message cannot be sent after configured retries.
#[instrument(skip(session, link, draft))]
pub async fn issue_receipt(
    session: &pubky::PubkySession,
    link: &mut EncryptedLink,
    draft: ReceiptDraft,
) -> Result<IssuedReceipt> {
    debug!("issuing encrypted receipt");
    let reference = draft.reference;
    let key = ReceiptDecryptionKey::generate();
    let receipt = Receipt {
        reference: reference.clone(),
        recipient_public_key: link.recipient.clone(),
        payment_endpoint_identifier: draft.payment_endpoint_identifier,
        amount: draft.amount,
        currency: draft.currency,
        metadata: draft.metadata,
    };
    let encrypted = receipt
        .encrypt(&key)
        .map_err(|err| map_error("issue_receipt", err))?;

    let location = pubky_routing::public_storage::for_session(session)
        .store_encrypted_receipt(&reference, encrypted)
        .await?;

    let access = ReceiptAccess {
        version: 1,
        kind: PrivateMessageKind::ReceiptAccess,
        reference: reference.clone(),
        location: location.clone(),
        key: key.clone(),
        algorithm: "XChaCha20Poly1305".to_string(),
    };
    let json =
        serialize_receipt_access_json(&access).map_err(|err| map_error("issue_receipt", err))?;
    send_private_message(link, json.as_bytes(), "receipt access")
        .await
        .map_err(|err| map_error("issue_receipt", err))?;

    Ok(IssuedReceipt {
        reference,
        location,
        key,
    })
}

/// Removes a Payment Endpoint via the authenticated Pubky session.
#[instrument(skip(client), fields(method = %method))]
pub async fn remove_payment_endpoint(
    client: &PubkySession,
    method: PaymentEndpointIdentifier,
) -> Result<()> {
    debug!("removing payment endpoint");
    pubky_routing::public_storage::for_session(client)
        .remove_payment_endpoint(&method)
        .await
        .map_err(|err| map_error("remove_payment_endpoint", err))
}

/// Retrieves the payee-published Payment List for the given payee.
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
/// # use paykit_lib::{get_payment_list, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentList, PublicStorage};
/// # async fn demo(reader: &PublicStorage, pk: &paykit_lib::PublicKey) -> paykit_lib::Result<()> {
/// let payments = get_payment_list(reader, pk).await?;
/// if payments.endpoints.is_empty() {
///     println!("payee published no endpoints yet");
/// } else {
///     for (method, data) in &payments.endpoints {
///         println!("method={} payload={}", method.as_str(), data.as_str());
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader))]
pub async fn get_payment_list(reader: &PublicStorage, payee: &PublicKey) -> Result<PaymentList> {
    debug!("fetching payment list");
    let result = pubky_routing::public_storage::for_reader(reader)
        .get_payment_list(payee)
        .await
        .map_err(|err| map_error("get_payment_list", err))?;
    debug!(count = result.endpoints.len(), "payment list retrieved");
    Ok(result)
}

/// Receives all currently available receipt access descriptors from the encrypted link.
///
/// Unlike [`get_private_payment_envelope`], this is FIFO/event-like. Every currently
/// available receipt access message is returned in send order in a single vector;
/// older receipt access messages are not collapsed when newer ones arrive.
/// Returns an empty vector when no receipt access messages are currently available.
///
/// Messages for other supported private app kinds remain buffered on the
/// [`EncryptedLink`] for their own typed receiver. Malformed unrelated app
/// messages are ignored by the shared dispatcher. Syntactically valid messages
/// with unsupported `kind` values are logged and dropped by the shared
/// dispatcher rather than buffered indefinitely. Malformed receipt-access
/// messages are dropped with diagnostics while later valid receipt-access
/// messages in the same batch are still returned.
///
/// Each selected receipt access location must match the canonical Paykit receipt
/// path for its [`PaymentReference`].
///
/// The returned [`ReceiptAccess::key`] values are sensitive. Their formatting is
/// redacted, but callers must still avoid logging raw key material from
/// [`ReceiptDecryptionKey::as_str`].
#[instrument(skip(link))]
pub async fn get_receipt_access(link: &mut EncryptedLink) -> Result<Vec<ReceiptAccess>> {
    debug!("receiving receipt access messages");

    let stats = link
        .private_messages
        .receive_available(&mut link.encryptor)
        .await?;
    let raw_messages = link
        .private_messages
        .take_all_fifo(PrivateMessageKind::ReceiptAccess);
    if raw_messages.is_empty() {
        debug!(
            received = stats.received,
            "no receipt access messages available"
        );
        return Ok(Vec::new());
    }

    let mut access = Vec::new();
    let mut malformed = 0usize;
    for raw in &raw_messages {
        match parse_receipt_access_json(raw.plaintext()) {
            Ok(parsed) => access.push(parsed),
            Err(err) => {
                malformed += 1;
                warn!(
                    error = ?err,
                    "dropping malformed receipt access message while preserving later valid messages"
                );
            }
        }
    }
    if malformed > 0 {
        warn!(
            malformed,
            selected = raw_messages.len(),
            "ignored malformed receipt access messages while preserving valid messages"
        );
    }
    debug!(
        count = access.len(),
        received = stats.received,
        pending = link.private_messages.len(),
        "receipt access messages received"
    );
    Ok(access)
}

/// Retrieves a specific Payment Endpoint for `payee` and `method`.
///
/// # Semantics
/// - Returns `Ok(None)` when the endpoint file is missing or empty.
/// - Returns `Err(PaykitError::InvalidData)` when the endpoint payload contains invalid UTF-8.
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # use paykit_lib::{get_payment_endpoint, PaymentEndpointIdentifier, PublicKey, PublicStorage};
/// # async fn inspect(reader: &PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
/// let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11")?;
/// if let Some(endpoint) = get_payment_endpoint(reader, pk, &lightning).await? {
///     println!("lightning endpoint: {}", endpoint.as_str());
/// } else {
///     println!("no lightning endpoint published");
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader), fields(method = %method))]
pub async fn get_payment_endpoint(
    reader: &PublicStorage,
    payee: &PublicKey,
    method: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    debug!("fetching payment endpoint");
    let result = pubky_routing::public_storage::for_reader(reader)
        .get_payment_endpoint(payee, method)
        .await
        .map_err(|err| map_error("get_payment_endpoint", err))?;
    debug!(found = result.is_some(), "payment endpoint lookup complete");
    Ok(result)
}

fn map_error(label: &'static str, err: PaykitError) -> PaykitError {
    match err {
        PaykitError::Transport { context, source } => PaykitError::Transport {
            context: format!("{label}: {context}"),
            source,
        },
        PaykitError::NotFound(msg) => PaykitError::NotFound(format!("{label}: {msg}")),
        PaykitError::InvalidData { context, source } => PaykitError::InvalidData {
            context: format!("{label}: {context}"),
            source,
        },
        PaykitError::Validation(msg) => PaykitError::Validation(format!("{label}: {msg}")),
    }
}

/// Unit tests for input validation (no network required).
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
        let m = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        assert_eq!(format!("{m}"), "btc-lightning-bolt11");
    }

    #[test]
    fn test_payment_endpoint_identifier_as_ref() {
        let m = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
        let s: &str = m.as_ref();
        assert_eq!(s, "btc-bitcoin-p2tr");
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
    fn test_payment_endpoint_identifier_reject_reserved_private() {
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

/// Integration tests (require `pubky` feature and ephemeral testnet).
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use pubky::PubkySession;
    use pubky_testnet::{embedded_postgres::EmbeddedPostgres, pubky::Keypair, EphemeralTestnet};
    use tokio::sync::{Mutex as TokioMutex, OnceCell};

    static SHARED_POSTGRES: OnceCell<EmbeddedPostgres> = OnceCell::const_new();
    static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

    async fn shared_postgres() -> &'static EmbeddedPostgres {
        SHARED_POSTGRES
            .get_or_init(|| async {
                EmbeddedPostgres::start()
                    .await
                    .expect("failed to start embedded postgres")
            })
            .await
    }

    async fn build_testnet() -> EphemeralTestnet {
        let _guard = TESTNET_BUILD_LOCK.lock().await;

        let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
            EphemeralTestnet::builder()
        } else {
            let postgres = shared_postgres()
                .await
                .connection_string()
                .expect("embedded postgres connection string should be valid");
            EphemeralTestnet::builder().postgres(postgres)
        };

        builder.build().await.unwrap()
    }

    struct TestSetup {
        _testnet: EphemeralTestnet,
        session: PubkySession,
        reader: PublicStorage,
        raw_session: PubkySession,
        public_key: PublicKey,
    }

    impl TestSetup {
        async fn new() -> Self {
            let testnet = build_testnet().await;

            let homeserver = testnet.homeserver_app();
            let sdk = testnet.sdk().unwrap();

            let pair = Keypair::random();
            let signer = sdk.signer(pair.clone());
            let session = signer.signup(&homeserver.public_key(), None).await.unwrap();

            let reader = sdk.public_storage();

            Self {
                _testnet: testnet,
                session: session.clone(),
                reader,
                raw_session: session,
                public_key: pair.public_key(),
            }
        }
    }

    #[test]
    fn test_send_attempts_from_retries_bounds() {
        assert_eq!(send_attempts_from_retries(0), 1);
        assert_eq!(send_attempts_from_retries(3), 4);
        assert_eq!(send_attempts_from_retries(u32::MAX), u32::MAX);
    }

    #[test]
    fn test_private_send_retry_classification() {
        assert!(is_retryable_private_send_error(
            &pubky_noise::PubkyNoiseError::HomeserverWriteError,
        ));

        for err in [
            pubky_noise::PubkyNoiseError::IsHandshake,
            pubky_noise::PubkyNoiseError::EncryptionError,
            pubky_noise::PubkyNoiseError::CounterOverflow,
            pubky_noise::PubkyNoiseError::NonceOverflow,
        ] {
            assert!(
                !is_retryable_private_send_error(&err),
                "{err:?} should not be retried"
            );
        }
    }

    #[tokio::test]
    async fn endpoint_round_trip_and_update() {
        let setup = TestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
        let endpoint = PaymentEndpointPayload::new("{\"address\":\"bc1...\"}");

        set_payment_endpoint(&setup.session, method.clone(), endpoint.clone())
            .await
            .unwrap();

        let fetched = get_payment_endpoint(&setup.reader, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(fetched, Some(endpoint.clone()));

        let list = get_payment_list(&setup.reader, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list,
            PaymentList {
                endpoints: vec![(method.clone(), endpoint.clone())]
                    .into_iter()
                    .collect()
            }
        );

        let new_endpoint = PaymentEndpointPayload::new("{\"address\":\"1c1...\"}");

        set_payment_endpoint(&setup.session, method.clone(), new_endpoint.clone())
            .await
            .unwrap();

        let updated = get_payment_endpoint(&setup.reader, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(updated, Some(new_endpoint.clone()));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn missing_endpoint_returns_none() {
        let setup = TestSetup::new().await;
        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();

        let missing = get_payment_endpoint(&setup.reader, &setup.public_key, &method)
            .await
            .unwrap();
        assert!(missing.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_additions_and_removals() {
        let setup = TestSetup::new().await;

        let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
        let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let onchain_data = PaymentEndpointPayload::new("bc1p...");
        let lightning_data = PaymentEndpointPayload::new("ln...");

        set_payment_endpoint(&setup.session, onchain.clone(), onchain_data.clone())
            .await
            .unwrap();
        set_payment_endpoint(&setup.session, lightning.clone(), lightning_data.clone())
            .await
            .unwrap();

        let list = get_payment_list(&setup.reader, &setup.public_key)
            .await
            .unwrap();
        let mut expected = HashMap::new();
        expected.insert(onchain.clone(), onchain_data.clone());
        expected.insert(lightning.clone(), lightning_data.clone());
        assert_eq!(list.endpoints, expected);

        remove_payment_endpoint(&setup.session, onchain.clone())
            .await
            .unwrap();
        let list = get_payment_list(&setup.reader, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list.endpoints,
            vec![(lightning.clone(), lightning_data.clone())]
                .into_iter()
                .collect()
        );

        remove_payment_endpoint(&setup.session, lightning.clone())
            .await
            .unwrap();
        let empty = get_payment_list(&setup.reader, &setup.public_key)
            .await
            .unwrap();
        assert!(empty.endpoints.is_empty());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn removing_missing_endpoint_is_error() {
        let setup = TestSetup::new().await;
        let method = PaymentEndpointIdentifier::new("unused").unwrap();

        remove_payment_endpoint(&setup.session, method)
            .await
            .expect_err("removing non-existent endpoint should fail");

        setup.raw_session.signout().await.unwrap();
    }

    // ── Private payments test infrastructure ────────────────────────────

    /// Test setup that creates two users and initializes handshake handles
    /// without driving them to completion.
    struct InProgressHandshakeSetup {
        _testnet: EphemeralTestnet,
        initiator_session: PubkySession,
        responder_session: PubkySession,
        initiator_handshake: EncryptedLinkHandshake,
        responder_handshake: EncryptedLinkHandshake,
    }

    impl InProgressHandshakeSetup {
        async fn new() -> Self {
            let testnet = build_testnet().await;
            let homeserver = testnet.homeserver_app();

            let initiator_sdk = testnet.sdk().unwrap();
            let responder_sdk = testnet.sdk().unwrap();

            let initiator_keypair = Keypair::random();
            let initiator_signer = initiator_sdk.signer(initiator_keypair.clone());
            let initiator_session = initiator_signer
                .signup(&homeserver.public_key(), None)
                .await
                .unwrap();

            let responder_keypair = Keypair::random();
            let responder_signer = responder_sdk.signer(responder_keypair.clone());
            let responder_session = responder_signer
                .signup(&homeserver.public_key(), None)
                .await
                .unwrap();

            let initiator_public_key = initiator_session.info().public_key();
            let responder_public_key = responder_session.info().public_key();

            let initiator_handshake = initiate_encrypted_link(
                initiator_session.clone(),
                initiator_keypair.secret_key(),
                responder_public_key,
                initiator_sdk,
            )
            .unwrap();

            let responder_handshake = accept_encrypted_link(
                responder_session.clone(),
                responder_keypair.secret_key(),
                initiator_public_key,
                responder_sdk,
            )
            .unwrap();

            Self {
                _testnet: testnet,
                initiator_session,
                responder_session,
                initiator_handshake,
                responder_handshake,
            }
        }
    }

    /// Test setup for private (encrypted) payment flows.
    ///
    /// Creates two users on the same ephemeral testnet, performs a full Noise XX
    /// handshake between them using the public `initiate_encrypted_link` /
    /// `accept_encrypted_link` / `advance_handshake` API, and produces ready-to-use
    /// [`EncryptedLink`] handles so that `set_private_payment_envelope` /
    /// `get_private_payment_envelope` can be exercised.
    struct PrivateTestSetup {
        _testnet: EphemeralTestnet,
        /// Sender's encrypted link (writes private payments).
        sender_link: EncryptedLink,
        /// Sender's session (kept for cleanup via `signout`).
        sender_session: PubkySession,
        /// Receiver's encrypted link (reads private payments).
        receiver_link: EncryptedLink,
        /// Receiver's session (kept for cleanup via `signout`).
        receiver_session: PubkySession,
    }

    /// Drives a handshake to completion by polling `advance_handshake` with a
    /// short sleep between retries. Panics on timeout (10 s).
    async fn drive_handshake_to_completion(mut handshake: EncryptedLinkHandshake) -> EncryptedLink {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let timeout = Duration::from_secs(10);

        loop {
            assert!(
                start.elapsed() < timeout,
                "handshake timed out after {timeout:?}"
            );

            match advance_handshake(handshake).await.unwrap() {
                HandshakeProgress::Pending(h) => {
                    handshake = h;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                HandshakeProgress::Complete(link) => return link,
            }
        }
    }

    impl PrivateTestSetup {
        async fn new() -> Self {
            let testnet = build_testnet().await;
            let homeserver = testnet.homeserver_app();

            // Each user gets its own Pubky SDK instance.
            let sender_sdk = testnet.sdk().unwrap();
            let receiver_sdk = testnet.sdk().unwrap();

            // Sign up two independent users.
            let sender_keypair = Keypair::random();
            let sender_signer = sender_sdk.signer(sender_keypair.clone());
            let sender_session = sender_signer
                .signup(&homeserver.public_key(), None)
                .await
                .unwrap();

            let receiver_keypair = Keypair::random();
            let receiver_signer = receiver_sdk.signer(receiver_keypair.clone());
            let receiver_session = receiver_signer
                .signup(&homeserver.public_key(), None)
                .await
                .unwrap();

            let sender_public_key = sender_session.info().public_key();
            let receiver_public_key = receiver_session.info().public_key();

            // Initiate handshake from sender side.
            let sender_handshake = initiate_encrypted_link(
                sender_session.clone(),
                sender_keypair.secret_key(),
                receiver_public_key,
                sender_sdk,
            )
            .unwrap();

            // Accept handshake from receiver side.
            let receiver_handshake = accept_encrypted_link(
                receiver_session.clone(),
                receiver_keypair.secret_key(),
                sender_public_key,
                receiver_sdk,
            )
            .unwrap();

            // Drive both handshakes to completion concurrently.
            let (sender_link, receiver_link) = tokio::join!(
                drive_handshake_to_completion(sender_handshake),
                drive_handshake_to_completion(receiver_handshake),
            );

            Self {
                _testnet: testnet,
                sender_link,
                sender_session,
                receiver_link,
                receiver_session,
            }
        }
    }

    // ── Private payments tests ──────────────────────────────────────────

    fn private_payload(
        entries: &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> PrivatePaymentEnvelope {
        PrivatePaymentEnvelope::new(PaymentReference::new_v4(), entries.clone()).unwrap()
    }

    const TEST_RECEIPT_ACCESS_JSON: &str = r#"{"version":1,"kind":"paykit.receipt_access","reference":"550e8400-e29b-41d4-a716-446655440000"}"#;

    async fn send_raw_private_message(link: &mut EncryptedLink, json: &str) {
        assert!(
            json.len() <= pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
            "test raw message exceeds pubky-noise message size"
        );
        link.encryptor
            .send_message(json.as_bytes())
            .await
            .expect("raw private message should send");
    }

    #[tokio::test]
    async fn private_payment_envelope_empty_returns_empty() {
        let mut setup = PrivateTestSetup::new().await;

        let result = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "fresh link with no messages should return no payload"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payment_envelope_round_trip() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        set_private_payment_envelope(
            &mut setup.sender_link,
            &PrivatePaymentEnvelope::new(reference.clone(), entries).unwrap(),
        )
        .await
        .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.reference, reference);
        assert_eq!(received.endpoints().len(), 1);
        assert_eq!(received.endpoints().get(&method), Some(&data));

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payment_envelope_multiple_methods() {
        let mut setup = PrivateTestSetup::new().await;

        let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
        let cashu = PaymentEndpointIdentifier::new("cashu-mint_id").unwrap();

        let mut entries = HashMap::new();
        entries.insert(lightning.clone(), PaymentEndpointPayload::new("ln..."));
        entries.insert(onchain.clone(), PaymentEndpointPayload::new("bc1p..."));
        entries.insert(
            cashu.clone(),
            PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"),
        );

        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.endpoints().len(), 3);
        assert_eq!(
            received.endpoints().get(&lightning),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
        assert_eq!(
            received.endpoints().get(&onchain),
            Some(&PaymentEndpointPayload::new("bc1p..."))
        );
        assert_eq!(
            received.endpoints().get(&cashu),
            Some(&PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"))
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payment_envelope_update_overwrites() {
        let mut setup = PrivateTestSetup::new().await;

        // First write: lightning only.
        let mut entries_v1 = HashMap::new();
        entries_v1.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("v1"),
        );
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries_v1))
            .await
            .unwrap();

        // Second write: completely different map (onchain only).
        let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
        let mut entries_v2 = HashMap::new();
        entries_v2.insert(onchain.clone(), PaymentEndpointPayload::new("v2"));
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries_v2))
            .await
            .unwrap();

        // The helper drains queued unread updates and returns the latest map.
        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.endpoints().len(), 1);
        assert_eq!(
            received.endpoints().get(&onchain),
            Some(&PaymentEndpointPayload::new("v2"))
        );

        // Backlog is drained, so a second immediate call returns empty.
        let empty = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap();
        assert!(empty.is_none());

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payment_envelope_rejects_oversized_payload() {
        let mut setup = PrivateTestSetup::new().await;

        // Build a map whose serialized JSON exceeds PUBKY_NOISE_MSG_LEN (1000 bytes).
        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let oversized_value = "x".repeat(1000);
        let mut entries = HashMap::new();
        entries.insert(method, PaymentEndpointPayload::new(oversized_value));

        let result =
            set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected Validation error about size, got: {err}"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_preserves_newer_receipt_access_messages() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();
        send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("private payments message should not be lost behind receipt message");
        assert_eq!(received.endpoints().get(&method), Some(&data));
        assert_eq!(setup.receiver_link.private_messages.len(), 1);
        assert_eq!(
            setup
                .receiver_link
                .private_messages
                .kind_at(0)
                .unwrap()
                .as_str(),
            "paykit.receipt_access"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_preserves_older_receipt_access_messages() {
        let mut setup = PrivateTestSetup::new().await;

        send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("private payments message should be found without dropping receipt message");
        assert_eq!(received.endpoints().get(&method), Some(&data));
        assert_eq!(setup.receiver_link.private_messages.len(), 1);
        assert_eq!(
            setup
                .receiver_link
                .private_messages
                .kind_at(0)
                .unwrap()
                .as_str(),
            "paykit.receipt_access"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_drops_unknown_messages_without_buffering_them() {
        let mut setup = PrivateTestSetup::new().await;

        send_raw_private_message(
            &mut setup.sender_link,
            r#"{"version":1,"kind":"paykit.future_kind","payload":"ignored"}"#,
        )
        .await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid private payments message should survive unknown earlier message");
        assert_eq!(received.endpoints().get(&method), Some(&data));
        assert!(setup.receiver_link.private_messages.len() == 0);

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_ignores_malformed_messages_before_valid_payment() {
        let mut setup = PrivateTestSetup::new().await;

        send_raw_private_message(&mut setup.sender_link, "not-json").await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid private payments message should survive malformed earlier message");
        assert_eq!(received.endpoints().get(&method), Some(&data));
        assert!(setup.receiver_link.private_messages.len() == 0);

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_ignores_malformed_messages_after_valid_payment() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();
        send_raw_private_message(&mut setup.sender_link, "not-json").await;

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid private payments message should survive malformed later message");
        assert_eq!(received.endpoints().get(&method), Some(&data));
        assert!(setup.receiver_link.private_messages.len() == 0);

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_returns_error_when_latest_payment_is_malformed() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let mut entries = HashMap::new();
        entries.insert(method, PaymentEndpointPayload::new("v1"));
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        send_raw_private_message(
            &mut setup.sender_link,
            r#"{"version":0,"kind":"paykit.private_payment_envelope","reference":"not-a-uuid","entries":{}}"#,
        )
        .await;

        let err = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .expect_err(
                "malformed latest Private Payment Envelope must supersede older valid state",
            );
        assert!(matches!(err, PaykitError::InvalidData { .. }));
        assert_eq!(setup.receiver_link.private_messages.len(), 0);

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_keeps_latest_payment_without_dropping_other_kinds() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
        let mut entries_v1 = HashMap::new();
        entries_v1.insert(method.clone(), PaymentEndpointPayload::new("v1"));
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries_v1))
            .await
            .unwrap();

        send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

        let mut entries_v2 = HashMap::new();
        entries_v2.insert(method.clone(), PaymentEndpointPayload::new("v2"));
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries_v2))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("latest private payments message should be returned");
        assert_eq!(
            received.endpoints().get(&method),
            Some(&PaymentEndpointPayload::new("v2"))
        );
        assert_eq!(setup.receiver_link.private_messages.len(), 1);
        assert_eq!(
            setup
                .receiver_link
                .private_messages
                .kind_at(0)
                .unwrap()
                .as_str(),
            "paykit.receipt_access"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    // ── Parallel writer/reader happy-path test ──────────────────────────

    /// Polls [`get_private_payment_envelope`] until a non-empty result is returned.
    /// Panics on timeout (10 s).
    async fn poll_private_payment_envelope(link: &mut EncryptedLink) -> PrivatePaymentEnvelope {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let timeout = Duration::from_secs(10);

        loop {
            assert!(
                start.elapsed() < timeout,
                "private payments poll timed out after {timeout:?}"
            );

            if let Some(result) = get_private_payment_envelope(link).await.unwrap() {
                if !result.endpoints().is_empty() {
                    return result;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// End-to-end test that spins up a testnet and homeserver in the main
    /// task, then exercises the private payment API from two concurrent
    /// tasks (writer and reader) that perform a Noise XX handshake and
    /// exchange encrypted payment data.
    ///
    /// Coverage:
    /// - Encrypted link: initiate, accept, handshake (polling loops)
    /// - Private payments: set, get (with polling)
    /// - Link cleanup: close
    /// - All interactions use only public `paykit_lib` functions
    #[tokio::test]
    async fn test_parallel_writer_reader_happy_path() {
        // ── Shared infrastructure (main task) ───────────────────────────

        let testnet = build_testnet().await;
        let homeserver = testnet.homeserver_app();

        // Writer (Alice): authenticated session + SDK for outbox reads.
        let writer_sdk = testnet.sdk().unwrap();
        let writer_keypair = Keypair::random();
        let writer_session = writer_sdk
            .signer(writer_keypair.clone())
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();
        let writer_pubkey = writer_session.info().public_key().clone();

        // Reader (Bob): authenticated session for the encrypted link
        // responder role + SDK for outbox reads.
        let reader_sdk = testnet.sdk().unwrap();
        let reader_keypair = Keypair::random();
        let reader_session = reader_sdk
            .signer(reader_keypair.clone())
            .signup(&homeserver.public_key(), None)
            .await
            .unwrap();
        let reader_pubkey = reader_session.info().public_key().clone();

        // ── Writer task ─────────────────────────────────────────────────

        let w_session = writer_session.clone();
        let w_reader_pubkey = reader_pubkey;

        let writer_handle = tokio::spawn(async move {
            // 1. Initiate encrypted link handshake.
            let handshake = initiate_encrypted_link(
                w_session.clone(),
                writer_keypair.secret_key(),
                &w_reader_pubkey,
                writer_sdk,
            )
            .unwrap();

            // 2. Drive handshake to completion (polling loop).
            let mut link = drive_handshake_to_completion(handshake).await;

            // 3. Send private payments.
            let mut entries = HashMap::new();
            entries.insert(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
                PaymentEndpointPayload::new("lnbcpriv..."),
            );
            entries.insert(
                PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap(),
                PaymentEndpointPayload::new("bc1priv..."),
            );
            set_private_payment_envelope(&mut link, &private_payload(&entries))
                .await
                .unwrap();

            // 4. Clean up.
            close_encrypted_link(link).await.unwrap();
            w_session.signout().await.unwrap();
        });

        // ── Reader task ─────────────────────────────────────────────────

        let r_session = reader_session.clone();
        let r_writer_pubkey = writer_pubkey;

        let reader_handle = tokio::spawn(async move {
            // 1. Accept encrypted link handshake.
            let handshake = accept_encrypted_link(
                r_session.clone(),
                reader_keypair.secret_key(),
                &r_writer_pubkey,
                reader_sdk,
            )
            .unwrap();

            // 2. Drive handshake to completion (polling loop).
            let mut link = drive_handshake_to_completion(handshake).await;

            // 3. Poll for private payments (writer may not have sent yet).
            let private = poll_private_payment_envelope(&mut link).await;
            assert_eq!(
                private.endpoints().len(),
                2,
                "expected 2 private payment methods, got {}",
                private.endpoints().len()
            );
            assert_eq!(
                private
                    .endpoints()
                    .get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
                Some(&PaymentEndpointPayload::new("lnbcpriv...")),
            );
            assert_eq!(
                private
                    .endpoints()
                    .get(&PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap()),
                Some(&PaymentEndpointPayload::new("bc1priv...")),
            );

            // 4. Clean up.
            close_encrypted_link(link).await.unwrap();
            r_session.signout().await.unwrap();
        });

        // ── Join both tasks ─────────────────────────────────────────────

        let (writer_result, reader_result) = tokio::join!(writer_handle, reader_handle);
        writer_result.expect("writer task panicked");
        reader_result.expect("reader task panicked");

        // Testnet drops here, cleaning up the ephemeral homeserver.
    }

    // ── Snapshot / restore tests ────────────────────────────────────────

    #[tokio::test]
    async fn test_handshake_snapshot_serialize_roundtrip() {
        let InProgressHandshakeSetup {
            _testnet,
            initiator_session,
            responder_session,
            initiator_handshake,
            responder_handshake: _responder_handshake,
        } = InProgressHandshakeSetup::new().await;

        let snapshot = initiator_handshake.snapshot();
        let bytes = snapshot.serialize();
        assert_eq!(bytes.len(), 197, "snapshot should be 197 bytes");

        let restored_snapshot = EncryptedLinkHandshakeSnapshot::deserialize(&bytes).unwrap();
        assert_eq!(
            restored_snapshot.recipient(),
            snapshot.recipient(),
            "recipient public key should survive serialize/deserialize"
        );

        let bytes2 = restored_snapshot.serialize();
        assert_eq!(
            bytes, bytes2,
            "double round-trip should produce identical bytes"
        );

        initiator_session.signout().await.unwrap();
        responder_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_restore_and_complete() {
        let InProgressHandshakeSetup {
            _testnet,
            initiator_session,
            responder_session,
            mut initiator_handshake,
            responder_handshake,
        } = InProgressHandshakeSetup::new().await;

        // Set a non-default value before snapshotting to verify restore resets
        // this knob back to the default.
        initiator_handshake.set_max_recovery_attempts(99);

        // Advance both sides once so snapshots capture an in-flight handshake.
        let initiator_handshake = match advance_handshake(initiator_handshake).await.unwrap() {
            HandshakeProgress::Pending(h) => h,
            HandshakeProgress::Complete(_) => {
                panic!("initiator handshake unexpectedly completed in one step")
            }
        };
        let responder_handshake = match advance_handshake(responder_handshake).await.unwrap() {
            HandshakeProgress::Pending(h) => h,
            HandshakeProgress::Complete(_) => {
                panic!("responder handshake unexpectedly completed in one step")
            }
        };

        let initiator_config = initiator_handshake.config().clone();
        let responder_config = responder_handshake.config().clone();

        let initiator_snapshot_bytes = initiator_handshake.serialize();
        let responder_snapshot_bytes = responder_handshake.serialize();
        let initiator_snapshot =
            EncryptedLinkHandshakeSnapshot::deserialize(&initiator_snapshot_bytes).unwrap();
        let responder_snapshot =
            EncryptedLinkHandshakeSnapshot::deserialize(&responder_snapshot_bytes).unwrap();

        let initiator_remote = initiator_snapshot.recipient().clone();
        let responder_remote = responder_snapshot.recipient().clone();

        let restored_initiator = restore_encrypted_link_handshake_from_config(
            initiator_config,
            &initiator_remote,
            initiator_snapshot,
        )
        .await
        .unwrap();
        let restored_responder = restore_encrypted_link_handshake_from_config(
            responder_config,
            &responder_remote,
            responder_snapshot,
        )
        .await
        .unwrap();

        assert_eq!(restored_initiator.recovery_attempts, 0);
        assert_eq!(
            restored_initiator.max_recovery_attempts,
            DEFAULT_MAX_RECOVERY_ATTEMPTS
        );

        let (mut initiator_link, mut responder_link) = tokio::join!(
            drive_handshake_to_completion(restored_initiator),
            drive_handshake_to_completion(restored_responder),
        );

        let mut entries = HashMap::new();
        entries.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnrestored..."),
        );
        set_private_payment_envelope(&mut initiator_link, &private_payload(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut responder_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.endpoints().len(), 1);
        assert_eq!(
            received
                .endpoints()
                .get(&PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap()),
            Some(&PaymentEndpointPayload::new("lnrestored..."))
        );

        close_encrypted_link(initiator_link).await.unwrap();
        close_encrypted_link(responder_link).await.unwrap();
        initiator_session.signout().await.unwrap();
        responder_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_restore_rejects_mismatched_remote_pubkey() {
        let InProgressHandshakeSetup {
            _testnet,
            initiator_session,
            responder_session,
            initiator_handshake,
            responder_handshake: _responder_handshake,
        } = InProgressHandshakeSetup::new().await;

        let snapshot = initiator_handshake.snapshot();
        let config = initiator_handshake.config().clone();
        let wrong_remote = initiator_session.info().public_key().clone();

        let result =
            restore_encrypted_link_handshake_from_config(config, &wrong_remote, snapshot).await;
        let err = match result {
            Ok(_) => panic!("restore should reject mismatched remote pubkey"),
            Err(err) => err,
        };
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match snapshot recipient")),
            "expected Validation mismatch error, got: {err}"
        );

        initiator_session.signout().await.unwrap();
        responder_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_restore_rejects_transport_phase_snapshot() {
        let setup = PrivateTestSetup::new().await;

        // Build a handshake snapshot value from a transport-mode link snapshot.
        let transport_bytes = setup.sender_link.serialize();
        let handshake_snapshot =
            EncryptedLinkHandshakeSnapshot::deserialize(&transport_bytes).unwrap();
        let sender_config = setup.sender_link.config().clone();
        let remote = handshake_snapshot.recipient().clone();

        let result = restore_encrypted_link_handshake_from_config(
            sender_config,
            &remote,
            handshake_snapshot,
        )
        .await;
        let err = match result {
            Ok(_) => panic!("handshake restore should reject transport-phase snapshot"),
            Err(err) => err,
        };
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("handshake-phase snapshot")),
            "expected handshake-phase validation error, got: {err}"
        );

        close_encrypted_link(setup.sender_link).await.unwrap();
        close_encrypted_link(setup.receiver_link).await.unwrap();
        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_handshake_snapshot_deserialize_rejects_garbage() {
        let result = EncryptedLinkHandshakeSnapshot::deserialize(&[0u8; 10]);
        assert!(result.is_err(), "deserializing garbage should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { .. }),
            "expected InvalidData error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_handshake_snapshot_deserialize_rejects_legacy_rc3_length() {
        let result = EncryptedLinkHandshakeSnapshot::deserialize(&[0u8; 189]);
        assert!(
            matches!(result, Err(PaykitError::InvalidData { .. })),
            "legacy 189-byte snapshots should fail under the 197-byte format"
        );
    }

    fn transport_snapshot_state_with_nonces(
        sending_nonce: u64,
        receiving_nonce: u64,
    ) -> pubky_noise::serializer::PubkyNoiseSessionState {
        pubky_noise::serializer::PubkyNoiseSessionState {
            version: pubky_noise::serializer::SESSION_STATE_VERSION,
            phase: pubky_noise::snow_crypto::NoisePhase::Transport,
            pattern: pubky_noise::snow_crypto::HandshakePattern::PatternXX,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: Some([2; 32]),
            counter: 2,
            noise_step: pubky_noise::snow_crypto::NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([3; 32]),
            link_id: Some([4; 32]),
            sending_nonce,
            receiving_nonce,
            write_counter: 3,
            read_counter: 3,
            endpoint_pubkey: Keypair::random().public_key().as_inner().to_bytes(),
        }
    }

    #[tokio::test]
    async fn test_encrypted_link_snapshot_serialize_roundtrip() {
        let mut setup = PrivateTestSetup::new().await;

        // Send a message to advance nonces beyond zero.
        let mut entries = HashMap::new();
        entries.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries))
            .await
            .unwrap();

        // Take a snapshot and serialize.
        let snapshot = setup.sender_link.snapshot();
        let bytes = snapshot.serialize();
        assert_eq!(bytes.len(), 197, "snapshot should be 197 bytes");

        // Deserialize and verify the recipient is reconstructed correctly.
        let restored_snapshot = EncryptedLinkSnapshot::deserialize(&bytes).unwrap();
        assert_eq!(
            restored_snapshot.recipient(),
            snapshot.recipient(),
            "recipient public key should survive serialize/deserialize"
        );

        // Re-serialize and verify byte-level equality.
        let bytes2 = restored_snapshot.serialize();
        assert_eq!(
            bytes, bytes2,
            "double round-trip should produce identical bytes"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_encrypted_link_restore_and_continue() {
        let mut setup = PrivateTestSetup::new().await;

        // Send a message before snapshotting.
        let mut entries_v1 = HashMap::new();
        entries_v1.insert(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnv1..."),
        );
        set_private_payment_envelope(&mut setup.sender_link, &private_payload(&entries_v1))
            .await
            .unwrap();

        // Consume the message on the receiver side.
        let received_v1 = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_v1.endpoints().len(), 1);

        // Snapshot both sides after the first exchange.
        let sender_snapshot = setup.sender_link.snapshot();
        let receiver_snapshot = setup.receiver_link.snapshot();

        // Serialize and deserialize (simulating persistence).
        let sender_bytes = sender_snapshot.serialize();
        let receiver_bytes = receiver_snapshot.serialize();
        let sender_state = EncryptedLinkSnapshot::deserialize(&sender_bytes).unwrap();
        let receiver_state = EncryptedLinkSnapshot::deserialize(&receiver_bytes).unwrap();

        // Restore both sides using the in-process config variant.
        let sender_config = setup.sender_link.config().clone();
        let receiver_config = setup.receiver_link.config().clone();
        let sender_recipient = sender_state.recipient().clone();
        let receiver_recipient = receiver_state.recipient().clone();

        let mut restored_sender =
            restore_encrypted_link_from_config(sender_config, &sender_recipient, sender_state)
                .await
                .unwrap();
        let mut restored_receiver = restore_encrypted_link_from_config(
            receiver_config,
            &receiver_recipient,
            receiver_state,
        )
        .await
        .unwrap();

        // Send a new message from the restored sender.
        let mut entries_v2 = HashMap::new();
        entries_v2.insert(
            PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap(),
            PaymentEndpointPayload::new("bc1pv2..."),
        );
        set_private_payment_envelope(&mut restored_sender, &private_payload(&entries_v2))
            .await
            .unwrap();

        // Receive on the restored receiver.
        let received_v2 = get_private_payment_envelope(&mut restored_receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_v2.endpoints().len(), 1);
        assert_eq!(
            received_v2
                .endpoints()
                .get(&PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap()),
            Some(&PaymentEndpointPayload::new("bc1pv2...")),
        );

        // Clean up.
        close_encrypted_link(restored_sender).await.unwrap();
        close_encrypted_link(restored_receiver).await.unwrap();
        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_encrypted_link_restore_rejects_mismatched_remote_pubkey() {
        let setup = PrivateTestSetup::new().await;

        let snapshot = setup.sender_link.snapshot();
        let sender_config = setup.sender_link.config().clone();
        let wrong_remote = setup.sender_session.info().public_key().clone();

        let result =
            restore_encrypted_link_from_config(sender_config, &wrong_remote, snapshot).await;
        let err = match result {
            Ok(_) => panic!("restore should reject mismatched remote pubkey"),
            Err(err) => err,
        };
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("does not match snapshot recipient")),
            "expected Validation mismatch error, got: {err}"
        );

        close_encrypted_link(setup.sender_link).await.unwrap();
        close_encrypted_link(setup.receiver_link).await.unwrap();
        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_encrypted_link_serialize_convenience() {
        let setup = PrivateTestSetup::new().await;

        // The convenience method should produce the same bytes as snapshot().serialize().
        let via_snapshot = setup.sender_link.snapshot().serialize();
        let via_convenience = setup.sender_link.serialize();
        assert_eq!(
            via_snapshot, via_convenience,
            "serialize() should equal snapshot().serialize()"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_encrypted_link_snapshot_deserialize_rejects_garbage() {
        let result = EncryptedLinkSnapshot::deserialize(&[0u8; 10]);
        assert!(result.is_err(), "deserializing garbage should fail");
        let err = result.unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { .. }),
            "expected InvalidData error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_encrypted_link_snapshot_deserialize_rejects_legacy_rc3_length() {
        let result = EncryptedLinkSnapshot::deserialize(&[0u8; 189]);
        assert!(
            matches!(result, Err(PaykitError::InvalidData { .. })),
            "legacy 189-byte snapshots should fail under the 197-byte format"
        );
    }

    #[test]
    fn test_encrypted_link_snapshot_deserialize_accepts_max_usable_noise_nonce() {
        let state = transport_snapshot_state_with_nonces(u64::MAX - 1, u64::MAX - 1);
        let bytes = state.serialize();

        let snapshot = EncryptedLinkSnapshot::deserialize(&bytes).unwrap();

        assert_eq!(snapshot.serialize(), bytes);
    }

    #[test]
    fn test_encrypted_link_snapshot_deserialize_rejects_reserved_noise_nonce() {
        for (sending_nonce, receiving_nonce) in [(u64::MAX, 0), (0, u64::MAX)] {
            let bytes =
                transport_snapshot_state_with_nonces(sending_nonce, receiving_nonce).serialize();

            assert!(
                matches!(
                    EncryptedLinkSnapshot::deserialize(&bytes),
                    Err(PaykitError::InvalidData { .. })
                ),
                "reserved Noise nonce should be rejected"
            );
        }
    }

    // ── PaymentReference tests ──────────────────────────────────────────

    #[test]
    fn test_payment_reference_accepts_uuid_v4() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(reference.as_str(), "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            format!("{reference}"),
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_payment_reference_canonicalizes_uuid_v4() {
        let reference = PaymentReference::new("550E8400-E29B-41D4-A716-446655440000").unwrap();
        assert_eq!(reference.as_str(), "550e8400-e29b-41d4-a716-446655440000");
    }

    #[test]
    fn test_payment_reference_rejects_non_uuid() {
        let err = PaymentReference::new("not-a-uuid").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("UUID v4")));
    }

    #[test]
    fn test_payment_reference_rejects_uuid_v1() {
        let err = PaymentReference::new("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("UUID v4")));
    }

    #[test]
    fn test_payment_reference_rejects_non_rfc4122_variant() {
        let err = PaymentReference::new("550e8400-e29b-41d4-0716-446655440000").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("RFC4122 UUID v4")));
    }

    #[tokio::test]
    async fn issue_receipt_stores_encrypted_receipt_and_sends_access_message() {
        let mut setup = PrivateTestSetup::new().await;
        let reference = PaymentReference::new_v4();
        let draft = ReceiptDraft {
            reference: reference.clone(),
            payment_endpoint_identifier: Some(
                PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            ),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::from([("note".to_string(), "paid".to_string())]),
        };

        let issued = issue_receipt(&setup.sender_session, &mut setup.sender_link, draft)
            .await
            .unwrap();

        assert_eq!(issued.reference, reference);
        assert_eq!(issued.location, ReceiptAccess::location_for(&reference));

        let stored = setup
            .sender_session
            .storage()
            .get(issued.location.clone())
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let receipt = decrypt_receipt(&stored, &issued.key, &issued.location).unwrap();
        assert_eq!(receipt.reference, reference);
        assert_eq!(
            receipt.recipient_public_key,
            setup.sender_link.recipient.clone()
        );
        assert_eq!(receipt.amount.as_deref(), Some("1000"));

        let access = get_receipt_access(&mut setup.receiver_link).await.unwrap();
        assert_eq!(access.len(), 1);
        assert_eq!(access[0].reference, reference);
        assert_eq!(access[0].location, issued.location);
        assert_eq!(access[0].key, issued.key);
    }

    #[tokio::test]
    async fn get_receipt_access_returns_all_available_receipts_in_fifo_order() {
        let mut setup = PrivateTestSetup::new().await;
        let first_reference =
            PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let second_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let first_access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            location: ReceiptAccess::location_for(&first_reference),
            key: ReceiptDecryptionKey::generate(),
            reference: first_reference.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let second_access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            location: ReceiptAccess::location_for(&second_reference),
            key: ReceiptDecryptionKey::generate(),
            reference: second_reference.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };

        let first_json = serialize_receipt_access_json(&first_access).unwrap();
        let second_json = serialize_receipt_access_json(&second_access).unwrap();
        send_raw_private_message(&mut setup.sender_link, &first_json).await;
        send_raw_private_message(&mut setup.sender_link, &second_json).await;

        let received = get_receipt_access(&mut setup.receiver_link).await.unwrap();
        let empty = get_receipt_access(&mut setup.receiver_link).await.unwrap();

        assert_eq!(received.len(), 2);
        assert_eq!(received[0].reference, first_reference);
        assert_eq!(received[1].reference, second_reference);
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn get_receipt_access_preserves_valid_receipts_when_one_selected_message_is_malformed() {
        let mut setup = PrivateTestSetup::new().await;
        let first_reference =
            PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let second_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let malformed_reference =
            PaymentReference::new("750e8400-e29b-41d4-a716-446655440000").unwrap();
        let first_access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            location: ReceiptAccess::location_for(&first_reference),
            key: ReceiptDecryptionKey::generate(),
            reference: first_reference.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let malformed_access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            location: ReceiptAccess::location_for(&malformed_reference),
            key: ReceiptDecryptionKey::generate(),
            reference: malformed_reference,
            algorithm: "bad-algorithm".to_string(),
        };
        let second_access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            location: ReceiptAccess::location_for(&second_reference),
            key: ReceiptDecryptionKey::generate(),
            reference: second_reference.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };

        let first_json = serialize_receipt_access_json(&first_access).unwrap();
        let malformed_json = serialize_receipt_access_json(&malformed_access).unwrap();
        let second_json = serialize_receipt_access_json(&second_access).unwrap();
        send_raw_private_message(&mut setup.sender_link, &first_json).await;
        send_raw_private_message(&mut setup.sender_link, &malformed_json).await;
        send_raw_private_message(&mut setup.sender_link, &second_json).await;

        let received = get_receipt_access(&mut setup.receiver_link).await.unwrap();
        let empty = get_receipt_access(&mut setup.receiver_link).await.unwrap();

        assert_eq!(received.len(), 2);
        assert_eq!(received[0].reference, first_reference);
        assert_eq!(received[1].reference, second_reference);
        assert!(empty.is_empty());
    }
}
