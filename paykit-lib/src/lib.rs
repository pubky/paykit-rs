#![doc = include_str!("../README.md")]

use std::collections::HashMap;
#[cfg(not(feature = "pubky"))]
use std::fmt;

use thiserror::Error;
use tracing::{debug, instrument, warn};

#[cfg(feature = "pubky")]
pub use pubky::PublicKey;

#[cfg(feature = "pubky")]
pub use pubky_noise;

#[cfg(not(feature = "pubky"))]
/// Public key placeholder used when the `pubky` feature is disabled.
///
/// Applications providing their own transport layer should define a richer type
/// and convert into this wrapper where necessary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey(pub String);

#[cfg(not(feature = "pubky"))]
impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(not(feature = "pubky"))]
impl std::str::FromStr for PublicKey {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(PublicKey(s.to_string()))
    }
}

mod transport;

pub use transport::{AuthenticatedTransport, UnauthenticatedTransportRead};

/// Pubky adapters are only exposed when the default `pubky` feature is enabled.
#[cfg(feature = "pubky")]
pub use transport::{PubkyAuthenticatedTransport, PubkyUnauthenticatedTransport};

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
    /// Returned when a caller-supplied value (such as a [`MethodId`]) violates
    /// structural invariants — for example containing path-traversal sequences,
    /// null bytes, or characters outside the allowed set.
    #[error("validation error: {0}")]
    Validation(String),
}

/// Identifier for a payment method specification.
///
/// A `MethodId` is a single, safe path segment stored under `/pub/paykit/v0/…`.
/// It is validated at construction time to prevent path injection attacks.
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
/// # use paykit_lib::MethodId;
/// let m = MethodId::new("lightning").unwrap();
/// assert_eq!(m.as_str(), "lightning");
///
/// // Path traversal is rejected:
/// assert!(MethodId::new("../etc/passwd").is_err());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MethodId(String);

/// Maximum length (in bytes) of a [`MethodId`] value.
const METHOD_ID_MAX_LEN: usize = 64;
/// Reserved [`MethodId`] value used by private-payment storage.
const METHOD_ID_RESERVED_PRIVATE: &str = "private";

impl MethodId {
    /// Create a new `MethodId` after validating the identifier.
    ///
    /// Returns `Err(PaykitError::Validation)` if the value is empty, too long,
    /// contains forbidden characters, resembles a path-traversal component,
    /// or collides with a reserved identifier.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();

        if id.is_empty() {
            return Err(PaykitError::Validation("MethodId must not be empty".into()));
        }

        if id.len() > METHOD_ID_MAX_LEN {
            return Err(PaykitError::Validation(format!(
                "MethodId must not exceed {METHOD_ID_MAX_LEN} characters, got {}",
                id.chars().count()
            )));
        }

        if id == METHOD_ID_RESERVED_PRIVATE {
            return Err(PaykitError::Validation(format!(
                "MethodId '{METHOD_ID_RESERVED_PRIVATE}' is reserved for private payments"
            )));
        }

        // Every character must be ASCII alphanumeric, hyphen, underscore, or dot.
        if let Some((pos, ch)) = id
            .char_indices()
            .find(|&(_, ch)| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.'))
        {
            return Err(PaykitError::Validation(format!(
                "MethodId contains forbidden character '{}' at byte {pos} in \"{id}\"",
                ch
            )));
        }

        // Reject pure-dot names that are path-traversal components.
        if id.bytes().all(|b| b == b'.') {
            return Err(PaykitError::Validation(format!(
                "MethodId must not be a path-traversal component: \"{id}\""
            )));
        }

        Ok(Self(id))
    }

    /// Access the inner identifier string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MethodId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for MethodId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Serialized payload served by a payment endpoint (UTF-8 text such as JSON, lnurl, etc.).
///
/// If you need to transmit binary payloads, encode them (e.g., base64) before wrapping
/// in `EndpointData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointData(String);

impl EndpointData {
    /// Wrap a payload string as endpoint data.
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

impl std::fmt::Display for EndpointData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for EndpointData {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Collection of supported payment entries keyed by method identifiers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupportedPayments {
    /// Map of `MethodId` to endpoint data.
    pub entries: HashMap<MethodId, EndpointData>,
}

#[cfg(feature = "pubky")]
/// Handle to an established encrypted Noise link with a peer.
///
/// Created by [`advance_handshake`] (via [`HandshakeProgress::Complete`]) after
/// a successful Noise handshake. Used by the private payment helper functions to
/// encrypt and decrypt payment data. Must be closed via [`close_encrypted_link`]
/// when no longer needed.
///
/// The link wraps a [`pubky_noise::PubkyNoiseEncryptor`] in transport mode.
///
/// # Session resumption
///
/// An established link can be snapshotted via [`snapshot`](Self::snapshot) (or
/// serialized directly via [`serialize`](Self::serialize)) and later restored
/// with [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// without re-doing the Noise handshake.
///
/// # Automatic send retry
///
/// [`set_private_payments`] automatically retries failed `send_message` calls
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
    /// [`set_private_payments`].
    max_send_retries: u32,
}

#[cfg(feature = "pubky")]
impl EncryptedLink {
    /// Set the maximum number of automatic `send_message` retries before
    /// [`set_private_payments`] gives up and returns [`PaykitError::Transport`].
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
}

#[cfg(feature = "pubky")]
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
/// The serialized representation is the 189-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise`. The remote peer's public key is
/// embedded in the snapshot (bytes 157-188) and reconstructed automatically
/// during deserialization.
pub struct EncryptedLinkSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
}

#[cfg(feature = "pubky")]
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

#[cfg(feature = "pubky")]
impl std::fmt::Debug for EncryptedLinkSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "pubky")]
impl EncryptedLinkSnapshot {
    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 189 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!("failed to deserialize encrypted link snapshot: {err:?}"),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "encrypted link snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

#[cfg(feature = "pubky")]
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
/// The serialized representation is the 189-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise`. The remote peer's public key is
/// embedded in the snapshot (bytes 157-188) and reconstructed automatically
/// during deserialization.
pub struct EncryptedLinkHandshakeSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
}

