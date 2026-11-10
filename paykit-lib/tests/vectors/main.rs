//! Paykit's test vectors (`specs/v0.2/vectors/`): one test per file, each rebuilding every
//! deterministic value byte for byte and refusing every rejection for its stated rule.
//!
//! The Paykit set builds on the Switch's set at the pinned revision, read through
//! `pubky_switch_core::spec_vectors_dir()`: its fixed keys and the certificates of its
//! `documents.json`.

mod common;

mod allowance_usage;
mod documents;
mod encoding;
mod identifiers;
mod messages;
mod periods;
mod sealed;
mod signatures;
mod writers;
