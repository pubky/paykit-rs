//! Transport abstractions used by Paykit.
//!
//! This module exposes the public traits that callers must implement as well as the
//! feature-gated Pubky adapters that satisfy those traits out of the box.
//!
//! Timeout handling is **not** enforced by the traits — each transport
//! implementation must configure its own timeouts. See [`traits`] module
//! documentation for guidance and examples.

pub mod traits;

#[cfg(feature = "pubky")]
pub mod pubky;

pub use traits::{AuthenticatedTransport, UnauthenticatedTransportRead};

#[cfg(feature = "pubky")]
pub use pubky::{
    authenticated_transport::PubkyAuthenticatedTransport,
    unauthenticated_transport::PubkyUnauthenticatedTransport,
};
