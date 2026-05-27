#![doc = include_str!("../README.md")]

use std::collections::HashMap;
use std::collections::VecDeque;

use thiserror::Error;
use tracing::{debug, instrument, warn};

pub use pubky::PublicKey;

pub use pubky_noise;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XChaCha20Poly1305,
};
use serde::{Deserialize, Serialize};

mod pubky_routing;

pub use pubky_routing::{PAYKIT_PATH_PREFIX, PAYKIT_PRIVATE_PATH_PREFIX};

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
    /// Returned when a payment endpoint or other resource is not found (404/GONE).
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
/// A `PaymentEndpointIdentifier` is a single, safe path segment stored under
/// `/pub/paykit/v0/...`. It is validated at construction time to prevent path
/// injection attacks.
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
/// let m = PaymentEndpointIdentifier::new("lightning").unwrap();
/// assert_eq!(m.as_str(), "lightning");
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
                "PaymentEndpointIdentifier '{PAYMENT_ENDPOINT_IDENTIFIER_RESERVED_PRIVATE}' is reserved for Private Payment Envelopes"
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentEndpointPayload(String);

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
    pub entries: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

/// UUID-v4 correlation reference used to connect Private Payment Envelopes and receipts.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentReference(String);

impl PaymentReference {
    /// Create a Payment Reference after validating that the input is a UUID v4 string.
    ///
    /// Accepted UUID-v4 inputs are canonicalized to lowercase hyphenated form.
    pub fn new(reference: impl Into<String>) -> Result<Self> {
        let reference = reference.into();
        let uuid = uuid::Uuid::try_parse(&reference).map_err(|err| {
            PaykitError::Validation(format!("Payment Reference must be a UUID v4 string: {err}"))
        })?;
        if uuid.get_version_num() != 4 || uuid.get_variant() != uuid::Variant::RFC4122 {
            return Err(PaykitError::Validation(
                "Payment Reference must be an RFC4122 UUID v4 string".into(),
            ));
        }
        Ok(Self(uuid.hyphenated().to_string()))
    }

    /// Generate a fresh random UUID-v4 Payment Reference.
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

/// Private Noise message kinds understood by Paykit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageKind {
    /// Private Payment Envelope Latest-State Message (`paykit.private_payments`).
    PrivatePaymentEnvelope,
    /// Receipt Access Event Message (`paykit.receipt_access`).
    ReceiptAccess,
}

impl PrivateMessageKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::PrivatePaymentEnvelope => "paykit.private_payments",
            Self::ReceiptAccess => "paykit.receipt_access",
        }
    }

    fn is_supported(kind: &str) -> bool {
        kind == Self::PrivatePaymentEnvelope.as_str() || kind == Self::ReceiptAccess.as_str()
    }
}

/// Versioned Private Payment Envelope sent over an established Encrypted Link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrivatePaymentEnvelope {
    version: u8,
    kind: PrivateMessageKind,
    /// UUID-v4 Payment Reference for this Private Payment Envelope.
    pub reference: PaymentReference,
    /// Complete Payment List carried by this Latest-State Message.
    pub entries: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
}

impl PrivatePaymentEnvelope {
    /// Construct a Private Payment Envelope using protocol version 1 and the
    /// `paykit.private_payments` message kind.
    ///
    /// `entries` must be the complete desired Payment List; callers should
    /// include all Payment Endpoints they want the counterparty to see, not
    /// just an incremental patch.
    pub fn new(
        reference: PaymentReference,
        entries: HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PrivatePaymentEnvelope,
            reference,
            entries,
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

    /// Number of Payment Endpoint entries in this envelope.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when this envelope contains no Payment Endpoint entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Look up a Payment Endpoint Payload by Payment Endpoint Identifier.
    pub fn get(&self, identifier: &PaymentEndpointIdentifier) -> Option<&PaymentEndpointPayload> {
        self.entries.get(identifier)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BufferedPrivateMessage {
    kind: String,
    plaintext: String,
}

impl BufferedPrivateMessage {
    fn is_kind(&self, kind: PrivateMessageKind) -> bool {
        self.kind == kind.as_str()
    }
}

#[derive(Deserialize)]
struct PrivateMessageHeader {
    kind: String,
}

/// Caller-provided receipt fields. [`issue_receipt`] fills in the recipient
/// public key from the established Encrypted Link before encrypting storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptDraft {
    /// Payment Reference being receipted.
    pub reference: PaymentReference,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined Receipt Metadata.
    pub metadata: HashMap<String, String>,
}

/// Canonical receipt plaintext encrypted before storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    /// Payment Reference this receipt corresponds to.
    pub reference: PaymentReference,
    /// Public key of the intended receipt recipient.
    pub recipient_public_key: PublicKey,
    /// Optional Payment Endpoint Identifier used for the payment.
    pub payment_endpoint_identifier: Option<PaymentEndpointIdentifier>,
    /// Optional amount string. Paykit does not interpret units or precision.
    pub amount: Option<String>,
    /// Optional currency/unit label paired with `amount`.
    pub currency: Option<String>,
    /// Caller-defined Receipt Metadata.
    pub metadata: HashMap<String, String>,
}

/// Symmetric key used to decrypt an encrypted Receipt.
///
/// The key material is intentionally redacted from [`Debug`](std::fmt::Debug)
/// and [`Display`](std::fmt::Display). Use [`as_str`](Self::as_str) only when
/// serializing Receipt Access for the intended counterparty or storing the key
/// in caller-managed secure storage.
#[derive(Clone, PartialEq, Eq)]
pub struct ReceiptDecryptionKey(String);

impl ReceiptDecryptionKey {
    /// Generate a fresh 256-bit Receipt Decryption Key encoded as base64url.
    pub fn generate() -> Self {
        let key = XChaCha20Poly1305::generate_key(&mut OsRng);
        Self(URL_SAFE_NO_PAD.encode(key))
    }

    /// Validate and construct a Receipt Decryption Key from base64url text.
    pub fn new(key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let bytes = URL_SAFE_NO_PAD.decode(&key).map_err(|err| {
            PaykitError::Validation(format!("Receipt Decryption Key must be base64url: {err}"))
        })?;
        if bytes.len() != 32 {
            return Err(PaykitError::Validation(format!(
                "Receipt Decryption Key must decode to 32 bytes, got {}",
                bytes.len()
            )));
        }
        Ok(Self(key))
    }

    /// Access the raw base64url key material.
    ///
    /// Treat this value as secret; do not log it or include it in telemetry.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn bytes(&self) -> Result<[u8; 32]> {
        let bytes = URL_SAFE_NO_PAD
            .decode(&self.0)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("Receipt Decryption Key is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| PaykitError::InvalidData {
                context: format!(
                    "Receipt Decryption Key must decode to 32 bytes, got {}",
                    bytes.len()
                ),
                source: None,
            })
    }
}

impl AsRef<str> for ReceiptDecryptionKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ReceiptDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReceiptDecryptionKey([redacted])")
    }
}

impl std::fmt::Display for ReceiptDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted Receipt Decryption Key]")
    }
}

/// Receipt Access descriptor sent over the existing Noise channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptAccess {
    /// Receipt Access envelope version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently always [`PrivateMessageKind::ReceiptAccess`].
    pub kind: PrivateMessageKind,
    /// Payment Reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted Receipt.
    pub location: String,
    /// Symmetric key needed to decrypt the Receipt.
    pub key: ReceiptDecryptionKey,
    /// Encryption algorithm. Currently `XChaCha20Poly1305`.
    pub algorithm: String,
}

/// Result returned after issuing and storing an encrypted receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedReceipt {
    /// Payment Reference for the receipt.
    pub reference: PaymentReference,
    /// Homeserver storage location of the encrypted Receipt.
    pub location: String,
    /// Symmetric key needed to decrypt the Receipt.
    pub key: ReceiptDecryptionKey,
}

