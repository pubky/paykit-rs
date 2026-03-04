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
/// Created by [`establish_encrypted_link`] after a successful Noise handshake.
/// Used by the private payment helper functions to encrypt and decrypt payment
/// data. Must be closed via [`close_encrypted_link`] when no longer needed.
///
/// The link wraps a [`pubky_data::PubkyDataEncryptor`] in transport mode and
/// the [`pubky_data::LinkId`] that identifies the session.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    encryptor: pubky_data::PubkyDataEncryptor,
    /// Identifier for the established transport session.
    link_id: pubky_data::LinkId,
    /// The counterparty's public key.
    recipient: PublicKey,
}

#[cfg(feature = "pubky")]
/// Computes the path component used to address a recipient's private payments
/// directory.
///
/// Currently returns the string representation of the counterparty's public key.
/// This will be replaced with a derivation function in the future.
fn compute_remote_path_component(receiver_pubkey: &PublicKey) -> String {
    // TODO: will do something like SHA(DH(sender_secret_key, receiver_pubkey)) in the future instead of raw pubkey
    receiver_pubkey.to_string()
}

#[cfg(feature = "pubky")]
/// Computes the path component used to address a sender's private payments
/// directory.
///
/// Currently returns the string representation of the our's public key.
/// This will be replaced with a derivation function in the future.
fn compute_local_path_component(sender_pubkey: &PublicKey) -> String {
    // TODO: will do something like SHA(DH(sender_secret_key, receiver_pubkey)) in the future instead of raw pubkey
    sender_pubkey.to_string()
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
/// Stores or updates a private payment endpoint for a specific method.
///
/// Performs a read-decrypt-modify-encrypt-write cycle: fetches the existing
/// encrypted payments blob (if any), decrypts it, inserts or updates the
/// given method entry, re-encrypts, and writes the blob back.
///
/// # Parameters
/// - `client` — authenticated transport for writing the encrypted blob.
/// - `reader` — unauthenticated transport for reading the existing blob.
/// - `link` — an established [`EncryptedLink`] for encrypt/decrypt operations.
/// - `method` — the payment method identifier to store.
/// - `data` — the endpoint payload to associate with the method.
///
/// # Errors
/// - Returns `PaykitError::Transport` for network failures.
/// - Returns `PaykitError::InvalidData` if the existing blob cannot be
///   decrypted or parsed.
#[instrument(skip(client, reader, link, data), fields(method = %method, recipient = %link.recipient))]
pub async fn set_private_payment_endpoint(
    client: &PubkyAuthenticatedTransport,
    reader: &PubkyUnauthenticatedTransport,
    link: &mut EncryptedLink,
    method: MethodId,
    data: EndpointData,
) -> Result<()> {
    debug!("storing private payment endpoint");
    let path_component = compute_local_path_component(&link.recipient);

    // Read existing blob, decrypt, and parse (or start with empty map).
    let mut entries = match reader
        .fetch_private_payments_blob(&link.recipient, &path_component)
        .await
        .map_err(|err| map_error("set_private_payment_endpoint", err))?
    {
        Some(_blob) => {
            // TODO: decrypt blob using link.encryptor / link.link_id
            // let plaintext = decrypt(&link, blob)?;
            // parse_private_payments_json(&plaintext)?
            todo!("decrypt private payments blob using pubky-data EncryptedLink")
        }
        None => HashMap::new(),
    };

    entries.insert(method, data);

    let _json = serialize_private_payments_json(&entries)
        .map_err(|err| map_error("set_private_payment_endpoint", err))?;

    // TODO: encrypt json using link.encryptor / link.link_id
    // let encrypted = encrypt(&link, json.as_bytes())?;
    let encrypted: Vec<u8> = todo!("encrypt private payments blob using pubky-data EncryptedLink");

    client
        .put_private_payments(&path_component, &encrypted)
        .await
        .map_err(|err| map_error("set_private_payment_endpoint", err))?;

    debug!("private payment endpoint stored successfully");
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

#[cfg(feature = "pubky")]
/// Removes a private payment endpoint for a specific method.
///
/// Performs a read-decrypt-modify-encrypt-write cycle: fetches the existing
/// encrypted payments blob, decrypts it, removes the given method entry,
/// re-encrypts, and writes the blob back. If the resulting map is empty,
/// the entire private payments file is removed.
///
/// # Parameters
/// - `client` — authenticated transport for writing/deleting the encrypted blob.
/// - `reader` — unauthenticated transport for reading the existing blob.
/// - `link` — an established [`EncryptedLink`] for encrypt/decrypt operations.
/// - `method` — the payment method identifier to remove.
///
/// # Errors
/// - Returns `PaykitError::NotFound` if no private payments blob exists.
/// - Returns `PaykitError::Transport` for network failures.
/// - Returns `PaykitError::InvalidData` if the existing blob cannot be
///   decrypted or parsed.
#[instrument(skip(client, reader, link), fields(method = %method, recipient = %link.recipient))]
pub async fn remove_private_payment_endpoint(
    client: &PubkyAuthenticatedTransport,
    reader: &PubkyUnauthenticatedTransport,
    link: &mut EncryptedLink,
    method: MethodId,
) -> Result<()> {
    debug!("removing private payment endpoint");
    let path_component = compute_local_path_component(&link.recipient);

    let _blob = reader
        .fetch_private_payments_blob(&link.recipient, &path_component)
        .await
        .map_err(|err| map_error("remove_private_payment_endpoint", err))?
        .ok_or_else(|| {
            PaykitError::NotFound("no private payments blob exists for this recipient".into())
        })?;

    // TODO: decrypt blob using link.encryptor / link.link_id
    // let plaintext = decrypt(&link, blob)?;
    // let mut entries = parse_private_payments_json(&plaintext)?;
    let mut entries: HashMap<MethodId, EndpointData> =
        todo!("decrypt private payments blob using pubky-data EncryptedLink");

    entries.remove(&method);

    if entries.is_empty() {
        client
            .remove_private_payments(&path_component)
            .await
            .map_err(|err| map_error("remove_private_payment_endpoint", err))?;
    } else {
        let _json = serialize_private_payments_json(&entries)
            .map_err(|err| map_error("remove_private_payment_endpoint", err))?;

        // TODO: encrypt json using link.encryptor / link.link_id
        // let encrypted = encrypt(&link, json.as_bytes())?;
        let encrypted: Vec<u8> =
            todo!("encrypt private payments blob using pubky-data EncryptedLink");

        client
            .put_private_payments(&path_component, &encrypted)
            .await
            .map_err(|err| map_error("remove_private_payment_endpoint", err))?;
    }

    debug!("private payment endpoint removed successfully");
    Ok(())
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
/// Retrieves the full private payment list for a given payee.
///
/// Fetches the encrypted payments blob, decrypts it using the established
/// link, and returns all method/endpoint pairs.
///
/// # Parameters
/// - `reader` — unauthenticated transport for reading the encrypted blob.
/// - `link` — an established [`EncryptedLink`] for decryption.
/// - `payee` — the public key of the payee whose private payments to fetch.
///
/// # Semantics
/// - Returns an empty [`SupportedPayments`] when no private payments blob
///   exists for this recipient.
/// - Returns `Err(PaykitError::InvalidData)` when the blob cannot be
///   decrypted or parsed.
/// - Returns `Err(PaykitError::Transport)` for network failures.
#[instrument(skip(reader, link), fields(payee = %payee, recipient = %link.recipient))]
pub async fn get_private_payment_list(
    reader: &PubkyUnauthenticatedTransport,
    link: &mut EncryptedLink,
    payee: &PublicKey,
) -> Result<SupportedPayments> {
    debug!("fetching private payment list");
    let path_component = compute_remote_path_component(&link.recipient);

    let blob = match reader
        .fetch_private_payments_blob(payee, &path_component)
        .await
        .map_err(|err| map_error("get_private_payment_list", err))?
    {
        Some(blob) => blob,
        None => {
            debug!("no private payments blob found, returning empty list");
            return Ok(SupportedPayments::default());
        }
    };

    // TODO: decrypt blob using link.encryptor / link.link_id
    // let plaintext = decrypt(&link, blob)?;
    let _blob = blob;
    let entries: HashMap<MethodId, EndpointData> =
        todo!("decrypt private payments blob using pubky-data EncryptedLink");

    debug!(count = entries.len(), "private payment list retrieved");
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

#[cfg(feature = "pubky")]
/// Retrieves a specific private payment endpoint for a given payee and method.
///
/// Fetches the encrypted payments blob, decrypts it using the established
/// link, and extracts the endpoint for the requested method.
///
/// # Parameters
/// - `reader` — unauthenticated transport for reading the encrypted blob.
/// - `link` — an established [`EncryptedLink`] for decryption.
/// - `payee` — the public key of the payee whose private endpoint to fetch.
/// - `method` — the payment method identifier to look up.
///
/// # Semantics
/// - Returns `Ok(None)` when no private payments blob exists or the blob
///   does not contain the requested method.
/// - Returns `Err(PaykitError::InvalidData)` when the blob cannot be
///   decrypted or parsed.
/// - Returns `Err(PaykitError::Transport)` for network failures.
#[instrument(skip(reader, link), fields(payee = %payee, method = %method, recipient = %link.recipient))]
pub async fn get_private_payment_endpoint(
    reader: &PubkyUnauthenticatedTransport,
    link: &mut EncryptedLink,
    payee: &PublicKey,
    method: &MethodId,
) -> Result<Option<EndpointData>> {
    debug!("fetching private payment endpoint");
    let path_component = compute_remote_path_component(&link.recipient);

    let blob = match reader
        .fetch_private_payments_blob(payee, &path_component)
        .await
        .map_err(|err| map_error("get_private_payment_endpoint", err))?
    {
        Some(blob) => blob,
        None => {
            debug!("no private payments blob found");
            return Ok(None);
        }
    };

    // TODO: decrypt blob using link.encryptor / link.link_id
    // let plaintext = decrypt(&link, blob)?;
    let _blob = blob;
    let entries: HashMap<MethodId, EndpointData> =
        todo!("decrypt private payments blob using pubky-data EncryptedLink");

    let result = entries.get(method).cloned();
    debug!(
        found = result.is_some(),
        "private payment endpoint lookup complete"
    );
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
/// Establishes an encrypted Noise link with a remote peer.
///
/// Drives a full Noise handshake (currently XX pattern) to completion using
/// `pubky-data`. The handshake messages are exchanged via the Pubky homeserver
/// (managed internally by `pubky-data`). Once the handshake completes, the
/// returned [`EncryptedLink`] can be used with the private payment helper
/// functions to encrypt and decrypt payment data.
///
/// # Parameters
/// - `session` — an authenticated Pubky session for writing handshake messages.
/// - `sender_secret_key` — the 32-byte Ed25519 secret key of the local peer.
/// - `receiver_pubkey` — the public key of the remote peer to establish a link with.
///
/// # Errors
/// - Returns `PaykitError::Transport` if the handshake fails due to network
///   issues or if `pubky-data` cannot initialize the encryption stack.
#[instrument(skip(session, sender_secret_key), fields(receiver = %receiver_pubkey))]
pub async fn establish_encrypted_link(
    session: &pubky::PubkySession,
    sender_secret_key: &[u8; 32],
    receiver_pubkey: &PublicKey,
) -> Result<EncryptedLink> {
    debug!("establishing encrypted link");
    // TODO: Initialize PubkyDataEncryptor, create context, drive handshake,
    // transition to transport mode, and return EncryptedLink.
    //
    // Rough sequence:
    // 1. PubkyDataEncryptor::init_encryptor_stack(...)
    // 2. encryptor.init_context(key_set, initiator=true, endpoint_pubkey)
    // 3. loop { encryptor.handle_handshake(...) } until Terminal
    // 4. link_id = encryptor.transition_transport(tmp_link_id)
    // 5. return EncryptedLink { encryptor, link_id, recipient }
    let _ = (session, sender_secret_key, receiver_pubkey);
    todo!("establish encrypted link using pubky-data Noise handshake")
}

#[cfg(feature = "pubky")]
/// Closes an encrypted link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link), fields(recipient = %link.recipient))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing encrypted link");
    link.encryptor
        .close_context(&link.link_id)
        .map_err(|err| PaykitError::Transport {
            context: format!("failed to close encrypted link: {err:?}"),
            source: anyhow::anyhow!("pubky-data close_context failed: {err:?}"),
        })?;
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
}
