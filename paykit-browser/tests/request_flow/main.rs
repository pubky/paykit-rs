//! A Payment Request flow of the Paykit SDK, run in a browser.
//!
//! A payer and a payee each join a Paykit App, on a scripted switch of their own: the Switch
//! client's in-memory switch (`pubky_switch_client::testing`), reached through one `Router`.
//! Everything runs in one WebAssembly instance, with no network. The apps are given this
//! crate's `JsTimer` as their timer, which the flow never sleeps on (the SDK waits on its timer
//! only when it pays), and the world's test clock starts at the browser's wall clock, read
//! through `JsClock`.
//!
//! The steps follow paykit-sdk's native flow tests (`paykit-sdk/tests/flows/`) up to the
//! payment, which is not made here: paying calls `std::time::Instant::now()`, which panics on
//! `wasm32-unknown-unknown`.
//!
//! The scripted switch exists only in the browser build of this target, so natively the
//! target is empty. wasm-bindgen-test runs it in a browser page or in a dedicated worker; the
//! runner's environment picks the browser and the context (`rig/runner.mjs`).

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod one_time;
mod world;