/// Handle to an established Encrypted Link with a counterparty.
///
/// Created by [`advance_handshake`] (via [`HandshakeProgress::Complete`]) after
/// a successful Noise handshake. Used by Private Application Message helpers to
/// encrypt and decrypt Paykit data. Must be closed via [`close_encrypted_link`]
/// when no longer needed.
///
/// The link wraps a [`pubky_noise::PubkyNoiseEncryptor`] in transport mode.
///
/// # Session resumption
///
/// An established Encrypted Link can be snapshotted via [`snapshot`](Self::snapshot) (or
/// serialized directly via [`serialize`](Self::serialize)) and later restored
/// with [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// without re-doing the Noise handshake.
///
/// # Private message dispatch
///
/// All Paykit application messages on this Noise link share one ordered stream.
/// The link therefore buffers decrypted messages after low-level receipt and
/// lets typed helpers consume only their own message kind. This prevents future
/// helpers (for example Receipt Access) from losing messages simply because a
/// different typed getter was called first.
///
/// The buffer is in-memory only. If callers need crash-safe processing of
/// Event Message kinds, they must persist handled/unhandled application state
/// before dropping or serializing the link.
///
/// # Automatic send retry
///
/// [`set_private_payment_envelope`] automatically retries failed `send_message` calls
/// up to [`max_send_retries`](Self::set_max_send_retries) times (default:
/// [`DEFAULT_MAX_SEND_RETRIES`]). Since transport-phase send failures do not
/// corrupt the Noise state, retries are safe without snapshot-based recovery.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's public key.
    recipient: PublicKey,
    /// Shared Noise configuration retained for snapshot-based session resumption.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Maximum number of automatic `send_message` retries in
    /// [`set_private_payment_envelope`].
    max_send_retries: u32,
    /// Decrypted application messages that have been read from the ordered
    /// Noise stream but not yet consumed by a typed Paykit helper.
    ///
    /// This prevents a typed getter such as [`get_private_payment_envelope`] from
    /// discarding unrelated supported message kinds (for example Receipt Access
    /// messages) after the underlying Noise read counter has advanced.
    pending_private_messages: VecDeque<BufferedPrivateMessage>,
}

impl EncryptedLink {
    /// Set the maximum number of automatic `send_message` retries before
    /// [`set_private_payment_envelope`] gives up and returns [`PaykitError::Transport`].
    ///
    /// Transport-phase send failures do not corrupt the Noise state, so retries
    /// are safe without snapshot-based recovery.
    ///
    /// Default: [`DEFAULT_MAX_SEND_RETRIES`] (3).
    pub fn set_max_send_retries(&mut self, max: u32) -> &mut Self {
        self.max_send_retries = max;
        self
    }

    /// Capture the current link state as a serializable snapshot.
    ///
    /// The snapshot contains everything needed to restore the session later
    /// via [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
    /// without re-doing the Noise handshake.
    ///
    /// # When to snapshot
    ///
    /// Take a snapshot after the link is established and periodically after
    /// exchanging messages (the snapshot includes nonce counters that must stay
    /// in sync). Persist serialized bytes only in encrypted durable storage.
    /// Snapshot bytes include sensitive key material and must be treated as
    /// secrets (never log or expose them in telemetry/crash reports).
    pub fn snapshot(&self) -> EncryptedLinkSnapshot {
        EncryptedLinkSnapshot {
            state: self.encryptor.snapshot(),
            recipient: self.recipient.clone(),
        }
    }

    /// Serialize the current link state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot().serialize()`.
    pub fn serialize(&self) -> Vec<u8> {
        self.snapshot().serialize()
    }

    /// Access the shared Noise configuration for this link.
    ///
    /// Useful for passing to [`restore_encrypted_link_from_config`] when
    /// performing in-process session recovery without an app restart.
    pub fn config(&self) -> &std::sync::Arc<pubky_noise::PubkyNoiseConfig> {
        &self.config
    }

    /// Access the counterparty public key for this Encrypted Link.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Serializable snapshot of an established [`EncryptedLink`].
///
/// Created by [`EncryptedLink::snapshot`]. Can be serialized to a compact
/// binary format via [`serialize`](Self::serialize) for durable storage, and
/// deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// to resume the session after an app restart without re-doing the Noise
/// handshake.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The counterparty public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
}

fn recipient_from_snapshot_state(
    state: &pubky_noise::serializer::PubkyNoiseSessionState,
    snapshot_kind: &'static str,
) -> Result<PublicKey> {
    let pkarr_pk =
        pubky::pkarr::PublicKey::try_from(state.endpoint_pubkey.as_slice()).map_err(|err| {
            PaykitError::InvalidData {
                context: format!(
                    "failed to reconstruct recipient public key from {snapshot_kind}: {err}"
                ),
                source: Some(err.into()),
            }
        })?;
    Ok(PublicKey::from(pkarr_pk))
}

impl std::fmt::Debug for EncryptedLinkSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkSnapshot {
    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 197 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed. Snapshots using the
    /// older 189-byte `pubky-noise` `0.1.0-rc3` format are rejected.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!("failed to deserialize Encrypted Link snapshot: {err:?}"),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Serializable snapshot of an in-progress [`EncryptedLinkHandshake`].
///
/// Created by [`EncryptedLinkHandshake::snapshot`]. Can be serialized to a
/// compact binary format via [`serialize`](Self::serialize) for durable
/// storage, and deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to [`restore_encrypted_link_handshake`] or
/// [`restore_encrypted_link_handshake_from_config`] to resume the handshake
/// after an app restart without starting over.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The counterparty public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkHandshakeSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
}

impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkHandshakeSnapshot {
    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 197 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed. Snapshots using the
    /// older 189-byte `pubky-noise` `0.1.0-rc3` format are rejected.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!(
                        "failed to deserialize Encrypted Link Handshake snapshot: {err:?}"
                    ),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Default maximum number of automatic `send_message` retries before
/// [`set_private_payment_envelope`] gives up and returns an error.
///
/// Override per-link via [`EncryptedLink::set_max_send_retries`].
pub const DEFAULT_MAX_SEND_RETRIES: u32 = 3;

/// Default maximum number of consecutive automatic recovery attempts before
/// [`advance_handshake`] gives up and returns an error.
///
/// Override per-handshake via [`EncryptedLinkHandshake::set_max_recovery_attempts`].
pub const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// Handle to an in-progress Noise handshake.
///
/// Created by [`initiate_encrypted_link`] (initiator) or
/// [`accept_encrypted_link`] (responder). Drive the handshake forward by
/// repeatedly calling [`advance_handshake`] until it returns
/// [`HandshakeProgress::Complete`].
///
/// The caller controls the polling strategy — timing between retries, timeouts,
/// back-off, etc. are all the caller's responsibility.
///
/// # Automatic recovery
///
/// If a homeserver write fails during the handshake (corrupting the internal
/// Noise state), [`advance_handshake`] automatically restores from a
/// pre-mutation snapshot and returns [`HandshakeProgress::Pending`] so the
/// caller's polling loop retries transparently. The maximum number of
/// consecutive recovery attempts is configurable via
/// [`set_max_recovery_attempts`](Self::set_max_recovery_attempts) (default:
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]).
///
/// # Session resumption
///
/// An in-progress handshake can be snapshotted via [`snapshot`](Self::snapshot)
/// (or serialized directly via [`serialize`](Self::serialize)) and later
/// restored with [`restore_encrypted_link_handshake`] or
/// [`restore_encrypted_link_handshake_from_config`].
///
/// Restored handshakes always reset recovery tuning to defaults:
/// `recovery_attempts` starts at `0` and `max_recovery_attempts` is set to
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`].
pub struct EncryptedLinkHandshake {
    /// The Noise session manager in handshake mode.
    encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's public key (used for homeserver path construction).
    remote_pubkey: PublicKey,
    /// Shared Noise configuration needed for snapshot-based recovery.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Number of consecutive recovery attempts so far.
    recovery_attempts: u32,
    /// Maximum consecutive recovery attempts before giving up.
    max_recovery_attempts: u32,
}

impl EncryptedLinkHandshake {
    /// Set the maximum number of consecutive automatic recovery attempts
    /// before [`advance_handshake`] gives up and returns
    /// [`PaykitError::Transport`].
    ///
    /// The recovery-attempt counter resets to zero after every successful
    /// handshake step.
    /// Default: [`DEFAULT_MAX_RECOVERY_ATTEMPTS`] (3).
    pub fn set_max_recovery_attempts(&mut self, max: u32) -> &mut Self {
        self.max_recovery_attempts = max;
        self
    }

    /// Capture the current handshake state as a serializable snapshot.
    ///
    /// The snapshot contains everything needed to restore and continue the
    /// handshake later via [`restore_encrypted_link_handshake`] or
    /// [`restore_encrypted_link_handshake_from_config`].
    pub fn snapshot(&self) -> EncryptedLinkHandshakeSnapshot {
        EncryptedLinkHandshakeSnapshot {
            state: self.encryptor.snapshot(),
            recipient: self.remote_pubkey.clone(),
        }
    }

    /// Serialize the current handshake state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot().serialize()`.
    pub fn serialize(&self) -> Vec<u8> {
        self.snapshot().serialize()
    }

    /// Access the shared Noise configuration for this handshake.
    ///
    /// Useful for passing to [`restore_encrypted_link_handshake_from_config`]
    /// when performing in-process recovery without an app restart.
    pub fn config(&self) -> &std::sync::Arc<pubky_noise::PubkyNoiseConfig> {
        &self.config
    }
}

/// Result of a single [`advance_handshake`] step.
pub enum HandshakeProgress {
    /// Handshake is still in progress. The peer may not have written their next
    /// message yet. Pass the returned handle back to [`advance_handshake`] after
    /// a caller-chosen delay.
    Pending(EncryptedLinkHandshake),

