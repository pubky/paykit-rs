//! The pay and receipt flow of the Paykit SDK, run in a browser.
//!
//! It continues where the Payment Request flow (`tests/request_flow/`) stops, in the same world
//! (`tests/world/`): two Paykit Apps on the Switch client's in-memory scripted switches, one
//! WebAssembly instance, no network. The payer's wallet pays an open one-time Payment Request
//! through the SDK's `FakeRail`, the payee's shop receives the Payment Proof and confirms it
//! with a Payment Receipt, and the wallet stores the receipt. The other tests pin down a
//! payment the rail refuses or fails, a request paid after its expiry, and a payment left in
//! progress that is resumed.
//!
//! The steps and the outcomes they check follow paykit-sdk's native flow tests
//! (`paykit-sdk/tests/flows/one_time.rs` and `recovery.rs`).
//!
//! The scripted switch exists only in the browser build of this target, so natively the
//! target is empty. wasm-bindgen-test runs it in a browser page or in a dedicated worker; the
//! runner's environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod one_time;
mod resume;
#[path = "../world/mod.rs"]
mod world;
