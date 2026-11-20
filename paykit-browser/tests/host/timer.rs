//! `JsTimer`: the SDK's timer on `setTimeout`.

use paykit_browser::timer::{next_timeout_ms, JsTimer, MAX_TIMEOUT_MS};
use paykit_sdk::Timer;
use wasm_bindgen_test::wasm_bindgen_test;

use crate::support::{next_task, performance_now, poll_once, TimeoutSpy};

/// How much longer than its delay a sleep may take before a test fails. Generous: hosted CI
/// runners can be slow, and a late timer is allowed. An early one is not.
const SLACK_MS: f64 = 5_000.0;

/// A sleep lasts at least its delay on `performance.now()`, and not much longer.
#[wasm_bindgen_test]
async fn test_timer_sleep_lasts_at_least_its_delay() {
    for ms in [1, 20, 100] {
        let before = performance_now();
        JsTimer.sleep(ms).await;
        let elapsed = performance_now() - before;
        assert!(elapsed >= ms as f64, "sleep({ms}) ended after {elapsed} ms");
        assert!(
            elapsed < ms as f64 + SLACK_MS,
            "sleep({ms}) took {elapsed} ms"
        );
    }
}

/// A sleep's delay counts from its first poll, not from the `sleep` call: a sleep created
/// well before it is awaited still lasts its whole delay once awaited.
#[wasm_bindgen_test]
async fn test_timer_sleep_counts_from_its_first_poll() {
    let sleep = JsTimer.sleep(100);
    JsTimer.sleep(150).await;
    let before = performance_now();
    sleep.await;
    let elapsed = performance_now() - before;
    assert!(
        elapsed >= 100.0,
        "sleep(100) ended {elapsed} ms after its first poll"
    );
    assert!(elapsed < 100.0 + SLACK_MS, "sleep(100) took {elapsed} ms");
}

/// `sleep(0)` sets no timeout: it is ready on its first poll.
#[wasm_bindgen_test]
fn test_timer_sleep_zero_is_ready_at_once() {
    let mut sleep = JsTimer.sleep(0);
    assert!(poll_once(sleep.as_mut()).is_ready());
}

/// When the browser fires timeouts early, the sleep sets another timeout for the rest of its
/// delay, and still ends no earlier than its delay. A timeout that fired is not cleared.
#[wasm_bindgen_test]
async fn test_timer_sleep_outlasts_early_timeouts() {
    // Each timeout of 50 to 1000 ms fires after a quarter of its delay.
    let spy = TimeoutSpy::install(50, 1_000, 0.25);
    let before = performance_now();
    JsTimer.sleep(400).await;
    let elapsed = performance_now() - before;
    let timeouts = spy.timeouts();
    drop(spy);

    assert!(elapsed >= 400.0, "sleep(400) ended after {elapsed} ms");
    assert!(elapsed < 400.0 + SLACK_MS, "sleep(400) took {elapsed} ms");
    // The first timeout is for the whole delay; after it fires early, the next ones are for
    // the time left, until a remainder below 50 ms runs unscaled.
    assert!(timeouts.len() >= 2, "{timeouts:?}");
    let first = timeouts[0]["ms"].as_u64().unwrap();
    assert!((399..=400).contains(&first), "{timeouts:?}");
    for timeout in &timeouts {
        assert_eq!(timeout["fired"], true, "{timeouts:?}");
        assert_eq!(timeout["cleared"], false, "{timeouts:?}");
    }
}

/// Dropping a pending sleep clears its timeout, so it never fires.
#[wasm_bindgen_test]
async fn test_timer_dropped_sleep_clears_its_timeout() {
    let spy = TimeoutSpy::install(4_000, 6_000, 1.0);
    let mut sleep = JsTimer.sleep(5_000);
    assert!(poll_once(sleep.as_mut()).is_pending());
    // The sleep's timeout task runs and sets its timeout.
    next_task().await;
    let timeouts = spy.timeouts();
    assert_eq!(timeouts.len(), 1, "{timeouts:?}");
    assert_eq!(timeouts[0]["fired"], false, "{timeouts:?}");
    assert_eq!(timeouts[0]["cleared"], false, "{timeouts:?}");

    drop(sleep);
    // The cancelled timeout task is dropped, and clears its timeout.
    next_task().await;
    let timeouts = spy.timeouts();
    assert_eq!(timeouts.len(), 1, "{timeouts:?}");
    assert_eq!(timeouts[0]["fired"], false, "{timeouts:?}");
    assert_eq!(timeouts[0]["cleared"], true, "{timeouts:?}");
}

/// A step covers the whole remaining time, rounded up to a whole millisecond.
#[wasm_bindgen_test]
fn test_timer_step_rounds_up_to_whole_milliseconds() {
    assert_eq!(next_timeout_ms(100.0), Some(100));
    assert_eq!(next_timeout_ms(99.2), Some(100));
    assert_eq!(next_timeout_ms(0.001), Some(1));
}

/// Once the deadline is reached there is no step.
#[wasm_bindgen_test]
fn test_timer_step_none_once_the_deadline_passed() {
    for remaining in [0.0, -0.0, -0.5, -1e12, f64::NAN] {
        assert_eq!(next_timeout_ms(remaining), None, "{remaining}");
    }
}

/// A step never exceeds `setTimeout`'s limit of `2^31 - 1` ms, which would overflow and fire
/// at once.
#[wasm_bindgen_test]
fn test_timer_step_caps_delays_above_the_set_timeout_limit() {
    assert_eq!(MAX_TIMEOUT_MS, i32::MAX as u32);
    let max = f64::from(MAX_TIMEOUT_MS);
    assert_eq!(next_timeout_ms(max - 0.5), Some(MAX_TIMEOUT_MS));
    assert_eq!(next_timeout_ms(max), Some(MAX_TIMEOUT_MS));
    assert_eq!(next_timeout_ms(max + 1.0), Some(MAX_TIMEOUT_MS));
    assert_eq!(next_timeout_ms(u64::MAX as f64), Some(MAX_TIMEOUT_MS));
    assert_eq!(next_timeout_ms(f64::INFINITY), Some(MAX_TIMEOUT_MS));
}

/// A 60-day sleep, on a clock that moves by exactly each timeout's delay, takes two timeouts
/// at the limit and one for the rest, and ends exactly at its deadline.
#[wasm_bindgen_test]
fn test_timer_step_long_sleep_takes_bounded_steps() {
    let total_ms: u64 = 60 * 24 * 60 * 60 * 1_000;
    let mut remaining = total_ms as f64;
    let mut steps = Vec::new();
    while let Some(step) = next_timeout_ms(remaining) {
        steps.push(step);
        remaining -= f64::from(step);
    }
    let rest = u32::try_from(total_ms - 2 * u64::from(MAX_TIMEOUT_MS)).unwrap();
    assert_eq!(steps, [MAX_TIMEOUT_MS, MAX_TIMEOUT_MS, rest]);
    assert_eq!(remaining, 0.0);
}