    /// Handshake completed successfully. The [`EncryptedLink`] is ready for use
    /// with [`set_private_payment_envelope`] and [`get_private_payment_envelope`].
    Complete(EncryptedLink),
}

/// Domain separation string for Paykit private payment path derivation.
///
/// Ensures that different applications using the same key pairs derive
/// different storage paths, preventing cross-protocol path collisions.
const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-peer-pair paths from a DH shared secret. The derivation formula is:
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_ed25519_seed), to_montgomery(remote_ed25519_pk))
/// write_path = "{base}/{hex(SHA-256(domain || dh_secret || local_pk))}"
/// read_path  = "{base}/{hex(SHA-256(domain || dh_secret || remote_pk))}"
/// ```
///
/// # Returns
///
/// A tuple `(write_path, read_path)` where:
/// - `write_path` — the full path the local party writes to on their own homeserver.
/// - `read_path` — the full path the local party reads from on the remote homeserver.
///
/// # Correctness
///
/// For parties Alice and Bob:
/// - `compute_private_paths(alice_sk, bob_pk).0 == compute_private_paths(bob_sk, alice_pk).1`
/// - `compute_private_paths(alice_sk, bob_pk).1 == compute_private_paths(bob_sk, alice_pk).0`
fn compute_private_payment_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_PATH_DOMAIN,
        PAYKIT_PRIVATE_PATH_PREFIX,
    )
}

#[derive(Deserialize)]
struct PrivatePaymentEnvelopeWire {
    version: u8,
    kind: String,
    reference: String,
    entries: HashMap<String, String>,
}

#[derive(Serialize)]
struct PrivatePaymentEnvelopeWireRef<'a> {
    version: u8,
    kind: &'static str,
    reference: &'a str,
    entries: HashMap<&'a str, &'a str>,
}

/// Deserializes a versioned Private Payment Envelope JSON message.
fn parse_private_payment_envelope_json(json: &str) -> Result<PrivatePaymentEnvelope> {
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
    let mut entries = HashMap::new();
    for (key, value) in wire.entries {
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
    Ok(PrivatePaymentEnvelope::new(reference, entries))
}

/// Serializes a Private Payment Envelope into its JSON wire representation.
fn serialize_private_payment_envelope_json(envelope: &PrivatePaymentEnvelope) -> Result<String> {
    let entries = envelope
        .entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let wire = PrivatePaymentEnvelopeWireRef {
        version: envelope.version,
        kind: envelope.kind.as_str(),
        reference: envelope.reference.as_str(),
        entries,
    };
    serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
        context: format!("failed to serialize Private Payment Envelope JSON: {err}"),
        source: Some(err.into()),
    })
}

fn decode_private_message(
    raw: &[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN],
) -> Result<BufferedPrivateMessage> {
    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    // Paykit application messages are JSON, so trailing NUL bytes are not valid
    // payload content.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
        context: format!("private message plaintext is not valid UTF-8: {err}"),
        source: Some(err.into()),
    })?;

    let header: PrivateMessageHeader =
        serde_json::from_str(plaintext).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse private message header JSON: {err}"),
            source: Some(err.into()),
        })?;

    Ok(BufferedPrivateMessage {
        kind: header.kind,
        plaintext: plaintext.to_owned(),
    })
}

#[derive(Serialize, Deserialize)]
struct ReceiptWire {
    version: u8,
    kind: String,
    reference: String,
    recipient_public_key: String,
    payment_endpoint_identifier: Option<String>,
    amount: Option<String>,
    currency: Option<String>,
    metadata: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
struct EncryptedReceiptWire {
    version: u8,
    kind: String,
    algorithm: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct ReceiptAccessWire {
    version: u8,
    kind: String,
    reference: String,
    location: String,
    key: String,
    algorithm: String,
}

impl ReceiptAccess {
    /// Return the canonical homeserver storage location for a Payment Reference.
    pub fn location_for(reference: &PaymentReference) -> String {
        format!(
            "{}private/receipts/{}",
            PAYKIT_PATH_PREFIX,
            reference.as_str()
        )
    }

    /// Validate that this access descriptor points at the canonical location for
    /// its Payment Reference.
    pub fn validate_location(&self) -> Result<()> {
        let expected_location = Self::location_for(&self.reference);
        if self.location != expected_location {
            return Err(PaykitError::InvalidData {
                context: "Receipt Access location does not match Payment Reference".into(),
                source: None,
            });
        }
        Ok(())
    }
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

impl Receipt {
    fn aad_for_location(location: &str) -> String {
        format!("paykit.receipt.v1:{location}")
    }

    /// Encrypt this receipt for storage at its canonical location using `key`.
    ///
    /// The location is derived from the Payment Reference and authenticated as
    /// AEAD associated data; callers must use that same canonical location when
    /// decrypting.
    pub fn encrypt(&self, key: &ReceiptDecryptionKey) -> Result<String> {
        let location = ReceiptAccess::location_for(&self.reference);
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plaintext = serde_json::to_vec(&ReceiptWire::from(self)).map_err(|err| {
            PaykitError::InvalidData {
                context: format!("failed to serialize receipt JSON: {err}"),
                source: Some(err.into()),
            }
        })?;
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &plaintext,
                    aad: Self::aad_for_location(&location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to encrypt receipt: {err}"),
                source: None,
            })?;
        let wire = EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: "XChaCha20Poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        serde_json::to_string(&wire).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to serialize encrypted receipt JSON: {err}"),
            source: Some(err.into()),
        })
    }

    /// Decrypt an encrypted Receipt fetched from a homeserver.
    ///
    /// `key` and `location` normally come from a [`ReceiptAccess`] message. The
    /// location is authenticated as AEAD associated data and the decrypted
    /// Payment Reference must match the canonical location.
    pub fn decrypt(
        encrypted_json: &str,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> Result<Self> {
        let wire: EncryptedReceiptWire =
            serde_json::from_str(encrypted_json).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse encrypted receipt JSON: {err}"),
                source: Some(err.into()),
            })?;
        if wire.version != 1
            || wire.kind != "paykit.receipt.encrypted"
            || wire.algorithm != "XChaCha20Poly1305"
        {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "unsupported encrypted receipt envelope version/kind/algorithm: {}/{}/{}",
                    wire.version, wire.kind, wire.algorithm
                ),
                source: None,
            });
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(wire.nonce)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("encrypted receipt nonce is not valid base64url: {err}"),
                source: Some(err.into()),
            })?;
        let ciphertext =
            URL_SAFE_NO_PAD
                .decode(wire.ciphertext)
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("encrypted receipt ciphertext is not valid base64url: {err}"),
                    source: Some(err.into()),
                })?;
        if nonce.len() != 24 {
            return Err(PaykitError::InvalidData {
                context: format!(
                    "encrypted receipt nonce must be 24 bytes, got {}",
                    nonce.len()
                ),
                source: None,
            });
        }
        let key_bytes = key.bytes()?;
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let plaintext = cipher
            .decrypt(
                nonce.as_slice().into(),
                chacha20poly1305::aead::Payload {
                    msg: &ciphertext,
                    aad: Self::aad_for_location(location).as_bytes(),
                },
            )
            .map_err(|err| PaykitError::InvalidData {
                context: format!("failed to decrypt receipt: {err}"),
                source: None,
            })?;
        let receipt_wire: ReceiptWire =
            serde_json::from_slice(&plaintext).map_err(|err| PaykitError::InvalidData {
                context: format!("failed to parse receipt plaintext JSON: {err}"),
                source: Some(err.into()),
            })?;
        let receipt = Self::try_from(receipt_wire)?;
        if ReceiptAccess::location_for(&receipt.reference) != location {
            return Err(PaykitError::InvalidData {
                context: "Receipt Payment Reference does not match Receipt Location".into(),
                source: None,
            });
        }
        Ok(receipt)
    }
}

/// Decrypts an encrypted Receipt fetched from a homeserver.
///
/// `encrypted_json` is the public receipt object stored by [`issue_receipt`].
/// `key` and `location` normally come from a [`ReceiptAccess`] message received
/// with [`get_receipt_access`]. The `location` is authenticated as additional
/// data, so decrypting with a different location fails even when the key and
/// ciphertext are otherwise correct.
///
/// Receipt Decryption Keys are sensitive. [`ReceiptDecryptionKey`] redacts its
/// `Debug` and `Display` output, but callers must still avoid logging values
/// returned by [`ReceiptDecryptionKey::as_str`].
///
/// # Errors
/// - Returns [`PaykitError::InvalidData`] if the encrypted envelope is malformed,
///   uses an unsupported version/kind/algorithm, has invalid base64url fields,
///   fails authenticated decryption, decrypts to malformed receipt JSON, or
///   decrypts to a receipt whose reference does not match the authenticated
///   Receipt Location.
pub fn decrypt_receipt(
    encrypted_json: &str,
    key: &ReceiptDecryptionKey,
    location: &str,
) -> Result<Receipt> {
    Receipt::decrypt(encrypted_json, key, location)
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

fn serialize_receipt_access_json(access: &ReceiptAccess) -> Result<String> {
    serde_json::to_string(&ReceiptAccessWire::from(access)).map_err(|err| {
        PaykitError::InvalidData {
            context: format!("failed to serialize Receipt Access JSON: {err}"),
            source: Some(err.into()),
        }
    })
}

fn parse_receipt_access_json(json: &str) -> Result<ReceiptAccess> {
    let wire: ReceiptAccessWire =
        serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse Receipt Access JSON: {err}"),
            source: Some(err.into()),
        })?;
    ReceiptAccess::try_from(wire)
}

