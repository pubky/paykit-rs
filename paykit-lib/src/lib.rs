//! Paykit library.
//!
//! `paykit-lib` is a stateless Rust SDK that focuses on the transport layer of the
//! Paykit protocol. It defines ergonomic helper types plus a pair of tiny traits that
//! callers implement (or wrap) to perform reads and writes against the routing network.
//! The crate includes first-party adapters for the Pubky SDK behind the default
//! `pubky` feature while remaining open for custom transports or mocks.
//!
//! ## Design goals
//! - Provide high-level helpers such as [`get_payment_list`] and [`set_payment_endpoint`]
//!   that work with any type implementing [`UnauthenticatedTransportRead`] or
//!   [`AuthenticatedTransport`].
//! - Keep storage/session management outside of the crate so integrators can inject their
//!   own security model, capability scoping, caching, or telemetry.
//! - Export the standard Pubky path prefixes (e.g. `/pub/paykit.app/v0/`) to keep file layout
//!   consistent across bindings.
//!
//! For an architectural overview and example workflows, see `paykit-lib/README.md`.

use std::collections::HashMap;
#[cfg(not(feature = "pubky"))]
use std::fmt;

use thiserror::Error;
use tracing::{debug, instrument};

#[cfg(feature = "pubky")]
pub use pubky::PublicKey;

#[cfg(feature = "pubky")]
pub use pubky_app_specs::PubkyAppUser as Profile;

#[cfg(feature = "pubky")]
pub use pubky_data;

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

#[cfg(feature = "pubky")]
/// Re-export pubky sdk to allow for non accounted usecases on transport level
pub use pubky;

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
    /// Returned when a profile or other resource is not found (404/GONE).
    /// Distinct from [`PaykitError::Profile`] which indicates the data exists but is malformed.
    #[error("not found: {0}")]
    NotFound(String),

    /// Retrieved data is corrupt or structurally invalid.
    ///
    /// Returned when a resource was successfully fetched from the network but its
    /// content cannot be interpreted — for example invalid UTF-8 bytes, an
    /// unparseable resource path, or a contact entry that is not a valid public
    /// key. This is distinct from [`PaykitError::Transport`] (the network call
    /// itself failed) and [`PaykitError::Profile`] (profile-specific parse
    /// errors).
    #[error("invalid data: {context}")]
    InvalidData {
        /// Human-readable description of the data problem.
        context: String,
        /// The underlying error, when available.
        #[source]
        source: Option<anyhow::Error>,
    },

    /// Profile data is malformed or invalid.
    ///
    /// Returned when profile data exists but cannot be parsed or validated.
    /// Distinct from [`PaykitError::NotFound`] which indicates the resource doesn't exist,
    /// and [`PaykitError::Transport`] which covers network/SDK errors.
    #[error("profile error: {0}")]
    Profile(String),

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
/// A `MethodId` is a single, safe path segment stored under `/pub/paykit.app/v0/…`.
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

