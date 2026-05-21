//! Pubky transport adapters and shared constants.
//!
//! These helpers wrap the upstream Pubky SDK so Paykit calls can operate over the
//! standardized [`crate::AuthenticatedTransport`] and
//! [`crate::UnauthenticatedTransportRead`] traits without depending on specific SDK
//! types.

pub mod authenticated_transport;
pub mod unauthenticated_transport;

/// Conventional prefix for public Paykit data hosted on Pubky storage.
/// `v0` means that the paykit conventions is to store data on pubky as following:
///  - /pub/paykit/v0/{payment_endpoint_identifier} -> with payload being the Payment Endpoint Payload
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit/v0/";
/// Conventional prefix for private (encrypted) Paykit data.
/// This prefix is used as the base path for pubky-noise's encrypted messaging.
/// The actual write and read paths are derived per-peer-pair using
/// [`pubky_noise::path_derivation::derive_asymmetric_paths`]. Pubky-noise manages
/// individual file slots within the derived folders using a counter-based scheme.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit/v0/private";
