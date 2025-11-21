//! Pubky transport adapters and shared constants.
//!
//! These helpers wrap the upstream Pubky SDK so Paykit calls can operate over the
//! standardized [`crate::AuthenticatedTransport`] and
//! [`crate::UnauthenticatedTransportRead`] traits without depending on specific SDK
//! types.

pub mod authenticated_transport;
pub mod unauthenticated_transport;

/// Conventional prefix for Paykit data hosted on Pubky storage.
/// `v0` means that the paykit conventions is to store data on pubky as following:
///  - /pub/paykit.app/v0/{method_id} -> with payload being the payment endpoint
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit.app/v0/";
/// Directory that stores contact/follow information (one file per known contact).
pub const PUBKY_FOLLOWS_PATH: &str = "/pub/pubky.app/follows/";
