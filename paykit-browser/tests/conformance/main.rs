//! Paykit v0.2 code run in a browser, compared with the same code run natively.
//!
//! Each test builds for the host, under `cargo test -p paykit-browser`, and for
//! `wasm32-unknown-unknown`, where wasm-bindgen-test runs it in a browser page or in a dedicated
//! worker. The test bodies are the same on both targets, so a browser result is compared with
//! the native one by construction. The runner's environment picks the browser and the context
//! (`rig/runner.mjs`); the tests don't, so one test binary runs in both.

/// A test that runs under libtest natively and under wasm-bindgen-test on wasm32.
macro_rules! conformance_test {
    ($(#[$meta:meta])* fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), test)]
        fn $name() $body
    };
}

mod randomness;
mod round_trips;
mod support;
mod vectors;
