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
//! - Export the standard Pubky path prefixes (see [`transport::pubky`]) to keep file layout
//!   consistent across bindings.
//!
//! For an architectural overview and example workflows, see `paykit-lib/README.md`.

use std::{collections::HashMap, fmt};

#[cfg(feature = "pubky")]
pub use pubky::PublicKey;

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
#[derive(Debug)]
pub enum PaykitError {
    /// Placeholder for unimplemented logic in the current scaffold.
    Unimplemented(&'static str),
    /// Wrapper for transport layer failures.
    ///
    /// Most user-facing failures bubble up through this variant, encapsulating
    /// lower-level SDK/network errors. Other variants are reserved for future use.
    Transport(String),
}

impl fmt::Display for PaykitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PaykitError::Unimplemented(label) => {
                write!(f, "{label} is not implemented yet")
            }
            PaykitError::Transport(msg) => write!(f, "transport error: {msg}"),
        }
    }
}

impl std::error::Error for PaykitError {}

/// Identifier for a payment method specification.
///
/// Typically based filename component stored under `/pub/paykit.app/v0/…`.
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
pub async fn set_payment_endpoint<S>(client: &S, method: MethodId, data: EndpointData) -> Result<()>
where
    S: AuthenticatedTransport,
{
    client
        .upsert_payment_endpoint(&method, &data)
        .await
        .map_err(|err| map_transport_error("set_payment_endpoint", err))
}

/// Removes a payment endpoint via the injected authenticated client.
pub async fn remove_payment_endpoint<S>(client: &S, method: MethodId) -> Result<()>
where
    S: AuthenticatedTransport,
{
    client
        .remove_payment_endpoint(&method)
        .await
        .map_err(|err| map_transport_error("remove_payment_endpoint", err))
}

/// Retrieves all supported payment methods for the given payee.
///
/// # Semantics
/// - Returns an empty map when the payee has not published any endpoints or their
///   storage directory is missing.
/// - Propagates transport failures (e.g., network errors) as `PaykitError::Transport`.
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
pub async fn get_payment_list<R>(reader: &R, payee: &PublicKey) -> Result<SupportedPayments>
where
    R: UnauthenticatedTransportRead,
{
    reader
        .fetch_supported_payments(payee)
        .await
        .map_err(|err| map_transport_error("get_payment_list", err))
}

/// Retrieves a specific payment endpoint for `payee` and `method`.
///
/// # Semantics
/// - Returns `Ok(None)` when the endpoint file is missing or empty.
/// - Returns `Err` only when the underlying transport fails (permissions, network, etc.).
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
pub async fn get_payment_endpoint<R>(
    reader: &R,
    payee: &PublicKey,
    method: &MethodId,
) -> Result<Option<EndpointData>>
where
    R: UnauthenticatedTransportRead,
{
    reader
        .fetch_payment_endpoint(payee, method)
        .await
        .map_err(|err| map_transport_error("get_payment_endpoint", err))
}

/// Returns known contacts of a given public key.
///
/// # Semantics
/// - Returns an empty vector when no contacts are stored under the follows path
///   or the directory does not exist yet.
/// - Returns `Err` only when listing fails due to a transport error.
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
pub async fn get_known_contacts<R>(reader: &R, key: &PublicKey) -> Result<Vec<PublicKey>>
where
    R: UnauthenticatedTransportRead,
{
    reader
        .fetch_known_contacts(key)
        .await
        .map_err(|err| map_transport_error("get_known_contacts", err))
}

fn map_transport_error(label: &'static str, err: PaykitError) -> PaykitError {
    match err {
        PaykitError::Transport(msg) => PaykitError::Transport(format!("{label}: {msg}")),
        _ => err,
    }
}

/// Tests
#[cfg(all(test, feature = "pubky"))]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::transport::pubky::PUBKY_FOLLOWS_PATH;
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
            let testnet = EphemeralTestnet::start().await.unwrap();
            let homeserver = testnet.homeserver();
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
}
