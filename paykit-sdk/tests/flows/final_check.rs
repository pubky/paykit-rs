//! The final payment check (Paykit spec 4, 7.2): the last thing before the payment method's
//! call, on a first dispatch, a resume and a recovery. A late switch answer that removes the
//! destination's app, or a payee copy that aged past its limit meanwhile, pays nothing; nor
//! does one that comes during the journal write of `Paying`, which the fence after it sees.

use std::sync::{Arc, Mutex};

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitConfig, RailDeclaration, RequestRef};
use pubky_switch_client::app::HomeserverReader;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::{StoreError, TransportError};
use pubky_switch_client::store::{KeyValue, MemoryStore, StateStore, Write, WriteBatch};
use pubky_switch_client::transport::{BoxFuture, Transport};
use pubky_switch_core::records::{RemovedApp, IDENTITY_PATH};

use crate::common::{draft, request_from, SlowSwitch, World, BOB_ORIGIN};

type Hook = Box<dyn FnOnce() + Send>;

/// One scripted read: the identity, its delay on the clock, and a hook run during it.
type Step = ([u8; 32], u64, Option<Hook>);

/// A store that runs a hook once, right after it commits the journal entry that holds the
/// signed acceptance: the last journal write before the final payment check. Armed, it
/// fails the commit after that one, applying nothing. It records the `msg_id` of every
/// acceptance journaled, to show the outbox never took one. It can
/// also run a hook during the commit that journals `Paying`, after the final payment check
/// and before the payment method's call, and then stop, as a process that died: every
/// later commit fails, applying nothing.
#[derive(Default)]
struct HookedStore {
    inner: MemoryStore,
    hook: Mutex<Option<Hook>>,
    fail_queueing: std::sync::atomic::AtomicBool,
    queueing_next: std::sync::atomic::AtomicBool,
    paying_hook: Mutex<Option<Hook>>,
    attempted_hook: Mutex<Option<Hook>>,
    stop_at_outbox: std::sync::atomic::AtomicBool,
    refuse_acceptances: std::sync::atomic::AtomicBool,
    stop_after_paying: std::sync::atomic::AtomicBool,
    stopped: std::sync::atomic::AtomicBool,
    acceptances: Mutex<Vec<String>>,
}

impl HookedStore {
    fn on_acceptance_journaled(&self, hook: Hook) {
        *self.hook.lock().unwrap() = Some(hook);
    }

    /// Run `hook` during the commit that journals `Attempted`: the local commit of the
    /// attempt's answer.
    fn on_attempted(&self, hook: Hook) {
        *self.attempted_hook.lock().unwrap() = Some(hook);
    }

    fn on_paying(&self, hook: Hook) {
        *self.paying_hook.lock().unwrap() = Some(hook);
    }

