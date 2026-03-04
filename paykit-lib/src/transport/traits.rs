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
//! # Public vs. private payload types
//!
//! Public payment methods use [`EndpointData`] — a validated UTF-8 string wrapper
//! representing human-readable payment endpoint payloads (addresses, invoices,
//! JSON, etc.). Each public method is stored as a separate file at a well-known
//! path (one file per [`MethodId`]).
//!
//! Private payment methods use raw bytes (`Vec<u8>` / `&[u8]`) at the transport
//! level because the data is encrypted before it reaches the transport and
//! decrypted after retrieval. All private methods for a given recipient are
//! stored together in a single encrypted blob whose plaintext is a JSON object
//! mapping method identifiers to endpoint values
//! (e.g. `{ "lightning": "ln...", "onchain": "bc1..." }`). The transport layer
//! treats these blobs as **opaque** — it has no knowledge of the encryption
//! scheme, key material, or plaintext structure.
//!
//! Encryption and decryption are handled by the higher-level helper functions in
//! [`crate`] (e.g. [`crate::set_private_payment_endpoint`]), which compose the
//! transport trait methods with application-level encryption (currently
//! [`pubky-data`]). Those helpers accept and return [`EndpointData`] to callers,
//! so the public API surface is consistent regardless of whether the underlying
//! storage is encrypted.
//!
//! This separation keeps the transport traits **crypto-agnostic**: implementors
//! only need to store and retrieve bytes at the correct paths, without importing
//! or understanding any encryption library. It also means alternative transports
//! can plug in their own encryption scheme without changing the trait contract.

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

    /// Fetches the raw (encrypted) private payments blob for a given `owner`
    /// and `recipient_id`.
    ///
    /// The `recipient_id` is an opaque path component computed by the caller
    /// (currently derived from the counterparty's public key; the derivation
    /// logic may change in the future).
    ///
    /// Returns `Ok(None)` when no private payments file exists for this
    /// recipient. The returned bytes are **opaque** to the transport —
    /// decryption is the caller's responsibility.
    ///
    /// See the [module-level documentation](self) for the rationale behind
    /// using raw bytes instead of [`EndpointData`] for private payments.
    async fn fetch_private_payments_blob(
        &self,
        owner: &PublicKey,
        recipient_id: &str,
    ) -> Result<Option<Vec<u8>>>;
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

    /// Writes an opaque (already encrypted) blob to the private payments
    /// path for the given `recipient_id`.
    ///
    /// The `recipient_id` is an opaque path component computed by the caller.
    /// The transport stores the bytes as-is without inspecting or transforming
    /// them.
    ///
    /// See the [module-level documentation](super::traits) for the rationale
    /// behind using raw bytes instead of [`EndpointData`] for private payments.
    async fn put_private_payments(&self, recipient_id: &str, data: &[u8]) -> Result<()>;

    /// Removes the private payments file for the given `recipient_id`.
    async fn remove_private_payments(&self, recipient_id: &str) -> Result<()>;
}
