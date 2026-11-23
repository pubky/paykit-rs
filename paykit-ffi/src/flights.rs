//! The host's `pay` calls in progress in this process, and the answers that came after the
//! wallet stopped waiting (Paykit spec 7.2: the attempt ID is the idempotency key of one
//! dispatch). Attempt IDs are random, so one record serves every wallet and every
//! `WalletHost` in the process.
//!
//! A wallet command waits for the host's `pay` at most its limit. The host's call runs as a
//! task of its own, so it always runs to its end. While it runs, the attempt is never
//! dispatched again and the host's lookup is not asked: both answer "pending", the method's
//! own answer for a payment in flight. When the host's `pay` answers after the limit, the
//! wallet open for that App ID resumes its payments. A paid or failed outcome the SDK can read
//! is kept under its attempt ID, so that resume takes it without asking the host; it stays kept,
//! and the resume is retried, until a resume completes, or the next wallet to open for that App
//! ID records it. A `pay` that never answers stops counting as in
//! flight after [`STALE_AFTER`] times its limit: the host's lookup, which answers "pending"
//! for any attempt `pay` was called with and that has not settled, then decides. Everything
//! here is in memory: after a restart the host's lookup answers instead.
use crate::{MethodError, MethodOutcome};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};
use tokio::{sync::oneshot, time::Instant};

type Answer = Result<MethodOutcome, MethodError>;

/// An App ID: the wallet, and the store namespace, an attempt belongs to.
pub(crate) type AppId = [u8; 16];

/// A running `pay` counts as in flight for this many times its limit.
pub(crate) const STALE_AFTER: u32 = 4;

/// A way to ask an open wallet to resume its payments.
pub(crate) type Wake = Arc<dyn Fn() + Send + Sync>;

struct Running {
    app: Option<AppId>,
    stale_at: Instant,
    /// The command waiting for the answer, while it still waits.
    waiting: Option<oneshot::Sender<Answer>>,
}

#[derive(Default)]
struct State {
    running: HashMap<String, Running>,
    /// Outcomes that came after their command stopped waiting, until the journal records them.
    late: HashMap<String, (Option<AppId>, MethodOutcome)>,
    /// The wallets open in this process, by App ID.
    open: HashMap<AppId, Wake>,
}

#[derive(Clone, Default)]
pub(crate) struct Flights(Arc<Mutex<State>>);

/// The result of waiting for one dispatch.
pub(crate) enum Waited {
    Answered(Answer),
    /// The limit passed: the payment may have been sent, and its answer will be kept.
    StillRunning,
    /// The same attempt's `pay` is already running.
    AlreadyRunning,
}

