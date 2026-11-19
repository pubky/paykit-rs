//! # paykit-browser
//!
//! Browser (`wasm32-unknown-unknown`) build support for the Paykit SDK, and the tests that run
//! Paykit in browser engines.
//!
//! Depending on this crate selects the browser random number backends
//! (`crypto.getRandomValues`) for the wasm32 build of Paykit, so the whole
//! `wasm32-unknown-unknown` graph builds. The tests under `tests/` run a subset of Paykit's
//! vectors, the random generator, signing, and sealing natively and in browser engines. The
//! SDK's flows are not run in a browser yet; the crate's README lists what is covered and the
//! known run-time hazards on wasm32.
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
