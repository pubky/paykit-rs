//! # paykit-lib
//!
//! Paykit's formats and rules, for Paykit v0.2.
//!
//! Paykit v0.2 is a payment protocol on the Pubky Switch. This crate holds
//! what Paykit adds on top of the Switch's formats: its scopes and the switch
//! permissions each one needs, its labels and derivations, its message kinds
//! and their bodies, its collections and their policies, and the payment
//! rules, Allowances and amounts included. It does no I/O and reads no clock:
//! every function that depends on time takes it as an argument.
//!
//! It builds on `pubky-switch-core`, which holds the Switch's own formats and
//! checks; Paykit never redefines them. The Paykit protocol is defined by the
//! Paykit spec and wire format in `specs/v0.2/`, and its vectors in
//! `specs/v0.2/vectors/` are normative.
//!
//! State, flows and network calls belong in `paykit-sdk`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
