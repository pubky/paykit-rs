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
}

/// Identifier for a payment method specification.
///
/// Typically a path-based filename component stored under `/pub/paykit.app/v0/…`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MethodId(pub String);

/// Serialized payload served by a payment endpoint (UTF-8 text such as JSON, lnurl, etc.).
///
/// If you need to transmit binary payloads, encode them (e.g., base64) before wrapping
/// in `EndpointData`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointData(pub String);

/// Collection of supported payment entries keyed by method identifiers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupportedPayments {
    /// Map of `MethodId` to endpoint data.
    pub entries: HashMap<MethodId, EndpointData>,
}

/// Stores or updates a payment endpoint via the injected authenticated client.
///
/// # Examples
/// ```
/// # use paykit_lib::{set_payment_endpoint, MethodId, EndpointData, PublicKey};
/// # use paykit_lib::AuthenticatedTransport;
/// # async fn demo(client: &impl AuthenticatedTransport) -> paykit_lib::Result<()> {
/// let method = MethodId("lightning".into());
/// let data = EndpointData("{\"bolt11\":\"ln...\"}".into());
/// set_payment_endpoint(client, method, data).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip(client, data), fields(method = %method.0))]
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

/// Removes a payment endpoint via the injected authenticated client.
#[instrument(skip(client), fields(method = %method.0))]
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
///         println!("method={} payload={}", method.0, data.0);
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
/// let lightning = MethodId("lightning".into());
/// if let Some(endpoint) = get_payment_endpoint(reader, pk, &lightning).await? {
///     println!("lightning endpoint: {}", endpoint.0);
/// } else {
///     println!("no lightning endpoint published");
/// }
/// # Ok(())
/// # }
/// ```
#[instrument(skip(reader), fields(payee = %payee, method = %method.0))]
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
    }
}

/// Tests
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

        let method = MethodId("onchain".into());
        let endpoint = EndpointData("{\"address\":\"bc1...\"}".into());

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

        let new_endpoint = EndpointData("{\"address\":\"1c1...\"}".into());
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
        let method = MethodId("bolt11".into());

        let missing = get_payment_endpoint(&setup.reader_transport, &setup.public_key, &method)
            .await
            .unwrap();
        assert!(missing.is_none());

        setup.raw_session.signout().await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_additions_and_removals() {
        let setup = TestSetup::new().await;

        let onchain = MethodId("onchain".into());
        let lightning = MethodId("lightning".into());
        let onchain_data = EndpointData("{\"address\":\"bc1...\"}".into());
        let lightning_data = EndpointData("{\"bolt11\":\"ln...\"}".into());

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
        let method = MethodId("unused".into());

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
