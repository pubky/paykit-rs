//! `JsClock`: the Switch client's clock on `Date.now()`.

use paykit_browser::clock::JsClock;
use pubky_switch_client::clock::Clock;
use wasm_bindgen_test::wasm_bindgen_test;

/// 2026-01-01T00:00:00Z in Unix milliseconds.
const START_OF_2026_MS: u64 = 1_767_225_600_000;

/// The clock reads `Date.now()`: its reading lies between two `Date.now()` calls around it,
/// and is a current Unix time in milliseconds, not zero or a time in seconds.
#[wasm_bindgen_test]
fn test_clock_agrees_with_date_now() {
    let before = js_sys::Date::now() as u64;
    let now = JsClock.now_ms();
    let after = js_sys::Date::now() as u64;
    assert!(
        before <= now && now <= after,
        "{before} <= {now} <= {after}"
    );
    assert!(now > START_OF_2026_MS, "{now} is not a current time");
}
