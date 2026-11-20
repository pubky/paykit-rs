//! Protected Paykit enrollment and wallet recovery for Kotlin and Swift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![recursion_limit = "512"]
mod enrollment;
mod environment;
mod error;
mod method;
mod payments;
mod review;
mod store;
mod switch_refusal;
mod wallet;
pub use enrollment::*;
pub use environment::*;
pub use error::*;
pub use method::*;
pub use payments::*;
pub use review::*;
pub use store::*;
pub use switch_refusal::*;
pub use wallet::*;
uniffi::setup_scaffolding!();
#[cfg(test)]
mod tests;
