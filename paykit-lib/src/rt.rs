//! THROWAWAY (#411): portable async runtime shim (time + task spawning).
//!
//! Native targets re-export tokio unchanged, so native behaviour (including
//! tokio's paused test clock) is identical to before this module existed.
//!
//! wasm32-unknown-unknown cannot run tokio's timer driver or task scheduler:
//! `std::time::Instant::now()` panics ("time not implemented on this
//! platform"), `tokio::spawn` panics ("there is no reactor running"), and a
//! current-thread runtime cannot park on the browser main thread. There:
//!
//! - [`Instant`] is `web_time::Instant` (`performance.now()`).
//! - [`sleep`] / [`sleep_until`] use `setTimeout` through `gloo-timers` and
//!   re-check the deadline after every wake, because `setTimeout` clamps
//!   delays above `i32::MAX` ms and browsers may fire throttled timers late.
//! - [`timeout`] / [`timeout_at`] race the future against [`sleep_until`].
//! - [`Interval`] re-implements the subset of `tokio::time::Interval` Paykit
//!   uses (`tick`, `Delay` and `Skip` missed-tick behaviour).
//! - [`spawn`] uses `wasm_bindgen_futures::spawn_local` (the JS microtask
//!   queue) and returns a [`JoinHandle`] backed by a oneshot plus a
//!   `futures_util` `AbortHandle`. Dropping it detaches the task, like tokio.
//!
//! Production note: this is the shape a real `rt` module would take, but the
//! wasm half has only been exercised by the scratch smoke test noted in the
//! spike report, not by a browser CI suite.

#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{spawn, AbortHandle, JoinError, JoinHandle};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{
    error::Elapsed, interval, interval_at, sleep, sleep_until, timeout, timeout_at, Instant,
    Interval, MissedTickBehavior,
};

#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(target_arch = "wasm32")]
mod wasm {
    use std::{
        fmt,
        future::Future,
        pin::Pin,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        task::{Context, Poll},
        time::Duration,
    };

    pub use web_time::Instant;

    /// Largest delay `setTimeout` honours; larger values fire immediately.
    const MAX_TIMEOUT_MS: u128 = i32::MAX as u128;