    fn stop_after_paying(&self) {
        self.stop_after_paying
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop, as a process that died, at the next commit that queues a message (the client's
    /// `out/` items): that commit fails, applying nothing, and every later one too.
    fn stop_at_outbox(&self) {
        self.stop_at_outbox
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Every commit that queues an acceptance fails, applying nothing, while `refuse` holds;
    /// the proof and every other message still queue.
    fn refuse_acceptances(&self, refuse: bool) {
        self.refuse_acceptances
            .store(refuse, std::sync::atomic::Ordering::SeqCst);
    }

    /// The process starts again on what was committed.
    fn restart(&self) {
        self.stopped
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// The `msg_id` of every acceptance a journal write held.
    fn acceptances(&self) -> Vec<String> {
        self.acceptances.lock().unwrap().clone()
    }

    fn fail_commit_after_acceptance(&self) {
        self.fail_queueing
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl StateStore for HookedStore {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        const SIGNED: &[u8] = b"\"acceptance_msg_id\":\"";
        const PAYING: &[u8] = b"\"stage\":\"paying\"";
        let journals = |marker: &[u8]| {
            batch.writes.iter().any(|write| match write {
                Write::Put { key, value, .. } => {
                    key.starts_with(b"p/j/") && value.windows(marker.len()).any(|w| w == marker)
                }
                _ => false,
            })
        };
        let signed = journals(SIGNED);
        let paying = journals(PAYING);
        let attempted = journals(b"\"stage\":\"attempted\"");
        for write in &batch.writes {
            if let Write::Put { key, value, .. } = write {
                if !key.starts_with(b"p/j/") {
                    continue;
                }
                if let Some(at) = value.windows(SIGNED.len()).position(|w| w == SIGNED) {
                    let rest = &value[at + SIGNED.len()..];
                    let end = rest.iter().position(|b| *b == b'"').unwrap_or(rest.len());
                    let id = String::from_utf8_lossy(&rest[..end]).into_owned();
                    let mut seen = self.acceptances.lock().unwrap();
                    if !seen.contains(&id) {
                        seen.push(id);
                    }
                }
            }
        }
        if self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        const ACCEPTANCE: &[u8] = b"paykit.payment_request_acceptance";
        let queues_acceptance = batch.writes.iter().any(|write| {
            matches!(write, Write::Put { key, value, .. }
                if key.starts_with(b"out/")
                    && value.windows(ACCEPTANCE.len()).any(|w| w == ACCEPTANCE))
        });
        if queues_acceptance
            && self
                .refuse_acceptances
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Unavailable);
        }
        let queues = batch
            .writes
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"out/")));
        if queues
            && self
                .stop_at_outbox
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.stopped
                .store(true, std::sync::atomic::Ordering::SeqCst);
            return Err(StoreError::Unavailable);
        }
        if self
            .queueing_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreError::Unavailable);
        }
        self.inner.commit(batch)?;
        if signed
            && self
                .fail_queueing
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.queueing_next
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if signed {
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
            }
        }
        if attempted {
            if let Some(hook) = self.attempted_hook.lock().unwrap().take() {
                hook();
            }
        }
        if paying {
            if let Some(hook) = self.paying_hook.lock().unwrap().take() {
                hook();
            }
            if self
                .stop_after_paying
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.stopped
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Ok(())
    }
}

/// A Homeserver whose reads of `identity.json` follow a script: each step names an
/// identity, how long its next read takes on the clock, and a hook run during it.
struct HookedReads {
    inner: Arc<pubky_switch_client::testing::TestHomeserver>,
    clock: Arc<pubky_switch_client::clock::TestClock>,
    steps: Mutex<std::collections::VecDeque<Step>>,
}

impl HookedReads {
    fn new(w: &World) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(&w.homeserver),
            clock: Arc::clone(&w.clock),
            steps: Mutex::new(std::collections::VecDeque::new()),
        })
    }

    fn then(&self, identity: [u8; 32], delay_ms: u64, hook: Option<Hook>) {
        self.steps
            .lock()
            .unwrap()
            .push_back((identity, delay_ms, hook));
    }
}

impl HomeserverReader for HookedReads {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        let step = {
            let mut steps = self.steps.lock().unwrap();
            let due =
                path == IDENTITY_PATH && steps.front().is_some_and(|(who, _, _)| *who == identity);
            if due {
                steps.pop_front()
            } else {
                None
            }
        };
        if let Some((_, delay_ms, hook)) = step {
            self.clock.advance_ms(delay_ms);
            if let Some(hook) = hook {
                hook();
            }
        }
        self.inner.get(identity, path)
    }
}

