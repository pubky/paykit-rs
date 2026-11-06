//! # paykit-sdk
//!
//! The Paykit v0.2 app SDK, for wallets and seller apps.
//!
//! This crate holds Paykit's payment flows and their state: requests and
//! their answers, proofs and receipts, attempts on claims, recurring requests,
//! Allowances, Private Payment Lists, labelled selling scopes, receiving and
//! re-sends, and the scopes an app asks for at approval. Payment rails stay
//! outside Paykit, behind a narrow rail interface.
//!
//! It builds on `pubky-switch-client` and `pubky-switch-signer` for the
//! Switch's side, and on `paykit-lib` for Paykit's formats and rules. The
//! protocol is defined by the Paykit spec and wire format in `specs/v0.2/`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
