//! Pubky storage helpers used by Paykit.
//!
//! Paykit Library uses Pubky as its concrete routing, discovery, and storage
//! substrate. Timeout handling is the caller's responsibility when constructing
//! Pubky SDK clients; Paykit does not wrap SDK calls with its own deadline.

pub mod pubky;
