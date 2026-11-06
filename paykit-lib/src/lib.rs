//! # paykit-lib
//!
//! The stateless protocol core of Paykit v0.2.
//!
//! This crate holds the pure parts of the protocol: encodings, key derivations,
//! signatures, sealed envelopes, signed documents, message checks and chain
//! verification. It does no I/O and reads no clock: every function that depends
//! on time takes it as an argument.
//!
//! The protocol is defined by `specs/v0.2/spec.md` and
//! `specs/v0.2/wire-format.md`; the shared vectors in `specs/v0.2/vectors/`
//! are normative.
//!
//! State, flows and network calls belong in `paykit-sdk`; the signer core
//! belongs in `paykit-signer`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