    /// Sleep until `deadline` (re-arming `setTimeout` until it has passed).
    pub async fn sleep_until(deadline: Instant) {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let remaining = deadline - now;
            // Round up so a sub-millisecond remainder does not spin with 0 ms timers.
            let millis = remaining
                .as_micros()
                .div_ceil(1000)
                .clamp(1, MAX_TIMEOUT_MS);
            gloo_timers::future::TimeoutFuture::new(millis as u32).await;
        }
    }

    /// Sleep for `duration`.
    pub async fn sleep(duration: Duration) {
        match Instant::now().checked_add(duration) {
            Some(deadline) => sleep_until(deadline).await,
            // Effectively infinite.
            None => std::future::pending().await,
        }
    }

    /// Error returned by [`timeout`] / [`timeout_at`] when the deadline passed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Elapsed(());

    impl fmt::Display for Elapsed {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("deadline has elapsed")
        }
    }

    impl std::error::Error for Elapsed {}

    /// Run `future` until `deadline`. Like tokio, the future is polled first,
    /// so an already-ready future wins over an already-passed deadline.
    pub async fn timeout_at<F: Future>(deadline: Instant, future: F) -> Result<F::Output, Elapsed> {
        let mut future = std::pin::pin!(future);
        let mut timer = std::pin::pin!(sleep_until(deadline));
        std::future::poll_fn(|cx| {
            if let Poll::Ready(value) = future.as_mut().poll(cx) {
                return Poll::Ready(Ok(value));
            }
            timer.as_mut().poll(cx).map(|()| Err(Elapsed(())))
        })
        .await
    }

    /// Run `future` for at most `duration`.
    pub async fn timeout<F: Future>(duration: Duration, future: F) -> Result<F::Output, Elapsed> {
        match Instant::now().checked_add(duration) {
            Some(deadline) => timeout_at(deadline, future).await,
            None => Ok(future.await),
        }
    }

    /// Subset of `tokio::time::MissedTickBehavior` used by Paykit.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum MissedTickBehavior {
        /// Fire missed ticks back to back.
        #[default]
        Burst,
        /// Schedule the next tick one period after the late tick fired.
        Delay,
        /// Skip missed ticks and keep the original phase.
        Skip,
    }

    /// Minimal `tokio::time::Interval` replacement. `tick` is cancel-safe:
    /// dropping a pending `tick` leaves the schedule unchanged.
    #[derive(Debug)]
    pub struct Interval {
        next: Instant,
        period: Duration,
        missed_tick_behavior: MissedTickBehavior,
    }

    /// Interval whose first tick completes immediately.
    pub fn interval(period: Duration) -> Interval {
        interval_at(Instant::now(), period)
    }

    /// Interval whose first tick completes at `start`.
    ///
    /// # Panics
    /// Panics if `period` is zero, matching tokio.
    pub fn interval_at(start: Instant, period: Duration) -> Interval {
        assert!(period > Duration::ZERO, "`period` must be non-zero.");
        Interval {
            next: start,
            period,
            missed_tick_behavior: MissedTickBehavior::Burst,
        }
    }

    impl Interval {
        /// Set the missed-tick behaviour.
        pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
            self.missed_tick_behavior = behavior;
        }

        /// Wait for the next tick and return its scheduled instant.
        pub async fn tick(&mut self) -> Instant {
            let scheduled = self.next;
            sleep_until(scheduled).await;
            let now = Instant::now();
            // tokio treats a tick as "missed" only when it is more than 5 ms late.
            let late = now.saturating_duration_since(scheduled) > Duration::from_millis(5);
            let next = if !late {
                scheduled.checked_add(self.period)
            } else {
                match self.missed_tick_behavior {
                    MissedTickBehavior::Burst => scheduled.checked_add(self.period),
                    MissedTickBehavior::Delay => now.checked_add(self.period),
                    MissedTickBehavior::Skip => {
                        let behind = now.saturating_duration_since(scheduled).as_nanos();
                        let periods = behind / self.period.as_nanos() + 1;
                        u32::try_from(periods)
                            .ok()
                            .and_then(|periods| self.period.checked_mul(periods))
                            .and_then(|offset| scheduled.checked_add(offset))
                    }
                }
            };
            // An overflowing schedule never fires again, as with a far-future tokio deadline.
            self.next = next.unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365 * 30));
            scheduled
        }
    }

    /// Suspend-aware elapsed time for write-lock leases (see the THROWAWAY
    /// note on `lease_time` in `pubky_routing/locks.rs`). Each reading adds the
    /// larger forward step of `performance.now()` and `Date.now()`, so the
    /// result never decreases, never runs slower than `performance.now()`, and
    /// still counts device sleep when only the wall clock saw it.
    pub fn lease_clock() -> std::io::Result<Duration> {
        use std::cell::Cell;
        // (performance.now() reading, Date.now() ms, accumulated lease time)
        thread_local! {
            static LAST: Cell<Option<(Instant, f64, Duration)>> = const { Cell::new(None) };
        }
        let monotonic = Instant::now();
        let wall_ms = js_sys::Date::now();
        if !wall_ms.is_finite() {
            return Err(std::io::Error::other("invalid wall clock reading"));
        }
        Ok(LAST.with(|last| {
            let elapsed = match last.get() {
                None => Duration::ZERO,
                Some((previous_monotonic, previous_wall_ms, elapsed)) => {
                    let monotonic_step = monotonic.saturating_duration_since(previous_monotonic);
                    // Overflow saturates, which expires every lease (fails closed).
                    let wall_step =
                        Duration::try_from_secs_f64((wall_ms - previous_wall_ms).max(0.0) / 1000.0)
                            .unwrap_or(Duration::MAX);
                    elapsed.saturating_add(monotonic_step.max(wall_step))
                }
            };
            last.set(Some((monotonic, wall_ms, elapsed)));
            elapsed
        }))
    }

    /// Why a task did not produce a value.
    #[derive(Debug)]
    pub struct JoinError(());

    impl JoinError {
        /// Whether the task was aborted (the only failure mode on wasm32;
        /// a panic traps the whole wasm instance because wasm32 uses panic=abort).
        pub fn is_cancelled(&self) -> bool {
            true
        }
    }

    impl fmt::Display for JoinError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("task was cancelled")
        }
    }

    impl std::error::Error for JoinError {}

    /// Handle to abort a spawned task or check whether it finished.
    #[derive(Clone, Debug)]
    pub struct AbortHandle {
        abort: futures_util::future::AbortHandle,
        finished: Arc<AtomicBool>,
    }

    impl AbortHandle {
        /// Request cancellation. The task is dropped at its next poll.
        pub fn abort(&self) {
            self.abort.abort();
        }

        /// Whether the task has completed or been cancelled.
        pub fn is_finished(&self) -> bool {
            self.finished.load(Ordering::Acquire)
        }
    }

    /// Owned handle to a spawned task. Dropping it detaches the task.
    #[derive(Debug)]
    pub struct JoinHandle<T> {
        result: tokio::sync::oneshot::Receiver<Result<T, futures_util::future::Aborted>>,
        abort: AbortHandle,
    }

    impl<T> JoinHandle<T> {
        /// Request cancellation.
        pub fn abort(&self) {
            self.abort.abort();
        }

        /// Whether the task has completed or been cancelled.
        pub fn is_finished(&self) -> bool {
            self.abort.is_finished()
        }

        /// Clone out an [`AbortHandle`].
        pub fn abort_handle(&self) -> AbortHandle {
            self.abort.clone()
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.result)
                .poll(cx)
                .map(|received| match received {
                    Ok(Ok(value)) => Ok(value),
                    // Aborted, or the sender vanished (only on abort, since a
                    // panic aborts the whole instance before the drop runs).
                    Ok(Err(_)) | Err(_) => Err(JoinError(())),
                })
        }
    }

    /// Spawn `future` on the JS event loop. No `Send` bound: wasm32 is single
    /// threaded and browser futures (fetch, streams) are `!Send`.
    pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (sender, result) = tokio::sync::oneshot::channel();
        let (abort, registration) = futures_util::future::AbortHandle::new_pair();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = Arc::clone(&finished);
        wasm_bindgen_futures::spawn_local(async move {
            let output = futures_util::future::Abortable::new(future, registration).await;
            task_finished.store(true, Ordering::Release);
            // The JoinHandle may have been dropped (detached task).
            let _ = sender.send(output);
        });
        JoinHandle {
            result,
            abort: AbortHandle { abort, finished },
        }
    }
}
