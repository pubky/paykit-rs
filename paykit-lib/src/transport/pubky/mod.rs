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
///  - /pub/paykit.app/v0/{method_id} -> with payload being the payment endpoint
pub const PAYKIT_PATH_PREFIX: &str = "/pub/paykit.app/v0/";
/// Conventional prefix for private (encrypted) Paykit data.
/// Private payments for a given recipient are stored as a single encrypted blob at:
///  - /pub/paykit.app/v0/private/{recipient_id}/{PAYKIT_PRIVATE_PAYMENTS_FILE}
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit.app/v0/private/";
/// Filename for the encrypted private payments blob within a recipient directory.
pub const PAYKIT_PRIVATE_PAYMENTS_FILE: &str = "payments.json";
/// Directory that stores contact/follow information (one file per known contact).
pub const PUBKY_FOLLOWS_PATH: &str = "/pub/pubky.app/follows/";
/// File that stores profile information
pub const PUBKY_PROFILE_FILE: &str = "/pub/pubky.app/profile.json";
