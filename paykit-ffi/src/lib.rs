//! Protected Paykit enrollment and wallet recovery for Kotlin and Swift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![recursion_limit = "512"]
mod allowances;
#[cfg(feature = "test-wallet")]
#[path = "../tests/hosts/checkout.rs"]
mod checkout;
mod config;
mod dispatch_refusal;
mod enrollment;
mod environment;
mod error;
mod events;
mod executor;
mod extensions;
#[cfg(feature = "test-wallet")]
#[path = "../tests/hosts/service.rs"]
mod fixture_service;
#[cfg(any(test, feature = "test-wallet"))]
#[path = "../tests/hosts/signer.rs"]
mod fixture_signer;
mod flights;
mod lists;
mod method;
mod notices;
mod opaque;
mod payee;
mod payer;
mod payments;
mod periods;
mod public;
mod receive;
mod reconcile;
mod requests;
mod restore;
mod review;
mod runtime;
mod store;
mod wallet;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use allowances::*;
#[cfg(feature = "test-wallet")]
pub use checkout::{enrolled_test_checkout, CheckoutFixture};
pub use config::*;
pub use enrollment::*;
pub use environment::*;
pub use error::*;
pub use events::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use executor::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use extensions::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use lists::*;
pub use method::*;
pub use notices::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use payee::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use payer::*;
pub use payments::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
#[allow(unused_imports)]
pub use periods::*;
// A group's module may export nothing yet; the allow stays, so binding a group never edits this file.
pub use dispatch_refusal::*;
#[allow(unused_imports)]
pub use public::*;
pub use receive::*;
pub use reconcile::*;
pub use requests::*;
pub use restore::*;
pub use review::*;
pub use store::*;
pub use wallet::*;
uniffi::setup_scaffolding!();
#[cfg(test)]
mod tests;
