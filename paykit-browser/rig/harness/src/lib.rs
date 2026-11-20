//! # paykit-browser-harness
//!
//! **Test rig code only.** This is a test harness, and not a proposal for the SDK's
//! JavaScript surface: the hosted-page tests of paykit-browser load it in a dedicated module
//! worker of a page served under a strict Content Security Policy, and run scenarios in it.
//!
//! It exports one function to JavaScript, `runScenario(name, args)`, which takes the
//! scenario's name and its arguments as a JSON object in a string, and resolves to its result
//! as a JSON object in a string, or rejects with an `Error` whose message is
//! `<code>: <text>`. Only strings cross. The scenarios are test scripts written in Rust: they
//! make their assertions here and return plain data.
//!
//! - `pure`: no network. A signature and a sealed record value round trip through the public
//!   API of the Switch's core, the crate's `JsClock`, and a `JsTimer` sleep.
//! - `switch_reach`: one request through `FetchTransport` to the rig's origin, nothing more.
//! - `flow_pay`: the one-time Payment Request flow of `tests/switch_flow/` against the real
//!   Switch service behind the rig, with each Paykit App on a store of its own: requested,
//!   paid through the fake rail, proven and receipted.
//! - `flow_read_back`: after a reload, opens the same stores, loads both apps from them alone,
//!   and reads back the Payment Receipt and the request's state.
//! - `cleanup`: destroys the stores `flow_pay` made for a run.
//! - `panic`: panics after an await, on purpose.
//!
//! The error codes are `bad_args`, `unknown_scenario`, `build` (the bundle was built without
//! the rig's settings), `rig` (the rig refused or failed to enrol a world), `switch` (a call
//! through the transport or the SDK failed), `store` (a store or the host's test key could
//! not be opened, read, written or destroyed) and `assertion`.
//!
//! Secrets stay in the worker: the identities' keys come from the rig's world inside Rust,
//! and each store keeps the App ID of its app (see `stores.rs`), so the read-back after a
//! reload finds both apps with nothing secret passed through JavaScript.
//!
//! The rig's origin and token are compiled in from `PAYKIT_BROWSER_RIG_ORIGIN` and
//! `PAYKIT_BROWSER_RIG_TOKEN`, as for the network test targets; the network scenarios refuse
//! a `switch` argument that is not that origin.
//!
//! On every target but `wasm32-unknown-unknown` the crate is empty.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

// Every item below carries `#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]`, the one
// target the harness runs on, so a native build of the crate compiles nothing. The attribute
// sits on each item rather than inside a macro so that rustfmt still follows the `mod`
// declarations.

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod failure;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod flow;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod pure;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod reach;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod stores;
// The rig helpers, the network flow's world and the scripted flows' request draft, shared with
// the test targets under `tests/`: one copy of each. The harness uses only some of their items.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[allow(dead_code)]
#[path = "../../../tests/switch_rig/mod.rs"]
mod switch_rig;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[allow(dead_code)]
#[path = "../../../tests/switch_flow/switch_world.rs"]
mod switch_world;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[allow(dead_code)]
#[path = "../../../tests/world/mod.rs"]
mod world;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
use wasm_bindgen::prelude::{wasm_bindgen, JsError};

/// Runs the scenario `name` with `args`, a JSON object, and returns its result, a JSON object.
///
/// The first call installs the crate's panic hook, which reports a panic to the console; later
/// calls leave it as it is.
///
/// # Errors
///
/// A `JsError` whose message is `<code>: <text>`, with one of the codes the crate docs list.
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[wasm_bindgen(js_name = runScenario)]
pub async fn run_scenario(name: String, args: String) -> Result<String, JsError> {
    paykit_browser::panic::install_panic_hook();
    // Each scenario's future goes on the heap: the SDK's futures nest deep.
    let result = match failure::Args::parse(&args) {
        Ok(args) => match name.as_str() {
            "pure" => Box::pin(pure::run()).await,
            "switch_reach" => Box::pin(reach::run(&args)).await,
            "flow_pay" => Box::pin(flow::pay(&args)).await,
            "flow_read_back" => Box::pin(flow::read_back(&args)).await,
            "cleanup" => Box::pin(stores::cleanup(&args)).await,
            "panic" => Box::pin(pure::panic_after_await()).await,
            _ => Err(failure::Failure::unknown_scenario()),
        },
        Err(failure) => Err(failure),
    };
    result
        .map(|value| value.to_string())
        .map_err(|failure| JsError::new(&failure.to_string()))
}