#[cfg(feature = "pubky")]
impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "pubky")]
impl EncryptedLinkHandshakeSnapshot {
    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 189 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!(
                        "failed to deserialize encrypted link handshake snapshot: {err:?}"
                    ),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "encrypted link handshake snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

#[cfg(feature = "pubky")]
/// Default maximum number of automatic `send_message` retries before
/// [`set_private_payments`] gives up and returns an error.
///
/// Override per-link via [`EncryptedLink::set_max_send_retries`].
pub const DEFAULT_MAX_SEND_RETRIES: u32 = 3;

#[cfg(feature = "pubky")]
/// Default maximum number of consecutive automatic recovery attempts before
/// [`advance_handshake`] gives up and returns an error.
///
/// Override per-handshake via [`EncryptedLinkHandshake::set_max_recovery_attempts`].
pub const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 3;

#[cfg(feature = "pubky")]
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

#[cfg(feature = "pubky")]
impl EncryptedLinkHandshake {
    /// Set the maximum number of consecutive automatic recovery attempts
    /// before [`advance_handshake`] gives up and returns
    /// [`PaykitError::Transport`].
    ///
    /// The counter resets to zero after every successful handshake step.
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

#[cfg(feature = "pubky")]
/// Result of a single [`advance_handshake`] step.
pub enum HandshakeProgress {
    /// Handshake is still in progress. The peer may not have written their next
    /// message yet. Pass the returned handle back to [`advance_handshake`] after
    /// a caller-chosen delay.
    Pending(EncryptedLinkHandshake),

    /// Handshake completed successfully. The [`EncryptedLink`] is ready for use
    /// with [`set_private_payments`] and [`get_private_payments`].
    Complete(EncryptedLink),
}

/// Domain separation string for Paykit private payment path derivation.
///
/// Ensures that different applications using the same key pairs derive
/// different storage paths, preventing cross-protocol path collisions.
#[cfg(feature = "pubky")]
const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

#[cfg(feature = "pubky")]
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
        transport::pubky::PAYKIT_PRIVATE_PATH_PREFIX,
    )
}

#[cfg(feature = "pubky")]
/// Deserializes a private payments JSON blob into a map of method IDs to
/// endpoint data.
///
/// The expected format is `{ "method_id": "endpoint_value", ... }`.
fn parse_private_payments_json(json: &str) -> Result<HashMap<MethodId, EndpointData>> {
    let map: HashMap<String, String> =
        serde_json::from_str(json).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to parse private payments JSON: {err}"),
            source: Some(err.into()),
        })?;

    let mut result = HashMap::new();
    for (key, value) in map {
        let method_id = MethodId::new(&key).map_err(|err| PaykitError::InvalidData {
            context: format!("private payments blob contains invalid method identifier '{key}'"),
            source: Some(err.into()),
        })?;
        result.insert(method_id, EndpointData::new(value));
    }
    Ok(result)
}

/// Serializes a map of method IDs to endpoint data into a JSON string.
#[cfg(feature = "pubky")]
fn serialize_private_payments_json(entries: &HashMap<MethodId, EndpointData>) -> Result<String> {
    let map: HashMap<&str, &str> = entries
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    serde_json::to_string(&map).map_err(|err| PaykitError::InvalidData {
        context: format!("failed to serialize private payments JSON: {err}"),
        source: Some(err.into()),
    })
}

#[cfg(feature = "pubky")]
fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

/// Stores or updates a payment endpoint via the injected authenticated client.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, MethodId, EndpointData, PublicKey};
/// # use paykit_lib::AuthenticatedTransport;
/// # async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
/// let method = MethodId::new("bitcoin-bolt11")?;
/// let data = EndpointData::new("ln...");
/// set_payment_endpoint(client, method, data).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(client, data), fields(method = %method))]
pub async fn set_payment_endpoint<S>(client: &S, method: MethodId, data: EndpointData) -> Result<()>
where
    S: AuthenticatedTransport,
{
    debug!("storing payment endpoint");
    client
        .upsert_payment_endpoint(&method, &data)
        .await
        .map_err(|err| map_error("set_payment_endpoint", err))
}