async fn receive_private_messages(link: &mut EncryptedLink) -> Result<usize> {
    let mut received = 0usize;
    let mut malformed = 0usize;
    let mut unknown = 0usize;

    loop {
        let messages =
            link.encryptor
                .receive_message()
                .await
                .map_err(|err| PaykitError::Transport {
                    context: format!("failed to receive private messages: {err:?}"),
                    source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
                })?;

        if messages.is_empty() {
            break;
        }

        received += messages.len();
        for raw in messages {
            match decode_private_message(&raw) {
                Ok(message) if PrivateMessageKind::is_supported(&message.kind) => {
                    link.pending_private_messages.push_back(message)
                }
                Ok(message) => {
                    unknown += 1;
                    warn!(
                        kind = %message.kind,
                        "dropping unsupported Private Application Message kind"
                    );
                }
                Err(err) => {
                    malformed += 1;
                    warn!(
                        error = ?err,
                        "dropping malformed Private Application Message"
                    );
                }
            }
        }
    }

    if malformed > 0 {
        warn!(
            received,
            malformed,
            "ignored malformed Private Application Messages while preserving later valid messages"
        );
    }
    if unknown > 0 {
        warn!(
            received,
            unknown, "dropped unsupported Private Application Message kinds"
        );
    }

    Ok(received)
}

fn take_latest_pending_message(
    pending: &mut VecDeque<BufferedPrivateMessage>,
    kind: PrivateMessageKind,
) -> Option<BufferedPrivateMessage> {
    let mut retained = VecDeque::with_capacity(pending.len());
    let mut latest = None;

    while let Some(message) = pending.pop_front() {
        if message.is_kind(kind) {
            latest = Some(message);
        } else {
            retained.push_back(message);
        }
    }

    *pending = retained;
    latest
}

fn take_all_pending_messages(
    pending: &mut VecDeque<BufferedPrivateMessage>,
    kind: PrivateMessageKind,
) -> Vec<BufferedPrivateMessage> {
    let mut retained = VecDeque::with_capacity(pending.len());
    let mut selected = Vec::new();

    while let Some(message) = pending.pop_front() {
        if message.is_kind(kind) {
            selected.push(message);
        } else {
            retained.push_back(message);
        }
    }

    *pending = retained;
    selected
}

fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

async fn send_private_message(
    link: &mut EncryptedLink,
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "{context} payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        )));
    }

    let max_attempts = send_attempts_from_retries(link.max_send_retries);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match link.encryptor.send_message(plaintext).await {
            Ok(()) => {
                debug!(context, "private message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = link.max_send_retries,
                        error = ?err,
                        context,
                        "send_message failed, retrying"
                    );
                }
            }
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: format!("failed to send {context}: {err:?}"),
                    source: anyhow::anyhow!(
                        "pubky-noise send_message failed with non-retryable error: {err:?}"
                    ),
                });
            }
        }
    }

    Err(PaykitError::Transport {
        context: format!("failed to send {context} after {max_attempts} attempts"),
        source: anyhow::anyhow!(
            "pubky-noise send_message failed on all {} attempts; last error: {}",
            max_attempts,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    })
}

/// Stores or updates a public payment endpoint in the authenticated Pubky session.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, PaymentEndpointIdentifier, PaymentEndpointPayload};
/// # async fn demo(session: &pubky::PubkySession) -> paykit_lib::Result<()> {
/// let identifier = PaymentEndpointIdentifier::new("bitcoin-bolt11")?;
/// let payload = PaymentEndpointPayload::new("ln...");
/// set_payment_endpoint(session, identifier, payload).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(session, payload), fields(identifier = %identifier))]
pub async fn set_payment_endpoint(
    session: &pubky::PubkySession,
    identifier: PaymentEndpointIdentifier,
    payload: PaymentEndpointPayload,
) -> Result<()> {
    debug!("storing payment endpoint");
    pubky_routing::upsert_payment_endpoint(session, &identifier, &payload)
        .await
        .map_err(|err| map_error("set_payment_endpoint", err))
}

/// Encrypts and sends a complete Private Payment Envelope via the established
/// Encrypted Link.
///
/// The caller must pass a [`PrivatePaymentEnvelope`] containing a validated
/// [`PaymentReference`] and the complete Payment List. The
/// caller is still responsible for managing the map contents (adding/removing
/// entries) and should pass the full desired entries map in `envelope.entries`
/// on every update.
///
/// The envelope is serialized as a versioned JSON message before being sent over
/// pubky-noise:
///
/// ```json
/// {
///   "version": 1,
///   "kind": "paykit.private_payments",
///   "reference": "550e8400-e29b-41d4-a716-446655440000",
///   "entries": {
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
/// (default: [`DEFAULT_MAX_SEND_RETRIES`]). Transport-phase homeserver write
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
///   required [`PaymentReference`] and complete entries map.
///
/// # Errors
/// - Returns [`PaykitError::Validation`] if the serialized envelope exceeds
///   the maximum message size.
/// - Returns [`PaykitError::InvalidData`] if the envelope cannot be serialized.
/// - Returns [`PaykitError::Transport`] if `send_message` fails after all
///   retry attempts are exhausted.
#[instrument(skip(link, envelope), fields(count = envelope.entries.len()))]
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

/// Issues, stores, and shares an encrypted payment receipt with the Linked Peer.
///
/// The encrypted receipt is written to the caller's homeserver at a deterministic
/// Receipt Location derived from `draft.reference`. A fresh symmetric
/// [`ReceiptDecryptionKey`] is generated for each call. The corresponding
/// [`ReceiptAccess`] descriptor is then sent over the existing Noise channel so
/// the counterparty can fetch and decrypt the stored receipt with [`decrypt_receipt`].
///
/// Receipt Access messages are Event Messages: every valid access descriptor matters.
/// Reissuing the same [`PaymentReference`] stores a new encrypted receipt at the
/// same location with a new key, so older access descriptors for that reference
/// may no longer decrypt after a later successful reissue.
///
/// # Identity binding
///
/// `session` is used for homeserver storage, while `link` is used to send the
/// Receipt Access message. Paykit does not currently verify that `session`
/// belongs to the same local identity that established `link`; callers must pass
/// the matching session or they may persist the receipt under the wrong identity
/// while sending access over a different Encrypted Link.
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
///   the Receipt Access Noise message cannot be sent after configured retries.
#[instrument(skip(session, link, draft))]
pub async fn issue_receipt(
    session: &pubky::PubkySession,
    link: &mut EncryptedLink,
    draft: ReceiptDraft,
) -> Result<IssuedReceipt> {
    debug!("issuing encrypted receipt");
    let reference = draft.reference;
    let location = ReceiptAccess::location_for(&reference);
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

    session
        .storage()
        .put(location.clone(), encrypted)
        .await
        .map_err(|err| PaykitError::Transport {
            context: format!("failed to store encrypted receipt at {location}"),
            source: err.into(),
        })?;

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
    send_private_message(link, json.as_bytes(), "Receipt Access")
        .await
        .map_err(|err| map_error("issue_receipt", err))?;

    Ok(IssuedReceipt {
        reference,
        location,
        key,
    })
}

/// Removes a public payment endpoint from the authenticated Pubky session.
#[instrument(skip(session), fields(identifier = %identifier))]
pub async fn remove_payment_endpoint(
    session: &pubky::PubkySession,
    identifier: PaymentEndpointIdentifier,
) -> Result<()> {
    debug!("removing payment endpoint");
    pubky_routing::delete_payment_endpoint(session, &identifier)
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
/// # use paykit_lib::get_payment_list;
/// # async fn demo(storage: &pubky::PublicStorage, pk: &paykit_lib::PublicKey) -> paykit_lib::Result<()> {
/// let payments = get_payment_list(storage, pk).await?;
/// if payments.entries.is_empty() {
///     println!("payee published no endpoints yet");
/// } else {
///     for (identifier, payload) in &payments.entries {
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
) -> Result<PaymentList> {
    debug!("fetching Payment List");
    let result = pubky_routing::fetch_payment_list(storage, payee)
        .await
        .map_err(|err| map_error("get_payment_list", err))?;
    debug!(count = result.entries.len(), "Payment List retrieved");
    Ok(result)
}

