//! Protected Paykit enrollment and wallet recovery for Kotlin and Swift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![recursion_limit = "512"]
mod enrollment;
mod environment;
mod error;
mod events;
mod method;
mod notices;
mod payments;
mod receive;
mod reconcile;
mod requests;
mod review;
mod store;
mod wallet;
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
pub use wallet::*;
uniffi::setup_scaffolding!();
#[cfg(test)]
mod tests;