#[cfg(feature = "pubky")]
/// Encrypts and sends the complete private payments map via the established
/// encrypted link.
///
/// The caller is responsible for managing the map contents (adding/removing
/// entries). This function serializes the map to JSON, encrypts it using
/// [`pubky_noise::PubkyNoiseEncryptor::send_message`], and pubky-noise handles
/// file naming and storage location on the homeserver.
///
/// # Automatic retry
///
/// If `send_message` fails, this function automatically retries up to
/// [`EncryptedLink::set_max_send_retries`] times (default:
/// [`DEFAULT_MAX_SEND_RETRIES`]). Transport-phase send failures do not
/// corrupt the Noise state, so retries are safe without snapshot-based
/// recovery.
///
/// # Payload size
///
/// The serialized JSON must fit within a single pubky-noise message
/// (`PUBKY_NOISE_MSG_LEN`, currently 1000 bytes). Exceeding this limit
/// returns [`PaykitError::Validation`].
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for encryption and I/O.
/// - `entries` — the complete map of payment methods to store.
///
/// # Errors
/// - Returns [`PaykitError::Validation`] if the serialized payload exceeds
///   the maximum message size.
/// - Returns [`PaykitError::InvalidData`] if the map cannot be serialized.
/// - Returns [`PaykitError::Transport`] if `send_message` fails after all
///   retry attempts are exhausted.
#[instrument(skip(link, entries), fields(count = entries.len()))]
pub async fn set_private_payments(
    link: &mut EncryptedLink,
    entries: &HashMap<MethodId, EndpointData>,
) -> Result<()> {
    debug!("sending private payments map");

    let json = serialize_private_payments_json(entries)
        .map_err(|err| map_error("set_private_payments", err))?;

    let plaintext = json.into_bytes();

    if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "private payments payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        )));
    }

    let max_attempts = send_attempts_from_retries(link.max_send_retries); // first try + retries
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match link.encryptor.send_message(&plaintext).await {
            Ok(()) => {
                debug!("private payments map sent successfully");
                return Ok(());
            }
            Err(err) => {
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = link.max_send_retries,
                        error = ?err,
                        "send_message failed, retrying"
                    );
                }
            }
        }
    }

    Err(PaykitError::Transport {
        context: format!(
            "failed to send private payments after {} attempts",
            max_attempts,
        ),
        source: anyhow::anyhow!(
            "pubky-noise send_message failed on all {} attempts; last error: {}",
            max_attempts,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    })
}

/// Removes a payment endpoint via the injected authenticated client.
#[instrument(skip(client), fields(method = %method))]
pub async fn remove_payment_endpoint<S>(client: &S, method: MethodId) -> Result<()>
where
    S: AuthenticatedTransport,
{
    debug!("removing payment endpoint");
    client
        .remove_payment_endpoint(&method)
        .await
        .map_err(|err| map_error("remove_payment_endpoint", err))
}

/// Retrieves all supported payment methods for the given payee.
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
/// # use paykit_lib::{get_payment_list, MethodId, EndpointData, SupportedPayments};
/// # use paykit_lib::{AuthenticatedTransport, UnauthenticatedTransportRead};
/// # async fn demo(reader: &impl UnauthenticatedTransportRead, pk: &paykit_lib::PublicKey) -> paykit_lib::Result<()> {
/// let payments = get_payment_list(reader, pk).await?;
/// if payments.entries.is_empty() {
///     println!("payee published no endpoints yet");
/// } else {
///     for (method, data) in &payments.entries {
///         println!("method={} payload={}", method.as_str(), data.as_str());
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader))]
pub async fn get_payment_list<R>(reader: &R, payee: &PublicKey) -> Result<SupportedPayments>
where
    R: UnauthenticatedTransportRead,
{
    debug!("fetching payment list");
    let result = reader
        .fetch_supported_payments(payee)
        .await
        .map_err(|err| map_error("get_payment_list", err))?;
    debug!(count = result.entries.len(), "payment list retrieved");
    Ok(result)
}

#[cfg(feature = "pubky")]
/// Receives and decrypts the private payments map from the remote peer
/// via the established encrypted link.
///
/// Returns the full map of payment methods. The caller can look up
/// individual methods from the returned [`SupportedPayments`].
///
/// # Parameters
/// - `link` — an established [`EncryptedLink`] for decryption and I/O.
///
/// # Semantics
/// - Returns an empty [`SupportedPayments`] when no messages are available.
/// - Drains all currently unread queued updates and returns the latest map.
///   Intermediate queued updates are consumed.
/// - Returns `Err(PaykitError::InvalidData)` when the decrypted payload
///   is not valid UTF-8 or cannot be parsed as a payments JSON map.
/// - Returns `Err(PaykitError::Transport)` for decryption or I/O failures.
#[instrument(skip(link))]
pub async fn get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments> {
    debug!("receiving private payments map");

    let mut latest: Option<[u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN]> = None;
    let mut drained = 0usize;

    loop {
        let messages =
            link.encryptor
                .receive_message()
                .await
                .map_err(|err| PaykitError::Transport {
                    context: format!("failed to receive private payments: {err:?}"),
                    source: anyhow::anyhow!("pubky-noise receive_message failed: {err:?}"),
                })?;
        if messages.is_empty() {
            break;
        }

        drained += messages.len();
        latest = messages.into_iter().last();
    }

    let Some(raw) = latest else {
        debug!("no private payments messages available, returning empty map");
        return Ok(SupportedPayments::default());
    };

    // Trim trailing zero-padding added by pubky-noise's fixed-size buffers.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
        context: format!("private payments plaintext is not valid UTF-8: {err}"),
        source: Some(err.into()),
    })?;

    let entries = parse_private_payments_json(plaintext)?;
    debug!(
        count = entries.len(),
        drained, "private payments map received"
    );
    Ok(SupportedPayments { entries })
}

