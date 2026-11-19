//! The browser host pieces of `paykit-browser`, run in a browser.
//!
//! The host pieces exist only on `wasm32-unknown-unknown`, so on any other target this test
//! binary is empty. On `wasm32-unknown-unknown`, wasm-bindgen-test runs it in a browser page or
//! in a dedicated worker; the runner's environment picks the browser and the context
//! (`rig/runner.mjs`), so one test binary runs in both.

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

mod clock;
mod fetch;
mod send;
mod support;
mod timer;
