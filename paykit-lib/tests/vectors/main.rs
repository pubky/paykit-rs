//! Paykit's test vectors (`specs/v0.2/vectors/`): one test per file, each rebuilding every
//! deterministic value byte for byte and refusing every rejection for its stated rule.
//!
//! The set is self-contained: its keys come from fixed seed labels (`keys.json`), and its
//! certificates, app records, record keys, policies and sealed values are built from them
//! with `pubky-switch-core`'s algorithms, at the Switch revision Paykit's spec builds on.

mod common;

mod allowance_usage;
mod documents;
mod encoding;
mod identifiers;
mod keys;
mod messages;
mod periods;
mod sealed;
mod signatures;
mod writers;