/// Retrieves a specific payment endpoint for `payee` and `method`.
///
/// # Semantics
/// - Returns `Ok(None)` when the endpoint file is missing or empty.
/// - Returns `Err(PaykitError::InvalidData)` when the endpoint payload contains invalid UTF-8.
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # use paykit_lib::{get_payment_endpoint, MethodId, PublicKey};
/// # use paykit_lib::UnauthenticatedTransportRead;
/// # async fn inspect(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
/// let lightning = MethodId::new("lightning")?;
/// if let Some(endpoint) = get_payment_endpoint(reader, pk, &lightning).await? {
///     println!("lightning endpoint: {}", endpoint.as_str());
/// } else {
///     println!("no lightning endpoint published");
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader), fields(method = %method))]
pub async fn get_payment_endpoint<R>(
    reader: &R,
    payee: &PublicKey,
    method: &MethodId,
) -> Result<Option<EndpointData>>
where
    R: UnauthenticatedTransportRead,
{
    debug!("fetching payment endpoint");
    let result = reader
        .fetch_payment_endpoint(payee, method)
        .await
        .map_err(|err| map_error("get_payment_endpoint", err))?;
    debug!(found = result.is_some(), "payment endpoint lookup complete");
    Ok(result)
}

#[cfg(feature = "pubky")]
/// Initiates a Noise XX handshake with a remote peer (initiator role).
///
/// Initializes the encryption stack and creates a handshake context. The actual
/// handshake messages are exchanged by repeatedly calling [`advance_handshake`]
/// until it returns [`HandshakeProgress::Complete`].
///
/// Ephemeral keys are managed internally by the Noise stack — callers only need
/// to provide their static identity key and the remote peer's public key.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `sender_secret_key` — 32-byte Ed25519 secret key of the local peer.
/// - `receiver_pubkey` — public key of the remote peer.
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
    debug!("initializing encrypted link handshake (initiator)");

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

#[cfg(feature = "pubky")]
/// Accepts a Noise XX handshake from a remote peer (responder role).
///
/// Initializes the encryption stack and creates a handshake context for the
/// responder side. The actual handshake messages are exchanged by repeatedly
/// calling [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `receiver_secret_key` — 32-byte Ed25519 secret key of the local peer.
/// - `sender_pubkey` — public key of the remote peer (the initiator).
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
    debug!("initializing encrypted link handshake (responder)");

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

#[cfg(feature = "pubky")]
/// Advances the handshake by one step.
///
/// This function is **polling-safe**: calling it when the remote peer has not
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
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]). The counter resets to zero after every
/// successful step. If the limit is exceeded, the function returns
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
#[cfg(feature = "pubky")]
fn finish_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    let _link_id =
        handshake
            .encryptor
            .transition_transport()
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to transition to transport mode: {err:?}"),
                source: anyhow::anyhow!("pubky-noise transition_transport failed: {err:?}"),
            })?;

    debug!("encrypted link established");
    Ok(HandshakeProgress::Complete(EncryptedLink {
        encryptor: handshake.encryptor,
        recipient: handshake.remote_pubkey,
        config: handshake.config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
    }))
}

#[cfg(feature = "pubky")]
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
/// - `remote_pubkey` — public key of the remote peer.
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
    debug!("restoring encrypted link handshake from snapshot (raw params)");

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

#[cfg(feature = "pubky")]
/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot
/// using an existing Noise configuration.
///
/// This is the in-process variant of [`restore_encrypted_link_handshake`] — use
/// it when the original `Arc<PubkyNoiseConfig>` is still available.
///
/// # Parameters
/// - `config` — shared Noise configuration matching the original handshake
///   session.
/// - `remote_pubkey` — public key of the remote peer.
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
    debug!("restoring encrypted link handshake from snapshot (existing config)");
    restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both handshake restore variants.
#[cfg(feature = "pubky")]
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
        context: format!("failed to restore encrypted link handshake: {err:?}"),
        source: anyhow::anyhow!("pubky-noise handshake restore failed: {err:?}"),
    })?;

    debug!("encrypted link handshake restored successfully (recovery tuning reset to defaults)");

    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: remote_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

