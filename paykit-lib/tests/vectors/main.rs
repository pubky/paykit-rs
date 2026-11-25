//! Paykit's test vectors (`specs/v0.2/vectors/`): one test per file, each rebuilding every
//! deterministic value byte for byte and refusing every rejection for its stated rule.
//!
//! The set is self-contained: its keys come from fixed seed labels (`keys.json`), and its
//! certificates, app records, record keys, policies and sealed values are built from them
//! with `pubky-dispatch-core`'s algorithms, at the Dispatch revision Paykit's spec builds on.
//!
//! The set is regenerated as one by the driver in `regenerate.rs`, which builds every file
//! in dependency order from one in-memory corpus:
//! `PAYKIT_WRITE_VECTORS=1 cargo test -p paykit-lib --test vectors regenerate`.

mod common;

mod allowance_usage;
mod documents;
mod encoding;
mod identifiers;
mod keys;
mod messages;
mod periods;
mod regenerate;
mod sealed;
mod signatures;
mod writers;