/// Receives and decrypts the latest Private Payment Envelope from the
/// counterparty via the established Encrypted Link.
///
/// Returns `Ok(Some(envelope))` when a Private Payment Envelope is available.
/// The caller can access the correlation reference at `envelope.reference` and
/// look up Payment Endpoint Payloads from `envelope.entries` or via
/// [`PrivatePaymentEnvelope::get`].
///
/// Returns `Ok(None)` when no Private Payment Envelope is currently available.
/// `None` means "no message yet"; it is distinct from receiving an envelope whose
/// `entries` map is empty.
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
///   including its required [`PaymentReference`] and complete entries map.
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
        count = envelope.entries.len(),
        received,
        pending = link.pending_private_messages.len(),
        "Private Payment Envelope received"
    );
    Ok(Some(envelope))
}

/// Receives all currently available Receipt Access descriptors from the Encrypted Link.
///
/// Unlike [`get_private_payment_envelope`], Receipt Access uses Event Message
/// semantics. Every currently available Receipt Access message is returned in
/// send order in a single vector; older Receipt Access messages are not collapsed
/// when newer ones arrive.
/// Returns an empty vector when no Receipt Access messages are currently available.
///
/// Messages for other supported private app kinds remain buffered on the
/// [`EncryptedLink`] for their own typed receiver. Malformed unrelated app
/// messages are ignored by the shared dispatcher. Syntactically valid messages
/// with unsupported `kind` values are logged and dropped by the shared
/// dispatcher rather than buffered indefinitely. Malformed Receipt Access
/// messages are dropped with diagnostics while later valid Receipt Access
/// messages in the same batch are still returned.
///
/// Each selected Receipt Access location must match the canonical Paykit
/// Receipt Location for its [`PaymentReference`].
///
/// The returned [`ReceiptAccess::key`] values are sensitive. Their formatting is
/// redacted, but callers must still avoid logging raw key material from
/// [`ReceiptDecryptionKey::as_str`].
#[instrument(skip(link))]
pub async fn get_receipt_access(link: &mut EncryptedLink) -> Result<Vec<ReceiptAccess>> {
    debug!("receiving Receipt Access messages");

    let received = receive_private_messages(link).await?;
    let raw_messages = take_all_pending_messages(
        &mut link.pending_private_messages,
        PrivateMessageKind::ReceiptAccess,
    );
    if raw_messages.is_empty() {
        debug!(received, "no Receipt Access messages available");
        return Ok(Vec::new());
    }

    let mut access = Vec::new();
    let mut malformed = 0usize;
    for raw in &raw_messages {
        match parse_receipt_access_json(&raw.plaintext) {
            Ok(parsed) => access.push(parsed),
            Err(err) => {
                malformed += 1;
                warn!(
                    error = ?err,
                    "dropping malformed Receipt Access message while preserving later valid messages"
                );
            }
        }
    }
    if malformed > 0 {
        warn!(
            malformed,
            selected = raw_messages.len(),
            "ignored malformed Receipt Access messages while preserving valid messages"
        );
    }
    debug!(
        count = access.len(),
        received,
        pending = link.pending_private_messages.len(),
        "Receipt Access messages received"
    );
    Ok(access)
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
/// # use paykit_lib::{get_payment_endpoint, PaymentEndpointIdentifier, PublicKey};
/// # async fn inspect(storage: &pubky::PublicStorage, pk: &PublicKey) -> paykit_lib::Result<()> {
/// let lightning = PaymentEndpointIdentifier::new("lightning")?;
/// if let Some(endpoint) = get_payment_endpoint(storage, pk, &lightning).await? {
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
    identifier: &PaymentEndpointIdentifier,
) -> Result<Option<PaymentEndpointPayload>> {
    debug!("fetching payment endpoint");
    let result = pubky_routing::fetch_payment_endpoint(storage, payee, identifier)
        .await
        .map_err(|err| map_error("get_payment_endpoint", err))?;
    debug!(found = result.is_some(), "payment endpoint lookup complete");
    Ok(result)
}

/// Initiates a Noise XX Encrypted Link Handshake with a counterparty
/// (initiator role).
///
/// Initializes the encryption stack and creates a handshake context. The actual
/// handshake messages are exchanged by repeatedly calling [`advance_handshake`]
/// until it returns [`HandshakeProgress::Complete`].
///
/// Ephemeral keys are managed internally by the Noise stack — callers only need
/// to provide their static identity key and the counterparty public key.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `sender_secret_key` — 32-byte Ed25519 secret key of the local party.
/// - `receiver_pubkey` — public key of the counterparty.
/// - `outbox_client` — HTTP client for reading from the remote homeserver
///   (consumed; caller should `.clone()` if needed elsewhere).
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the encryption stack cannot be
/// initialized or if the context creation fails.
#[instrument(skip(session, sender_secret_key, outbox_client))]
pub fn initiate_encrypted_link(
    session: pubky::PubkySession,
    sender_secret_key: [u8; 32],
    receiver_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (initiator)");

    let (write_path, read_path) =
        compute_private_payment_paths(&sender_secret_key, receiver_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        sender_secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
        config.clone(),
        sender_secret_key,
        true,
        receiver_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (initiator)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: receiver_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Accepts a Noise XX Encrypted Link Handshake from a counterparty
/// (responder role).
///
/// Initializes the encryption stack and creates a handshake context for the
/// responder side. The actual handshake messages are exchanged by repeatedly
/// calling [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `receiver_secret_key` — 32-byte Ed25519 secret key of the local party.
/// - `sender_pubkey` — public key of the counterparty (the initiator).
/// - `outbox_client` — HTTP client for reading from the remote homeserver
///   (consumed; caller should `.clone()` if needed elsewhere).
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the encryption stack cannot be
/// initialized or if the context creation fails.
#[instrument(skip(session, receiver_secret_key, outbox_client))]
pub fn accept_encrypted_link(
    session: pubky::PubkySession,
    receiver_secret_key: [u8; 32],
    sender_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (responder)");

    let (write_path, read_path) =
        compute_private_payment_paths(&receiver_secret_key, sender_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        receiver_secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
        config.clone(),
        receiver_secret_key,
        false,
        sender_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (responder)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: sender_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Advances the handshake by one step.
///
/// This function is **polling-safe**: calling it when the counterparty has not
/// written their next message yet returns [`HandshakeProgress::Pending`] without
/// corrupting internal state. The caller can safely retry after a delay.
///
/// # Automatic recovery
///
/// If the homeserver write fails during a handshake step
/// (`HomeserverWriteError`), the internal Noise state is irreversibly
/// corrupted. This function automatically recovers by restoring from the
/// pre-mutation snapshot captured at the start of the failed step and returns
/// [`HandshakeProgress::Pending`] so the caller's polling loop retries
/// transparently.
///
/// The maximum number of **consecutive** recovery attempts is configurable via
/// [`EncryptedLinkHandshake::set_max_recovery_attempts`] (default:
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]). The recovery-attempt counter resets to
/// zero after every successful step. If the limit is exceeded, the function returns
/// [`PaykitError::Transport`].
///
/// # Polling strategy
///
/// The caller controls the polling strategy. Common patterns:
///
/// **Fixed interval:**
/// ```ignore
/// loop {
///     match advance_handshake(handshake).await? {
///         HandshakeProgress::Pending(h) => {
///             handshake = h;
///             tokio::time::sleep(Duration::from_millis(100)).await;
///         }
///         HandshakeProgress::Complete(link) => break link,
///     }
/// }
/// ```
///
/// **With timeout:**
/// ```ignore
/// let deadline = Instant::now() + Duration::from_secs(60);
/// loop {
///     if Instant::now() > deadline {
///         return Err(/* timeout */);
///     }
///     match advance_handshake(handshake).await? {
///         HandshakeProgress::Pending(h) => {
///             handshake = h;
///             tokio::time::sleep(Duration::from_millis(100)).await;
///         }
///         HandshakeProgress::Complete(link) => break link,
///     }
/// }
/// ```
///
/// # Parameters
/// - `handshake` — the in-progress handshake handle (consumed; returned inside
///   [`HandshakeProgress::Pending`] if the handshake is not yet finished).
///
/// # Errors
/// - Returns [`PaykitError::Transport`] if the handshake processing fails, if
///   the context is in an invalid state, or if automatic recovery is exhausted.
#[instrument(skip(handshake))]
pub async fn advance_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    // Check whether the handshake has already finished.
    if handshake.encryptor.is_handshake_complete() {
        return finish_handshake(handshake);
    }

    // Process the next handshake step.
    match handshake.encryptor.handle_handshake().await {
        Ok(pubky_noise::HandshakeResult::Pending) => {
            debug!("handshake step pending (waiting for peer)");
            handshake.recovery_attempts = 0;
            Ok(HandshakeProgress::Pending(handshake))
        }
        Ok(pubky_noise::HandshakeResult::Terminal) => {
            debug!("handshake terminal, transitioning to transport");
            finish_handshake(handshake)
        }
        Err(pubky_noise::PubkyNoiseError::HomeserverWriteError) => {
            handshake.recovery_attempts += 1;

            if handshake.recovery_attempts > handshake.max_recovery_attempts {
                return Err(PaykitError::Transport {
                    context: format!(
                        "handshake recovery exhausted after {} consecutive attempts",
                        handshake.max_recovery_attempts,
                    ),
                    source: anyhow::anyhow!(
                        "HomeserverWriteError persisted beyond recovery limit ({})",
                        handshake.max_recovery_attempts,
                    ),
                });
            }

            warn!(
                attempts = handshake.recovery_attempts,
                max = handshake.max_recovery_attempts,
                "handshake write failed, attempting automatic recovery from snapshot"
            );

            let snapshot = handshake
                .encryptor
                .last_good_snapshot()
                .cloned()
                .ok_or_else(|| PaykitError::Transport {
                    context: "handshake recovery failed: missing last-good snapshot".into(),
                    source: anyhow::anyhow!(
                        "pubky-noise returned HomeserverWriteError but no recovery snapshot"
                    ),
                })?;

            let restored = pubky_noise::PubkyNoiseEncryptor::restore(
                handshake.config.clone(),
                snapshot,
                handshake.remote_pubkey.clone(),
            )
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("handshake recovery via restore() failed: {err:?}"),
                source: anyhow::anyhow!("restore after HomeserverWriteError failed: {err:?}"),
            })?;

            debug!("handshake recovered successfully, returning Pending");
            Ok(HandshakeProgress::Pending(EncryptedLinkHandshake {
                encryptor: restored,
                config: handshake.config,
                remote_pubkey: handshake.remote_pubkey,
                recovery_attempts: handshake.recovery_attempts,
                max_recovery_attempts: handshake.max_recovery_attempts,
            }))
        }
        Err(err) => Err(PaykitError::Transport {
            context: format!("handshake step failed: {err:?}"),
            source: anyhow::anyhow!("pubky-noise handle_handshake failed: {err:?}"),
        }),
    }
}