#[cfg(feature = "pubky")]
/// Closes an encrypted link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing encrypted link");
    link.encryptor.close();
    debug!("encrypted link closed successfully");
    Ok(())
}

#[cfg(feature = "pubky")]
/// Restores an [`EncryptedLink`] from a previously saved snapshot.
///
/// Use this to resume an encrypted session after an app restart without
/// re-doing the Noise handshake. The restore mechanism replays all handshake
/// messages from the homeservers through a fresh Noise state built with the
/// same ephemeral key material, then transitions to transport mode and sets
/// the nonces/counter from the saved state.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing messages
///   (a fresh session after app restart).
/// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
///   used in the original [`initiate_encrypted_link`] or
///   [`accept_encrypted_link`] call).
/// - `remote_pubkey` — public key of the remote peer.
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
    debug!("restoring encrypted link from snapshot (raw params)");

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

#[cfg(feature = "pubky")]
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
/// - `remote_pubkey` — public key of the remote peer.
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
    debug!("restoring encrypted link from snapshot (existing config)");
    restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both restore variants.
#[cfg(feature = "pubky")]
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
            "encrypted link restore requires transport-phase snapshot, got {:?}",
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
        context: format!("failed to restore encrypted link: {err:?}"),
        source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
    })?;

    debug!("encrypted link restored successfully");
    Ok(EncryptedLink {
        encryptor,
        recipient: remote_pubkey.clone(),
        config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
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

    // ── MethodId: valid inputs ──────────────────────────────────────────

    #[test]
    fn test_method_id_valid_simple_names() {
        for name in ["bitcoin-bolt11", "bitcoin-bolt12", "bitcoin-p2tr"] {
            assert!(MethodId::new(name).is_ok(), "expected '{name}' to be valid");
        }
    }

    #[test]
    fn test_method_id_valid_with_dots() {
        let m = MethodId::new("method.v2").unwrap();
        assert_eq!(m.as_str(), "method.v2");
    }

    #[test]
    fn test_method_id_valid_with_underscores() {
        let m = MethodId::new("my_method").unwrap();
        assert_eq!(m.as_str(), "my_method");
    }

    #[test]
    fn test_method_id_valid_mixed_case() {
        let m = MethodId::new("LnUrl-Pay").unwrap();
        assert_eq!(m.as_str(), "LnUrl-Pay");
    }

    #[test]
    fn test_method_id_valid_max_length() {
        let name = "a".repeat(METHOD_ID_MAX_LEN);
        assert!(MethodId::new(&name).is_ok());
    }

    #[test]
    fn test_method_id_valid_single_char() {
        assert!(MethodId::new("x").is_ok());
    }

    #[test]
    fn test_method_id_display() {
        let m = MethodId::new("lightning").unwrap();
        assert_eq!(format!("{m}"), "lightning");
    }

    #[test]
    fn test_method_id_as_ref() {
        let m = MethodId::new("onchain").unwrap();
        let s: &str = m.as_ref();
        assert_eq!(s, "onchain");
    }

    // ── MethodId: invalid inputs ────────────────────────────────────────

    #[test]
    fn test_method_id_reject_empty() {
        let err = MethodId::new("").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("empty")));
    }

    #[test]
    fn test_method_id_reject_path_traversal_dotdot() {
        assert!(MethodId::new("..").is_err());
    }

    #[test]
    fn test_method_id_reject_path_traversal_dot() {
        assert!(MethodId::new(".").is_err());
    }

    #[test]
    fn test_method_id_reject_path_traversal_sequence() {
        // Slashes are rejected by the character allowlist, but verify the
        // specific traversal pattern is caught.
        assert!(MethodId::new("../etc/passwd").is_err());
        assert!(MethodId::new("../../foo").is_err());
    }

    #[test]
    fn test_method_id_reject_forward_slash() {
        assert!(MethodId::new("foo/bar").is_err());
    }

    #[test]
    fn test_method_id_reject_backslash() {
        assert!(MethodId::new("a\\b").is_err());
    }

    #[test]
    fn test_method_id_reject_null_byte() {
        assert!(MethodId::new("foo\0bar").is_err());
    }

    #[test]
    fn test_method_id_reject_too_long() {
        let name = "a".repeat(METHOD_ID_MAX_LEN + 1);
        let err = MethodId::new(&name).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("exceed")));
    }

    #[test]
    fn test_method_id_reject_space() {
        assert!(MethodId::new("foo bar").is_err());
    }

    #[test]
    fn test_method_id_reject_special_chars() {
        for bad in ["foo@bar", "foo:bar", "foo?bar", "foo#bar", "foo=bar"] {
            assert!(
                MethodId::new(bad).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn test_method_id_reject_unicode() {
        assert!(MethodId::new("⚡lightning").is_err());
    }

    #[test]
    fn test_method_id_reject_triple_dots() {
        assert!(MethodId::new("...").is_err());
    }

    #[test]
    fn test_method_id_reject_reserved_private() {
        let err = MethodId::new("private").unwrap_err();
        assert!(matches!(err, PaykitError::Validation(msg) if msg.contains("reserved")));
    }

    // ── EndpointData: basic accessors ───────────────────────────────────

    #[test]
    fn test_endpoint_data_new_and_accessors() {
        let d = EndpointData::new("ln...");
        assert_eq!(d.as_str(), "ln...");
        assert_eq!(format!("{d}"), "ln...");
    }

    #[test]
    fn test_endpoint_data_into_inner() {
        let d = EndpointData::new("payload");
        assert_eq!(d.into_inner(), "payload");
    }

    #[test]
    fn test_endpoint_data_as_ref() {
        let d = EndpointData::new("data");
        let s: &str = d.as_ref();
        assert_eq!(s, "data");
    }
}

/// Integration tests (require `pubky` feature and ephemeral testnet).
#[cfg(all(test, feature = "pubky"))]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use pubky::PubkySession;
    use pubky_testnet::{pubky::Keypair, EphemeralTestnet};

    struct TestSetup {
        _testnet: EphemeralTestnet,
        session_transport: PubkyAuthenticatedTransport,
        reader_transport: PubkyUnauthenticatedTransport,
        raw_session: PubkySession,
        public_key: PublicKey,
    }

    impl TestSetup {
        async fn new() -> Self {
            let testnet = EphemeralTestnet::builder()
                .with_embedded_postgres()
                .build()
                .await
                .unwrap();

            let homeserver = testnet.homeserver_app();
            let sdk = testnet.sdk().unwrap();

            let pair = Keypair::random();
            let signer = sdk.signer(pair.clone());
            let session = signer.signup(&homeserver.public_key(), None).await.unwrap();

            let session_transport = PubkyAuthenticatedTransport::new(session.clone());
            let reader_transport = PubkyUnauthenticatedTransport::new(sdk.public_storage());

            Self {
                _testnet: testnet,
                session_transport,
                reader_transport,
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

    #[tokio::test]
    async fn endpoint_round_trip_and_update() {
        let setup = TestSetup::new().await;

        let method = MethodId::new("onchain").unwrap();
        let endpoint = EndpointData::new("{\"address\":\"bc1...\"}");

        set_payment_endpoint(&setup.session_transport, method.clone(), endpoint.clone())
            .await
            .unwrap();

        let fetched = get_payment_endpoint(&setup.reader_transport, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(fetched, Some(endpoint.clone()));

        let list = get_payment_list(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list,
            SupportedPayments {
                entries: vec![(method.clone(), endpoint.clone())]
                    .into_iter()
                    .collect()
            }
        );

        let new_endpoint = EndpointData::new("{\"address\":\"1c1...\"}");

        set_payment_endpoint(
            &setup.session_transport,
            method.clone(),
            new_endpoint.clone(),
        )
        .await
        .unwrap();

        let updated = get_payment_endpoint(&setup.reader_transport, &setup.public_key, &method)
            .await
            .unwrap();
        assert_eq!(updated, Some(new_endpoint.clone()));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn missing_endpoint_returns_none() {
        let setup = TestSetup::new().await;
        let method = MethodId::new("bitcoin-bolt11").unwrap();

        let missing = get_payment_endpoint(&setup.reader_transport, &setup.public_key, &method)
            .await
            .unwrap();
        assert!(missing.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_additions_and_removals() {
        let setup = TestSetup::new().await;

        let onchain = MethodId::new("bitcoin-p2tr").unwrap();
        let lightning = MethodId::new("bitcoin-bolt11").unwrap();
        let onchain_data = EndpointData::new("bc1p...");
        let lightning_data = EndpointData::new("ln...");

        set_payment_endpoint(
            &setup.session_transport,
            onchain.clone(),
            onchain_data.clone(),
        )
        .await
        .unwrap();
        set_payment_endpoint(
            &setup.session_transport,
            lightning.clone(),
            lightning_data.clone(),
        )
        .await
        .unwrap();

        let list = get_payment_list(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();
        let mut expected = HashMap::new();
        expected.insert(onchain.clone(), onchain_data.clone());
        expected.insert(lightning.clone(), lightning_data.clone());
        assert_eq!(list.entries, expected);

        remove_payment_endpoint(&setup.session_transport, onchain.clone())
            .await
            .unwrap();
        let list = get_payment_list(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();
        assert_eq!(
            list.entries,
            vec![(lightning.clone(), lightning_data.clone())]
                .into_iter()
                .collect()
        );

        remove_payment_endpoint(&setup.session_transport, lightning.clone())
            .await
            .unwrap();
        let empty = get_payment_list(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();
        assert!(empty.entries.is_empty());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn removing_missing_endpoint_is_error() {
        let setup = TestSetup::new().await;
        let method = MethodId::new("unused").unwrap();

        remove_payment_endpoint(&setup.session_transport, method)
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
            let testnet = EphemeralTestnet::builder()
                .with_embedded_postgres()
                .build()
                .await
                .unwrap();
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
    /// [`EncryptedLink`] handles so that `set_private_payments` /
    /// `get_private_payments` can be exercised.
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
            let testnet = EphemeralTestnet::builder()
                .with_embedded_postgres()
                .build()
                .await
                .unwrap();
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

    #[tokio::test]
    async fn private_payments_empty_returns_empty() {
        let mut setup = PrivateTestSetup::new().await;

        let result = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert!(
            result.entries.is_empty(),
            "fresh link with no messages should return empty map"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payments_round_trip() {
        let mut setup = PrivateTestSetup::new().await;

        let method = MethodId::new("bitcoin-bolt11").unwrap();
        let data = EndpointData::new("lnbc1...");
        let mut entries = HashMap::new();
        entries.insert(method.clone(), data.clone());

        set_private_payments(&mut setup.sender_link, &entries)
            .await
            .unwrap();

        let received = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(received.entries.get(&method), Some(&data));

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payments_multiple_methods() {
        let mut setup = PrivateTestSetup::new().await;

        let lightning = MethodId::new("bitcoin-bolt11").unwrap();
        let onchain = MethodId::new("bitcoin-p2tr").unwrap();
        let cashu = MethodId::new("cashu-mint_id").unwrap();

        let mut entries = HashMap::new();
        entries.insert(lightning.clone(), EndpointData::new("ln..."));
        entries.insert(onchain.clone(), EndpointData::new("bc1p..."));
        entries.insert(
            cashu.clone(),
            EndpointData::new("{\"mint\":\"https://...\"}"),
        );

        set_private_payments(&mut setup.sender_link, &entries)
            .await
            .unwrap();

        let received = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert_eq!(received.entries.len(), 3);
        assert_eq!(
            received.entries.get(&lightning),
            Some(&EndpointData::new("ln..."))
        );
        assert_eq!(
            received.entries.get(&onchain),
            Some(&EndpointData::new("bc1p..."))
        );
        assert_eq!(
            received.entries.get(&cashu),
            Some(&EndpointData::new("{\"mint\":\"https://...\"}"))
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payments_update_overwrites() {
        let mut setup = PrivateTestSetup::new().await;

        // First write: lightning only.
        let mut entries_v1 = HashMap::new();
        entries_v1.insert(
            MethodId::new("bitcoin-lightning").unwrap(),
            EndpointData::new("v1"),
        );
        set_private_payments(&mut setup.sender_link, &entries_v1)
            .await
            .unwrap();

        // Second write: completely different map (onchain only).
        let onchain = MethodId::new("bitcoin-p2tr").unwrap();
        let mut entries_v2 = HashMap::new();
        entries_v2.insert(onchain.clone(), EndpointData::new("v2"));
        set_private_payments(&mut setup.sender_link, &entries_v2)
            .await
            .unwrap();

        // The helper drains queued unread updates and returns the latest map.
        let received = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(
            received.entries.get(&onchain),
            Some(&EndpointData::new("v2"))
        );

        // Backlog is drained, so a second immediate call returns empty.
        let empty = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert!(empty.entries.is_empty());

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payments_rejects_oversized_payload() {
        let mut setup = PrivateTestSetup::new().await;

        // Build a map whose serialized JSON exceeds PUBKY_NOISE_MSG_LEN (1000 bytes).
        let method = MethodId::new("bitcoin-bolt11").unwrap();
        let oversized_value = "x".repeat(1000);
        let mut entries = HashMap::new();
        entries.insert(method, EndpointData::new(oversized_value));

        let result = set_private_payments(&mut setup.sender_link, &entries).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
            "expected Validation error about size, got: {err}"
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    // ── Parallel writer/reader happy-path test ──────────────────────────

    /// Polls [`get_private_payments`] until a non-empty result is returned.
    /// Panics on timeout (10 s).
    async fn poll_private_payments(link: &mut EncryptedLink) -> SupportedPayments {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let timeout = Duration::from_secs(10);

        loop {
            assert!(
                start.elapsed() < timeout,
                "private payments poll timed out after {timeout:?}"
            );

            let result = get_private_payments(link).await.unwrap();
            if !result.entries.is_empty() {
                return result;
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

        let testnet = EphemeralTestnet::builder()
            .with_embedded_postgres()
            .build()
            .await
            .unwrap();
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
                MethodId::new("bitcoin-bolt11").unwrap(),
                EndpointData::new("lnbcpriv..."),
            );
            entries.insert(
                MethodId::new("bitcoin-p2tr").unwrap(),
                EndpointData::new("bc1priv..."),
            );
            set_private_payments(&mut link, &entries).await.unwrap();

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
            let private = poll_private_payments(&mut link).await;
            assert_eq!(
                private.entries.len(),
                2,
                "expected 2 private payment methods, got {}",
                private.entries.len()
            );
            assert_eq!(
                private
                    .entries
                    .get(&MethodId::new("bitcoin-bolt11").unwrap()),
                Some(&EndpointData::new("lnbcpriv...")),
            );
            assert_eq!(
                private.entries.get(&MethodId::new("bitcoin-p2tr").unwrap()),
                Some(&EndpointData::new("bc1priv...")),
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
        assert_eq!(bytes.len(), 189, "snapshot should be 189 bytes");

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
            MethodId::new("bitcoin-bolt11").unwrap(),
            EndpointData::new("lnrestored..."),
        );
        set_private_payments(&mut initiator_link, &entries)
            .await
            .unwrap();

        let received = get_private_payments(&mut responder_link).await.unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(
            received
                .entries
                .get(&MethodId::new("bitcoin-bolt11").unwrap()),
            Some(&EndpointData::new("lnrestored..."))
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
    async fn test_encrypted_link_snapshot_serialize_roundtrip() {
        let mut setup = PrivateTestSetup::new().await;

        // Send a message to advance nonces beyond zero.
        let mut entries = HashMap::new();
        entries.insert(
            MethodId::new("bitcoin-bolt11").unwrap(),
            EndpointData::new("ln..."),
        );
        set_private_payments(&mut setup.sender_link, &entries)
            .await
            .unwrap();

        // Take a snapshot and serialize.
        let snapshot = setup.sender_link.snapshot();
        let bytes = snapshot.serialize();
        assert_eq!(bytes.len(), 189, "snapshot should be 189 bytes");

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
            MethodId::new("bitcoin-bolt11").unwrap(),
            EndpointData::new("lnv1..."),
        );
        set_private_payments(&mut setup.sender_link, &entries_v1)
            .await
            .unwrap();

        // Consume the message on the receiver side.
        let received_v1 = get_private_payments(&mut setup.receiver_link)
            .await
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
            MethodId::new("bitcoin-p2tr").unwrap(),
            EndpointData::new("bc1pv2..."),
        );
        set_private_payments(&mut restored_sender, &entries_v2)
            .await
            .unwrap();

        // Receive on the restored receiver.
        let received_v2 = get_private_payments(&mut restored_receiver).await.unwrap();
        assert_eq!(received_v2.entries.len(), 1);
        assert_eq!(
            received_v2
                .entries
                .get(&MethodId::new("bitcoin-p2tr").unwrap()),
            Some(&EndpointData::new("bc1pv2...")),
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

    // ── parse_private_payments_json tests ───────────────────────────────

    #[test]
    fn test_parse_private_payments_json_empty_string() {
        let err = parse_private_payments_json("").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData parse error for empty string, got: {err}"
        );
    }

    // ── Malformed JSON ──────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payments_json_truncated_object() {
        let err = parse_private_payments_json("{").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for truncated JSON, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_array_instead_of_object() {
        let err = parse_private_payments_json(r#"["lightning","onchain"]"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON array, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_plain_string() {
        let err = parse_private_payments_json(r#""just a string""#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for plain JSON string, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_number() {
        let err = parse_private_payments_json("42").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON number, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_non_string_values() {
        let err =
            parse_private_payments_json(r#"{"lightning": 123, "onchain": true}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for non-string values, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_trailing_comma() {
        let err = parse_private_payments_json(r#"{"lightning": "ln...",}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for trailing comma, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_null() {
        let err = parse_private_payments_json("null").unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("failed to parse")),
            "expected InvalidData for JSON null, got: {err}"
        );
    }

    // ── Invalid method IDs inside valid JSON ────────────────────────────

    #[test]
    fn test_parse_private_payments_json_empty_key() {
        let err = parse_private_payments_json(r#"{"": "ln..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData for empty key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_path_traversal_key() {
        let err = parse_private_payments_json(r#"{"..": "ln..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData for path-traversal key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_slash_in_key() {
        let err = parse_private_payments_json(r#"{"foo/bar": "ln..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData for key with slash, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_reserved_private_key() {
        let err = parse_private_payments_json(r#"{"private": "secret..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData for reserved 'private' key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_oversized_key() {
        let long_key = "a".repeat(65);
        let json = format!(r#"{{"{long_key}": "ln..."}}"#);
        let err = parse_private_payments_json(&json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData for oversized key, got: {err}"
        );
    }

    #[test]
    fn test_parse_private_payments_json_one_valid_one_invalid_key() {
        // The valid key should not mask the invalid one.
        let err =
            parse_private_payments_json(r#"{"lightning": "ln...", "": "bc1..."}"#).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("invalid method identifier")),
            "expected InvalidData when one key is invalid, got: {err}"
        );
    }

    // ── Happy path ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_private_payments_json_valid_single_entry() {
        let result = parse_private_payments_json(r#"{"lightning": "ln..."}"#).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result.get(&MethodId::new("lightning").unwrap()),
            Some(&EndpointData::new("ln..."))
        );
    }

    #[test]
    fn test_parse_private_payments_json_valid_multiple_entries() {
        let result =
            parse_private_payments_json(r#"{"lightning": "ln...", "onchain": "bc1..."}"#).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result.get(&MethodId::new("lightning").unwrap()),
            Some(&EndpointData::new("ln..."))
        );
        assert_eq!(
            result.get(&MethodId::new("onchain").unwrap()),
            Some(&EndpointData::new("bc1..."))
        );
    }

    #[test]
    fn test_parse_private_payments_json_empty_object() {
        let result = parse_private_payments_json("{}").unwrap();
        assert!(result.is_empty());
    }
}
