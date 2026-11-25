//! The final payment check (Paykit spec 4, 7.2): the last thing before the payment method's
//! call, on a first handoff, a resume and a recovery. A late Dispatch answer that removes the
//! destination's app, or a payee copy that aged past its limit meanwhile, pays nothing.
//! An unchanged confirmation received while journaling `Paying` can complete the first call.

use std::sync::{Arc, Mutex};

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitConfig, RailDeclaration, RequestRef};
use pubky_dispatch_client::app::HomeserverReader;
use pubky_dispatch_client::clock::Clock;
use pubky_dispatch_client::error::{StoreError, TransportError};
use pubky_dispatch_client::store::{KeyValue, MemoryStore, StateStore, Write};
use pubky_dispatch_client::transport::{BoxFuture, Transport};
use pubky_dispatch_core::records::{IdentityBody, RemovedApp, IDENTITY_PATH};

use crate::common::{draft, request_from, SlowDispatch, World, BOB_ORIGIN};

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
    deny_hook: Mutex<Option<Hook>>,
    lookup_hook: Mutex<Option<(Vec<u8>, Hook)>>,
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
        let value = self.inner.get(ns, key)?;
        if key == b"p/paykit/dl" {
            if let Some(hook) = self.deny_hook.lock().unwrap().take() {
                hook();
            }
        }
        Ok(value)
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

    fn commit(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<(), pubky_dispatch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_dispatch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
    ) -> Result<
        pubky_dispatch_client::store::AcquireOutcome,
        pubky_dispatch_client::error::StoreError,
    > {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_dispatch_client::store::StoreOwner,
        batch: pubky_dispatch_client::store::WriteBatch,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl HookedStore {
    fn commit_with(
        &self,
        batch: pubky_dispatch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_dispatch_client::store::WriteBatch,
        ) -> Result<
            pubky_dispatch_client::store::CommitOutcome,
            pubky_dispatch_client::error::StoreError,
        >,
    ) -> Result<pubky_dispatch_client::store::CommitOutcome, pubky_dispatch_client::error::StoreError>
    {
        const SIGNED: &[u8] = b"\"acceptance_msg_id\":\"";
        const PAYING: &[u8] = b"\"stage\":\"paying\"";
        let journals = |marker: &[u8]| {
            batch.writes().iter().any(|write| match write {
                Write::Put { key, value, .. } => {
                    key.starts_with(b"p/paykit/j/")
                        && value.windows(marker.len()).any(|w| w == marker)
                }
                _ => false,
            })
        };
        let signed = journals(SIGNED);
        let paying = journals(PAYING);
        let attempted = journals(b"\"stage\":\"attempted\"");
        for write in batch.writes() {
            if let Write::Put { key, value, .. } = write {
                if !key.starts_with(b"p/paykit/j/") {
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
        let queues_acceptance = batch.writes().iter().any(|write| {
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
            .writes()
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
        let mut lookup = self.lookup_hook.lock().unwrap();
        let confirmed = lookup.as_ref().is_some_and(|(seller, _)| {
            batch
                .writes()
                .iter()
                .any(|write| matches!(write, Write::Put { key, .. } if key == seller))
        });
        let applied = apply(batch)?;
        if applied != pubky_dispatch_client::store::CommitOutcome::Applied {
            return Ok(applied);
        }
        if confirmed {
            let (_, hook) = lookup.take().unwrap();
            hook();
        }
        drop(lookup);
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
        Ok(pubky_dispatch_client::store::CommitOutcome::Applied)
    }
}

/// A Homeserver whose reads of `identity.json` follow a script: each step names an
/// identity, how long its next read takes on the clock, and a hook run during it.
struct HookedReads {
    inner: Arc<pubky_dispatch_client::testing::TestHomeserver>,
    clock: Arc<pubky_dispatch_client::clock::TestClock>,
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
/// Dispatch through `slow`.
async fn wallet(
    w: &World,
    store: Arc<dyn StateStore>,
    app_id: [u8; 16],
    reads: &Arc<HookedReads>,
    slow: &Arc<SlowDispatch>,
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

/// A slow Dispatch that answers nothing until released, for bob's public reads only.
fn held_dispatch(w: &World) -> Arc<SlowDispatch> {
    Arc::new(SlowDispatch::new(Arc::clone(&w.router), BOB_ORIGIN))
}

/// Only bob's Dispatch sees the new identity document; the Homeserver still serves its predecessor.
fn change_shop_on_the_dispatch_only(w: &World, change: impl FnOnce(&mut IdentityBody)) {
    let older = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    let doc = w.bob.sign_next(change).unwrap();
    w.bob.switch().deliver_identity(&doc).unwrap();
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, older);
}

fn revoke_shop_on_the_dispatch_only(w: &World) {
    let shop = w.bob.identity().app_keys("shop").app_id_bytes();
    let now = w.clock.now_ms();
    change_shop_on_the_dispatch_only(w, |body| {
        body.apps.retain(|a| a.app_id != shop);
        body.removed.push(RemovedApp {
            app_id: shop,
            distrust_from: now,
        });
    });
}

/// An idempotent rail without a history: a resume repeats the handoff, through the checks.
fn repeatable(w: &World) {
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
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

/// A payment of the request left in flight, its handoff made once.
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
    use pubky_dispatch_client::store::{Fault, FaultStore};
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
        let _ = crashing
            .pay_request(&request, PayOptions::default())
            .await
            .outcome;
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
// The payer's own Homeserver confirmation at handoff.

#[tokio::test]
async fn test_final_check_own_observation_rollback_during_read_requires_a_new_read() {
    use pubky_dispatch_client::clock::TestClock;
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let now = w.clock.now_ms() + 10_000;
    let device = Arc::new(TestClock::new(now));
    let reads = HookedReads::new(&w);
    let changed = Arc::clone(&device);
    reads.then(
        w.alice_id(),
        0,
        Some(Box::new(move || changed.set_ms(now - 1))),
    );
    let mut env = w.env(&w.alice, store);
    env.clock = Arc::clone(&device) as Arc<dyn Clock>;
    env.homeserver = reads;
    let mut wallet = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::IdentityUnavailable(_))
        ),
        "{refused:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());
    // Catching up cannot restore the discarded confirmation. A new actual read can.
    device.set_ms(now);
    assert_eq!(wallet.app().identity_confirmed_at(), None);
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_final_check_raw_host_takes_a_newer_own_version_across_a_clock_adjustment() {
    use pubky_dispatch_client::app::IdentityRefresh;
    use pubky_dispatch_client::clock::TestClock;
    let w = World::new();
    let (saved, app_id, _) = with_a_request(&w).await;
    let now = w.clock.now_ms() + 100;
    let device = Arc::new(TestClock::new(now));
    let reads = HookedReads::new(&w);
    let mut env = w.env(&w.alice, saved);
    env.clock = Arc::clone(&device) as Arc<dyn Clock>;
    env.homeserver = reads.clone();
    let mut wallet = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let higher = w
        .alice
        .sign_next(|body| body.record_max_age = Some(86_400))
        .unwrap();
    let changed = Arc::clone(&device);
    reads.then(
        w.alice_id(),
        0,
        Some(Box::new(move || changed.set_ms(now - 1))),
    );
    assert_eq!(
        wallet.app_mut().refresh_identity().await.unwrap(),
        IdentityRefresh::ClockMovedBack { newer: true }
    );
    assert_eq!(wallet.app().own_identity().hash(), higher.hash());
    device.set_ms(now);
    assert_eq!(wallet.app().identity_confirmation_age_ms(), None);
    assert_eq!(
        wallet.app_mut().refresh_identity().await.unwrap(),
        IdentityRefresh::Current { confirmed_at: now }
    );
    assert_eq!(wallet.app().identity_confirmation_age_ms(), Some(0));
}

/// The device returns one earlier observation after its paying journal commit,
/// then reads its previous time again before the method is called.
struct BackwardsOnce {
    now: u64,
    back: std::sync::atomic::AtomicBool,
}

impl Clock for BackwardsOnce {
    fn now_ms(&self) -> u64 {
        self.now - u64::from(self.back.swap(false, std::sync::atomic::Ordering::SeqCst))
    }
}

#[tokio::test]
async fn test_final_check_own_observation_rollback_after_read_stays_invalid_at_handoff() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let clock = Arc::new(BackwardsOnce {
        now: w.clock.now_ms() + 10_000,
        back: false.into(),
    });
    let changed = Arc::clone(&clock);
    store.on_paying(Box::new(move || {
        changed
            .back
            .store(true, std::sync::atomic::Ordering::SeqCst)
    }));
    let mut env = w.env(&w.alice, store);
    env.clock = clock;
    let mut wallet = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
    assert_eq!(wallet.app().identity_confirmed_at(), None);
}

/// Observe the device reads between the paying commit and the actual rail call.
struct FenceClock {
    rail: Arc<paykit_sdk::rail::FakeRail>,
    now: u64,
    changed_at: Option<(usize, u64)>,
    samples: Mutex<Option<usize>>,
}

impl Clock for FenceClock {
    fn now_ms(&self) -> u64 {
        let mut samples = self.samples.lock().unwrap();
        if let Some(count) = samples.as_mut() {
            if self.rail.all_executions().is_empty() {
                *count += 1;
                if let Some((at, value)) = self.changed_at {
                    if *count == at {
                        return value;
                    }
                }
            }
        }
        self.now
    }
}

async fn at_last_fence_sample(change: Option<(usize, i64)>) -> (PayOutcome, usize, bool) {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let now = w.clock.now_ms();
    let clock = Arc::new(FenceClock {
        rail: w.rail.clone(),
        now,
        changed_at: change.map(|(at, delta)| (at, now.checked_add_signed(delta).unwrap())),
        samples: Mutex::new(None),
    });
    let armed = clock.clone();
    store.on_paying(Box::new(move || *armed.samples.lock().unwrap() = Some(0)));
    let mut env = w.env(&w.alice, store);
    env.clock = clock.clone();
    let mut wallet = Paykit::load(
        env,
        PaykitConfig::default(),
        app_id,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let reads = clock.samples.lock().unwrap().unwrap();
    (outcome, reads, !w.rail.all_executions().is_empty())
}

#[tokio::test]
async fn test_final_check_own_observation_at_the_last_clock_sample_before_the_rail() {
    let (ordinary, last, paid) = at_last_fence_sample(None).await;
    assert!(matches!(ordinary, PayOutcome::Paid { .. }) && paid && last > 0);
    let mut outcomes = Vec::new();
    for delta in [-1, 59_999, 60_000] {
        let (outcome, _, paid) = at_last_fence_sample(Some((last, delta))).await;
        outcomes.push((delta, matches!(outcome, PayOutcome::Paid { .. }), paid));
    }
    assert_eq!(
        outcomes,
        [
            (-1, false, false),
            (59_999, true, true),
            (60_000, false, false)
        ]
    );
}

// A late Dispatch answer that removes the destination's app

#[tokio::test]
async fn test_final_check_current_public_read_remains_usable_without_a_second_get() {
    let w = Arc::new(World::new());
    let saved = Arc::new(HookedStore::default());
    let mut payer = w
        .wallet_on("wallet", saved.clone() as Arc<dyn StateStore>)
        .await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    crate::public::publish(
        &w,
        crate::public::one_endpoint("btc-lightning-bolt12", "lno1shop"),
        Some(w.clock.now_ms() + 86_400_000),
    );
    let owner = w.bob_id();
    let path = pubky_dispatch_core::records::app_record_path(&shop.app_id());
    let changed = Arc::clone(&w);
    saved.on_attempted(Box::new(move || changed.homeserver.remove(owner, &path)));
    let paid = payer
        .pay_without_request(crate::public::public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_final_check_late_public_lookup_rechecks_the_selected_endpoint() {
    for (on_acceptance, replace) in [(false, false), (false, true), (true, false), (true, true)] {
        let w = Arc::new(World::new());
        let store = Arc::new(HookedStore::default());
        let mut original = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let app_id = original.app_id();
        let mut shop = w.shop().await;
        shop.app_mut().invite(&w.alice_id()).await.unwrap();
        let mut invoice = draft("1", w.clock.now_ms());
        invoice.destination = paykit_lib::bodies::RequestDestination::Any;
        let request = request_from(&mut shop, w.alice_id(), invoice).await;
        original.receive(None).await.unwrap();
        drop(original);
        w.clock.advance_ms(11 * 60_000);
        let endpoint = |payload| crate::public::one_endpoint("btc-lightning-bolt12", payload);
        let expires = Some(w.clock.now_ms() + 86_400_000);
        crate::public::publish(&w, endpoint("lno1first"), expires);
        // Only the Dispatch knows this newer document. It still admits the same app and certificate.
        let older = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
        let higher = w
            .bob
            .sign_next(|body| body.record_max_age = Some(86_400))
            .unwrap();
        w.bob.switch().deliver_identity(&higher).unwrap();
        w.homeserver.put(w.bob_id(), IDENTITY_PATH, older);
        w.clock.advance_ms(1);
        let slow = held_dispatch(&w);
        let release = Arc::clone(&slow);
        let changed = Arc::clone(&w);
        let hook: Hook = Box::new(move || {
            if replace {
                crate::public::publish(&changed, endpoint("lno1next"), expires);
            }
            release.release();
        });
        if on_acceptance {
            store.on_acceptance_journaled(hook);
        } else {
            store.on_attempted(hook);
        }
        let mut payer = self::wallet(&w, store, app_id, &HookedReads::new(&w), &slow).await;
        let mut payment = crate::public::public_to_bob(&w);
        payment.options.unconfirmed_identity_confirmed =
            Some(pubky_dispatch_client::app::UserConfirmed::new());
        let result = if on_acceptance {
            payer.pay_request(&request, payment.options).await.outcome
        } else {
            payer.pay_without_request(payment).await.outcome
        };
        if replace {
            if on_acceptance {
                assert!(
                    matches!(result, Ok(PayOutcome::Pending { .. })),
                    "{result:?}"
                );
            } else {
                let refused = result.unwrap_err();
                assert!(
                    matches!(refused.refusal(), Some(paykit_sdk::Refusal::AttemptFailed(reason)) if matches!(reason.as_ref(), paykit_sdk::Refusal::Destination(_))),
                    "{refused:?}"
                );
            }
            assert!(w.rail.all_executions().is_empty());
        } else {
            assert!(matches!(result, Ok(PayOutcome::Paid { .. })), "{result:?}");
            assert_eq!(w.rail.all_executions().len(), 1);
        }
    }
}

#[tokio::test]
async fn test_final_check_first_handoff_late_answer_removing_the_app_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    revoke_shop_on_the_dispatch_only(&w);
    // The wallet's copy of bob's identity.json is old: paying looks it up, and his Dispatch
    // answers only after the last journal write before the final payment check.
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(&w);
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
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_resume_late_answer_removing_the_app_not_paid() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    revoke_shop_on_the_dispatch_only(&w);
    w.clock.advance_ms(11 * 60_000);
    // Bob's Dispatch answers while the own identity.json is read, after the destination's
    // check: the answer is taken just before the repeat.
    let slow = held_dispatch(&w);
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
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_late_answer_removing_the_app_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    revoke_shop_on_the_dispatch_only(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(&w);
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
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

// ---------------------------------------------------------------------------------------
// The payee identity reaches its ten-minute limit during a current own identity read

/// Read Bob's signed document now so its ten-minute boundary is known.
async fn observe_payee(w: &World, wallet: &mut Paykit) -> u64 {
    let at = w.clock.now_ms();
    let identity = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(identity.read_at_ms, Some(at));
    at
}

#[tokio::test]
async fn test_final_check_first_handoff_payee_copy_aged_during_the_own_read_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 0, None);
    reads.then(w.alice_id(), 1_000, None);
    let clock = Arc::clone(&w.clock);
    store.on_attempted(Box::new(move || clock.advance_ms(5 * 60_000 - 1_000)));
    let slow = held_dispatch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    // The attempt starts five minutes after the payee observation. Its journal commit
    // takes just under five minutes. The final one-second own read takes the payee to
    // ten minutes and, with the five-minute clock tolerance, exhausts the attempt budget.
    let observed = observe_payee(&w, &mut wallet).await;
    w.clock.set_ms(observed + 5 * 60_000);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(&**inner, paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AttemptCheck(why)) if why.contains("budget"))
        ),
        "{refused:?}"
    );
    assert_eq!(w.clock.now_ms(), observed + 10 * 60_000);
    assert!(w.rail.all_executions().is_empty());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_resume_payee_copy_aged_during_the_own_read_not_paid() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 1_000, None);
    let slow = held_dispatch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let observed = observe_payee(&w, &mut wallet).await;
    w.clock.set_ms(observed + 10 * 60_000 - 1_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.clock.now_ms(), observed + 10 * 60_000);
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_payee_copy_aged_during_the_own_read_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    let reads = HookedReads::new(&w);
    reads.then(w.alice_id(), 1_000, None);
    let slow = held_dispatch(&w);
    slow.release();
    let mut wallet = wallet(
        &w,
        Arc::clone(&store) as Arc<dyn StateStore>,
        app_id,
        &reads,
        &slow,
    )
    .await;
    let observed = observe_payee(&w, &mut wallet).await;
    w.clock.set_ms(observed + 10 * 60_000 - 1_000);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.clock.now_ms(), observed + 10 * 60_000);
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

// ---------------------------------------------------------------------------------------
// A repeat's idempotency retention ending during the final check's await

#[tokio::test]
async fn test_final_check_repeat_retention_ending_during_the_lookup_not_paid() {
    let w = World::new();
    const RETENTION_MS: u64 = 60 * 60_000;
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true)
        .with_idempotency_retention_ms(RETENTION_MS),
    );
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
    let PayOutcome::Pending { attempt_id, .. } = first
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("pending");
    };
    drop(first);
    // Resumed 30 s before the retention ends: every check passes, and the kept lookup of
    // bob's Dispatch answers in the final check, 31 s later, past the retention's end. The
    // method may have forgotten the key: no repeat (Paykit spec 7.2).
    w.clock.set_ms(journaled + RETENTION_MS - 30_000);
    let slow = held_dispatch(&w);
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
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

// ---------------------------------------------------------------------------------------
// Conflicting identity documents arriving with the late answer

/// Bob's signer equivocates: his Dispatch holds one document at the next version, his
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
        pubky_dispatch_core::records::IdentityDoc::sign(w.bob.identity().key(), other).unwrap();
    w.homeserver.publish(&homeserver_copy).unwrap();
}