/// The wallet on `store`, its Homeserver reads through `reads` and its requests to bob's
/// switch through `slow`.
async fn wallet(
    w: &World,
    store: Arc<dyn StateStore>,
    app_id: [u8; 16],
    reads: &Arc<HookedReads>,
    slow: &Arc<SlowSwitch>,
) -> Paykit {
    let mut env = w.env(&w.alice, store);
    env.homeserver = Arc::clone(reads) as Arc<dyn HomeserverReader>;
    env.transport = Arc::clone(slow) as Arc<dyn Transport>;
    Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

/// A slow switch that answers nothing until released, for bob's public reads only.
fn held_switch(w: &World) -> Arc<SlowSwitch> {
    Arc::new(SlowSwitch::new(Arc::clone(&w.router), BOB_ORIGIN))
}

/// Bob's signer removes the shop, and only his switch has the copy saying so: his
/// Homeserver still serves the one before.
fn revoke_shop_on_the_switch_only(w: &World) {
    let shop = w.bob.identity().app_keys("shop").app_id_bytes();
    let older = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    let now = w.clock.now_ms();
    let doc = w
        .bob
        .sign_next(|body| {
            body.apps.retain(|a| a.app_id != shop);
            body.removed.push(RemovedApp {
                app_id: shop,
                distrust_from: now,
            });
        })
        .unwrap();
    w.bob.switch().deliver_identity(&doc).unwrap();
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, older);
}

/// An idempotent rail without a history: a resume repeats the dispatch, through the checks.
fn repeatable(w: &World) {
    w.rail.set_declaration(RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: None,
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
    });
}

/// Alice's wallet on a hooked store, and bob's request it holds.
async fn with_a_request(w: &World) -> (Arc<HookedStore>, [u8; 16], RequestRef) {
    let store = Arc::new(HookedStore::default());
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
        .await;
    let app_id = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    (store, app_id, request)
}

/// A payment of the request left in flight, its dispatch made once.
async fn in_flight(w: &World) -> (Arc<HookedStore>, [u8; 16], paykit_lib::AttemptId) {
    repeatable(w);
    let (store, app_id, request) = with_a_request(w).await;
    let mut wallet = w
        .load_with(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
            crate::common::day_window(),
        )
        .await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    (store, app_id, attempt_id)
}

/// A payment whose app stopped right after journaling `Paying`, before any call of the
/// payment method: the commit that journals it applied, then failed, as a process that
/// died at once. A fresh world for each commit tried, until one is found.
async fn stopped_before_the_rail() -> (World, Arc<HookedStore>, [u8; 16], paykit_lib::AttemptId) {
    use pubky_switch_client::store::{Fault, FaultStore};
    for n in 1..64 {
        let w = World::new();
        repeatable(&w);
        let (store, app_id, request) = with_a_request(&w).await;
        let faulty: Arc<dyn StateStore> = Arc::new(FaultStore::new(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Fault::FailAfter(n),
        ));
        let mut crashing = w
            .load_with(
                &w.alice,
                faulty,
                app_id,
                Arc::clone(&w.clock),
                crate::common::day_window(),
            )
            .await;
        let _ = crashing.pay_request(&request, PayOptions::default()).await;
        drop(crashing);
        assert!(
            w.rail.all_executions().is_empty(),
            "the rail was reached at {n}"
        );
        let mut loaded = w
            .load_on_clock(
                &w.alice,
                Arc::clone(&store) as Arc<dyn StateStore>,
                app_id,
                Arc::clone(&w.clock),
            )
            .await;
        let payments = loaded.payments().await.unwrap();
        if let [payment] = payments.as_slice() {
            if payment.stage == paykit_sdk::Stage::Paying {
                let attempt_id = payment.attempt_id;
                // Whatever the outbox holds goes out meanwhile, so the recovery's reads are
                // its checks' alone.
                loaded.flush().await.unwrap();
                drop(loaded);
                return (w, store, app_id, attempt_id);
            }
        }
    }
    panic!("no commit journals Paying");
}

// ---------------------------------------------------------------------------------------
// A late switch answer that removes the destination's app

