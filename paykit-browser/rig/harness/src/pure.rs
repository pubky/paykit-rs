//! The scenarios with no network: `pure`, and `panic`.

use std::cmp::Ordering;

use js_sys::Reflect;
use paykit_browser::clock::JsClock;
use paykit_browser::timer::JsTimer;
use paykit_lib::grant::{EXECUTOR_GRANT_LABEL, LEDGER_HANDOFF_LABEL};
use paykit_sdk::Timer;
use pubky_switch_client::clock::Clock;
use pubky_switch_core::keys::{AppKeys, IdentityKey};
use pubky_switch_core::sealed::{RecordValue, SealedValue};
use pubky_switch_core::sig::verify;
use serde_json::json;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::Performance;

use crate::failure::{ensure, Failure, Outcome};

/// How long the `JsTimer` sleep of `pure` lasts, in milliseconds.
const SLEEP_MS: u64 = 20;

/// Unix milliseconds of 2020-01-01: a wall clock that reads earlier is not set.
const CLOCK_FLOOR_MS: u64 = 1_577_836_800_000;

/// `pure`, with no arguments:
///
/// - an app's signature over Paykit's fields verifies under its key and label, and not under
///   another label (Switch wire 2.1, Paykit wire 5.4);
/// - a record value sealed under a scope key opens to the same JSON (Switch wire 6.2);
/// - `JsClock` reads a set wall clock;
/// - a `JsTimer` sleep of 20 ms lasts at least that long by `performance.now()`.
///
/// Returns `{"signature": "verified", "sealed": "opened", "clock_ms", "sleep_ms",
/// "slept_ms"}`.
pub async fn run() -> Outcome {
    let app = AppKeys::generate().map_err(|_| Failure::assertion("an app key is made"))?;
    let fields: [&[u8]; 2] = [b"identity", b"counterparty"];
    let signature = app.sign(EXECUTOR_GRANT_LABEL, &fields);
    let public = app.signing_public();
    ensure!(
        verify(&public, EXECUTOR_GRANT_LABEL, &fields, &signature).is_ok(),
        "the signature verifies under its key and label"
    );
    ensure!(
        verify(&public, LEDGER_HANDOFF_LABEL, &fields, &signature).is_err(),
        "the signature does not verify under another label"
    );

    let identity =
        IdentityKey::generate().map_err(|_| Failure::assertion("an identity key is made"))?;
    let owner = identity.public();
    let scope = "paykit_pay";
    let key = identity.scope_key(1, scope);
    let record = [7u8; 32];
    let value = br#"{"note":"from the harness"}"#;
    let sealed = SealedValue::seal(&key, &owner, 1, scope, &record, value)
        .map_err(|_| Failure::assertion("the record value seals"))?;
    let opened = sealed
        .open(&key, &owner, &record)
        .map_err(|_| Failure::assertion("the sealed record value opens"))?;
    ensure!(
        opened == RecordValue::Json(value.to_vec()),
        "the sealed record value opens to the JSON sealed"
    );

    let clock_ms = JsClock.now_ms();
    ensure!(clock_ms >= CLOCK_FLOOR_MS, "JsClock reads a set wall clock");

    let performance = performance()?;
    let before = performance.now();
    JsTimer.sleep(SLEEP_MS).await;
    let slept_ms = performance.now() - before;
    // A reading that is not a number fails too.
    ensure!(
        slept_ms
            .partial_cmp(&(SLEEP_MS as f64))
            .is_some_and(Ordering::is_ge),
        "a sleep of {SLEEP_MS} ms lasted {slept_ms} ms"
    );

    Ok(json!({
        "signature": "verified",
        "sealed": "opened",
        "clock_ms": clock_ms,
        "sleep_ms": SLEEP_MS,
        "slept_ms": slept_ms,
    }))
}

/// `panic`: sleeps a millisecond on `JsTimer`, then panics, so the panic happens in a task
/// the browser runs after the call returned its promise. The promise never settles, and the
/// worker's global scope gets an uncaught error. The WebAssembly instance is not to be used
/// again afterwards.
pub async fn panic_after_await() -> Outcome {
    JsTimer.sleep(1).await;
    panic!("the panic scenario panics on purpose after an await");
}

/// The global `performance`, read through `Reflect`, so this works in a page and in a worker.
fn performance() -> Result<Performance, Failure> {
    Reflect::get(&js_sys::global(), &JsValue::from_str("performance"))
        .ok()
        .and_then(|value| value.dyn_into::<Performance>().ok())
        .ok_or_else(|| Failure::assertion("the global scope has performance"))
}