#[tokio::test]
async fn test_final_check_first_handoff_late_equivocation_refused() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    equivocate(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(&w);
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
    // The Dispatch's copy arrives before the final payment check, another body at the version
    // the Homeserver's has: equivocation. Nothing is paid, `failed` is written, and the
    // payment is refused (Dispatch wire 3.3).
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
    let slow = held_dispatch(&w);
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
    // A repeat may follow a handoff that paid: it stays "may be paid".
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_final_check_recovery_late_equivocation_not_paid() {
    let (w, store, app_id, attempt_id) = stopped_before_the_rail().await;
    equivocate(&w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(&w);
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
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
}

#[tokio::test]
async fn test_final_check_repeat_on_an_own_identity_read_that_errs_refused() {
    let w = World::new();
    let (store, app_id, attempt_id) = in_flight(&w).await;
    let slow = held_dispatch(&w);
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
    // The repeat is refused for that entry alone, which is left pending for the next resume.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::Pending { attempt_id: left, .. }] if *left == attempt_id
        ),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

// ---------------------------------------------------------------------------------------
// A first handoff stopped before the payment method's call, on a rail that can't tell

/// A rail neither idempotent nor with a history: a handoff it may have made can't be
/// resolved, so only a journal that shows none started lets a resume write `failed`.
fn blind(w: &World) {
    w.rail.set_declaration(RailDeclaration::new(
        paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
        paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
    ));
}

/// The rail of `w` keeps its history again, for good and across a restore.
fn sighted(w: &World) {
    w.rail.set_declaration(
        RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_history(true)
        .with_history_survives_restore(true),
    );
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
        .outcome
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
        .outcome
        .is_err());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    drop(wallet);
    resumed_failed(&w, store, app_id, &request).await;
}

#[tokio::test]
async fn test_final_check_held_first_handoff_leaves_the_payment_unsubmitted() {
    let w = World::new();
    blind(&w);
    let (store, app_id, request) = with_a_request(&w).await;
    revoke_shop_on_the_dispatch_only(&w);
    // Bob's Dispatch answers after the acceptance is journaled: the final payment check holds
    // the payment back.
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(&w);
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
        .outcome
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
/// Dispatch answers only during the commit that journals `Paying`, after the final payment
/// check took the Homeserver's copy.
async fn answered_during_paying(
    w: &World,
    change: impl FnOnce(&World),
) -> (Arc<HookedStore>, [u8; 16], RequestRef, Paykit) {
    blind(w);
    let (store, app_id, request) = with_a_request(w).await;
    change(w);
    w.clock.advance_ms(11 * 60_000);
    let slow = held_dispatch(w);
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
async fn test_fence_seller_dispatch_failing_during_paying_pays_the_unchanged_document() {
    // The seller's Dispatch can't be read when the lookup that the final payment check left
    // running ends, during the commit that journals `Paying`: the Homeserver still serves
    // the document the payment was checked against, so nothing changed, and the payment of
    // the signed request goes ahead as it would have one check earlier (Paykit spec 7.2).
    let w = World::new();
    let (_, _, request, mut wallet) = answered_during_paying(&w, |w| {
        w.router.remove(BOB_ORIGIN);
    })
    .await;
    w.rail.next(Behaviour::InSettlement([17; 32]));
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_fence_unchanged_seller_during_paying_completes_the_original_attempt() {
    use paykit_sdk::rail::{Rail, RailOutcome, FAKE_LONGEST_COMPLETION_MS};

    let w = World::new();
    let (_, _, request, mut wallet) = answered_during_paying(&w, |_| {}).await;
    w.rail.set_declaration(
        RailDeclaration::new(
            FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_history(true),
    );
    w.rail.next(Behaviour::InSettlement([17; 32]));
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid {
        attempt_id,
        proof_msg_id: Some(_),
        ..
    } = paid
    else {
        panic!("{paid:?}");
    };
    let attempts = wallet.payments().await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].attempt_id, attempt_id);
    assert_eq!(attempts[0].stage, paykit_sdk::Stage::Finished);
    assert_eq!(w.rail.all_pay_calls(), [(attempt_id, 1)].into());
    assert_eq!(w.rail.all_executions(), [(attempt_id, 1)].into());
    assert!(matches!(
        w.rail.payment(attempt_id).await.unwrap(),
        Some(RailOutcome::Paid(_))
    ));
}

#[tokio::test]
async fn test_fence_unchanged_seller_cannot_authorize_a_changed_transfer() {
    let w = World::new();
    let (store, _, request, mut wallet) = answered_during_paying(&w, |_| {}).await;
    let rail = Arc::clone(&w.rail);
    // As the wallet records the seller's Dispatch confirmation, the method changes
    // what it would actually transfer for the selected endpoint.
    let key = format!(
        "ident/{}/sw",
        pubky_dispatch_core::enc::pubky_id(&request.payee)
    );
    *store.lookup_hook.lock().unwrap() = Some((
        key.into_bytes(),
        Box::new(move || {
            rail.set_transfer_amount(Some(crate::common::btc("2")));
        }),
    ));
    let refusal = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refusal.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(reason))
                if matches!(**reason, paykit_sdk::Refusal::Transfer { .. })
        ),
        "{refusal:?}"
    );
    assert!(store.lookup_hook.lock().unwrap().is_none());
    assert!(w.rail.all_pay_calls().is_empty());
}

