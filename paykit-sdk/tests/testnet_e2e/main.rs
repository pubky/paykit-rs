//! Testnet-backed end-to-end tests for the paykit-sdk runtime.
//!
//! These tests run the real network paths (publish, handshake, private
//! send/receive, recovery markers, profiles) against an ephemeral Pubky
//! homeserver, complementing the mock-based unit tests under `src/`.

mod harness;

mod encrypted_links;
mod private_lists;
mod profiles;
mod public_endpoints;
mod recovery;
mod shared_identity;
