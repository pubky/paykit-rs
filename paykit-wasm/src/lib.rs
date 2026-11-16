//! THROWAWAY (#411): thin wasm-bindgen facade over `paykit-sdk` for the
//! browser end-to-end spike.
//!
//! It exists to prove that the real SDK runs in a browser against a local
//! Pubky testnet: sign-up, SDK initialization, public Payment Endpoint
//! publication and read-back, an Encrypted Link Handshake, a Private Payment
//! List round trip, private intake, and grant revocation; and (round 2,
//! `web/flows.rs`) Payment Requests, Receipts and Receipt Access,
//! Allowances and allowance accounting, backup restore, key rotation, two
//! runtimes over one IndexedDB store and a call deadline. It is not a
//! production binding. The production path recommended by the #411 spike is
//! UniFFI (`paykit-ffi`) plus uniffi-bindgen-react-native's web flavor.
//!
//! Everything is compiled only for `wasm32`; the native build of this crate
//! is empty so `cargo check --workspace` keeps working.

#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
pub use web::*;