#[tokio::test]
async fn test_fence_new_seller_document_during_paying_holds_even_when_the_shop_remains() {
    let w = World::new();
    let (_, _, request, mut wallet) = answered_during_paying(&w, |w| {
        change_shop_on_the_dispatch_only(w, |body| body.record_max_age = Some(86_400));
    })
    .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Attempted
    );
    assert!(w.rail.all_pay_calls().is_empty());
}

#[tokio::test]
async fn test_fence_older_dispatch_answer_with_a_newer_homeserver_copy_holds_the_payment() {
    let w = World::new();
    let (_, _, request, mut wallet) = answered_during_paying(&w, |w| {
        // The seller publishes a new signed document that still lists the shop. The
        // Dispatch answers the ongoing lookup with its previous signed version.
        w.bob
            .sign_next(|body| body.record_max_age = Some(86_400))
            .unwrap();
    })
    .await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    let attempts = wallet.payments().await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].stage, paykit_sdk::Stage::Attempted);
    assert!(w.rail.all_pay_calls().is_empty());
    let held = wallet
        .app_mut()
        .identity_of(&request.payee, false)
        .await
        .unwrap();
    assert_eq!(held.doc.version(), w.bob.body().version);
    assert!(held.homeserver_read_ms.is_some());
    assert!(held.switch_read_ms.is_none());
}