/// Transitions a completed handshake into an [`EncryptedLink`].
fn finish_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    let _link_id =
        handshake
            .encryptor
            .transition_transport()
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to transition to transport mode: {err:?}"),
                source: anyhow::anyhow!("pubky-noise transition_transport failed: {err:?}"),
            })?;

    debug!("Encrypted Link established");
    Ok(HandshakeProgress::Complete(EncryptedLink {
        encryptor: handshake.encryptor,
        recipient: handshake.remote_pubkey,
        config: handshake.config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
        pending_private_messages: VecDeque::new(),
    }))
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot.
///
/// Use this to resume an in-progress handshake after an app restart. A fresh
/// [`pubky_noise::PubkyNoiseConfig`] is built from the supplied session and key
/// material, then replay restore reconstructs the handshake state from the
/// persisted snapshot and homeserver data.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (a fresh session after app restart).
/// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
///   used in the original [`initiate_encrypted_link`] or
///   [`accept_encrypted_link`] call).
/// - `remote_pubkey` — public key of the counterparty.
/// - `outbox_client` — HTTP client for reading from the remote homeserver.
/// - `snapshot` — saved in-progress handshake snapshot (from
///   [`EncryptedLinkHandshake::snapshot`] or
///   [`EncryptedLinkHandshakeSnapshot::deserialize`]).
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored handshakes always reset recovery tuning to defaults:
/// - `recovery_attempts = 0`
/// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
/// created or if the underlying `restore()` fails. Returns
/// [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`, or when the snapshot is not in handshake
/// phase.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link_handshake(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (raw params)");

    let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config for handshake restore: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot
/// using an existing Noise configuration.
///
/// This is the in-process variant of [`restore_encrypted_link_handshake`] — use
/// it when the original `Arc<PubkyNoiseConfig>` is still available.
///
/// # Parameters
/// - `config` — shared Noise configuration matching the original handshake
///   session.
/// - `remote_pubkey` — public key of the counterparty.
/// - `snapshot` — saved in-progress handshake snapshot.
///
/// # Restore behavior
///
/// Restored handshakes always reset recovery tuning to defaults:
/// - `recovery_attempts = 0`
/// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
/// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`, or when the snapshot is not in handshake
/// phase.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_handshake_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (existing config)");
    restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both handshake restore variants.
async fn restore_encrypted_link_handshake_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    if snapshot.recipient() != remote_pubkey {
        return Err(PaykitError::Validation(format!(
            "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
            remote_pubkey,
            snapshot.recipient(),
        )));
    }

    if !matches!(
        snapshot.state.phase,
        pubky_noise::snow_crypto::NoisePhase::HandShake
    ) {
        return Err(PaykitError::Validation(format!(
            "handshake restore requires handshake-phase snapshot, got {:?}",
            snapshot.state.phase,
        )));
    }

    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        snapshot.state,
        remote_pubkey.clone(),
    )
    .await
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to restore Encrypted Link handshake: {err:?}"),
        source: anyhow::anyhow!("pubky-noise handshake restore failed: {err:?}"),
    })?;

    debug!("Encrypted Link handshake restored successfully (recovery tuning reset to defaults)");

    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: remote_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Closes an Encrypted Link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing Encrypted Link");
    link.encryptor.close();
    debug!("Encrypted Link closed successfully");
    Ok(())
}

/// Restores an [`EncryptedLink`] from a previously saved snapshot.
///
/// Use this to resume an encrypted session after an app restart without
/// re-doing the Noise handshake. The restore mechanism replays all handshake
/// messages from the homeservers through a fresh Noise state built with the
/// same ephemeral key material, then transitions to transport mode and sets
/// the nonces and transport slot counters from the saved state.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing messages
///   (a fresh session after app restart).
/// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
///   used in the original [`initiate_encrypted_link`] or
///   [`accept_encrypted_link`] call).
/// - `remote_pubkey` — public key of the counterparty.
/// - `outbox_client` — HTTP client for reading from the remote homeserver.
/// - `snapshot` — the saved snapshot (from [`EncryptedLink::snapshot`] or
///   [`EncryptedLinkSnapshot::deserialize`]).
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// Call [`EncryptedLink::set_max_send_retries`] after restore if you need a
/// non-default value.
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
/// created or if the underlying `restore()` fails (e.g. handshake messages
/// are no longer available on the homeservers, or the replayed handshake
/// hash does not match the saved one). Returns [`PaykitError::Validation`]
/// when `remote_pubkey` does not match the recipient embedded in `snapshot`.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring Encrypted Link from snapshot (raw params)");

    let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config for restore: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
}

/// Restores an [`EncryptedLink`] from a previously saved snapshot using an
/// existing Noise configuration.
///
/// This is the in-process variant of [`restore_encrypted_link`] — use it when
/// the original `Arc<PubkyNoiseConfig>` is still available (e.g. the link
/// needs rebuilding without an app restart). For cross-restart recovery, use
/// [`restore_encrypted_link`] instead.
///
/// # Parameters
/// - `config` — the shared Noise configuration (must match the original
///   session's write/read paths and keypair).
/// - `remote_pubkey` — public key of the counterparty.
/// - `snapshot` — the saved snapshot.
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// Call [`EncryptedLink::set_max_send_retries`] after restore if you need a
/// non-default value.
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
/// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring Encrypted Link from snapshot (existing config)");
    restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both restore variants.
