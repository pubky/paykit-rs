//! Bounded waits on the browser's timer. A panic aborts the WebAssembly instance, so every wait
//! in a network test has a bound, and a test fails rather than hangs.

use std::future::{poll_fn, Future};
use std::pin::{pin, Pin};
use std::task::Poll;

use paykit_browser::timer::JsTimer;
use paykit_sdk::Timer;

/// How often a bounded wait reads again, in milliseconds.
pub const POLL_MS: u64 = 50;

/// The wall clock, in milliseconds, to measure how long a call took.
pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Sleeps `ms` milliseconds on the browser's timer.
pub async fn sleep(ms: u64) {
    JsTimer.sleep(ms).await;
}

/// Polls `future` until it finishes or `ms` milliseconds pass, whichever comes first: its
/// output, or `None` when the time passed first. An unfinished `future` is left as it is, for
/// the caller to drop or poll again.
pub async fn race<F>(future: &mut F, ms: u64) -> Option<F::Output>
where
    F: Future + Unpin,
{
    let mut timer = JsTimer.sleep(ms);
    poll_fn(|cx| {
        if let Poll::Ready(output) = Pin::new(&mut *future).poll(cx) {
            return Poll::Ready(Some(output));
        }
        timer.as_mut().poll(cx).map(|()| None)
    })
    .await
}

/// Runs `a` and `b` at once until both finish or `ms` milliseconds pass: the output of each
/// one that finished in time.
pub async fn both<A, B>(a: A, b: B, ms: u64) -> (Option<A::Output>, Option<B::Output>)
where
    A: Future,
    B: Future,
{
    let mut a = pin!(a);
    let mut b = pin!(b);
    let (mut out_a, mut out_b) = (None, None);
    let mut timer = JsTimer.sleep(ms);
    poll_fn(|cx| {
        if out_a.is_none() {
            if let Poll::Ready(output) = a.as_mut().poll(cx) {
                out_a = Some(output);
            }
        }
        if out_b.is_none() {
            if let Poll::Ready(output) = b.as_mut().poll(cx) {
                out_b = Some(output);
            }
        }
        if out_a.is_some() && out_b.is_some() {
            return Poll::Ready(());
        }
        timer.as_mut().poll(cx)
    })
    .await;
    (out_a, out_b)
}

/// Reads with `read` until `done` holds for what it read or `bound_ms` milliseconds pass,
/// [`POLL_MS`] apart, and returns the last value read, for the caller to check.
pub async fn wait_until<T, F>(
    bound_ms: u64,
    mut read: impl FnMut() -> F,
    done: impl Fn(&T) -> bool,
) -> T
where
    F: Future<Output = T>,
{
    let deadline = now_ms() + bound_ms as f64;
    loop {
        let value = read().await;
        if done(&value) || now_ms() >= deadline {
            return value;
        }
        sleep(POLL_MS).await;
    }
}