impl MethodId {
    /// Create a new `MethodId` after validating the identifier.
    ///
    /// Returns `Err(PaykitError::Validation)` if the value is empty, too long,
    /// contains forbidden characters, or resembles a path-traversal component.
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
/// The link wraps a [`pubky_data::PubkyDataEncryptor`] in transport mode.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    encryptor: pubky_data::PubkyDataEncryptor,
    /// The counterparty's public key.
    recipient: PublicKey,
}

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
pub struct EncryptedLinkHandshake {
    /// The Noise session manager in handshake mode.
    encryptor: pubky_data::PubkyDataEncryptor,
    /// The counterparty's public key (used for homeserver path construction).
    remote_pubkey: PublicKey,
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
/// Uses [`pubky_data::path_derivation::derive_asymmetric_paths`] to derive
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
/// - `compute_private_paths(alice_sk, bob_pk).write == compute_private_paths(bob_sk, alice_pk).read`
/// - `compute_private_paths(alice_sk, bob_pk).read == compute_private_paths(bob_sk, alice_pk).write`
fn compute_private_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
) -> (String, String) {
    pubky_data::path_derivation::derive_asymmetric_paths(
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

/// Stores or updates a payment endpoint via the injected authenticated client.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, MethodId, EndpointData, PublicKey};
/// # use paykit_lib::AuthenticatedTransport;
/// # async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
/// let method = MethodId::new("lightning")?;
/// let data = EndpointData::new("{\"bolt11\":\"ln...\"}");
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
/// [`pubky_data::PubkyDataEncryptor::send_message`], and pubky-data handles
/// file naming and storage location on the homeserver.
///
/// # Payload size
///
/// The serialized JSON must fit within a single pubky-data message
/// (`PUBKY_DATA_MSG_LEN`, currently 1000 bytes). Exceeding this limit
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
/// - Returns [`PaykitError::Transport`] if `send_message` fails.
#[instrument(skip(link, entries), fields(recipient = %link.recipient, count = entries.len()))]
pub async fn set_private_payments(
    link: &mut EncryptedLink,
    entries: &HashMap<MethodId, EndpointData>,
) -> Result<()> {
    debug!("sending private payments map");

    let json = serialize_private_payments_json(entries)
        .map_err(|err| map_error("set_private_payments", err))?;

    let plaintext = json.into_bytes();

    if plaintext.len() > pubky_data::snow_crypto::PUBKY_DATA_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "private payments payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_data::snow_crypto::PUBKY_DATA_MSG_LEN,
        )));
    }

    let success = link.encryptor.send_message(plaintext).await;

    if !success {
        return Err(PaykitError::Transport {
            context: "failed to send private payments via encrypted link".into(),
            source: anyhow::anyhow!("pubky-data send_message returned false"),
        });
    }

    debug!("private payments map sent successfully");
    Ok(())
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
#[instrument(skip(reader), fields(payee = %payee))]
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
/// - Returns `Err(PaykitError::InvalidData)` when the decrypted payload
///   is not valid UTF-8 or cannot be parsed as a payments JSON map.
/// - Returns `Err(PaykitError::Transport)` for decryption or I/O failures.
#[instrument(skip(link), fields(recipient = %link.recipient))]
pub async fn get_private_payments(link: &mut EncryptedLink) -> Result<SupportedPayments> {
    debug!("receiving private payments map");

    let messages = link.encryptor.receive_message().await;

    if messages.is_empty() {
        debug!("no private payments messages available, returning empty map");
        return Ok(SupportedPayments::default());
    }

    // Take the last message (latest state of the payments map).
    let raw = &messages[messages.len() - 1];

    // Trim trailing zero-padding added by pubky-data's fixed-size buffers.
    let end = raw.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let plaintext = std::str::from_utf8(&raw[..end]).map_err(|err| PaykitError::InvalidData {
        context: format!("private payments plaintext is not valid UTF-8: {err}"),
        source: Some(err.into()),
    })?;

    let entries = parse_private_payments_json(plaintext)?;
    debug!(count = entries.len(), "private payments map received");
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
#[instrument(skip(reader), fields(payee = %payee, method = %method))]
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

/// Returns known contacts of a given public key.
///
/// # Semantics
/// - Returns an empty vector when no contacts are stored under the follows path
///   or the directory does not exist yet.
/// - Returns `Err(PaykitError::InvalidData)` when a contact entry cannot be parsed
///   as a valid [`PublicKey`].
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # use paykit_lib::{get_known_contacts, PublicKey};
/// # use paykit_lib::UnauthenticatedTransportRead;
/// # async fn contacts(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
/// for contact in get_known_contacts(reader, pk).await? {
///     println!("known contact: {}", contact);
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader), fields(owner = %key))]
pub async fn get_known_contacts<R>(reader: &R, key: &PublicKey) -> Result<Vec<PublicKey>>
where
    R: UnauthenticatedTransportRead,
{
    debug!("fetching known contacts");
    let result = reader
        .fetch_known_contacts(key)
        .await
        .map_err(|err| map_error("get_known_contacts", err))?;
    debug!(count = result.len(), "known contacts retrieved");
    Ok(result)
}