#[tokio::test]
async fn test_final_check_first_dispatch_late_answer_removing_the_app_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    revoke_shop_on_the_switch_only(&w);
    // The wallet's copy of bob's identity.json is old: paying looks it up, and his switch
    // answers only after the last journal write before the final payment check.
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    store.on_acceptance_journaled(Box::new(move || release.release()));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Pending { attempt_id } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_resume_late_answer_removing_the_app_not_paid() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    revoke_shop_on_the_switch_only(&w);
    w.clock.advance_ms(11 * 60_000);
    // Bob's switch answers while the own identity.json is read, after the destination's
    // check: the answer is taken just before the repeat.
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    reads.then(w.alice_id(), 0, Some(Box::new(move || release.release())));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Pending { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_late_answer_removing_the_app_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    revoke_shop_on_the_switch_only(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    reads.then(w.alice_id(), 0, Some(Box::new(move || release.release())));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Pending { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

// ---------------------------------------------------------------------------------------
// An own identity.json read that succeeds after 11 minutes

#[tokio::test]
async fn test_final_check_first_dispatch_payee_copy_aged_during_the_own_read_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    w.clock.advance_ms(11 * 60_000);
    // Before the attempt: the own identity.json is read at once, then bob's, which takes
    // 61 s. After it, the own copy is over 60 s old and read again: that read succeeds after
    // 11 minutes, leaving bob's copy past its 10.
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 0, None);
    reads.then(w.bob_id(), 61_000, None);
    reads.then(w.alice_id(), 11 * 60_000, None);
    let slow = held_switch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    // A read that long also takes the attempt past its pre-dispatch budget: nothing is
    // paid, and the attempt is written `failed`.
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(&**inner, paykit_sdk::Refusal::NotNow(why) if why.contains("budget"))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_resume_payee_copy_aged_during_the_own_read_not_paid() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    w.clock.advance_ms(11 * 60_000);
    // The destination's check reads bob's copy; the own identity.json's read after it
    // succeeds after 11 minutes.
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 11 * 60_000, None);
    let slow = held_switch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Pending { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_payee_copy_aged_during_the_own_read_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    w.clock.advance_ms(11 * 60_000);
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 11 * 60_000, None);
    let slow = held_switch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Pending { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

// ---------------------------------------------------------------------------------------
// A repeat's idempotency retention ending during the final check's await

#[tokio::test]
async fn test_final_check_repeat_retention_ending_during_the_lookup_not_paid() {
    let w = World::new();
    const RETENTION_MS: u64 = 60 * 60_000;
    w.rail.set_declaration(RailDeclaration {
        idempotent: true,
        idempotency_retention_ms: Some(RETENTION_MS),
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
    });
    let (store, app_id, request) = with_a_request(&w).await;
    let journaled = w.clock.now_ms();
    let mut first = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = first
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    drop(first);
    // Resumed 30 s before the retention ends: every check passes, and the kept lookup of
    // bob's switch answers in the final check, 31 s later, past the retention's end. The
    // method may have forgotten the key: no repeat (Paykit spec 7.2).
    w.clock.set_ms(journaled + RETENTION_MS - 30_000);
    let slow = held_switch(&w);
    slow.delay_answer(Arc::clone(&w.clock), 31_000);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    reads.then(w.alice_id(), 0, Some(Box::new(move || release.release())));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

// ---------------------------------------------------------------------------------------
// Conflicting identity documents arriving with the late answer

/// Bob's signer equivocates: his switch holds one document at the next version, his
/// Homeserver another body at that same version.
fn equivocate(w: &World) {
    let switch_copy = w
        .bob
        .sign_next(|body| body.record_max_age = Some(30 * 86_400))
        .unwrap();
    w.bob.switch().deliver_identity(&switch_copy).unwrap();
    let mut other = switch_copy.body().clone();
    other.record_max_age = Some(60 * 86_400);
    let homeserver_copy =
        pubky_switch_core::records::IdentityDoc::sign(w.bob.identity().key(), other).unwrap();
    w.homeserver.publish(&homeserver_copy).unwrap();
}

#[tokio::test]
async fn test_final_check_first_dispatch_late_equivocation_refused() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    equivocate(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    store.on_acceptance_journaled(Box::new(move || release.release()));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    // The switch's copy arrives before the final payment check, another body at the version
    // the Homeserver's has: equivocation. Nothing is paid, `failed` is written, and the
    // payment is refused (Switch wire 3.3).
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(**inner, paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_resume_late_equivocation_not_paid() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    equivocate(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    reads.then(w.alice_id(), 0, Some(Box::new(move || release.release())));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    // A repeat may follow a dispatch that paid: it stays "may be paid".
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_late_equivocation_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    equivocate(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    reads.then(w.alice_id(), 0, Some(Box::new(move || release.release())));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_repeat_on_an_own_identity_read_that_errs_refused() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    let slow = held_switch(&w);
    slow.release();
    let reads = HookedReads::new(&w);
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    assert!(wallet
        .app_mut()
        .refresh_identity()
        .await
        .unwrap()
        .confirmed_at()
        .is_some());
    // Within the 60 s of that confirmation, the repeat's read of the own identity.json
    // finds a copy that doesn't verify: an error, not "unavailable", and it stops the
    // payment rather than leaving the earlier confirmation to stand.
    w.homeserver.put(
        w.alice_id(),
        IDENTITY_PATH,
        b"{\"body\":\"x\",\"sig\":\"y\"}".to_vec(),
    );
    let refused = wallet.resume_payments().await.unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

// ---------------------------------------------------------------------------------------
// A first dispatch stopped before the payment method's call, on a rail that can't tell

/// A rail neither idempotent nor with a history: a dispatch it may have made can't be
/// resolved, so only a journal that shows none started lets a resume write `failed`.
fn blind(w: &World) {
    w.rail.set_declaration(RailDeclaration {
        idempotent: false,
        idempotency_retention_ms: None,
        history: false,
        history_retention_ms: None,
        history_survives_restore: false,
        longest_completion_ms: paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
    });
}

/// After an explicit flush, no acceptance `store` journaled is known to the wallet's
/// outbox: none was ever queued, so none could reach the payee (Paykit spec 7.2 step 2).
async fn no_acceptance_queued(store: &HookedStore, wallet: &mut Paykit) {
    wallet.flush().await.unwrap();
    let journaled = store.acceptances();
    assert!(!journaled.is_empty(), "no acceptance was journaled");
    for msg_id in journaled {
        assert!(
            wallet.app().delivery(&msg_id).await.unwrap().is_none(),
            "the acceptance {msg_id} was queued"
        );
    }
}

/// A resume of the wallet on `store`: the payment stopped before the rail is written
/// `failed`, as known unsubmitted, and a replacement goes ahead.
async fn resumed_failed(
    w: &World,
    store: Arc<HookedStore>,
    app_id: [u8; 16],
    request: &RequestRef,
) {
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let again = wallet
        .pay_request(request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
}

#[tokio::test]
async fn test_final_check_commit_after_the_acceptance_failing_leaves_the_payment_unsubmitted() {
    let w = World::new();
    blind(&w);
    let (store, app_id, request) = with_a_request(&w).await;
    store.fail_commit_after_acceptance();
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    drop(wallet);
    resumed_failed(&w, store, app_id, &request).await;
}

#[tokio::test]
async fn test_final_check_held_first_dispatch_leaves_the_payment_unsubmitted() {
    let w = World::new();
    blind(&w);
    let (store, app_id, request) = with_a_request(&w).await;
    revoke_shop_on_the_switch_only(&w);
    // Bob's switch answers after the acceptance is journaled: the final payment check holds
    // the payment back.
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    store.on_acceptance_journaled(Box::new(move || release.release()));
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    // Held before the call: an explicit flush sends no acceptance.
    no_acceptance_queued(&store, &mut wallet).await;
    drop(wallet);
    // Bob's shop is removed: the replacement can't pay it, but the held payment is still
    // written `failed` rather than left "may be paid".
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    // Nor after the restart and the resume.
    no_acceptance_queued(&store, &mut wallet).await;
}

// ---------------------------------------------------------------------------------------
// The fence after the journal write of `Paying`: an answer that comes during that commit

/// Alice's wallet on a hooked store, with bob's request, a rail neither idempotent nor with
/// a history, and bob's identity.json past its 10 minutes, so paying looks it up; his
/// switch answers only during the commit that journals `Paying`, after the final payment
/// check took the Homeserver's copy.
async fn answered_during_paying(
    w: &World,
    change: impl FnOnce(&World),
) -> (Arc<HookedStore>, [u8; 16], RequestRef, Paykit) {
    blind(w);
    let (store, app_id, request) = with_a_request(w).await;
    change(w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(w);
    let reads = HookedReads::new(w);
    let release = Arc::clone(&slow);
    store.on_paying(Box::new(move || release.release()));
    let wallet = wallet(
        w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    (store, app_id, request, wallet)
}

#[tokio::test]
async fn test_fence_removal_answered_during_paying_not_paid() {
    let w = World::new();
    let (store, app_id, request, mut wallet) =
        answered_during_paying(&w, revoke_shop_on_the_switch_only).await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let PayOutcome::Pending { attempt_id } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    // Nothing was submitted: the entry is written back to `Attempted`, and a resume writes
    // the attempt `failed` rather than "may be paid".
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    // `Paying` was journaled, and the fence stopped the call: no acceptance was queued.
    no_acceptance_queued(&store, &mut wallet).await;
    drop(wallet);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Failed { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    no_acceptance_queued(&store, &mut wallet).await;
}

#[tokio::test]
async fn test_fence_equivocation_answered_during_paying_refused() {
    let w = World::new();
    let (store, _app_id, request, mut wallet) = answered_during_paying(&w, equivocate).await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(**inner, paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
    no_acceptance_queued(&store, &mut wallet).await;
}

#[tokio::test]
async fn test_fence_stop_lost_to_a_crash_stays_paying_and_uncertain() {
    let w = World::new();
    let (store, app_id, request, mut wallet) =
        answered_during_paying(&w, revoke_shop_on_the_switch_only).await;
    // The process dies after the fence stops the payment, before the write-back.
    store.stop_after_paying();
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    drop(wallet);
    assert!(w.rail.all_executions().is_empty());
    store.restart();
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Paying);
    // Read as possibly dispatched, the safe direction: on a rail that can't tell, it stays
    // "may be paid", and nothing is paid.
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(
        resumed,
        vec![PayOutcome::Uncertain {
            attempt_id: payment.attempt_id
        }]
    );
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
    // "May be paid" isn't a payment the method's history shows: no acceptance goes out.
    no_acceptance_queued(&store, &mut wallet).await;
}

#[tokio::test]
async fn test_acceptance_queued_once_the_call_is_made() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The control for `no_acceptance_queued`: the call was made, and the outbox holds the
    // one acceptance journaled.
    let journaled = store.acceptances();
    assert_eq!(journaled.len(), 1);
    assert!(wallet
        .app()
        .delivery(&journaled[0])
        .await
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------------------
// The pre-dispatch budget and the floor of `orphan_after`

#[tokio::test]
async fn test_fence_attempt_past_its_pre_dispatch_budget_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    // The journal write of `Paying` takes the attempt past its budget.
    let clock = Arc::clone(&w.clock);
    store.on_paying(Box::new(move || {
        clock.advance_ms(paykit_sdk::PRE_DISPATCH_BUDGET_MS);
    }));
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(&**inner, paykit_sdk::Refusal::NotNow(why) if why.contains("budget"))
        ),
        "{refused:?}"
    );
    // Nothing was submitted, and the attempt is written `failed`: a new payment is a new
    // decision.
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
}

#[tokio::test]
async fn test_orphan_after_below_the_floor_refused() {
    let w = World::new();
    let floor = paykit_sdk::orphan_floor_s(&paykit_sdk::Rail::declaration(&*w.rail));
    assert_eq!(
        floor,
        (paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS + paykit_sdk::PRE_DISPATCH_BUDGET_MS) / 1000
    );
    let scopes = paykit_sdk::AppScopes::new().pay().build().unwrap();
    let config = PaykitConfig {
        orphan_after_s: Some(floor - 1),
        ..PaykitConfig::default()
    };
    let refused = Paykit::join(
        w.env(&w.alice, crate::common::store()),
        config,
        w.approve(&w.alice, "wallet", &scopes),
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .err()
    .unwrap();
    assert!(
        matches!(
            refused,
            paykit_sdk::PaykitError::Lib(paykit_lib::Error::Validation(_))
        ),
        "{refused:?}"
    );
    // At the floor, another app joins.
    let config = PaykitConfig {
        orphan_after_s: Some(floor),
        ..PaykitConfig::default()
    };
    w.join_with(&w.alice, "phone", scopes, config, crate::common::store())
        .await;
}

#[tokio::test]
async fn test_repeat_too_close_to_the_orphan_age_not_made() {
    // A resume's repeat completes, at the method's longest, only after the attempt is
    // orphaned, on the service clock taken late (the device clock plus the tolerance): it
    // isn't made, and the attempt stays "may be paid".
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    w.clock.advance_ms(
        crate::common::day_window().orphan_after_s_for(&paykit_sdk::Rail::declaration(&*w.rail))
            * 1000
            - paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS
            - pubky_switch_core::records::CLOCK_TOLERANCE_MS,
    );
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert_eq!(resumed, vec![PayOutcome::Uncertain { attempt_id }]);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_default_config_takes_a_one_day_rail() {
    // A rail that may take a day: the default config's `orphan_after` is that day plus the
    // pre-dispatch budget, so join, load and paying all take it.
    let w = World::new();
    let declaration = RailDeclaration {
        longest_completion_ms: 86_400_000,
        ..paykit_sdk::Rail::declaration(&*w.rail)
    };
    w.rail.set_declaration(declaration);
    assert_eq!(
        PaykitConfig::default().orphan_after_s_for(&declaration),
        86_400 + paykit_sdk::PRE_DISPATCH_BUDGET_MS / 1000
    );
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_fence_answer_committed_past_the_budget_not_paid() {
    // The attempt's answer is committed locally only once the pre-dispatch budget and the
    // attempt's orphan age have passed, so another own app could have resolved it
    // meanwhile: the age counts from before the attempt was sent, and nothing is paid.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let clock = Arc::clone(&w.clock);
    let past = paykit_sdk::orphan_floor_s(&paykit_sdk::Rail::declaration(&*w.rail)) * 1000;
    store.on_attempted(Box::new(move || clock.advance_ms(past + 60_000)));
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(**inner, paykit_sdk::Refusal::NotNow(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
}

#[tokio::test]
async fn test_acceptance_after_a_paid_outcome_goes_only_once_its_proof_is_journaled() {
    // The method reports the payment made; the process dies as the acceptance is queued.
    // On a rail with no history, only the journal can tell it paid: the proof is there, and
    // the resume reports the payment, sending its proof.
    let w = World::new();
    blind(&w);
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    store.stop_at_outbox();
    let _ = wallet.pay_request(&request, PayOptions::default()).await;
    drop(wallet);
    let attempt_id = {
        store.restart();
        let wallet = w
            .load_on_clock(
                &w.alice,
                Arc::clone(&store) as Arc<dyn StateStore>,
                app_id,
                Arc::clone(&w.clock),
            )
            .await;
        let payment = wallet.payments().await.unwrap().remove(0);
        assert_eq!(payment.stage, paykit_sdk::Stage::RailPaid);
        payment.attempt_id
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_acceptance_owed_after_the_outcome_is_queued_once_at_a_resume() {
    // Every try to queue the acceptance fails, after the call and in the outcome, while the
    // outcome itself is written: the entry stays owed. After a restart the resume queues
    // the acceptance exactly once and finishes, and a second resume queues nothing more.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    store.refuse_acceptances(true);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::AcceptanceOwed
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    drop(wallet);
    store.refuse_acceptances(false);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
}

/// How many of the acceptances `store` journaled the wallet's outbox holds.
async fn queued_acceptances(store: &HookedStore, wallet: &Paykit) -> usize {
    let mut n = 0;
    for id in store.acceptances() {
        if wallet.app().delivery(&id).await.unwrap().is_some() {
            n += 1;
        }
    }
    n
}
