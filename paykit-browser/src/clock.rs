//! The host's wall clock in a browser: [`JsClock`].

use pubky_switch_client::clock::Clock;

/// A [`Clock`] that reads JavaScript's `Date.now()`, for the Switch client and the SDK on
/// `wasm32-unknown-unknown`.
///
/// It is the browser's wall clock, in Unix milliseconds. It is not monotonic: it jumps when the
/// user or the system changes the time, backwards as well as forwards. The Switch client
/// expects a wall clock of this kind and handles a backwards jump itself.
///
/// Use it instead of the Switch client's `SystemClock` on wasm32: `SystemClock` reads
/// `std::time::SystemTime::now()`, which panics on `wasm32-unknown-unknown`, where the
/// standard library has no clock.
///
/// It works in a page and in a dedicated worker.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsClock;

impl Clock for JsClock {
    fn now_ms(&self) -> u64 {
        // `Date.now()` is a whole number of milliseconds. The cast saturates: a clock set
        // before 1970 reads as 0, as it does with `SystemClock`.
        js_sys::Date::now() as u64
    }
}
