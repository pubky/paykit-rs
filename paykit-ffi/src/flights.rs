//! The host's `pay` calls in progress, and the answers that came after the wallet stopped
//! waiting (Paykit spec 7.2: the attempt ID is the idempotency key of one dispatch).
//!
//! A wallet command waits for the host's `pay` at most [`PAY_LIMIT`]. The host's call runs as
//! a task of its own, so it always runs to its end. An answer that comes after the limit is
//! kept here under its attempt ID and the wallet is asked to resume its payments, which
//! takes the kept answer through the method's lookup. An attempt whose `pay` is still running
//! is never dispatched again and never looked up at the host: both answer "pending", the
//! method's own answer for a payment in flight.
//! Everything here is in memory: after a restart the host's own lookup answers instead.
#[cfg(doc)]
use crate::runtime::PAY_LIMIT;
use crate::{MethodError, MethodOutcome};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::oneshot;

type Answer = Result<MethodOutcome, MethodError>;

/// What to do once a late answer is kept: queue a resume on the wallet that dispatched.
pub(crate) type Settle = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct State {
    /// Attempts whose `pay` is running, with the command waiting for it, if it still waits.
    running: HashMap<String, Option<oneshot::Sender<Answer>>>,
    /// Outcomes that came after their command stopped waiting, until a lookup takes them.
    late: HashMap<String, MethodOutcome>,
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
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `pay` as its own task and wait for it at most `limit` ([`PAY_LIMIT`] unless a
    /// test sets another).
    pub(crate) async fn dispatch<F>(
        &self,
        attempt: String,
        pay: F,
        limit: std::time::Duration,
        settle: Option<Settle>,
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
            state.running.insert(attempt.clone(), Some(waiter));
        }
        let call = Answering {
            flights: self.clone(),
            attempt: attempt.clone(),
            settle,
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
                    Some(waiting) => {
                        *waiting = None;
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

    /// Whether this attempt's `pay` is still running.
    pub(crate) fn running(&self, attempt: &str) -> bool {
        self.state().running.contains_key(attempt)
    }

    /// Take the outcome a late `pay` answered for this attempt, if one is kept.
    pub(crate) fn take_late(&self, attempt: &str) -> Option<MethodOutcome> {
        self.state().late.remove(attempt)
    }
}

/// The running `pay` of one attempt. Its answer goes to the waiting command, or is kept and
/// the wallet asked to resume. Dropped without an answer, it frees the attempt and asks too.
struct Answering {
    flights: Flights,
    attempt: String,
    settle: Option<Settle>,
    done: bool,
}

impl Answering {
    fn finish(mut self, answer: Answer) {
        self.done = true;
        let late = {
            let mut state = self.flights.state();
            let unsent = match state.running.remove(&self.attempt).flatten() {
                Some(waiter) => waiter.send(answer).err(),
                None => Some(answer),
            };
            match unsent {
                None => false,
                Some(answer) => {
                    // Only an outcome is kept: after an error the host's lookup decides.
                    if let Ok(outcome) = answer {
                        state.late.insert(self.attempt.clone(), outcome);
                    }
                    true
                }
            }
        };
        if late {
            self.settle();
        }
    }

    fn settle(&mut self) {
        if let Some(settle) = self.settle.take() {
            settle();
        }
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // The host's call ended without an answer: it panicked, or its runtime stopped. A
        // waiting command sees no answer; the resume asks the host's lookup.
        self.flights.state().running.remove(&self.attempt);
        self.settle();
    }
}
