//! The host's timer ([`Timer`]): the SDK keeps no timer of its own, and is given one, as it
//! is given its clock.
//!
//! The one wait the SDK bounds is the payee's switch lookup (Paykit spec 4, "Public endpoints
//! are read from the Homeserver"; Switch spec 4.3, "No added latency"): it runs beside the
//! Homeserver's reads and is waited on for at most [`crate::IDENTITY_GRACE_MS`] after them.

use pubky_switch_client::transport::BoxFuture;

/// A timer of the host's: its futures resolve once `ms` milliseconds have passed on the clock
/// the app runs on. Object-safe; the SDK holds an `Arc<dyn Timer>`.
pub trait Timer: Send + Sync {
    /// Resolve after `ms` milliseconds.
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()>;
}
