//! The Paykit SDK on `FetchTransport` against a real Switch service while the network
//! misbehaves, in a browser.
//!
//! `rig/test.sh --switch` runs the service behind the rig on `https://localhost:<rig port>`
//! (`rig/switch-rig.mjs`). Each test asks the rig for a world of its own (`POST /rig/world`,
//! `tests/switch_flow/switch_world.rs`), arms a fault on the rig for the requests it forwards
//! to the service (`/rig/fault`, through `faults.rs`), and counts what reached the service
//! (`/rig/hits`). The SDK sees only what the browser's `fetch` reports, as an app would.
//!
//! - `payment.rs`: the payer's operation batches are lost or refused while it pays.
//! - `hold.rs`: the rig holds a public identity read, which then completes no earlier than the
//!   hold, or ends at the transport's deadline.
//!
//! The rig holds one fault for the whole run. wasm-bindgen-test runs the tests of a target one
//! at a time, so each test arms its fault on a path of its own world, after disarming any
//! fault left armed, and disarms it before it ends.
//!
//! The transport exists only on `wasm32-unknown-unknown`, so natively the target is empty.
//! wasm-bindgen-test runs it in a browser page or in a dedicated worker; the runner's
//! environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod faults;
mod hold;
mod payment;
#[allow(dead_code)]
#[path = "../switch_rig/mod.rs"]
mod switch_rig;
#[path = "../switch_flow/switch_world.rs"]
mod switch_world;
// Only `DAY_MS`, `draft` and `send` are used here; the scripted world goes unused.
#[allow(dead_code)]
#[path = "../world/mod.rs"]
mod world;