#[tokio::test]
async fn test_fence_unchanged_seller_then_unanswered_handoff_stays_uncertain_on_reopen() {
    let w = World::new();
    let (store, app_id, request, mut wallet) = answered_during_paying(&w, |_| {}).await;
    w.rail.next(Behaviour::LoseAnswer);
    let pending = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = pending else {
        panic!("{pending:?}")
    };
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
    drop(wallet);
    let mut reopened = w
        .load_on_clock(&w.alice, store, app_id, Arc::clone(&w.clock))
        .await;
    let outcomes = reopened.resume_payments().await.unwrap();
    assert!(
        matches!(outcomes.as_slice(), [PayOutcome::Uncertain { attempt_id: id, .. }] if *id == attempt_id)
    );
    assert_eq!(w.rail.all_pay_calls(), [(attempt_id, 1)].into());
    assert_eq!(w.rail.all_executions(), [(attempt_id, 1)].into());
}

#[tokio::test]
async fn test_fence_removal_answered_during_paying_not_paid() {
    let w = World::new();
    let (store, app_id, request, mut wallet) =
        answered_during_paying(&w, revoke_shop_on_the_dispatch_only).await;
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending { attempt_id, .. } = outcome else {
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
        .outcome
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
        answered_during_paying(&w, revoke_shop_on_the_dispatch_only).await;
    // The process dies after the fence stops the payment, before the write-back.
    store.stop_after_paying();
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
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
    // Read as possibly handed off, the safe direction: on a rail that can't tell, it stays
    // "may be paid", and nothing is paid.
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), payment.attempt_id);
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
        .outcome
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

/// Alice's wallet holding bob's one-time request, its payment's fence unable to read the
/// request record: once `Paying` is journaled, the next record read, the fence's, can't
/// reach the dispatch. With `failed_write_fails`, the next write, the one that would record
/// the attempt failed, can't either. Returns the wallet, the request, the refusal and
/// whether `Paying` was journaled before it.
async fn fence_unread(
    w: &World,
    failed_write_fails: bool,
) -> (paykit_sdk::Paykit, RequestRef, paykit_sdk::PaykitError) {
    use pubky_dispatch_client::testing::Fault;
    let (store, app_id, request) = with_a_request(w).await;
    let paying = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = Arc::clone(&paying);
    let dispatch = w.alice.switch().clone();
    store.on_paying(Box::new(move || {
        seen.store(true, std::sync::atomic::Ordering::SeqCst);
        dispatch.fail_next(Fault::Unreachable {
            path: "/records/".into(),
            after: 0,
        });
        if failed_write_fails {
            dispatch.fail_next(Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            });
        }
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
        .outcome
        .unwrap_err();
    assert!(
        paying.load(std::sync::atomic::Ordering::SeqCst),
        "the fence was reached: {refused:?}"
    );
    assert!(w.rail.all_pay_calls().is_empty(), "{refused:?}");
    (wallet, request, refused)
}

#[tokio::test]
async fn test_fence_read_failure_writes_the_attempt_failed_and_a_new_payment_pays_once() {
    // The fence's read failing, after `Paying` and just before the payment method's call, is
    // the caller's error, not a pending payment: nothing was submitted, so the attempt is
    // written failed with the Dispatch's error as its cause, and a new payment goes at once
    // (Paykit spec 7.2 step 1).
    let w = World::new();
    let (mut wallet, request, refused) = fence_unread(&w, false).await;
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(why))
                if matches!(**why, paykit_sdk::Refusal::DispatchUnavailable(_))
        ),
        "{refused:?}"
    );
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.rail.all_pay_calls().values().sum::<u64>(), 1);
}

#[tokio::test]
async fn test_fence_read_failure_with_the_failed_write_failing_waits_for_a_resume() {
    // As above, but the write that would record the attempt failed can't reach the Dispatch
    // either: the attempt stays known unsubmitted and the Dispatch's error is returned. A new
    // payment is refused as in progress until a resume writes the attempt failed; then it
    // pays once.
    let w = World::new();
    let (mut wallet, request, refused) = fence_unread(&w, true).await;
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::DispatchUnavailable(_))
        ),
        "{refused:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap().remove(0).stage,
        paykit_sdk::Stage::Attempted
    );
    let again = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            again.refusal(),
            Some(paykit_sdk::Refusal::PaymentInProgress)
        ),
        "{again:?}"
    );
    wallet.resume_payments().await.unwrap();
    let paid = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
    assert_eq!(w.rail.all_pay_calls().values().sum::<u64>(), 1);
}

#[tokio::test]
async fn test_fence_holds_a_marked_one_time_request_before_the_first_method_call() {
    use paykit_lib::records::{ActivityRecord, ActivitySide};

    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut other = w.wallet("other-payer").await;
    other.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let key = other.app().record_key(&target).unwrap();
    store.on_paying(Box::new(move || {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async move {
                    let current = other.app_mut().read_record(&target).await.unwrap().unwrap();
                    let mut record = ActivityRecord::bind(
                        current.content.value().unwrap(),
                        &other.identity(),
                        ActivitySide::Payer,
                        other.app().naming_key(),
                        &key,
                        None,
                        &|_, proof| paykit_sdk::rail::fake_settlement_id(proof),
                    )
                    .unwrap();
                    record.lost_to_delete = Some([73; 32]);
                    other
                        .app_mut()
                        .put_record(&target, &record.to_json().unwrap(), Vec::new())
                        .await
                        .unwrap();
                });
        })
        .join()
        .unwrap();
    }));
    let mut wallet = w
        .load_on_clock(&w.alice, store, app_id, Arc::clone(&w.clock))
        .await;
    w.rail.next(Behaviour::Refuse);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_pay_calls().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::MayBePaid
    );
}

// ---------------------------------------------------------------------------------------
// The pre-handoff budget and the floor of `orphan_after`

