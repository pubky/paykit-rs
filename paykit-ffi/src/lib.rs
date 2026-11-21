//! Protected Paykit enrollment and wallet recovery for Kotlin and Swift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![recursion_limit = "512"]
#[cfg(feature = "test-wallet")]
#[path = "../tests/hosts/checkout.rs"]
mod checkout;
mod enrollment;
mod environment;
mod error;
mod events;
#[cfg(feature = "test-wallet")]
#[path = "../tests/hosts/service.rs"]
mod fixture_service;
#[cfg(any(test, feature = "test-wallet"))]
#[path = "../tests/hosts/signer.rs"]
mod fixture_signer;
mod method;
mod notices;
mod payments;
mod receive;
mod reconcile;
mod requests;
mod review;
mod store;
mod switch_refusal;
mod wallet;
#[cfg(feature = "test-wallet")]
pub use checkout::{enrolled_test_checkout, CheckoutFixture};
pub use enrollment::*;
pub use environment::*;
pub use error::*;
pub use events::*;
pub use method::*;
pub use notices::*;
pub use payments::*;
pub use receive::*;
pub use reconcile::*;
pub use requests::*;
pub use review::*;
pub use store::*;
pub use switch_refusal::*;
pub use wallet::*;
uniffi::setup_scaffolding!();
#[cfg(test)]
mod tests;
