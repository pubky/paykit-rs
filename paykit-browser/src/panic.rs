//! Reporting a Rust panic to the browser console: [`install_panic_hook`].

use std::sync::Once;

use wasm_bindgen::prelude::wasm_bindgen;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(message: &str);
}

/// Report every Rust panic to the JavaScript console, with `console.error`.
///
/// A `wasm32-unknown-unknown` build aborts on a panic, and the standard library has nowhere to
/// print the panic message, so the browser shows only `RuntimeError: unreachable`. After this
/// call, a panic first logs its message and source location (`panicked at src/file.rs:10:5:`
/// and the message), then runs the panic hook that was installed before, so a hook the host
/// set earlier keeps working.
///
/// Call it once at start-up, before any other Paykit call. Calling it again does nothing.
///
/// It is also written to do nothing once a panic has happened in this instance, a case the
/// tests here do not reach: the standard library refuses to change the panic hook while a
/// panic is in progress, and after an abort on wasm32 one stays in progress for good. Such an
/// instance should be replaced, not reused.
pub fn install_panic_hook() {
    static INSTALL: Once = Once::new();
    // `take_hook` and `set_hook` panic on a panicking thread. Checking here, outside
    // `call_once`, keeps that panic from leaving `INSTALL` half-run.
    if std::thread::panicking() {
        return;
    }
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            console_error(&info.to_string());
            previous(info);
        }));
    });
}
