//! Protected Paykit enrollment for Kotlin and Swift.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
mod enrollment;
mod error;
mod store;
pub use enrollment::*;
pub use error::*;
pub use store::*;
uniffi::setup_scaffolding!();
#[cfg(test)]
mod tests;