impl Flights {
    /// The record every `WalletHost` in this process shares.
    pub(crate) fn process() -> Self {
        static PROCESS: LazyLock<Flights> = LazyLock::new(Flights::default);
        PROCESS.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `pay` for `app`'s attempt as its own task and wait for it at most `limit`.
    pub(crate) async fn dispatch<F>(
        &self,
        attempt: String,
        app: Option<AppId>,
        pay: F,
        limit: Duration,
    ) -> Waited
    where
        F: std::future::Future<Output = Answer> + Send + 'static,
    {
        let (waiter, mut answered) = oneshot::channel();
        {
            let mut state = self.state();
            if state.running.contains_key(&attempt) {
                return Waited::AlreadyRunning;
            }
            let stale_at = Instant::now() + limit.saturating_mul(STALE_AFTER);
            let running = Running {
                app,
                stale_at,
                waiting: Some(waiter),
            };
            state.running.insert(attempt.clone(), running);
        }
        let call = Answering {
            flights: self.clone(),
            attempt: attempt.clone(),
            done: false,
        };
        // The wallet worker runs on the bindings' runtime, so the task does too.
        tokio::spawn(async move { call.finish(pay.await) });
        match tokio::time::timeout(limit, &mut answered).await {
            Ok(Ok(answer)) => Waited::Answered(answer),
            _ => {
                let mut state = self.state();
                match state.running.get_mut(&attempt) {
                    // Still running: stop waiting, so its answer is kept when it comes.
                    Some(running) => {
                        running.waiting = None;
                        Waited::StillRunning
                    }
                    // It answered while the limit passed.
                    None => match answered.try_recv() {
                        Ok(answer) => Waited::Answered(answer),
                        Err(_) => Waited::StillRunning,
                    },
                }
            }
        }
    }

    /// Whether this attempt's `pay` is running and still counts as in flight.
    pub(crate) fn in_flight(&self, attempt: &str) -> bool {
        self.state()
            .running
            .get(attempt)
            .is_some_and(|running| Instant::now() < running.stale_at)
    }

    /// The outcome a late `pay` answered for this attempt, kept until it is recorded.
    pub(crate) fn late(&self, attempt: &str) -> Option<MethodOutcome> {
        self.state()
            .late
            .get(attempt)
            .map(|(_, outcome)| outcome.clone())
    }

    /// The attempts of `app` whose late outcome is kept.
    pub(crate) fn late_of(&self, app: &AppId) -> Vec<String> {
        let state = self.state();
        let late = state.late.iter();
        late.filter(|(_, (owner, _))| owner.as_ref() == Some(app))
            .map(|(attempt, _)| attempt.clone())
            .collect()
    }

    /// The journal has recorded this attempt's outcome: its late answer is no longer needed.
    pub(crate) fn recorded(&self, attempt: &str) {
        self.state().late.remove(attempt);
    }

    /// Register the open wallet for `app`, so a late answer of its attempts wakes it.
    pub(crate) fn open(&self, app: AppId, wake: Wake) {
        self.state().open.insert(app, wake);
    }

    /// The wallet for `app` that registered `wake` has closed.
    pub(crate) fn close(&self, app: &AppId, wake: &Wake) {
        let mut state = self.state();
        if state
            .open
            .get(app)
            .is_some_and(|open| Arc::ptr_eq(open, wake))
        {
            state.open.remove(app);
        }
    }

    fn wake(&self, app: Option<AppId>) {
        let wake = app.and_then(|app| self.state().open.get(&app).cloned());
        if let Some(wake) = wake {
            wake();
        }
    }
}

/// The running `pay` of one attempt. Its answer goes to the waiting command, or a paid or
/// failed outcome is kept and the attempt's wallet asked to resume. Dropped without an answer,
/// it frees the attempt and asks too.
struct Answering {
    flights: Flights,
    attempt: String,
    done: bool,
}

impl Answering {
    fn finish(mut self, answer: Answer) {
        self.done = true;
        let mut state = self.flights.state();
        let Some(running) = state.running.remove(&self.attempt) else {
            return;
        };
        let app = running.app;
        let unsent = match running.waiting {
            Some(waiter) => waiter.send(answer).err(),
            None => Some(answer),
        };
        let Some(answer) = unsent else {
            return;
        };
        // Only a definitive outcome the SDK can read, for a wallet that can record it, is kept:
        // otherwise the host's lookup decides, as for any open attempt. The wallet resumes its
        // payments either way.
        if let (Some(owner), Ok(outcome @ (MethodOutcome::Paid { .. } | MethodOutcome::Failed))) =
            (app, answer)
        {
            if outcome.clone().into_rail().is_ok() {
                state
                    .late
                    .insert(self.attempt.clone(), (Some(owner), outcome));
            }
        }
        drop(state);
        self.flights.wake(app);
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // The host's call ended without an answer: it panicked, or its runtime stopped. A
        // waiting command sees no answer; the wallet's resume asks the host's lookup.
        let app = self
            .flights
            .state()
            .running
            .remove(&self.attempt)
            .and_then(|running| running.app);
        self.flights.wake(app);
    }
}
