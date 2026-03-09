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
/// This prefix is used as the base path for pubky-data's encrypted messaging.
/// The actual write and read paths are derived per-peer-pair using
/// [`pubky_data::path_derivation::derive_asymmetric_paths`]. Pubky-data manages
/// individual file slots within the derived folders using a counter-based scheme.
pub const PAYKIT_PRIVATE_PATH_PREFIX: &str = "/pub/paykit.app/v0/private";
/// Directory that stores contact/follow information (one file per known contact).
pub const PUBKY_FOLLOWS_PATH: &str = "/pub/pubky.app/follows/";
/// File that stores profile information
pub const PUBKY_PROFILE_FILE: &str = "/pub/pubky.app/profile.json";
