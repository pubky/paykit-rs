//! Core transport traits that decouple Paykit logic from specific SDKs or backends.

use async_trait::async_trait;

use crate::{EndpointData, MethodId, Profile, PublicKey, Result};

/// Trait describing read-only access to public Paykit transport.
#[async_trait]
pub trait UnauthenticatedTransportRead {
    /// Fetches the raw Supported Payments List for the provided `payee`.
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

    /// Returns the profile of the given user.
    ///
    /// # Errors
    /// - Returns `PaykitError::NotFound` if the profile does not exist.
    /// - Returns `PaykitError::Profile` if the profile exists but cannot be parsed.
    /// - Returns `PaykitError::Transport` for network failures.
    async fn fetch_profile(&self, user: &PublicKey) -> Result<Profile>;
}

/// Trait describing authenticated write (and optional read) access.
#[async_trait]
pub trait AuthenticatedTransport {
    /// Writes or updates a payment endpoint document.
    async fn upsert_payment_endpoint(&self, method: &MethodId, data: &EndpointData) -> Result<()>;

    /// Removes an existing payment endpoint for the provided method.
    async fn remove_payment_endpoint(&self, method: &MethodId) -> Result<()>;
}
