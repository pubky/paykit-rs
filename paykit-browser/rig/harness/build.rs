//! Gives the harness bundle a 2 MiB stack instead of the linker's 1 MiB default.
//!
//! `paykit-browser/build.rs` sets the stack of that crate's test binaries only, and a cdylib
//! gets the linker's default. The `flow_pay` scenario runs the network flow of
//! `tests/switch_flow/`, whose debug build needs up to 1216 KiB (1152 KiB fails), and 192 KiB
//! in `--release` (160 KiB fails), as the paykit-browser README's "Stack" section measured in
//! Chromium, Firefox and WebKit, in a page and in a dedicated worker. 2 MiB leaves about 68
//! percent above the debug need, as for the test binaries, so a debug build of the bundle runs
//! the deepest scenario too. The setting is for wasm32 only.

/// The stack size of the wasm32 harness bundle, in bytes.
const HARNESS_STACK_BYTES: u32 = 2 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-cdylib=-zstack-size={HARNESS_STACK_BYTES}");
    }
}
