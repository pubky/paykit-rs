//! Gives this crate's wasm32 test binaries a 2 MiB stack instead of the linker's 1 MiB default.
//!
//! The pay flow in `tests/pay_flow/` and the network flow in `tests/switch_flow/` overflow
//! 1 MiB in the debug test build. Measured with `-zstack-size`, in Chromium, Firefox and
//! WebKit, in a page and in a dedicated worker, in steps of 64 KiB: 1216 KiB passes and
//! 1152 KiB fails for each (`--release`, in steps of 32 KiB: 192 KiB passes, 160 KiB fails).
//! 2 MiB leaves about 68 percent above the largest need measured. The setting is for the test
//! binaries only: a web app that links Paykit into its own WebAssembly module sets that
//! module's stack size itself.
//!
//! This linker argument comes after those of `RUSTFLAGS` and `cargo rustc -- -C link-arg=...`,
//! so it wins over them, as the initial value of `__stack_pointer` in each measurement binary
//! showed. To measure again, edit the `TEST_STACK_BYTES` const below to each size and build.

/// The stack size of the wasm32 test binaries, in bytes.
const TEST_STACK_BYTES: u32 = 2 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg-tests=-zstack-size={TEST_STACK_BYTES}");
    }
}
