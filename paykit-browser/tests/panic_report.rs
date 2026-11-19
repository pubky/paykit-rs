//! `install_panic_hook` writes a Rust panic to `console.error`, run in a browser.
//!
//! This is a test target of its own, with one test, so that its call of `install_panic_hook`
//! is the first in the WebAssembly instance: the hook installed before it is then the one the
//! test sets. The panic also ends the instance, which no other test then shares. The hook
//! exists only on `wasm32-unknown-unknown`, so on any other target this test binary is empty.

#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use std::panic;

use js_sys::{Array, Function, Reflect};
use paykit_browser::panic::install_panic_hook;
use wasm_bindgen::JsValue;
use wasm_bindgen_test::wasm_bindgen_test;

/// The global that holds every message `console.error` received after [`SPY`] ran.
const SEEN: &str = "paykitConsoleErrors";

/// Wraps `console.error`, so that it also keeps each message, its arguments joined by spaces,
/// in `globalThis.paykitConsoleErrors`.
const SPY: &str = r#"
const seen = [];
const real = console.error;
console.error = function (...args) {
  seen.push(args.map(String).join(" "));
  return real.apply(this, args);
};
globalThis.paykitConsoleErrors = seen;
"#;

/// The messages `console.error` received since [`SPY`] ran.
fn console_errors() -> Vec<String> {
    let seen = Reflect::get(&js_sys::global(), &SEEN.into()).unwrap();
    Array::from(&seen)
        .iter()
        .filter_map(|message| message.as_string())
        .collect()
}

/// A panic is written to `console.error`, with its message and source location, before the
/// hook installed earlier runs. The hook is installed twice: the second call changes nothing.
///
/// The test sets that earlier hook itself. It passes the panic on to the test harness's hook,
/// which records the panic message that `should_panic` checks, only once `console.error` has
/// received that message and location. A hook that did not write the panic to the console
/// would leave the harness without the message, and the test would fail.
#[wasm_bindgen_test]
#[should_panic(expected = "panic written to the console")]
fn test_panic_report_written_to_console_error() {
    Function::new_no_args(SPY)
        .call0(&JsValue::UNDEFINED)
        .unwrap();
    let harness = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // A panic in a panic hook aborts at once, so a missing location fails the test the
        // same way: the harness's hook does not run.
        let Some(location) = info.location() else {
            return;
        };
        let at = format!(
            "{}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
        let written = console_errors().iter().any(|message| {
            message.contains("panic written to the console") && message.contains(&at)
        });
        if written {
            harness(info);
        }
    }));
    install_panic_hook();
    install_panic_hook();
    panic!("panic written to the console");
}