#[tokio::test]
async fn test_fence_attempt_past_its_pre_handoff_budget_not_paid() {
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    // The journal write of `Paying` takes the attempt past its budget.
    let clock = Arc::clone(&w.clock);
    store.on_paying(Box::new(move || {
        clock.advance_ms(paykit_sdk::PRE_HANDOFF_BUDGET_MS);
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(&**inner, paykit_sdk::Refusal::HeldForUser(paykit_sdk::HeldForUserReason::AttemptCheck(why)) if why.contains("budget"))
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
        (paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS + paykit_sdk::PRE_HANDOFF_BUDGET_MS) / 1000
    );
    let scopes = paykit_sdk::AppScopes::new().pay().build().unwrap();
    let config = PaykitConfig::default().with_orphan_after_s(floor - 1);
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
    let config = PaykitConfig::default().with_orphan_after_s(floor);
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
            - pubky_dispatch_core::records::CLOCK_TOLERANCE_MS,
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
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_default_config_takes_a_one_day_rail() {
    // A rail that may take a day: the default config's `orphan_after` is that day plus the
    // pre-handoff budget, so join, load and paying all take it.
    let w = World::new();
    let mut declaration = paykit_sdk::Rail::declaration(&*w.rail);
    declaration.longest_completion_ms = 86_400_000;
    w.rail.set_declaration(declaration);
    assert_eq!(
        PaykitConfig::default().orphan_after_s_for(&declaration),
        86_400 + paykit_sdk::PRE_HANDOFF_BUDGET_MS / 1000
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
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_fence_answer_committed_past_the_budget_not_paid() {
    // The attempt's answer is committed locally only once the pre-handoff budget and the
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
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(inner))
                if matches!(**inner, paykit_sdk::Refusal::HeldForUser(
                    paykit_sdk::HeldForUserReason::AttemptCheck(_)
                ))
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
    let _ = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
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
async fn test_acceptance_store_failure_leaves_the_made_payment_for_the_resume() {
    // The method makes the payment, and the store fails the commit that queues its
    // acceptance: the call returns that store failure. The payment is neither lost nor
    // failed: the journal shows it paid by the method. Once the store is sound, after a
    // restart, the resume reports it paid, queues the acceptance exactly once and finishes,
    // and a second resume queues nothing more. The method was called once.
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
    let attempt_id = paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    let listed = wallet.payments().await.unwrap().remove(0);
    assert_eq!(listed.stage, paykit_sdk::Stage::RailPaid);
    assert!(listed.paid_by_method);
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
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
        matches!(resumed.as_slice(), [PayOutcome::Paid { attempt_id: paid, .. }] if *paid == attempt_id),
        "{resumed:?}"
    );
    let finished = wallet.payments().await.unwrap().remove(0);
    assert_eq!(finished.stage, paykit_sdk::Stage::Finished);
    assert!(finished.paid_by_method);
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

/// Pay `request` while `store` fails every commit that queues an acceptance. That store
/// failure stops the wallet as a whole, so the call returns it, with the payment kept in the
/// journal: `Paying` while the method's call runs, `RailPaid` once the method reports it
/// paid. The store is sound again afterwards. Returns the payment's attempt.
async fn paid_while_acceptances_fail(
    store: &HookedStore,
    wallet: &mut Paykit,
    request: &RequestRef,
) -> paykit_lib::AttemptId {
    store.refuse_acceptances(true);
    let outcome = wallet
        .pay_request(request, PayOptions::default())
        .await
        .outcome;
    assert!(
        matches!(
            &outcome,
            Err(paykit_sdk::PaykitError::Client(
                pubky_dispatch_client::app::AppError::Store(_)
            ))
        ),
        "{outcome:?}"
    );
    store.refuse_acceptances(false);
    wallet.payments().await.unwrap()[0].attempt_id
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

#[tokio::test]
async fn test_acceptance_store_failure_backs_up_and_restores_the_made_payment() {
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
    paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    let backup = wallet
        .backup()
        .await
        .expect("a backup with a payment the method made");
    let backup = paykit_sdk::PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .expect("restore of a payment the method made");
    let _ = restored.resume_payments().await;
    // Restored and resumed, the payment the method made is still the one payment.
    assert_eq!(restored.payments().await.unwrap().len(), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_acceptance_owed_sent_with_a_users_done_on_a_pending_payment() {
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
    w.rail.next(Behaviour::Hang);
    let attempt_id = paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    // The user's `done` queues the acceptance it still owed, once.
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
}

#[tokio::test]
async fn test_acceptance_of_a_pending_payment_retried_at_resume() {
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
    w.rail.next(Behaviour::Hang);
    paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_acceptance_owed_sent_when_another_app_resolves_the_payment_done() {
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
    w.rail.next(Behaviour::Hang);
    let attempt_id = paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    // The method keeps no history of it, and the user marks it paid from the phone.
    w.rail.forget_history(&attempt_id);
    let mut declaration = paykit_sdk::Rail::declaration(&*w.rail);
    declaration.history = false;
    declaration.idempotent = true;
    declaration.idempotency_retention_ms = Some(7 * 24 * crate::common::HOUR_MS);
    w.rail.set_declaration(declaration);
    // Past the attempt's orphan window, another own app may resolve it (Dispatch wire 6.8).
    w.clock.advance_ms(4201 * 1000);
    let mut phone = w.wallet("phone").await;
    phone
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    // The wallet finds its attempt `done`: the payment was made, so its acceptance goes.
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

/// Alice's wallet, its payment of `request` stopped at `Attempted`, known unsubmitted, with
/// its acceptance signed and journaled: the commit after it failed.
async fn stopped_unsubmitted(w: &World) -> (Arc<HookedStore>, Paykit, paykit_lib::AttemptId) {
    blind(w);
    let (store, app_id, request) = with_a_request(w).await;
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
        .outcome
        .is_err());
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    assert_eq!(store.acceptances().len(), 1);
    (store, wallet, payment.attempt_id)
}

#[tokio::test]
async fn test_acceptance_never_sent_for_an_unsubmitted_payment_another_app_resolves_done() {
    let w = World::new();
    let (store, mut wallet, attempt_id) = stopped_unsubmitted(&w).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    // Past the attempt's orphan window, the user marks it paid from the phone.
    w.clock.advance_ms(4201 * 1000);
    let mut phone = w.wallet("phone").await;
    phone
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    // This app never called the method: its entry finishes, and the payee never sees an
    // acceptance of a payment that never ran (Paykit spec 7.2 step 2).
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_acceptance_never_sent_for_an_unsubmitted_payment_the_user_resolves_done() {
    let w = World::new();
    let (store, mut wallet, attempt_id) = stopped_unsubmitted(&w).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    wallet.resume_payments().await.unwrap();
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_acceptance_sent_earlier_is_placed_when_another_app_resolves_the_payment_done() {
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
    // In flight: the call is made and the acceptance is queued.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("in flight");
    };
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    w.rail.forget_history(&attempt_id);
    let mut declaration = paykit_sdk::Rail::declaration(&*w.rail);
    declaration.history = false;
    declaration.idempotent = true;
    declaration.idempotency_retention_ms = Some(7 * 24 * crate::common::HOUR_MS);
    w.rail.set_declaration(declaration);
    w.clock.advance_ms(4201 * 1000);
    let mut phone = w.wallet("phone").await;
    phone
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_dispatch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    // The wallet's write placing the acceptance meets a conflict: the entry waits.
    w.alice
        .switch()
        .fail_next(pubky_dispatch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_dispatch_core::http::ErrorCode::Conflict,
            current: serde_json::Value::Null,
        });
    wallet.resume_payments().await.unwrap();
    // The next resume places the acceptance queued before, and finishes.
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_acceptance_of_a_made_payment_sent_once_confirmed_over_a_divergent_view() {
    // The method made the payment, and the store failed the commit that queues its
    // acceptance. The request record then shows divergent: the resume holds the outcome and
    // its acceptance while the record is paused, and sends it once the user confirms, the
    // view still divergent, as the outcome's own gate does: it records the payment made.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = reload(&w, &store, app_id).await;
    paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    let record = wallet.request(&request).await.unwrap().record;
    let base = Arc::clone(&store) as Arc<dyn StateStore>;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 0);
    crate::divergent::confirm(&mut wallet, &record)
        .await
        .unwrap();
    assert!(wallet.divergent_records().await.unwrap()[0].divergent);
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
}

#[tokio::test]
async fn test_acceptance_queued_then_unplaced_finishes_after_a_restart() {
    // The method made the payment, and the store failed the commit that queues its
    // acceptance. A resume then queues it and saves the entry released, and the outcome's
    // batch fails at every try, as a crash there would leave it. After a restart the journal
    // still reads, and a resume places the acceptance without queueing it again, and
    // finishes.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = reload(&w, &store, app_id).await;
    let attempt_id = paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    // The outcome is written in three tries.
    for _ in 0..3 {
        w.alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_dispatch_core::http::ErrorCode::Conflict,
                current: serde_json::Value::Null,
            });
    }
    wallet.resume_payments().await.unwrap();
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::RailPaid);
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    drop(wallet);
    let mut wallet = reload(&w, &store, app_id).await;
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1);
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

/// Alice's wallet `app_id` loaded again on `store`, as after a restart.
async fn reload(w: &World, store: &Arc<HookedStore>, app_id: [u8; 16]) -> Paykit {
    w.load_on_clock(
        &w.alice,
        Arc::clone(store) as Arc<dyn StateStore>,
        app_id,
        Arc::clone(&w.clock),
    )
    .await
}

/// After a crash at `fault` in `act`, the wallet reloads from what was committed: its journal
/// reads, a resume (and, for a user's `done` cut short, the user asking again) finishes the
/// payment, and its acceptance was queued exactly once and placed. Returns whether the fault
/// was hit.
async fn crash_owed_acceptance_path(
    fault: pubky_dispatch_client::store::Fault,
    users_done: bool,
) -> bool {
    use pubky_dispatch_client::store::FaultStore;
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = reload(&w, &store, app_id).await;
    // The acceptance can't be queued: a paid payment owes it, or a pending one does.
    if users_done {
        w.rail.next(Behaviour::Hang);
    }
    let attempt_id = paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    drop(wallet);
    let done = || paykit_sdk::Resolution::Done;
    let faulty = Arc::new(FaultStore::new(
        Arc::clone(&store) as Arc<dyn StateStore>,
        fault,
    ));
    let mut crashing = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&faulty) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
        )
        .await;
    let _ = if users_done {
        crashing
            .resolve_attempt(
                &obligation,
                attempt_id,
                done(),
                pubky_dispatch_client::app::UserConfirmed::new(),
            )
            .await
    } else {
        crashing.resume_payments().await.map(|_| ())
    };
    let hit = faulty.stopped();
    drop(crashing);
    let mut wallet = reload(&w, &store, app_id).await;
    wallet.resume_payments().await.unwrap();
    if users_done && wallet.payments().await.unwrap()[0].stage != paykit_sdk::Stage::Finished {
        let _ = wallet
            .resolve_attempt(
                &obligation,
                attempt_id,
                done(),
                pubky_dispatch_client::app::UserConfirmed::new(),
            )
            .await;
        wallet.resume_payments().await.unwrap();
    }
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished,
        "{fault:?}"
    );
    assert_eq!(queued_acceptances(&store, &wallet).await, 1, "{fault:?}");
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["response"].is_null(), "{fault:?}: {value}");
    assert_eq!(w.rail.executions(&attempt_id), 1, "{fault:?}");
    hit
}

/// Every commit point of `users_done`'s path, stopping after and before each.
async fn crash_at_every_commit(users_done: bool) {
    use pubky_dispatch_client::store::Fault;
    for stop in [Fault::StopAfter as fn(u64) -> Fault, Fault::StopBefore] {
        let mut n = 1;
        while crash_owed_acceptance_path(stop(n), users_done).await {
            n += 1;
            assert!(n < 64, "the path never finished");
        }
        assert!(n > 1, "the path commits");
    }
}

#[tokio::test]
async fn test_crash_finishing_a_made_payments_acceptance_queues_it_once() {
    crash_at_every_commit(false).await;
}

#[tokio::test]
async fn test_crash_in_a_users_done_on_a_pending_payment_queues_its_acceptance_once() {
    crash_at_every_commit(true).await;
}

#[tokio::test]
async fn test_acceptance_of_a_made_payment_restored_bare_is_placed_at_the_first_resume() {
    // Paid by the method, its acceptance not queued as the store failed, then backed up; a
    // bare restore resumes first. The records are listed before the acceptance is placed:
    // it goes into the record the Dispatch holds, and only then does the entry finish.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = reload(&w, &store, app_id).await;
    paid_while_acceptances_fail(&store, &mut wallet, &request).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.resume_payments().await.unwrap();
    assert_eq!(
        app.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    let value = crate::outcome_record::record_value(&mut app, &request)
        .await
        .unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
}

#[derive(Clone, Copy)]
enum DenyBoundary {
    Commit,
    LocalRead,
    Unavailable,
}

/// A user payment whose deny copy expires immediately before handoff.
async fn manual_deny_boundary(
    change: Option<fn(&mut paykit_sdk::BlockedPayment)>,
    boundary: DenyBoundary,
) -> (World, PayOutcome) {
    let (w, outcome) = manual_deny_boundary_result(change, boundary).await;
    (w, outcome.unwrap())
}

/// [`manual_deny_boundary`], with the call's result as it is.
async fn manual_deny_boundary_result(
    change: Option<fn(&mut paykit_sdk::BlockedPayment)>,
    boundary: DenyBoundary,
) -> (World, paykit_sdk::Result<PayOutcome>) {
    let w = World::new();
    let store = Arc::new(HookedStore::default());
    let scopes = paykit_sdk::AppScopes::new()
        .pay()
        .manage_contacts()
        .build()
        .unwrap();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            scopes.clone(),
            crate::common::day_window(),
            Arc::clone(&store) as Arc<dyn StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            scopes,
            PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(
        &mut shop,
        w.alice_id(),
        draft("0.000000001", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let Some(paykit_sdk::Refusal::Blocked { payment }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    let mut payment = (**payment).clone();
    assert_eq!(payment.amount, crate::common::btc("0.00000001"));
    let options = if let Some(change) = change {
        change(&mut payment);
        PayOptions::default().with_blocked_confirmed(payment)
    } else {
        PayOptions::default()
    };
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    phone.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS - 1);
    let clock = Arc::clone(&w.clock);
    let dispatch = w.alice.switch().clone();
    let local = Arc::clone(&store);
    store.on_paying(Box::new(move || {
        if matches!(boundary, DenyBoundary::LocalRead) {
            *local.deny_hook.lock().unwrap() = Some(Box::new(move || clock.advance_ms(1)));
        } else {
            clock.advance_ms(1);
        }
        if matches!(boundary, DenyBoundary::Unavailable) {
            dispatch.fail_next(pubky_dispatch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_dispatch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
        }
    }));
    let outcome = wallet.pay_request(&request, options).await.outcome;
    (w, outcome)
}

#[tokio::test]
async fn test_final_check_manual_handoff_refreshes_the_deny_copy_at_ten_minutes() {
    let (w, outcome) = manual_deny_boundary(None, DenyBoundary::Commit).await;
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_final_check_manual_handoff_binds_every_confirmed_payment_field() {
    let changes: [fn(&mut paykit_sdk::BlockedPayment); 5] = [
        |_| {},
        |p| p.amount = crate::common::btc("0.000000001"),
        |p| p.payee = [71; 32],
        |p| p.request.as_mut().unwrap().request_hash = [71; 32],
        |p| {
            p.period = Some(paykit_lib::periods::PeriodRef {
                starts_at: 1,
                ends_at: 2,
            })
        },
    ];
    for (i, change) in changes.into_iter().enumerate() {
        let (w, outcome) = manual_deny_boundary(Some(change), DenyBoundary::Commit).await;
        assert_eq!(
            matches!(outcome, PayOutcome::Paid { .. }),
            i == 0,
            "{i}: {outcome:?}"
        );
        assert_eq!(w.rail.all_executions().is_empty(), i != 0, "{i}");
    }
}

#[tokio::test]
async fn test_final_check_manual_confirmation_cannot_replace_a_due_deny_read() {
    // The deny list due again can't be read just before the call: the user's confirmation of
    // the blocked payee doesn't stand in for it. The attempt is written failed with the read's
    // failure as its cause, and nothing is paid.
    let (w, outcome) = manual_deny_boundary_result(Some(|_| {}), DenyBoundary::Unavailable).await;
    let refused = outcome.unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::AttemptFailed(why))
                if matches!(**why, paykit_sdk::Refusal::DispatchUnavailable(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_final_check_manual_deny_age_is_judged_after_the_local_read() {
    let (w, outcome) = manual_deny_boundary(None, DenyBoundary::LocalRead).await;
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_final_check_manual_override_keeps_its_selected_transfer_amount() {
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
    w.rail.set_transfer_amount(Some(crate::common::btc("3")));
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    let Some(paykit_sdk::Refusal::Transfer { quote, .. }) = refused.refusal() else {
        panic!("{refused:?}")
    };
    let rail = Arc::clone(&w.rail);
    store.on_paying(Box::new(move || {
        rail.set_transfer_amount(Some(crate::common::btc("1")))
    }));
    let refused = wallet
        .pay_request(
            &request,
            PayOptions::default().with_transfer_confirmed((**quote).clone()),
        )
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::AttemptFailed(inner))
        if matches!(**inner, paykit_sdk::Refusal::Transfer { .. })),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

/// Alice's wallet on a hooked store with two of bob's requests paid: the first made by the
/// method while the store failed the commit queueing its acceptance, so it is journaled
/// `RailPaid` with its outcome unwritten, and the second in flight. Worlds are made until the
/// first comes first in the journal, which is in attempt-ID order, so a resume meets it
/// before the other. Returns the wallet, its world and the second's attempt.
async fn made_then_in_flight() -> (World, Paykit, paykit_sdk::AttemptId) {
    for _ in 0..40 {
        let w = World::new();
        let store = Arc::new(HookedStore::default());
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        paid_while_acceptances_fail(&store, &mut wallet, &first).await;
        w.rail.next(Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } = wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
            .unwrap()
        else {
            panic!("the second payment is in flight");
        };
        let order = wallet.payments().await.unwrap();
        if order[0].attempt_id == attempt_id {
            continue;
        }
        assert_eq!(order[0].stage, paykit_sdk::Stage::RailPaid);
        drop(shop);
        return (w, wallet, attempt_id);
    }
    panic!("no world put the made payment first");
}

#[tokio::test]
async fn test_resume_ends_at_the_dispatchs_answer_that_the_wallet_must_stop() {
    // The made payment's outcome write is answered that the app is no longer current, that
    // its certificate expired, or that the account awaits a recovery: a stop of the wallet
    // as a whole, which the resume returns. The second, which the method shows paid, was
    // journaled with its proof before the stop was met, from the method's history alone:
    // nothing of it goes to the Dispatch, and it stays `RailPaid`.
    use pubky_dispatch_core::http::ErrorCode as E;
    for code in [E::NotCurrent, E::CertExpired, E::RecoveryRequired] {
        let (w, mut wallet, attempt_id) = made_then_in_flight().await;
        w.rail.settle(&attempt_id, true);
        w.alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::Error {
                path: "/ops".into(),
                code,
                current: serde_json::Value::Null,
            });
        let resumed = wallet.resume_payments().await;
        assert_eq!(
            resumed
                .as_ref()
                .err()
                .and_then(paykit_sdk::PaykitError::code),
            Some(code),
            "{resumed:?}"
        );
        let second = wallet
            .payments()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap();
        assert_eq!(second.stage, paykit_sdk::Stage::RailPaid, "{code:?}");
        assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    }
}

#[tokio::test]
async fn test_resume_ends_when_its_first_claim_read_installs_a_recovery_notice() {
    // The first entry's payment stopped before its handoff, so the resume reads its claim
    // first. The Dispatch answers that read that the account awaits a recovery, with the
    // notice, which the client keeps. The wallet is stopped from then on: the resume returns
    // the stop. The later payment, which the method shows paid, was journaled with its proof
    // before, from the method's history alone, and nothing of it is written to the dispatch.
    for _ in 0..40 {
        let w = World::new();
        blind(&w);
        let store = Arc::new(HookedStore::default());
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        store.fail_commit_after_acceptance();
        assert!(wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome
            .is_err());
        w.rail.next(Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } = wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
            .unwrap()
        else {
            panic!("the second payment is in flight");
        };
        let order = wallet.payments().await.unwrap();
        if order[0].attempt_id == attempt_id {
            continue;
        }
        assert_eq!(order[0].stage, paykit_sdk::Stage::Attempted);
        assert_eq!(order[1].stage, paykit_sdk::Stage::Paying);
        sighted(&w);
        w.rail.settle(&attempt_id, true);
        let generation = wallet.app().own_identity().generation();
        w.alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::Error {
                path: "/claims".into(),
                code: pubky_dispatch_core::http::ErrorCode::RecoveryRequired,
                current: serde_json::json!({
                    "generation": generation,
                    "reason": "behind",
                    "permanent": false,
                }),
            });
        let resumed = wallet.resume_payments().await;
        assert!(resumed.is_err(), "{resumed:?}");
        assert!(wallet.app().recovery_required().is_some());
        let later = wallet
            .payments()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap();
        assert_eq!(later.stage, paykit_sdk::Stage::RailPaid);
        assert_eq!(w.rail.pay_calls(&attempt_id), 1);
        return;
    }
    panic!("no world put the stopped payment first");
}

#[tokio::test]
async fn test_resume_ends_at_a_store_failure_a_step_below_it_meets() {
    // The store fails every commit that queues an acceptance. The first payment's acceptance
    // can't be queued when it is handed off, and that store failure is the call's answer: the
    // payment may be running, and the journal keeps it in flight. The second is handed off
    // with the store sound. Both are then shown paid. Resuming the first meets the store
    // failure again while releasing its acceptance: it ends the resume, so the second payment
    // is neither looked up nor written.
    for _ in 0..40 {
        let w = World::new();
        let store = Arc::new(HookedStore::default());
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        store.refuse_acceptances(true);
        w.rail.next(Behaviour::Hang);
        let refused = wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome;
        assert!(
            matches!(
                &refused,
                Err(paykit_sdk::PaykitError::Client(
                    pubky_dispatch_client::app::AppError::Store(_)
                ))
            ),
            "{refused:?}"
        );
        store.refuse_acceptances(false);
        w.rail.next(Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } = wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
            .unwrap()
        else {
            panic!("the second payment is in flight");
        };
        let order = wallet.payments().await.unwrap();
        if order[0].attempt_id == attempt_id {
            continue;
        }
        assert!(order.iter().all(|p| p.stage == paykit_sdk::Stage::Paying));
        for payment in &order {
            w.rail.settle(&payment.attempt_id, true);
        }
        store.refuse_acceptances(true);
        let resumed = wallet.resume_payments().await;
        assert!(
            matches!(
                &resumed,
                Err(paykit_sdk::PaykitError::Client(
                    pubky_dispatch_client::app::AppError::Store(_)
                ))
            ),
            "{resumed:?}"
        );
        let later = wallet
            .payments()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap();
        assert_eq!(later.stage, paykit_sdk::Stage::Paying);
        assert_eq!(w.rail.executions(&attempt_id), 1);
        return;
    }
    panic!("no world put the first payment first");
}

#[tokio::test]
async fn test_pay_whose_outcome_write_is_answered_not_current_returns_the_stop() {
    // The method makes the payment, and the Dispatch answers its outcome write that the app is
    // no longer current: a stop of the wallet as a whole, which the call returns. The host
    // gets an error for a payment that was made, so `payments` lists it paid by the method
    // with its outcome unwritten (`RailPaid`), not failed. The method was called once.
    let w = World::new();
    let (store, app_id, request) = with_a_request(&w).await;
    let mut wallet = reload(&w, &store, app_id).await;
    let dispatch = Arc::clone(w.alice.switch());
    store.on_paying(Box::new(move || {
        dispatch.fail_next(pubky_dispatch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_dispatch_core::http::ErrorCode::NotCurrent,
            current: serde_json::Value::Null,
        })
    }));
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert_eq!(
        outcome
            .as_ref()
            .err()
            .and_then(paykit_sdk::PaykitError::code),
        Some(pubky_dispatch_core::http::ErrorCode::NotCurrent),
        "{outcome:?}"
    );
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::RailPaid);
    assert!(payment.paid_by_method);
    assert_eq!(w.rail.pay_calls(&payment.attempt_id), 1);
}

/// Two of bob's requests paid by alice's wallet, both left in flight: the wallet, its world,
/// and their attempts in journal order.
async fn two_in_flight() -> (World, Paykit, [paykit_sdk::AttemptId; 2]) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let requests = [
        request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await,
        request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await,
    ];
    wallet.receive(None).await.unwrap();
    for request in &requests {
        w.rail.next(Behaviour::Hang);
        let outcome = wallet
            .pay_request(request, PayOptions::default())
            .await
            .outcome
            .unwrap();
        assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    }
    let order = wallet.payments().await.unwrap();
    assert!(order.iter().all(|p| p.stage == paykit_sdk::Stage::Paying));
    let attempts = [order[0].attempt_id, order[1].attempt_id];
    drop(shop);
    (w, wallet, attempts)
}

#[tokio::test]
async fn test_resume_visits_the_later_payment_past_a_lookup_that_fails() {
    // Two payments in flight, both shown paid by the method. Its history can't be read for
    // the first: a wait, so the first is left for the next resume, reported pending, and the
    // later payment is still resolved behind it, its outcome written.
    let (w, mut wallet, [first, later]) = two_in_flight().await;
    for attempt in wallet.payments().await.unwrap() {
        w.rail.settle(&attempt.attempt_id, true);
    }
    w.rail.set_lookup_unavailable(&first, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        resumed
            .iter()
            .any(|o| matches!(o, PayOutcome::Pending { attempt_id, .. } if *attempt_id == first)),
        "{resumed:?}"
    );
    let stage = |payments: &[paykit_sdk::PaymentView], attempt| {
        payments
            .iter()
            .find(|p| p.attempt_id == attempt)
            .unwrap()
            .stage
    };
    let payments = wallet.payments().await.unwrap();
    assert_eq!(stage(&payments, first), paykit_sdk::Stage::Paying);
    assert!(
        matches!(
            stage(&payments, later),
            paykit_sdk::Stage::Finished | paykit_sdk::Stage::AcceptanceOwed
        ),
        "{payments:?}"
    );
    // The method answers again: the first is resolved too, neither paid twice.
    w.rail.set_lookup_unavailable(&first, false);
    wallet.resume_payments().await.unwrap();
    let payments = wallet.payments().await.unwrap();
    assert_ne!(stage(&payments, first), paykit_sdk::Stage::Paying);
    assert_eq!(w.rail.pay_calls(&first), 1);
    assert_eq!(w.rail.pay_calls(&later), 1);
}

#[tokio::test]
async fn test_resume_returns_a_service_answer_no_retry_passes_after_the_later_payment() {
    // The first payment stopped before its handoff, so the resume reads its claim first, and
    // the Dispatch answers that read with an error that is no stop and that no retry passes
    // (Dispatch spec 6.4). The later payment, which the method shows paid, is still resolved
    // behind it, its outcome written, and the call then returns the answer as it is, so the
    // caller learns why the first is left rather than seeing it pending at every resume.
    for _ in 0..40 {
        let w = World::new();
        blind(&w);
        let store = Arc::new(HookedStore::default());
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        store.fail_commit_after_acceptance();
        assert!(wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome
            .is_err());
        w.rail.next(Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } = wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
            .unwrap()
        else {
            panic!("the second payment is in flight");
        };
        let order = wallet.payments().await.unwrap();
        if order[0].attempt_id == attempt_id {
            continue;
        }
        sighted(&w);
        w.rail.settle(&attempt_id, true);
        let code = pubky_dispatch_core::http::ErrorCode::PermissionMissing;
        w.alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::Error {
                path: "/claims".into(),
                code,
                current: serde_json::Value::Null,
            });
        let resumed = wallet.resume_payments().await;
        assert_eq!(
            resumed
                .as_ref()
                .err()
                .and_then(paykit_sdk::PaykitError::code),
            Some(code),
            "{resumed:?}"
        );
        let later = wallet
            .payments()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.attempt_id == attempt_id)
            .unwrap();
        assert!(
            matches!(
                later.stage,
                paykit_sdk::Stage::Finished | paykit_sdk::Stage::AcceptanceOwed
            ),
            "{later:?}"
        );
        assert_eq!(w.rail.pay_calls(&attempt_id), 1);
        // The call that ended it returned an error, so it is told by the next one, once.
        let resumed = wallet.resume_payments().await.unwrap();
        assert_eq!(
            resumed
                .iter()
                .filter(|outcome| matches!(
                    outcome,
                    PayOutcome::Paid { attempt_id: paid, proof_msg_id: Some(_), .. } if *paid == attempt_id
                ))
                .count(),
            1,
            "{resumed:?}"
        );
        assert!(!wallet
            .resume_payments()
            .await
            .unwrap()
            .iter()
            .any(|outcome| outcome.attempt_id() == attempt_id));
        return;
    }
    panic!("no world put the stopped payment first");
}

#[tokio::test]
async fn test_pay_reports_paid_when_a_stop_follows_its_written_outcome() {
    // The store stops after some commit of the payment. Wherever that falls after the outcome
    // batch committed, which finishes the entry, the payment is reported `Paid` once: by the
    // call, where it marked the outcome told first, or else by the next resume. The one
    // exception is a stop right after the told mark, the call's last commit: the host never
    // gets the call's answer, and the journal shows the payment made.
    use pubky_dispatch_client::store::{Fault, FaultStore};
    let mut reported_by_resume = false;
    let mut stopped_after_told = None;
    for after in 1..64 {
        let w = World::new();
        let (store, app_id, request) = with_a_request(&w).await;
        let faulty = Arc::new(FaultStore::new(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Fault::StopAfter(after),
        ));
        let mut wallet = w
            .load_on_clock(
                &w.alice,
                Arc::clone(&faulty) as Arc<dyn StateStore>,
                app_id,
                Arc::clone(&w.clock),
            )
            .await;
        let outcome = wallet
            .pay_request(&request, PayOptions::default())
            .await
            .outcome;
        let mut reloaded = reload(&w, &store, app_id).await;
        let journal = reloaded.payments().await.unwrap();
        if journal
            .first()
            .is_some_and(|p| p.stage == paykit_sdk::Stage::Finished)
        {
            assert!(journal[0].paid_by_method);
            let resumed = reloaded.resume_payments().await.unwrap();
            if matches!(outcome, Ok(PayOutcome::Paid { .. })) {
                assert!(resumed.is_empty(), "after {after}: {resumed:?}");
            } else if resumed.is_empty() {
                stopped_after_told = Some(after);
            } else {
                assert!(
                    matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
                    "after {after}: {outcome:?} then {resumed:?}"
                );
                reported_by_resume = true;
                assert!(reloaded.resume_payments().await.unwrap().is_empty());
            }
        }
        if !faulty.stopped() {
            assert!(
                matches!(outcome, Ok(PayOutcome::Paid { .. })),
                "{outcome:?}"
            );
            assert!(reported_by_resume, "no stop fell after the written outcome");
            // The only stop after which nothing reports it is the one after the last commit.
            assert_eq!(stopped_after_told, Some(after - 1));
            return;
        }
    }
    panic!("the payment never finished within the commits tried");
}

#[tokio::test]
async fn test_resume_releases_no_new_acceptance_while_the_payer_side_awaits_keys() {
    // The payment's acceptance couldn't be queued at its handoff, and the method then shows
    // it paid. The wallet is renewed to read more with no bundle, so the payer's side waits
    // for its keys: the resume journals the proof and signs or sends no acceptance, nor
    // anything else. Once the keys come and the records are listed, the acceptance goes.
    let w = World::new();
    let store = Arc::new(HookedStore::default());
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
        .await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    store.refuse_acceptances(true);
    w.rail.next(Behaviour::Hang);
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome;
    assert!(refused.is_err(), "{refused:?}");
    store.refuse_acceptances(false);
    let attempt_id = wallet.payments().await.unwrap()[0].attempt_id;
    let old = wallet.app().cert().clone();
    let wider = crate::certificate::renew(
        &w,
        me,
        &paykit_sdk::AppScopes::new()
            .pay()
            .request()
            .build()
            .unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    crate::certificate::list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    w.rail.settle(&attempt_id, true);
    let journaled = store.acceptances().len();
    let before = w.router.log().len();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    // No acceptance signed, queued or sent, and nothing written.
    assert_eq!(store.acceptances().len(), journaled);
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), 0);
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    crate::certificate::store_bundle(&w, me, &wider);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    // The acceptance and the proof, each once.
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), 2);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_resume_waits_past_an_unverifiable_gateway_error_on_one_claim_read() {
    // The first payment stopped before its handoff, so the resume reads its claim first, and
    // that read is answered `503` under a key that isn't the service's, as from a proxy in
    // front of it: nothing of it is used, and it is a wait (Dispatch wire 7.1). The first is
    // reported pending, and the later payment, which the method shows paid, is resolved
    // behind it; the call returns both outcomes.
    for _ in 0..40 {
        let w = World::new();
        blind(&w);
        let store = Arc::new(HookedStore::default());
        let mut wallet = w
            .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
            .await;
        let mut shop = w.shop().await;
        let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        store.fail_commit_after_acceptance();
        assert!(wallet
            .pay_request(&first, PayOptions::default())
            .await
            .outcome
            .is_err());
        w.rail.next(Behaviour::Hang);
        let PayOutcome::Pending { attempt_id, .. } = wallet
            .pay_request(&second, PayOptions::default())
            .await
            .outcome
            .unwrap()
        else {
            panic!("the second payment is in flight");
        };
        let order = wallet.payments().await.unwrap();
        if order[0].attempt_id == attempt_id {
            continue;
        }
        let stopped = order[0].attempt_id;
        sighted(&w);
        w.rail.settle(&attempt_id, true);
        w.alice
            .switch()
            .fail_next(pubky_dispatch_client::testing::Fault::ErrorSignedWith {
                path: "claims".into(),
                code: pubky_dispatch_core::http::ErrorCode::Unavailable,
                secret: [9; 32],
            });
        let resumed = wallet.resume_payments().await.unwrap();
        assert!(
            resumed.iter().any(
                |o| matches!(o, PayOutcome::Pending { attempt_id, .. } if *attempt_id == stopped)
            ),
            "{resumed:?}"
        );
        assert!(
            resumed.iter().any(
                |o| matches!(o, PayOutcome::Paid { attempt_id: paid, .. } if *paid == attempt_id)
            ),
            "{resumed:?}"
        );
        assert_eq!(w.rail.pay_calls(&attempt_id), 1);
        return;
    }
    panic!("no world put the stopped payment first");
}
