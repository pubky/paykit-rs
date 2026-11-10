//! The final payment check (Paykit spec 4, 7.2): the last thing before the payment method's
//! call, on a first dispatch, a resume and a recovery. A late switch answer that removes the
//! destination's app, or a payee copy that aged past its limit meanwhile, pays nothing.

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

/// A store that runs a hook once, right after it commits the journal entry that starts a
/// payment (`Paying`): the final journal write before the payment method's call.
#[derive(Default)]
struct HookedStore {
    inner: MemoryStore,
    hook: Mutex<Option<Hook>>,
}

impl HookedStore {
    fn on_paying(&self, hook: Hook) {
        *self.hook.lock().unwrap() = Some(hook);
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
        let paying = batch.writes.iter().any(|write| match write {
            Write::Put { key, value, .. } => {
                key.starts_with(b"p/j/")
                    && value
                        .windows(b"\"stage\":\"paying\"".len())
                        .any(|w| w == b"\"stage\":\"paying\"")
            }
            _ => false,
        });
        self.inner.commit(batch)?;
        if paying {
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
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
        .load_on_clock(
            &w.alice,
            Arc::clone(&store) as Arc<dyn StateStore>,
            app_id,
            Arc::clone(&w.clock),
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
/// payment method: a fresh world for each commit tried, until one is found.
async fn stopped_before_the_rail() -> (World, Arc<HookedStore>, [u8; 16], paykit_lib::AttemptId) {
    use pubky_switch_client::store::{Fault, FaultStore};
    for n in 1..64 {
        let w = World::new();
        repeatable(&w);
        let (store, app_id, request) = with_a_request(&w).await;
        let faulty: Arc<dyn StateStore> = Arc::new(FaultStore::new(
            Arc::clone(&store) as Arc<dyn StateStore>,
            Fault::StopAfter(n),
        ));
        let mut crashing = w
            .load_on_clock(&w.alice, faulty, app_id, Arc::clone(&w.clock))
            .await;
        let _ = crashing.pay_request(&request, PayOptions::default()).await;
        drop(crashing);
        assert!(
            w.rail.all_executions().is_empty(),
            "the rail was reached at {n}"
        );
        let loaded = w
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
    // answers only during the final journal write.
    w.clock.advance_ms(11 * 60_000);
    let slow = held_switch(&w);
    let reads = HookedReads::new(&w);
    let release = Arc::clone(&slow);
    store.on_paying(Box::new(move || release.release()));
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
