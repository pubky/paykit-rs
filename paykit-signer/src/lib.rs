//! # paykit-signer
//!
//! The Paykit v0.2 signer core.
//!
//! The signer holds an identity's key through a narrow interface the host app
//! implements. It approves apps, signs `identity.json`, issues certificates and
//! key bundles, and carries out revocations, provider moves, recovery, account
//! erasure and the hand-over to another signer (spec Sections 2, 3 and 13).
//!
//! The protocol is defined by `specs/v0.2/spec.md` and
//! `specs/v0.2/wire-format.md`.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
