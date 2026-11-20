//! The host's timer in a browser: [`JsTimer`], on `setTimeout`.

use js_sys::{Function, Promise};
use paykit_sdk::Timer;
use pubky_switch_client::transport::BoxFuture;
use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use crate::send::run_local;

// The global functions, not methods of `window`: a dedicated worker has no `window`, and
// `setTimeout`, `clearTimeout` and `performance` are globals in pages and workers alike.
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = setTimeout)]
    fn set_timeout(handler: &Function, ms: u32) -> JsValue;

    #[wasm_bindgen(js_name = clearTimeout)]
    fn clear_timeout(id: &JsValue);

    #[wasm_bindgen(js_namespace = performance, js_name = now)]
    fn performance_now() -> f64;
}

/// The longest delay `setTimeout` takes: `2^31 - 1` milliseconds, about 24.8 days.
///
/// Public, but hidden from the docs, only so the tests in `tests/host/` can check the steps of
/// a sleep: this crate's library has no test binary of its own.
///
/// Browsers store the delay as a signed 32-bit integer, and a longer one overflows: the timer
/// then fires at once instead of late.
#[doc(hidden)]
pub const MAX_TIMEOUT_MS: u32 = 2_147_483_647;

/// The delay of the next `setTimeout` for a sleep with `remaining_ms` milliseconds left, or
/// `None` once no time is left.
///
/// The delay is the remaining time rounded up to a whole millisecond, so a timer that keeps
/// its delay never wakes before the deadline, and at most [`MAX_TIMEOUT_MS`]: a longer sleep
/// takes several timeouts. Zero, a negative remainder and a remainder that is not a number
/// all give `None`.
///
/// Public, but hidden from the docs, only for the tests in `tests/host/`, as
/// [`MAX_TIMEOUT_MS`] is.
#[doc(hidden)]
pub fn next_timeout_ms(remaining_ms: f64) -> Option<u32> {
    if remaining_ms > 0.0 {
        // The value is at least 1 and at most `MAX_TIMEOUT_MS`, so the cast is exact.
        Some(remaining_ms.ceil().min(f64::from(MAX_TIMEOUT_MS)) as u32)
    } else {
        None
    }
}

/// A [`Timer`] on JavaScript's `setTimeout`, for the SDK on `wasm32-unknown-unknown`.
///
/// It works in a page and in a dedicated worker. A sleep never ends before its delay has
/// passed as `performance.now()`, the browser's monotonic clock, reads it. Where a browser
/// coarsens that clock, as some do to protect privacy, a sleep can end up to one step of it
/// early in real time. A sleep can end late: browsers throttle timers, most of all in hidden
/// pages. Dropping a sleep's future clears its pending
/// timeout. `sleep(0)` is ready on its first poll.
///
/// A sleep's delay counts from its first poll, not from the `sleep` call: the SDK creates its
/// wait for a payee's switch before the Homeserver's reads and waits on it only after them
/// (Paykit spec 4).
///
/// The delay is measured on `performance.now()`, not on the app's wall clock (such as
/// [`JsClock`](crate::clock::JsClock)), so a change of the system time does not shorten or
/// lengthen a sleep. Across such a change, the wall clock can show more or less time passed
/// than the sleep's delay.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsTimer;

impl Timer for JsTimer {
    fn sleep(&self, ms: u64) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            // The deadline is set at the first poll, and measured on `performance.now()`,
            // which is monotonic, not on the wall clock, which can jump. The conversion rounds
            // delays above 2^53 ms, hundreds of thousands of years, which changes nothing.
            let deadline = performance_now() + ms as f64;
            // A timeout can end before the deadline: a delay above `MAX_TIMEOUT_MS` takes
            // several, and an engine may fire a timer a fraction of a millisecond early by
            // `performance.now()`, whose resolution browsers coarsen. Throttled timers in a
            // hidden page fire late instead, which only makes the sleep longer. So the
            // deadline is checked after every timeout, and the sleep goes on until it has
            // passed. The first timeout is for the whole delay: `deadline` minus a new
            // reading of the same instant can come out a fraction above `ms` in floating
            // point, which would round the first step up by a millisecond.
            let mut remaining = ms as f64;
            while let Some(step) = next_timeout_ms(remaining) {
                // An error means the timeout task was dropped before it fired. The loop then
                // checks the deadline again, so the sleep still cannot end early.
                let _ = run_local(timeout(step)).await;
                remaining = deadline - performance_now();
            }
        })
    }
}

/// One `setTimeout` of `ms` milliseconds, which is cleared if this future is dropped before
/// it fires.
async fn timeout(ms: u32) {
    let mut id = JsValue::UNDEFINED;
    let fired = JsFuture::from(Promise::new(&mut |resolve, _reject| {
        id = set_timeout(&resolve, ms);
    }));
    let mut pending = PendingTimeout(Some(id));
    // The promise never rejects: the timeout only calls `resolve`.
    let _ = fired.await;
    // It fired: there is nothing left to clear.
    pending.0 = None;
}

/// The ID of a timeout that has not fired yet. Dropping it clears the timeout, so a dropped
/// sleep leaves no timer behind.
struct PendingTimeout(Option<JsValue>);

impl Drop for PendingTimeout {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            clear_timeout(&id);
        }
    }
}
