//! Core transport traits that decouple Paykit logic from specific SDKs or backends.
//!
//! # Timeout handling
//!
//! These traits intentionally do **not** enforce timeouts. Each transport
//! implementation is responsible for configuring appropriate timeout behaviour
//! at its own layer. For example, the Pubky SDK exposes
//! [`PubkyHttpClientBuilder::request_timeout`][pubky-timeout] which governs
//! all HTTP requests made through the client.
//!
//! [pubky-timeout]: https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout

use async_trait::async_trait;

use crate::{EndpointData, MethodId, Profile, PublicKey, Result};

/// Trait describing read-only access to public Paykit transport.
///
/// # Timeout handling
///
/// Implementors are responsible for enforcing their own timeouts. Paykit does
/// not wrap calls with any deadline — a slow or unresponsive backend will block
/// the caller indefinitely unless the underlying transport layer applies a
/// timeout. For the Pubky adapter the SDK exposes
/// [`PubkyHttpClientBuilder::request_timeout`][pubky-timeout] for this purpose.
///
/// [pubky-timeout]: https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout
#[async_trait]
pub trait UnauthenticatedTransportRead {
    /// Fetches the raw Supported Payments List for the provided `payee`.
    ///
    /// # Consistency
    ///
    /// This method first lists available payment method entries and then fetches
    /// each one individually. Because the underlying transport does not support
    /// atomic/transactional reads, a **race condition** exists: between the
    /// directory listing and the individual fetches, endpoints may be added,
    /// removed, or modified by the payee. The returned [`SupportedPayments`] is
    /// therefore a **best-effort snapshot** and may be inconsistent.
    ///
    /// ## Recommended caller strategy
    ///
    /// If a payment execution fails with an error that suggests the endpoint
    /// has already been consumed or is no longer valid (evidence of a race
    /// condition), callers should:
    ///
    /// 1. Re-fetch the specific endpoint via
    ///    [`fetch_payment_endpoint`](Self::fetch_payment_endpoint).
    /// 2. Compare the newly retrieved [`EndpointData`] with the value used in
    ///    the failed attempt.
    /// 3. If the endpoint data differs, it is safe to retry the payment with
    ///    the updated value.
    ///
    /// [`SupportedPayments`]: crate::SupportedPayments
    async fn fetch_supported_payments(&self, payee: &PublicKey)
        -> Result<crate::SupportedPayments>;

    /// Fetches an individual payment endpoint document if it exists.
    async fn fetch_payment_endpoint(
        &self,
        payee: &PublicKey,
        method: &MethodId,
    ) -> Result<Option<EndpointData>>;

    /// Returns the set of known contacts (public keys) reachable to the caller.
    async fn fetch_known_contacts(&self, owner: &PublicKey) -> Result<Vec<PublicKey>>;

    #[cfg(feature = "pubky")]
    /// Returns the profile of the given user.
    ///
    /// # Errors
    /// - Returns `PaykitError::NotFound` if the profile does not exist.
    /// - Returns `PaykitError::Profile` if the profile exists but cannot be parsed.
    /// - Returns `PaykitError::Transport` for network failures.
    async fn fetch_profile(&self, user: &PublicKey) -> Result<Profile>;
}

/// Trait describing authenticated write (and optional read) access.
///
/// # Timeout handling
///
/// Implementors are responsible for enforcing their own timeouts. Paykit does
/// not wrap calls with any deadline — a slow or unresponsive backend will block
/// the caller indefinitely unless the underlying transport layer applies a
/// timeout. For the Pubky adapter the SDK exposes
/// [`PubkyHttpClientBuilder::request_timeout`][pubky-timeout] for this purpose.
///
/// [pubky-timeout]: https://docs.rs/pubky/latest/pubky/struct.PubkyHttpClientBuilder.html#method.request_timeout
#[async_trait]
pub trait AuthenticatedTransport {
    /// Writes or updates a payment endpoint document.
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()>;

    /// Removes an existing payment endpoint for the provided method.
    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()>;
}
