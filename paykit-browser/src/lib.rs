//! # paykit-browser
//!
//! Browser (`wasm32-unknown-unknown`) build support for the Paykit SDK, the first host pieces
//! a browser host gives the SDK and the Switch client, and the tests that run Paykit in
//! browser engines.
//!
//! Depending on this crate selects the browser random number backends
//! (`crypto.getRandomValues`) for the wasm32 build of Paykit, so the whole
//! `wasm32-unknown-unknown` graph builds.
//!
//! On `wasm32-unknown-unknown` the crate also holds these host pieces, Rust items for a host
//! written in Rust, which work in a page and in a dedicated worker:
//!
//! - `clock`: `JsClock`, the Switch client's `Clock` on JavaScript's `Date.now()`;
//! - `timer`: `JsTimer`, the SDK's `Timer` on JavaScript's `setTimeout`;
//! - `send`: `run_local`, which runs a future that waits on the browser, and so is not `Send`,
//!   behind a `Send` future, as the host traits require;
//! - `panic`: `install_panic_hook`, which reports a Rust panic to the browser console;
//! - `fetch`: `FetchTransport`, the Switch client's `Transport` on the browser's `fetch`.
//!
//! The tests under `tests/` run a subset of Paykit's vectors, the random generator, signing,
//! and sealing natively and in browser engines. In browser engines only, they run the host
//! pieces, and the SDK's Payment Request and pay flows with no network; and, against a real
//! Switch service behind a local TLS and CORS rig, `FetchTransport` and the one-time Payment
//! Request flow. The crate's README lists what is covered, what is not run in a browser yet,
//! the limits of the network tests, and the known run-time hazards on wasm32.
//!
//! On any other target the crate builds as an empty crate, so workspace-wide commands keep
//! working.
//!
//! The crate builds for single-threaded `wasm32` only. Browser futures are not `Send`, so a
//! build with WebAssembly threads (the `atomics` target feature) is refused at compile time.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

// A threaded wasm32 build needs a nightly toolchain that rebuilds std with `atomics`, and that
// toolchain sets this cfg. Stable rustc hides unstable target features from `cfg`, but its
// prebuilt std has no `atomics`, so it cannot link a threaded build either.
#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
compile_error!(
    "paykit-browser does not support WebAssembly threads: build for wasm32 without the \
     `atomics` target feature"
);

// The host pieces call JavaScript, so only `wasm32-unknown-unknown`, the browser target, gets
// them: other wasm32 targets, such as WASI, have no JavaScript host to call.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod clock;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod fetch;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod panic;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod send;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub mod timer;
