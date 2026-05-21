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
//!
//! # Public vs. private payment data
//!
//! ## Public Payment Endpoints
//!
//! Public Payment Endpoints use [`EndpointData`] — the current implementation
//! wrapper for a Payment Endpoint Payload (addresses, invoices, JSON, etc.).
//! Each public Payment Endpoint is stored as a separate file at a well-known
//! path, one file per [`MethodId`] (legacy implementation name for Payment
//! Endpoint Identifier).
//!
//! The transport traits in this module handle **only public Payment Endpoints**.
//! All public Payment Endpoint operations go through [`UnauthenticatedTransportRead`]
//! and [`AuthenticatedTransport`].
//!
//! ## Private Payment Envelopes
//!
//! Private Payment Envelopes are handled entirely by [`pubky-noise`]'s encrypted
//! messaging layer via `PubkyNoiseEncryptor::send_message` and `receive_message`.
//! This layer manages file naming, storage locations, and end-to-end encryption
//! independently. **The transport traits have no involvement with private
//! payments.**
//!
//! Higher-level helper functions in [`crate`] (e.g.
//! [`crate::set_private_payments`]) compose `pubky-noise` encryption
//! directly with storage operations, bypassing the transport traits entirely.
//! This keeps the transport traits focused on public, unencrypted storage
//! operations.

use async_trait::async_trait;

use crate::{EndpointData, MethodId, PublicKey, Result};

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
    /// Fetches the payee's public Payment List.
    ///
    /// The method name `fetch_supported_payments` is legacy public API naming.
    /// It returns the payee-published Payment List snapshot, not the payer-side
    /// Supported Payment List described in the domain language.
    ///
    /// # Consistency
    ///
    /// This method first lists available Payment Endpoint entries and then fetches
    /// each one individually. Because the underlying transport does not support
    /// atomic/transactional reads, a **race condition** exists: between the
    /// directory listing and the individual fetches, Payment Endpoints may be added,
    /// removed, or modified by the payee. The returned [`SupportedPayments`] is
    /// therefore a **best-effort Payment List snapshot** and may be inconsistent.
    ///
    /// ## Recommended caller strategy
    ///
    /// If a payment execution fails with an error that suggests the endpoint
    /// has already been consumed or is no longer valid (evidence of a race
    /// condition), callers should:
    ///
    /// 1. Re-fetch the specific Payment Endpoint via
    ///    [`fetch_payment_endpoint`](Self::fetch_payment_endpoint).
    /// 2. Compare the newly retrieved [`EndpointData`] (Payment Endpoint Payload)
    ///    with the value used in the failed attempt.
    /// 3. If the Payment Endpoint Payload differs, it is safe to retry the payment
    ///    with the updated value.
    ///
    /// [`SupportedPayments`]: crate::SupportedPayments
    async fn fetch_supported_payments(&self, payee: &PublicKey)
        -> Result<crate::SupportedPayments>;

    /// Fetches an individual Payment Endpoint document if it exists.
    async fn fetch_payment_endpoint(
        &self,
        payee: &PublicKey,
        method: &MethodId,
    ) -> Result<Option<EndpointData>>;
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
    /// Writes or updates a Payment Endpoint document.
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()>;

    /// Removes an existing Payment Endpoint for the provided Payment Endpoint Identifier.
    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()>;
}
