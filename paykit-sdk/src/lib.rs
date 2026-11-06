//! # paykit-sdk
//!
//! The Paykit v0.2 app SDK, for wallets and seller apps.
//!
//! This crate holds an app's state, its flows and its network I/O: the Pubky
//! Switch client, the Homeserver reads and writes, the app's state store,
//! receiving and sending messages, and the payment flows. It builds on the
//! stateless core in `paykit-lib`.
//!
//! The protocol is defined by `specs/v0.2/spec.md` and
//! `specs/v0.2/wire-format.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