async fn restore_encrypted_link_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    if snapshot.recipient() != remote_pubkey {
        return Err(PaykitError::Validation(format!(
            "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
            remote_pubkey,
            snapshot.recipient(),
        )));
    }

    if !matches!(
        snapshot.state.phase,
        pubky_noise::snow_crypto::NoisePhase::Transport
    ) {
        return Err(PaykitError::Validation(format!(
            "Encrypted Link restore requires transport-phase snapshot, got {:?}",
            snapshot.state.phase,
        )));
    }

    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        snapshot.state,
        remote_pubkey.clone(),
    )
    .await
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to restore Encrypted Link: {err:?}"),
        source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
    })?;

    debug!("Encrypted Link restored successfully");
    Ok(EncryptedLink {
        encryptor,
        recipient: remote_pubkey.clone(),
        config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
        pending_private_messages: VecDeque::new(),
    })
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
        for name in ["bitcoin-bolt11", "bitcoin-bolt12", "bitcoin-p2tr"] {
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

/// Integration tests (require an ephemeral Pubky testnet).
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
        public_storage: pubky::PublicStorage,
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

            let public_storage = sdk.public_storage();

            Self {
                _testnet: testnet,
                session: session.clone(),
                public_storage,
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

        let method = PaymentEndpointIdentifier::new("onchain").unwrap();
        let endpoint = PaymentEndpointPayload::new("{\"address\":\"bc1...\"}");

        set_payment_endpoint(&setup.session, method.clone(), endpoint.clone())
            .await
            .unwrap();

        let fetched = get_payment_endpoint(&setup.public_storage, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(fetched, Some(endpoint.clone()));

        let list = get_payment_list(&setup.public_storage, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list,
            PaymentList {
                entries: vec![(method.clone(), endpoint.clone())]
                    .into_iter()
                    .collect()
            }
        );

        let new_endpoint = PaymentEndpointPayload::new("{\"address\":\"1c1...\"}");

        set_payment_endpoint(&setup.session, method.clone(), new_endpoint.clone())
            .await
            .unwrap();

        let updated = get_payment_endpoint(&setup.public_storage, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(updated, Some(new_endpoint.clone()));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn missing_endpoint_returns_none() {
        let setup = TestSetup::new().await;
        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();

        let missing = get_payment_endpoint(&setup.public_storage, &setup.public_key, &method)
            .await
            .unwrap();
        assert!(missing.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_additions_and_removals() {
        let setup = TestSetup::new().await;

        let onchain = PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap();
        let lightning = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let onchain_data = PaymentEndpointPayload::new("bc1p...");
        let lightning_data = PaymentEndpointPayload::new("ln...");

        set_payment_endpoint(&setup.session, onchain.clone(), onchain_data.clone())
            .await
            .unwrap();
        set_payment_endpoint(&setup.session, lightning.clone(), lightning_data.clone())
            .await
            .unwrap();

        let list = get_payment_list(&setup.public_storage, &setup.public_key)
            .await
            .unwrap();
        let mut expected = HashMap::new();
        expected.insert(onchain.clone(), onchain_data.clone());
        expected.insert(lightning.clone(), lightning_data.clone());
        assert_eq!(list.entries, expected);

        remove_payment_endpoint(&setup.session, onchain.clone())
            .await
            .unwrap();
        let list = get_payment_list(&setup.public_storage, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list.entries,
            vec![(lightning.clone(), lightning_data.clone())]
                .into_iter()
                .collect()
        );

        remove_payment_endpoint(&setup.session, lightning.clone())
            .await
            .unwrap();
        let empty = get_payment_list(&setup.public_storage, &setup.public_key)
            .await
            .unwrap();
        assert!(empty.entries.is_empty());

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

    // ── Private Payment Envelopes test infrastructure ────────────────────────────

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
        /// Sender's Encrypted Link (writes Private Payment Envelopes).
        sender_link: EncryptedLink,
        /// Sender's session (kept for cleanup via `signout`).
        sender_session: PubkySession,
        /// Receiver's Encrypted Link (reads Private Payment Envelopes).
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

    // ── Private Payment Envelopes tests ──────────────────────────────────────────

    fn private_payment_envelope(
        entries: &HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>,
    ) -> PrivatePaymentEnvelope {
        PrivatePaymentEnvelope::new(PaymentReference::new_v4(), entries.clone())
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

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        set_private_payment_envelope(
            &mut setup.sender_link,
            &PrivatePaymentEnvelope::new(reference.clone(), entries),
        )
        .await
        .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.reference, reference);
        assert_eq!(received.entries.len(), 1);
        assert_eq!(received.entries.get(&method), Some(&data));

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payment_envelope_multiple_methods() {
        let mut setup = PrivateTestSetup::new().await;

        let lightning = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let onchain = PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap();
        let cashu = PaymentEndpointIdentifier::new("cashu-mint_id").unwrap();

        let mut entries = HashMap::new();
        entries.insert(lightning.clone(), PaymentEndpointPayload::new("ln..."));
        entries.insert(onchain.clone(), PaymentEndpointPayload::new("bc1p..."));
        entries.insert(
            cashu.clone(),
            PaymentEndpointPayload::new("{\"mint\":\"https://...\"}"),
        );

        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.entries.len(), 3);
        assert_eq!(
            received.entries.get(&lightning),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
        assert_eq!(
            received.entries.get(&onchain),
            Some(&PaymentEndpointPayload::new("bc1p..."))
        );
        assert_eq!(
            received.entries.get(&cashu),
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
            PaymentEndpointIdentifier::new("bitcoin-lightning").unwrap(),
            PaymentEndpointPayload::new("v1"),
        );
        set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries_v1),
        )
        .await
        .unwrap();

        // Second write: completely different map (onchain only).
        let onchain = PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap();
        let mut entries_v2 = HashMap::new();
        entries_v2.insert(onchain.clone(), PaymentEndpointPayload::new("v2"));
        set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries_v2),
        )
        .await
        .unwrap();

        // The helper drains queued unread updates and returns the latest map.
        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(
            received.entries.get(&onchain),
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
        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let oversized_value = "x".repeat(1000);
        let mut entries = HashMap::new();
        entries.insert(method, PaymentEndpointPayload::new(oversized_value));

        let result = set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries),
        )
        .await;
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

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();
        send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("Private Payment Envelope should not be lost behind Receipt Access message");
        assert_eq!(received.entries.get(&method), Some(&data));
        assert_eq!(setup.receiver_link.pending_private_messages.len(), 1);
        assert_eq!(
            setup.receiver_link.pending_private_messages[0]
                .kind
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

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect(
                "Private Payment Envelope should be found without dropping Receipt Access message",
            );
        assert_eq!(received.entries.get(&method), Some(&data));
        assert_eq!(setup.receiver_link.pending_private_messages.len(), 1);
        assert_eq!(
            setup.receiver_link.pending_private_messages[0]
                .kind
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

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid Private Payment Envelope should survive unknown earlier message");
        assert_eq!(received.entries.get(&method), Some(&data));
        assert!(setup.receiver_link.pending_private_messages.is_empty());

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_ignores_malformed_messages_before_valid_payment() {
        let mut setup = PrivateTestSetup::new().await;

        send_raw_private_message(&mut setup.sender_link, "not-json").await;

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid Private Payment Envelope should survive malformed earlier message");
        assert_eq!(received.entries.get(&method), Some(&data));
        assert!(setup.receiver_link.pending_private_messages.is_empty());

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_ignores_malformed_messages_after_valid_payment() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let data = PaymentEndpointPayload::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());
        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
            .await
            .unwrap();
        send_raw_private_message(&mut setup.sender_link, "not-json").await;

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("valid Private Payment Envelope should survive malformed later message");
        assert_eq!(received.entries.get(&method), Some(&data));
        assert!(setup.receiver_link.pending_private_messages.is_empty());

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn get_private_payment_envelope_keeps_latest_payment_without_dropping_other_kinds() {
        let mut setup = PrivateTestSetup::new().await;

        let method = PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap();
        let mut entries_v1 = HashMap::new();
        entries_v1.insert(method.clone(), PaymentEndpointPayload::new("v1"));
        set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries_v1),
        )
        .await
        .unwrap();

        send_raw_private_message(&mut setup.sender_link, TEST_RECEIPT_ACCESS_JSON).await;

        let mut entries_v2 = HashMap::new();
        entries_v2.insert(method.clone(), PaymentEndpointPayload::new("v2"));
        set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries_v2),
        )
        .await
        .unwrap();

        let received = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .expect("latest Private Payment Envelope should be returned");
        assert_eq!(
            received.entries.get(&method),
            Some(&PaymentEndpointPayload::new("v2"))
        );
        assert_eq!(setup.receiver_link.pending_private_messages.len(), 1);
        assert_eq!(
            setup.receiver_link.pending_private_messages[0]
                .kind
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
                "Private Payment Envelopes poll timed out after {timeout:?}"
            );

            if let Some(result) = get_private_payment_envelope(link).await.unwrap() {
                if !result.entries.is_empty() {
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
    /// - Encrypted Link: initiate, accept, handshake (polling loops)
    /// - Private Payment Envelopes: set, get (with polling)
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

        // Reader (Bob): authenticated session for the Encrypted Link
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
            // 1. Initiate Encrypted Link handshake.
            let handshake = initiate_encrypted_link(
                w_session.clone(),
                writer_keypair.secret_key(),
                &w_reader_pubkey,
                writer_sdk,
            )
            .unwrap();

            // 2. Drive handshake to completion (polling loop).
            let mut link = drive_handshake_to_completion(handshake).await;

            // 3. Send Private Payment Envelopes.
            let mut entries = HashMap::new();
            entries.insert(
                PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap(),
                PaymentEndpointPayload::new("lnbcpriv..."),
            );
            entries.insert(
                PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap(),
                PaymentEndpointPayload::new("bc1priv..."),
            );
            set_private_payment_envelope(&mut link, &private_payment_envelope(&entries))
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
            // 1. Accept Encrypted Link handshake.
            let handshake = accept_encrypted_link(
                r_session.clone(),
                reader_keypair.secret_key(),
                &r_writer_pubkey,
                reader_sdk,
            )
            .unwrap();

            // 2. Drive handshake to completion (polling loop).
            let mut link = drive_handshake_to_completion(handshake).await;

            // 3. Poll for Private Payment Envelopes (writer may not have sent yet).
            let private = poll_private_payment_envelope(&mut link).await;
            assert_eq!(
                private.entries.len(),
                2,
                "expected 2 Payment Endpoints, got {}",
                private.entries.len()
            );
            assert_eq!(
                private
                    .entries
                    .get(&PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap()),
                Some(&PaymentEndpointPayload::new("lnbcpriv...")),
            );
            assert_eq!(
                private
                    .entries
                    .get(&PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap()),
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
            PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnrestored..."),
        );
        set_private_payment_envelope(&mut initiator_link, &private_payment_envelope(&entries))
            .await
            .unwrap();

        let received = get_private_payment_envelope(&mut responder_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(
            received
                .entries
                .get(&PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap()),
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
            PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        set_private_payment_envelope(&mut setup.sender_link, &private_payment_envelope(&entries))
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
            PaymentEndpointIdentifier::new("bitcoin-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnv1..."),
        );
        set_private_payment_envelope(
            &mut setup.sender_link,
            &private_payment_envelope(&entries_v1),
        )
        .await
        .unwrap();

        // Consume the message on the receiver side.
        let received_v1 = get_private_payment_envelope(&mut setup.receiver_link)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_v1.entries.len(), 1);

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
            PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap(),
            PaymentEndpointPayload::new("bc1pv2..."),
        );
        set_private_payment_envelope(&mut restored_sender, &private_payment_envelope(&entries_v2))
            .await
            .unwrap();

        // Receive on the restored receiver.
        let received_v2 = get_private_payment_envelope(&mut restored_receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received_v2.entries.len(), 1);
        assert_eq!(
            received_v2
                .entries
                .get(&PaymentEndpointIdentifier::new("bitcoin-p2tr").unwrap()),
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

    // ── Receipt tests ───────────────────────────────────────────────────

    #[test]
    fn test_receipt_location_uses_payment_reference() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            ReceiptAccess::location_for(&reference),
            "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn test_encrypt_receipt_roundtrip_binds_location() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let recipient_public_key = Keypair::random().public_key();
        let receipt = Receipt {
            reference: reference.clone(),
            recipient_public_key,
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::from([("preimage".to_string(), "abc".to_string())]),
        };
        let location = ReceiptAccess::location_for(&reference);
        let key = ReceiptDecryptionKey::generate();

        let encrypted = receipt.encrypt(&key).unwrap();
        let decrypted = decrypt_receipt(&encrypted, &key, &location).unwrap();
        assert_eq!(decrypted, receipt);

        let wrong_location = "/pub/paykit/v0/private/receipts/650e8400-e29b-41d4-a716-446655440000";
        let err = decrypt_receipt(&encrypted, &key, wrong_location).unwrap_err();
        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }

    fn encrypt_receipt_for_test_location(
        receipt: &Receipt,
        key: &ReceiptDecryptionKey,
        location: &str,
    ) -> String {
        let key_bytes = key.bytes().unwrap();
        let cipher = XChaCha20Poly1305::new((&key_bytes).into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let plaintext = serde_json::to_vec(&ReceiptWire::from(receipt)).unwrap();
        let ciphertext = cipher
            .encrypt(
                &nonce,
                chacha20poly1305::aead::Payload {
                    msg: &plaintext,
                    aad: Receipt::aad_for_location(location).as_bytes(),
                },
            )
            .unwrap();
        serde_json::to_string(&EncryptedReceiptWire {
            version: 1,
            kind: "paykit.receipt.encrypted".to_string(),
            algorithm: "XChaCha20Poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        })
        .unwrap()
    }

    #[test]
    fn test_decrypt_receipt_rejects_plaintext_reference_that_does_not_match_location() {
        let location_reference =
            PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let plaintext_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let recipient_public_key = Keypair::random().public_key();
        let receipt = Receipt {
            reference: plaintext_reference,
            recipient_public_key,
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
            amount: Some("1000".to_string()),
            currency: Some("sats".to_string()),
            metadata: HashMap::new(),
        };
        let location = ReceiptAccess::location_for(&location_reference);
        let key = ReceiptDecryptionKey::generate();
        let encrypted = encrypt_receipt_for_test_location(&receipt, &key, &location);

        let err = decrypt_receipt(&encrypted, &key, &location).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt Payment Reference does not match Receipt Location")),
            "expected Receipt/Receipt Location mismatch error, got: {err}"
        );
    }

    #[tokio::test]
    async fn issue_receipt_stores_encrypted_receipt_and_sends_access_message() {
        let mut setup = PrivateTestSetup::new().await;
        let reference = PaymentReference::new_v4();
        let draft = ReceiptDraft {
            reference: reference.clone(),
            payment_endpoint_identifier: Some(PaymentEndpointIdentifier::new("lightning").unwrap()),
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

    #[test]
    fn test_parse_receipt_access_json_rejects_location_that_does_not_match_reference() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let other_reference =
            PaymentReference::new("650e8400-e29b-41d4-a716-446655440000").unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference: reference.clone(),
            location: ReceiptAccess::location_for(&other_reference),
            key: ReceiptDecryptionKey::generate(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let json = serialize_receipt_access_json(&access).unwrap();

        let err = parse_receipt_access_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("Receipt Access location does not match Payment Reference")),
            "expected mismatched location error, got: {err}"
        );
    }

    #[test]
    fn test_receipt_decryption_key_debug_and_display_are_redacted() {
        let key = ReceiptDecryptionKey::generate();
        let raw_key = key.as_str().to_string();
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let access = ReceiptAccess {
            version: 1,
            kind: PrivateMessageKind::ReceiptAccess,
            reference: reference.clone(),
            location: ReceiptAccess::location_for(&reference),
            key: key.clone(),
            algorithm: "XChaCha20Poly1305".to_string(),
        };
        let issued = IssuedReceipt {
            reference,
            location: access.location.clone(),
            key,
        };

        assert!(!format!("{issued:?}").contains(&raw_key));
        assert!(!format!("{access:?}").contains(&raw_key));
        assert!(!format!("{:?}", access.key).contains(&raw_key));
        assert!(!format!("{}", access.key).contains(&raw_key));
    }

    #[test]
    fn test_serialize_private_payment_envelope_json_uses_versioned_envelope() {
        let reference = PaymentReference::new("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let mut entries = HashMap::new();
        entries.insert(
            PaymentEndpointIdentifier::new("lightning").unwrap(),
            PaymentEndpointPayload::new("ln..."),
        );
        let payload = PrivatePaymentEnvelope::new(reference.clone(), entries);
        let json = serialize_private_payment_envelope_json(&payload).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["kind"], "paykit.private_payments");
        assert_eq!(value["reference"], reference.as_str());
        assert_eq!(value["entries"]["lightning"], "ln...");
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
        let err = parse_private_payment_envelope_json(r#"{"version":2,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope version 2")),
            "expected unsupported version error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_unsupported_kind() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.receipt","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("unsupported Private Payment Envelope kind")),
            "expected unsupported kind error, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_rejects_invalid_reference() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"not-a-uuid","entries":{}}"#).unwrap_err();
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
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for empty key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_path_traversal_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"..":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for path-traversal key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_slash_in_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"foo/bar":"ln..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for key with slash, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_reserved_private_key() {
        let err = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"private":"secret..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData for reserved 'private' key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_oversized_key() {
        let long_key = "a".repeat(65);
        let json = format!(
            r#"{{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{{"{long_key}":"ln..."}}}}"#
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
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"lightning":"ln...","":"bc1..."}}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid Payment Endpoint Identifier")),
            "expected InvalidData when one key is invalid, got: {err}"
        );
    }

    // ── Happy path ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payment_envelope_json_valid_single_entry() {
        let result = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"lightning":"ln..."}}"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&PaymentEndpointIdentifier::new("lightning").unwrap()),
            Some(&PaymentEndpointPayload::new("ln..."))
        );
    }

    #[test]
    fn test_parse_private_payment_envelope_json_valid_multiple_entries() {
        let result =
            parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{"lightning":"ln...","onchain":"bc1..."}}"#).unwrap();
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
        let result = parse_private_payment_envelope_json(r#"{"version":1,"kind":"paykit.private_payments","reference":"550e8400-e29b-41d4-a716-446655440000","entries":{}}"#).unwrap();
        assert!(result.is_empty());
    }
}