/// Returns the profile of a given public key.
///
/// # Semantics
/// - Returns `Ok(Profile)` when the profile exists and is valid.
/// - Returns `Err(PaykitError::NotFound)` if the profile does not exist.
/// - Returns `Err(PaykitError::Profile)` if the profile exists but is malformed.
/// - Returns `Err(PaykitError::Transport)` for network or transport-layer failures.
///
/// # Examples
/// ```
/// # use paykit_lib::{get_profile, PublicKey, Profile};
/// # use paykit_lib::UnauthenticatedTransportRead;
/// # async fn demo(reader: &impl UnauthenticatedTransportRead, pk: &PublicKey) -> paykit_lib::Result<()> {
/// let profile = get_profile(reader, pk).await?;
/// println!("user name: {}", profile.name);
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader), fields(user = %key))]
pub async fn get_profile<R>(reader: &R, key: &PublicKey) -> Result<Profile>
where
    R: UnauthenticatedTransportRead,
{
    debug!("fetching user profile");
    reader
        .fetch_profile(key)
        .await
        .map_err(|err| map_error("get_profile", err))
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
#[instrument(
    skip(session, sender_secret_key, outbox_client),
    fields(receiver = %receiver_pubkey)
)]
pub fn initiate_encrypted_link(
    session: pubky::PubkySession,
    sender_secret_key: [u8; 32],
    receiver_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing encrypted link handshake (initiator)");

    let (write_path, read_path) = compute_private_paths(&sender_secret_key, receiver_pubkey);

    let config = pubky_data::PubkyDataConfig::new_with_paths(
        sender_secret_key,
        0,
        "XX".to_string(),
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-data PubkyDataConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_data::PubkyDataEncryptor::new(
        config,
        sender_secret_key,
        receiver_pubkey.clone(),
        true,
        receiver_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-data PubkyDataEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (initiator)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: receiver_pubkey.clone(),
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
#[instrument(
    skip(session, receiver_secret_key, outbox_client),
    fields(sender = %sender_pubkey)
)]
pub fn accept_encrypted_link(
    session: pubky::PubkySession,
    receiver_secret_key: [u8; 32],
    sender_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing encrypted link handshake (responder)");

    let (write_path, read_path) = compute_private_paths(&receiver_secret_key, sender_pubkey);

    let config = pubky_data::PubkyDataConfig::new_with_paths(
        receiver_secret_key,
        0,
        "XX".to_string(),
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-data PubkyDataConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_data::PubkyDataEncryptor::new(
        config,
        receiver_secret_key,
        sender_pubkey.clone(),
        false,
        sender_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-data PubkyDataEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (responder)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: sender_pubkey.clone(),
    })
}

#[cfg(feature = "pubky")]
/// Advances the handshake by one step.
///
/// This function is **polling-safe**: calling it when the remote peer has not
/// written their next message yet returns [`HandshakeProgress::Pending`] without
/// corrupting internal state. The caller can safely retry after a delay.
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
/// Returns [`PaykitError::Transport`] if the handshake processing fails or if
/// the context is in an invalid state.
#[instrument(skip(handshake), fields(remote = %handshake.remote_pubkey))]
pub async fn advance_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    // Check whether the handshake has already finished.
    match handshake.encryptor.is_handshake() {
        Ok(()) => {
            // Still in handshake phase — drive it forward.
            debug!("advancing handshake step");
        }
        Err(pubky_data::PubkyDataError::IsTransport) => {
            // Handshake already finished — transition to transport.
            debug!("handshake complete, transitioning to transport");
            return finish_handshake(handshake);
        }
        Err(err) => {
            return Err(PaykitError::Transport {
                context: format!("handshake context error: {err:?}"),
                source: anyhow::anyhow!("pubky-data is_handshake failed: {err:?}"),
            });
        }
    }

    // Process the next handshake step.
    match handshake.encryptor.handle_handshake().await {
        Ok(pubky_data::HandshakeResult::Pending) => {
            debug!("handshake step pending (waiting for peer)");
            Ok(HandshakeProgress::Pending(handshake))
        }
        Ok(pubky_data::HandshakeResult::Terminal) => {
            debug!("handshake terminal, transitioning to transport");
            finish_handshake(handshake)
        }
        Err(err) => Err(PaykitError::Transport {
            context: format!("handshake step failed: {err:?}"),
            source: anyhow::anyhow!("pubky-data handle_handshake failed: {err:?}"),
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
                source: anyhow::anyhow!("pubky-data transition_transport failed: {err:?}"),
            })?;

    debug!("encrypted link established");
    Ok(HandshakeProgress::Complete(EncryptedLink {
        encryptor: handshake.encryptor,
        recipient: handshake.remote_pubkey,
    }))
}

#[cfg(feature = "pubky")]
/// Closes an encrypted link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link), fields(recipient = %link.recipient))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing encrypted link");
    link.encryptor.close();
    debug!("encrypted link closed successfully");
    Ok(())
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
        PaykitError::Profile(msg) => PaykitError::Profile(format!("{label}: {msg}")),
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
        for name in ["lightning", "onchain", "bolt11", "lnurl-pay"] {
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

    // ── EndpointData: basic accessors ───────────────────────────────────

    #[test]
    fn test_endpoint_data_new_and_accessors() {
        let d = EndpointData::new("{\"bolt11\":\"ln...\"}");
        assert_eq!(d.as_str(), "{\"bolt11\":\"ln...\"}");
        assert_eq!(format!("{d}"), "{\"bolt11\":\"ln...\"}");
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
    use crate::transport::pubky::{PUBKY_FOLLOWS_PATH, PUBKY_PROFILE_FILE};
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
            let testnet = EphemeralTestnet::builder().build().await.unwrap();

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
        let method = MethodId::new("bolt11").unwrap();

        let missing = get_payment_endpoint(&setup.reader_transport, &setup.public_key, &method)
            .await
            .unwrap();
        assert!(missing.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_additions_and_removals() {
        let setup = TestSetup::new().await;

        let onchain = MethodId::new("onchain").unwrap();
        let lightning = MethodId::new("lightning").unwrap();
        let onchain_data = EndpointData::new("{\"address\":\"bc1...\"}");
        let lightning_data = EndpointData::new("{\"bolt11\":\"ln...\"}");

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

    #[tokio::test]
    async fn lists_known_contacts() {
        let setup = TestSetup::new().await;

        let contacts = get_known_contacts(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();
        assert!(contacts.is_empty());

        // Seed two contacts under the follows path using the authenticated session.
        let contact_a = Keypair::random().public_key();
        let contact_b = Keypair::random().public_key();
        setup
            .raw_session
            .storage()
            .put(format!("{PUBKY_FOLLOWS_PATH}{}", contact_a), "")
            .await
            .unwrap();
        setup
            .raw_session
            .storage()
            .put(format!("{PUBKY_FOLLOWS_PATH}{}", contact_b), "")
            .await
            .unwrap();

        let contacts = get_known_contacts(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();

        assert!(contacts.contains(&contact_a));
        assert!(contacts.contains(&contact_b));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn known_contacts_skips_invalid_entries() {
        let setup = TestSetup::new().await;

        // Seed one valid contact.
        let valid_contact = Keypair::random().public_key();
        setup
            .raw_session
            .storage()
            .put(format!("{PUBKY_FOLLOWS_PATH}{}", valid_contact), "")
            .await
            .unwrap();

        // Seed an entry that cannot be parsed as a PublicKey.
        setup
            .raw_session
            .storage()
            .put(format!("{PUBKY_FOLLOWS_PATH}not-a-valid-public-key"), "")
            .await
            .unwrap();

        // fetch_known_contacts should succeed, returning only the valid contact.
        let contacts = get_known_contacts(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();

        assert_eq!(contacts.len(), 1, "invalid entry should be skipped");
        assert!(contacts.contains(&valid_contact));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_fetch_profile_success() {
        let setup = TestSetup::new().await;

        // Seed a valid profile using raw session
        let profile_json =
            r#"{"name":"Alice","bio":"Hello world","image":null,"links":null,"status":"online"}"#;
        setup
            .raw_session
            .storage()
            .put(PUBKY_PROFILE_FILE, profile_json)
            .await
            .unwrap();

        let profile = get_profile(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();

        assert_eq!(profile.name, "Alice");
        assert_eq!(profile.bio, Some("Hello world".into()));
        assert_eq!(profile.status, Some("online".into()));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_fetch_profile_not_found() {
        let setup = TestSetup::new().await;

        let result = get_profile(&setup.reader_transport, &setup.public_key).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, PaykitError::NotFound(msg) if msg.contains("not found")));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_fetch_profile_invalid_json() {
        let setup = TestSetup::new().await;

        // Seed malformed JSON
        setup
            .raw_session
            .storage()
            .put(PUBKY_PROFILE_FILE, "not valid json {{{")
            .await
            .unwrap();

        let result = get_profile(&setup.reader_transport, &setup.public_key).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, PaykitError::Profile(msg) if msg.contains("parse")));

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn test_fetch_profile_minimal() {
        let setup = TestSetup::new().await;

        let profile_json = r#"{"name":"Bob"}"#;
        setup
            .raw_session
            .storage()
            .put(PUBKY_PROFILE_FILE, profile_json)
            .await
            .unwrap();

        let profile = get_profile(&setup.reader_transport, &setup.public_key)
            .await
            .unwrap();

        assert_eq!(profile.name, "Bob");
        assert!(profile.bio.is_none());
        assert!(profile.image.is_none());
        assert!(profile.links.is_none());
        assert!(profile.status.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    // ── Private payments test infrastructure ────────────────────────────

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
            let testnet = EphemeralTestnet::builder().build().await.unwrap();
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

        let method = MethodId::new("lightning").unwrap();
        let data = EndpointData::new("{\"bolt11\":\"lnbc1...\"}");
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

        let lightning = MethodId::new("lightning").unwrap();
        let onchain = MethodId::new("onchain").unwrap();
        let cashu = MethodId::new("cashu").unwrap();

        let mut entries = HashMap::new();
        entries.insert(
            lightning.clone(),
            EndpointData::new("{\"bolt11\":\"ln...\"}"),
        );
        entries.insert(
            onchain.clone(),
            EndpointData::new("{\"address\":\"bc1...\"}"),
        );
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
            Some(&EndpointData::new("{\"bolt11\":\"ln...\"}"))
        );
        assert_eq!(
            received.entries.get(&onchain),
            Some(&EndpointData::new("{\"address\":\"bc1...\"}"))
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
        entries_v1.insert(MethodId::new("lightning").unwrap(), EndpointData::new("v1"));
        set_private_payments(&mut setup.sender_link, &entries_v1)
            .await
            .unwrap();

        // Second write: completely different map (onchain only).
        let onchain = MethodId::new("onchain").unwrap();
        let mut entries_v2 = HashMap::new();
        entries_v2.insert(onchain.clone(), EndpointData::new("v2"));
        set_private_payments(&mut setup.sender_link, &entries_v2)
            .await
            .unwrap();

        // pubky-data's receive_message reads one slot per call (counter-based).
        // The first call consumes v1 (slot N), the second reads v2 (slot N+1).
        let _v1 = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();

        // Second call should yield the latest map.
        let received = get_private_payments(&mut setup.receiver_link)
            .await
            .unwrap();
        assert_eq!(received.entries.len(), 1);
        assert_eq!(
            received.entries.get(&onchain),
            Some(&EndpointData::new("v2"))
        );

        setup.sender_session.signout().await.unwrap();
        setup.receiver_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn private_payments_rejects_oversized_payload() {
        let mut setup = PrivateTestSetup::new().await;

        // Build a map whose serialized JSON exceeds PUBKY_DATA_MSG_LEN (1000 bytes).
        let method = MethodId::new("lightning").unwrap();
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

        let testnet = EphemeralTestnet::builder().build().await.unwrap();
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
                MethodId::new("lightning").unwrap(),
                EndpointData::new("{\"bolt11\":\"lnbc_priv...\"}"),
            );
            entries.insert(
                MethodId::new("onchain").unwrap(),
                EndpointData::new("{\"address\":\"bc1_priv...\"}"),
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
                private.entries.get(&MethodId::new("lightning").unwrap()),
                Some(&EndpointData::new("{\"bolt11\":\"lnbc_priv...\"}")),
            );
            assert_eq!(
                private.entries.get(&MethodId::new("onchain").unwrap()),
                Some(&EndpointData::new("{\"address\":\"bc1_priv...\"}")),
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
}
