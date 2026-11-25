//! # paykit-lib
//!
//! Paykit's formats and rules, for Paykit v0.2.
//!
//! Paykit v0.2 is a payment protocol on the Pubky Dispatch. This crate holds
//! what Paykit adds on top of the Dispatch's formats: its scopes and the Dispatch
//! permissions each one needs, its labels and derivations, its message kinds
//! and their bodies, its collections and their policies, and the payment
//! rules, Allowances and amounts included. It does no I/O and reads no clock:
//! every function that depends on time takes it as an argument.
//!
//! It builds on `pubky-dispatch-core`, which holds the Dispatch's own formats and
//! checks; Paykit never redefines them. The Paykit protocol is defined by the
//! Paykit spec and wire format in `specs/v0.2/`, and its vectors in
//! `specs/v0.2/vectors/` are normative.
//!
//! State, flows and network calls belong in `paykit-sdk`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)
)]

pub mod allowances;
pub mod amount;
pub mod app_record;
pub mod bodies;
pub mod collections;
pub mod derive;
pub mod error;
pub mod grant;
pub mod identifiers;
mod json;
pub mod kinds;
mod list_version;
pub mod periods;
pub mod records;
pub mod recovery;
pub mod scopes;
#[cfg(feature = "signer")]
pub mod signer;

pub use amount::{
    amount_for_endpoint, rate_for_endpoint, Asset, Decimal, EndpointAmount, PaymentAmount, Rate,
};
pub use bodies::Body;
pub use collections::Collection;
pub use error::{Error, Result};
pub use grant::ExecutorGrant;
pub use identifiers::{
    AllowanceId, AttemptId, PaymentEndpointIdentifier, PaymentEndpointPayload, PaymentReference,
};
pub use kinds::{Kind, PaykitKindRules};
pub use list_version::ListVersion;
pub use scopes::{Label, PaykitScope};
