//! Paykit's local state read back bound to its key and to what its caller looks it up for
//! (`state::Stored` in the SDK): a value naming another attempt, claim, record, sender,
//! message or stored entry than the one it was looked up for is a corrupt store, and
//! nothing is decided, written or forgotten from it.

use std::sync::{Arc, Mutex};

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::UnrequestedPayment;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaykitError, RequestRef, Resolution};
use pubky_switch_client::app::{
    AppError, Collection as SwitchCollection, LocalWrite, OpBatch, RecordTarget, UserConfirmed,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{KeyValue, MemoryStore, StateStore};
use pubky_switch_core::claims::Outcome;
use pubky_switch_core::enc::b64;
use serde_json::{json, Value};

use crate::common::refuse_writes;
use crate::common::{btc, draft, endpoint, list_version, request_from, World};
use crate::receipts::deliver_proved_receipt;
use crate::restore::{kept, kept_message};

/// A `msg_id` no message here has.
const OTHER_MSG_ID: &str = "223e4567-e89b-42d3-a456-426614174000";

/// Whether `error` is the store holding a value that doesn't bind.
fn corrupt(error: &PaykitError) -> bool {
    matches!(
        error,
        PaykitError::Client(AppError::Store(StoreError::Corrupt))
    )
}

/// The bytes Paykit keeps at `key`.
async fn held(app: &Paykit, key: &str) -> Option<Vec<u8>> {
    app.app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
}

/// The value Paykit keeps at `key`, as JSON.
async fn held_json(app: &Paykit, key: &str) -> Value {
    serde_json::from_slice(&held(app, key).await.unwrap()).unwrap()
}

/// Put `value` at `key` of Paykit's state, as a store that holds it would serve it.
async fn put(app: &mut Paykit, key: &str, value: &Value) {
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.as_bytes().to_vec(),
                value: serde_json::to_vec(value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

/// Delete `key` of Paykit's state.
async fn delete(app: &mut Paykit, key: &str) {
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Delete {
                key: key.as_bytes().to_vec(),
            }],
        )
        .await
        .unwrap();
}

/// The keys of Paykit's state under `prefix`.
async fn keys(app: &Paykit, prefix: &str) -> Vec<String> {
    app.app()
        .protocol_scan("paykit", prefix.as_bytes())
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| String::from_utf8(key).unwrap())
        .collect()
}

fn journal(attempt: &AttemptId) -> String {
    format!("j/{}", attempt.to_text())
}

/// The outcome of `attempt` on the claim of `obligation`, as the switch holds it.
async fn outcome(wallet: &mut Paykit, obligation: &[u8; 32], attempt: &AttemptId) -> Outcome {
    let read = wallet
        .app_mut()
        .read_claim(obligation)
        .await
        .unwrap()
        .unwrap();
    read.view.attempt(&attempt.to_bytes()).unwrap().outcome
}

/// A store that serves set bytes for single reads of one key, its scans untouched: the value
/// under the key changes between the scan that found an entry and a later read of it.
#[derive(Default)]
struct SwappedReads {
    inner: MemoryStore,
    served: Mutex<Option<(Vec<u8>, Vec<u8>)>>,
}

impl SwappedReads {
    /// Serve `bytes` for reads of Paykit's `key`.
    fn serve(&self, key: &str, bytes: Vec<u8>) {
        *self.served.lock().unwrap() = Some((format!("p/paykit/{key}").into_bytes(), bytes));
    }
}

impl StateStore for SwappedReads {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        if let Some((served, bytes)) = &*self.served.lock().unwrap() {
            if served == key {
                return Ok(Some(bytes.clone()));
            }
        }
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

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl SwappedReads {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        apply(batch)
    }
}

/// Two payments of bob's requests: `b` paid in full, `a` pending.
struct Two {
    a: RequestRef,
    a_attempt: AttemptId,
    a_obligation: [u8; 32],
    b: RequestRef,
    b_attempt: AttemptId,
    b_obligation: [u8; 32],
}

/// Bob sends requests `a` and `b`; alice pays `b` in full and leaves her payment of `a`
/// pending.
async fn paid_and_pending(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> Two {
    let b = request_from(shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    let a = request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&b, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid {
        attempt_id: b_attempt,
        ..
    } = paid
    else {
        panic!("{paid:?}");
    };
    w.rail.next(Behaviour::Hang);
    let pending = wallet
        .pay_request(&a, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending {
        attempt_id: a_attempt,
        ..
    } = pending
    else {
        panic!("{pending:?}");
    };
    let payments = wallet.payments().await.unwrap();
    let obligation = |attempt: AttemptId| {
        payments
            .iter()
            .find(|p| p.attempt_id == attempt)
            .unwrap()
            .obligation
    };
    Two {
        a_obligation: obligation(a_attempt),
        b_obligation: obligation(b_attempt),
        a,
        a_attempt,
        b,
        b_attempt,
    }
}

/// Bob sends one request; alice's first attempt on it is refused by the payment method and
/// written `failed`, and her second, `a`, is left pending. `b` is the first attempt, on the
/// same claim.
async fn refused_then_pending(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> Two {
    let request = request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Refuse);
    let failed = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Failed {
        attempt_id: b_attempt,
        ..
    } = failed
    else {
        panic!("{failed:?}");
    };
    w.rail.next(Behaviour::Hang);
    let pending = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let PayOutcome::Pending {
        attempt_id: a_attempt,
        ..
    } = pending
    else {
        panic!("{pending:?}");
    };
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    Two {
        a: request,
        a_attempt,
        a_obligation: obligation,
        b: request,
        b_attempt,
        b_obligation: obligation,
    }
}

/// What is put at `a`'s key.
#[derive(Clone, Copy)]
enum Swap {
    /// `b`'s journal entry: another attempt's.
    AnotherAttempt,
    /// The finished entry of an earlier attempt on `a`'s own claim: another attempt's, of the
    /// same claim and request.
    EarlierAttempt,
    /// `b`'s journal entry with `a`'s attempt ID: an entry of `a`'s attempt on `b`'s claim,
    /// paying `b`'s request.
    AnotherClaim,
}

/// The two payments `swap` is about.
async fn two_payments(w: &World, wallet: &mut Paykit, shop: &mut Paykit, swap: Swap) -> Two {
    match swap {
        Swap::EarlierAttempt => refused_then_pending(w, wallet, shop).await,
        Swap::AnotherAttempt | Swap::AnotherClaim => paid_and_pending(w, wallet, shop).await,
    }
}

/// The entry `swap` puts at `a`'s key.
async fn swapped(wallet: &Paykit, two: &Two, swap: Swap) -> Value {
    let mut entry = held_json(wallet, &journal(&two.b_attempt)).await;
    if let Swap::AnotherClaim = swap {
        entry["d"]["attempt_id"] = Value::String(two.a_attempt.to_text());
    }
    entry
}

/// A finished entry read at `a`'s key once its payment is found paid: the outcome's write
/// reads the entry again there, as a batch kept from before may have finished it. Returns
/// what the resume returns, and `a`'s outcome on its claim.
async fn outcome_written_over(swap: Swap) -> (paykit_sdk::Result<Vec<PayOutcome>>, Outcome) {
    let w = World::new();
    let base = Arc::new(SwappedReads::default());
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&base) as Arc<dyn StateStore>)
        .await;
    let mut shop = w.shop().await;
    let two = two_payments(&w, &mut wallet, &mut shop, swap).await;
    let value = swapped(&wallet, &two, swap).await;
    base.serve(
        &journal(&two.a_attempt),
        serde_json::to_vec(&value).unwrap(),
    );
    w.rail.settle(&two.a_attempt, true);
    let resumed = wallet.resume_payments().await;
    let claim = outcome(&mut wallet, &two.a_obligation, &two.a_attempt).await;
    (resumed, claim)
}

/// The resume ended at the corrupt store: a store failure stops the wallet as a whole.
fn assert_corrupt(resumed: &paykit_sdk::Result<Vec<PayOutcome>>) {
    assert!(
        matches!(
            resumed,
            Err(paykit_sdk::PaykitError::Client(
                pubky_switch_client::app::AppError::Store(
                    pubky_switch_client::error::StoreError::Corrupt
                )
            ))
        ),
        "{resumed:?}"
    );
}

#[tokio::test]
async fn test_local_state_finished_entry_of_another_attempt_never_finishes_a_payment() {
    // `b`'s finished entry under `a`'s key: `a`'s outcome isn't taken as written. The entry
    // doesn't bind to its key, a corrupt store, which stops the wallet: the resume returns it,
    // and the claim shows the payment open still, for a resume once the store is sound.
    let (reported, claim) = outcome_written_over(Swap::AnotherAttempt).await;
    assert_corrupt(&reported);
    assert_eq!(claim, Outcome::Open);
}

#[tokio::test]
async fn test_local_state_finished_entry_of_an_earlier_attempt_never_finishes_a_payment() {
    // The finished entry of an earlier attempt on the same claim, under `a`'s key: of the
    // claim, but not of the attempt the key names, so it finishes nothing either.
    let (reported, claim) = outcome_written_over(Swap::EarlierAttempt).await;
    assert_corrupt(&reported);
    assert_eq!(claim, Outcome::Open);
}

#[tokio::test]
async fn test_local_state_finished_entry_of_the_attempt_on_another_claim_finishes_nothing() {
    // `a`'s attempt ID on `b`'s finished entry: the entry is of another claim than the one
    // the outcome is written on, so it finishes nothing.
    let (reported, claim) = outcome_written_over(Swap::AnotherClaim).await;
    assert_corrupt(&reported);
    assert_eq!(claim, Outcome::Open);
}

/// What a refused resolution leaves as it was: both claims, both records and the journal
/// entry at `a`'s key.
#[derive(Debug, PartialEq)]
struct Untouched {
    a_claim: (u64, Outcome),
    b_claim: u64,
    a_writes: usize,
    b_writes: usize,
    a_entry: Option<Vec<u8>>,
}

impl Untouched {
    async fn of(w: &World, wallet: &mut Paykit, two: &Two) -> Self {
        let a_read = wallet
            .app_mut()
            .read_claim(&two.a_obligation)
            .await
            .unwrap()
            .unwrap();
        let b_read = wallet
            .app_mut()
            .read_claim(&two.b_obligation)
            .await
            .unwrap()
            .unwrap();
        let record_key = |request: &RequestRef| {
            wallet
                .app()
                .record_key(&crate::outcome_record::payer_record(request))
                .unwrap()
        };
        Self {
            a_claim: (
                a_read.view.step,
                a_read
                    .view
                    .attempt(&two.a_attempt.to_bytes())
                    .unwrap()
                    .outcome,
            ),
            b_claim: b_read.view.step,
            a_writes: w.record_writes(&w.alice, &record_key(&two.a)),
            b_writes: w.record_writes(&w.alice, &record_key(&two.b)),
            a_entry: held(wallet, &journal(&two.a_attempt)).await,
        }
    }
}

/// `entry` at `a`'s key, then the user's `done` on `a`'s attempt: refused as corrupt, with
/// nothing written on either claim, either record or the journal.
async fn resolved_over(swap: Swap) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let two = two_payments(&w, &mut wallet, &mut shop, swap).await;
    let entry = swapped(&wallet, &two, swap).await;
    put(&mut wallet, &journal(&two.a_attempt), &entry).await;
    let before = Untouched::of(&w, &mut wallet, &two).await;
    let error = wallet
        .resolve_attempt(
            &two.a_obligation,
            two.a_attempt,
            Resolution::Done,
            UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert_eq!(Untouched::of(&w, &mut wallet, &two).await, before);
    assert_eq!(before.a_claim.1, Outcome::Open);
}

#[tokio::test]
async fn test_local_state_resolve_refuses_another_attempts_entry_writing_nothing() {
    // `b`'s entry under `a`'s key: `resolve_attempt(a)` would take `b`'s proof, deadline and
    // record for `a`'s.
    resolved_over(Swap::AnotherAttempt).await;
}

#[tokio::test]
async fn test_local_state_resolve_refuses_an_earlier_attempts_entry_writing_nothing() {
    // An earlier attempt's entry on the same claim under `a`'s key: of the claim the
    // resolution is for, but not of its attempt.
    resolved_over(Swap::EarlierAttempt).await;
}

#[tokio::test]
async fn test_local_state_resolve_refuses_the_attempts_entry_on_another_claim() {
    // `a`'s attempt ID on an entry naming `b`'s claim and request: structurally valid at
    // `a`'s key, but not of the claim the resolution is for.
    resolved_over(Swap::AnotherClaim).await;
}

#[tokio::test]
async fn test_local_state_receipt_parked_on_an_entry_of_another_claim() {
    // A receipt of `a`'s attempt, while the entry at `a`'s key names `b`'s claim: the
    // receipt settles nothing from it, and is parked as for a corrupt store.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let two = paid_and_pending(&w, &mut wallet, &mut shop).await;
    let entry = swapped(&wallet, &two, Swap::AnotherClaim).await;
    put(&mut wallet, &journal(&two.a_attempt), &entry).await;
    deliver_proved_receipt(&w, &mut wallet, &shop, &two.a, two.a_attempt).await;
    let _ = wallet.receive(None).await;
    assert_eq!(
        outcome(&mut wallet, &two.a_obligation, &two.a_attempt).await,
        Outcome::Open
    );
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_local_state_receipt_written_later_kept_on_an_entry_of_another_claim() {
    // The receipt's step is refused, so it is written later; meanwhile the entry at `a`'s key
    // comes to name `b`'s claim. The later write settles nothing from it and keeps its note.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let two = paid_and_pending(&w, &mut wallet, &mut shop).await;
    deliver_proved_receipt(&w, &mut wallet, &shop, &two.a, two.a_attempt).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(keys(&wallet, "se/").await.len(), 1);
    let entry = swapped(&wallet, &two, Swap::AnotherClaim).await;
    put(&mut wallet, &journal(&two.a_attempt), &entry).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert_eq!(keys(&wallet, "se/").await.len(), 1);
    assert_eq!(
        outcome(&mut wallet, &two.a_obligation, &two.a_attempt).await,
        Outcome::Open
    );
}

#[tokio::test]
async fn test_local_state_entry_keeping_another_request_is_corrupt() {
    // `a`'s entry keeps `b`'s request: its record would be rebuilt with another request
    // than the one its claim key and record name.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let two = paid_and_pending(&w, &mut wallet, &mut shop).await;
    let other = held_json(&wallet, &journal(&two.b_attempt)).await;
    let mut entry = held_json(&wallet, &journal(&two.a_attempt)).await;
    assert_ne!(entry["d"]["request"], other["d"]["request"]);
    entry["d"]["request"] = other["d"]["request"].clone();
    put(&mut wallet, &journal(&two.a_attempt), &entry).await;
    let error = wallet.payments().await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

fn to_bob(w: &World, value: &str) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc(value))
        .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
}

#[tokio::test]
async fn test_local_state_entry_naming_another_payments_record_is_corrupt() {
    // Two payments without a request; the first's entry names the second's record. Its
    // outcome and receipt would go into the other payment's record.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let mut attempts = Vec::new();
    for value in ["0.0015", "0.0025"] {
        let paid = wallet
            .pay_without_request(to_bob(&w, value))
            .await
            .outcome
            .unwrap();
        let PayOutcome::Paid { attempt_id, .. } = paid else {
            panic!("{paid:?}");
        };
        attempts.push(attempt_id);
    }
    let other = held_json(&wallet, &journal(&attempts[1])).await;
    let mut entry = held_json(&wallet, &journal(&attempts[0])).await;
    assert_ne!(entry["d"]["natural_key"], other["d"]["natural_key"]);
    entry["d"]["natural_key"] = other["d"]["natural_key"].clone();
    put(&mut wallet, &journal(&attempts[0]), &entry).await;
    let error = wallet.payments().await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

#[tokio::test]
async fn test_local_state_list_ask_of_another_payee_is_corrupt() {
    // The mark of a list request asked of bob names alice: no mark of this ask, and no
    // reason to ask again either. It is a corrupt store.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let refused = wallet
        .pay_without_request(to_bob(&w, "1"))
        .await
        .outcome
        .unwrap_err();
    assert!(refused.refusal().is_some(), "{refused:?}");
    let key = format!("lq/{}", b64(&w.bob_id()));
    let mut mark = held_json(&wallet, &key).await;
    mark["d"]["payee"] = Value::String(b64(&w.alice_id()));
    put(&mut wallet, &key, &mark).await;
    let error = wallet
        .pay_without_request(to_bob(&w, "1"))
        .await
        .outcome
        .unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

/// A receive that fails as corrupt and leaves `key` where it was.
async fn refused_keeping(wallet: &mut Paykit, key: &str) {
    let before = held(wallet, key).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert_eq!(held(wallet, key).await, before);
}

#[tokio::test]
async fn test_local_state_queued_ask_of_another_sender_sends_nothing() {
    // A re-send to ask of the sender its key names, naming bob instead.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let key = format!("ask/{}/{:020}", b64(&[7u8; 32]), 5);
    let ask = json!({
        "sender": b64(&w.bob_id()),
        "from_ms": 5,
        "until_ms": null,
        "queued": null,
    });
    put(&mut wallet, &key, &kept(ask)).await;
    refused_keeping(&mut wallet, &key).await;
}

#[tokio::test]
async fn test_local_state_missed_item_at_another_time_asks_nothing() {
    // A `missed` item's re-sends are asked from its value: one other than its key's time.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let key = format!("missed/{:020}", 5);
    put(&mut wallet, &key, &kept(json!(7))).await;
    refused_keeping(&mut wallet, &key).await;
}

#[tokio::test]
async fn test_local_state_queued_answer_under_another_key_answers_nothing() {
    // A `resend_request` kept to answer, at a `seq` other than its key's.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let shop = w.bob.identity().app_keys("shop");
    let cert = w.bob.switch().app(&shop.app_id_bytes()).cert.unwrap();
    let key = format!("ans/{:020}/{OTHER_MSG_ID}", 1);
    let answer = json!({
        "seq": 2,
        "msg_id": OTHER_MSG_ID,
        "requester": b64(&w.bob_id()),
        "cert": serde_json::to_value(cert.signed()).unwrap(),
        "body": { "msg_ids": [OTHER_MSG_ID] },
    });
    put(&mut wallet, &key, &kept(answer)).await;
    refused_keeping(&mut wallet, &key).await;
}

#[tokio::test]
async fn test_local_state_kept_side_of_another_message_is_corrupt() {
    // Bob cancels his request; the side kept for the cancellation comes to name another
    // message. Asked to re-send it, the shop neither re-sends it nor takes the side as none.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    let cancellation = shop.cancel_sent_request(&record, None).await.unwrap();
    let key = format!("side/{cancellation}");
    let mut side = held_json(&shop, &key).await;
    side["d"]["msg_id"] = Value::String(OTHER_MSG_ID.into());
    put(&mut shop, &key, &side).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.resend_request",
        Vec::new(),
        json!({ "msg_ids": [cancellation] })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let error = shop.receive(None).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

/// Bob's request `r2`, which alice holds, with a cancellation of another request of bob's
/// kept at its record's key. Returns the request and the key.
async fn cancellation_under_another_requests_key(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
) -> (RequestRef, String) {
    let request = request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let key =
        String::from_utf8(crate::one_time::stop_key(w, wallet, request.request_hash)).unwrap();
    let cancellation = crate::one_time::receive_stop(w, wallet, [9; 32]).await;
    put(
        wallet,
        &key,
        &serde_json::from_slice(&cancellation).unwrap(),
    )
    .await;
    (request, key)
}

#[tokio::test]
async fn test_local_state_cancellation_under_another_requests_key_not_shown() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, _) = cancellation_under_another_requests_key(&w, &mut wallet, &mut shop).await;
    let error = wallet.request(&request).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

#[tokio::test]
async fn test_local_state_cancellation_under_another_requests_key_stops_as_corrupt() {
    // The kept-copy stop of the request reads the copy at its key: not a stop of this
    // request, and not an absent one either.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, _) = cancellation_under_another_requests_key(&w, &mut wallet, &mut shop).await;
    let error = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_local_state_cancellation_under_another_requests_key_fails_a_receive() {
    // The receive places every kept cancellation: one under another request's key fails it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (_, key) = cancellation_under_another_requests_key(&w, &mut wallet, &mut shop).await;
    refused_keeping(&mut wallet, &key).await;
}

#[tokio::test]
async fn test_local_state_cancellation_under_another_requests_key_parks_the_real_one() {
    // Bob cancels the request for real: the receive step finds the copy at its key, which
    // isn't this cancellation, so the cancellation is parked rather than taken as kept.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    cancellation_under_another_requests_key(&w, &mut wallet, &mut shop).await;
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
}

/// The client's target of bob's app `app_id`'s `payment_lists` record.
fn lists_of(w: &World, app_id: &[u8; 16]) -> RecordTarget {
    let collection = Collection::PaymentLists;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::payment_list(&w.bob_id(), app_id),
    )
}

/// A list of bob's app `app`, kept as a held list is.
fn list_of(w: &World, app: &str) -> Value {
    kept(kept_message(
        w,
        app,
        app,
        &w.alice,
        "paykit.private_payment_list",
        Value::Object(
            paykit_lib::bodies::PrivatePaymentList {
                version: list_version(1, 1),
                payment_endpoints: std::collections::BTreeMap::new(),
                expires_at: w.clock.now_ms() + 86_400_000,
            }
            .to_body()
            .unwrap(),
        ),
    ))
}

/// The shop's list, held at the key of bob's app `app_id`'s record.
async fn list_under_another_apps_key(w: &World, wallet: &mut Paykit, app_id: &[u8; 16]) {
    let record = wallet.app().record_key(&lists_of(w, app_id)).unwrap();
    put(wallet, &format!("lh/{}", b64(&record)), &list_of(w, "shop")).await;
}

#[tokio::test]
async fn test_local_state_held_list_under_another_apps_key_not_listed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    list_under_another_apps_key(&w, &mut wallet, &[9; 16]).await;
    let error = wallet.held_lists().await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

#[tokio::test]
async fn test_local_state_held_list_under_another_apps_key_never_placed_there() {
    // The list held at app 9's key is the shop's: a receive that places held lists never
    // writes it into app 9's record.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    list_under_another_apps_key(&w, &mut wallet, &[9; 16]).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    let record = wallet.app().record_key(&lists_of(&w, &[9; 16])).unwrap();
    assert_eq!(w.record_writes(&w.alice, &record), 0);
}

#[tokio::test]
async fn test_local_state_held_list_of_another_app_parks_a_list_to_hold() {
    // The shop's record doesn't read, so its next list is held; the list held at its key is
    // another app's. The receive step neither compares the new list with it nor replaces it:
    // the list is parked.
    let w = World::new();
    let marks = std::sync::Arc::new(crate::outcome_record::NoSeenMarks::default());
    let mut wallet = w.wallet_on("wallet", marks.clone()).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let _other = w.labelled_shop("shop2", "tips").await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let target = lists_of(&w, &shop.app_id());
    // The wallet's seen mark is set aside, as by a store that lost it, so it takes the write.
    marks.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    phone.app_mut().read_record(&target).await.unwrap();
    let op = phone
        .app_mut()
        .record_op(&target, Some(b"{\"not\":\"a list record\"}"), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let record = wallet.app().record_key(&target).unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&record)),
        &list_of(&w, "shop2"),
    )
    .await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_local_state_held_list_of_another_app_refuses_a_stored_lists_write() {
    // The shop's list is stored for good, its step refused; the list held at the shop's key
    // is another app's. The stored list's later write neither compares with it nor
    // replaces it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _other = w.labelled_shop("shop2", "tips").await;
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    let key = format!("lh/{}", b64(&record));
    put(&mut wallet, &key, &list_of(&w, "shop2")).await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    refuse_writes(&w.alice, 1);
    refused_keeping(&mut wallet, &key).await;
}

#[tokio::test]
async fn test_local_state_seen_records_of_another_app_are_corrupt() {
    // The shop's records seen, kept at another of bob's apps' key: that app's view is never
    // built from the shop's records.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    let other = w.labelled_shop("shop2", "tips").await;
    crate::public::publish(
        &w,
        json!({ "public_endpoints": { "btc-lightning-bolt12": "lno1tips" } }),
        Some(w.clock.now_ms() + 86_400_000),
    );
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    let seen = |app: [u8; 16]| format!("ar/{}/{}", b64(&w.bob_id()), b64(&app));
    let records = held_json(&wallet, &seen(shop.app_id())).await;
    put(&mut wallet, &seen(other.app_id()), &records).await;
    let error = wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

/// Two requests of bob's stored for good, their steps and first later writes refused: each
/// a parked entry with its note. Returns the notes' keys.
async fn two_stored(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> Vec<String> {
    // The second receive's later writes take the first request's too: one more refused.
    for (value, refused) in [("1", 2), ("2", 3)] {
        request_from(shop, w.alice_id(), draft(value, w.clock.now_ms())).await;
        refuse_writes(&w.alice, refused);
        wallet.receive(None).await.unwrap();
    }
    let notes = keys(wallet, "se/").await;
    assert_eq!(notes.len(), 2);
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 2);
    notes
}

/// A's note made to carry B's entry ID, and B's hash with it when `with_hash`; B's own note
/// gone, so only A's follows B's ID. Once A's message is written, its entry is forgotten by
/// the note: neither entry goes, and A's note stays.
async fn note_following_another_entry(with_hash: bool) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let notes = two_stored(&w, &mut wallet, &mut shop).await;
    let mut a = held_json(&wallet, &notes[0]).await;
    let b = held_json(&wallet, &notes[1]).await;
    assert_ne!(a["d"]["stored_id"], b["d"]["stored_id"]);
    a["d"]["stored_id"] = b["d"]["stored_id"].clone();
    if with_hash {
        a["d"]["hash"] = b["d"]["hash"].clone();
    }
    put(&mut wallet, &notes[0], &a).await;
    delete(&mut wallet, &notes[1]).await;
    let error = wallet.receive(None).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 2);
    assert_eq!(keys(&wallet, "se/").await, vec![notes[0].clone()]);
}

#[tokio::test]
async fn test_local_state_note_carrying_another_entrys_id_forgets_neither() {
    // The entry at B's ID holds B's message, whose hash isn't the one A's note keeps.
    note_following_another_entry(false).await;
}

#[tokio::test]
async fn test_local_state_note_carrying_another_entrys_id_and_hash_forgets_neither() {
    // The hash a note keeps is its own message's: B's hash on A's note doesn't read.
    note_following_another_entry(true).await;
}

#[tokio::test]
async fn test_local_state_note_without_its_entrys_hash_fails_closed() {
    // A note keeps its entry's hash with its ID. One without it is no note a run writes: it
    // fails closed and stays, its entry with it, and past its window nothing is raised.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    let key = keys(&wallet, "se/").await.remove(0);
    let mut note = held_json(&wallet, &key).await;
    note["d"].as_object_mut().unwrap().remove("hash").unwrap();
    put(&mut wallet, &key, &note).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    refused_keeping(&mut wallet, &key).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    assert!(keys(&wallet, "nt/").await.is_empty());
}

#[tokio::test]
async fn test_local_state_notice_handle_of_another_entry_forgets_nothing() {
    // A given-up message's notice carries the handle of another stored entry, B's: its
    // acknowledgement follows the handle only to the entry of the message it names.
    let w = World::new();
    let (mut wallet, notice) = crate::stored::given_up(&w).await;
    let mut other = w.labelled_shop("shop2", "tips").await;
    request_from(&mut other, w.alice_id(), draft("2", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    let key = format!("nt/{}", notice.id);
    let mut kept_notice = held_json(&wallet, &key).await;
    let entries = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(entries.len(), 2);
    let own = kept_notice["d"]["stored"]["stored_id"].as_u64().unwrap();
    let other = entries.iter().find(|e| e.stored_id != own).unwrap();
    kept_notice["d"]["stored"] = json!({ "stored_id": other.stored_id, "hash": b64(&other.hash) });
    put(&mut wallet, &key, &kept_notice).await;
    let error = wallet.acknowledge_notice(&notice.id).await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 2);
    assert!(held(&wallet, &key).await.is_some());
}

/// A list of bob's shop at `version`, kept as a held list is.
fn shop_list_at(w: &World, version: paykit_lib::ListVersion) -> Value {
    kept(kept_message(
        w,
        "shop",
        "shop",
        &w.alice,
        "paykit.private_payment_list",
        Value::Object(
            paykit_lib::bodies::PrivatePaymentList {
                version,
                payment_endpoints: std::collections::BTreeMap::new(),
                expires_at: w.clock.now_ms() + 86_400_000,
            }
            .to_body()
            .unwrap(),
        ),
    ))
}

/// The `msg_id` of the list the `payment_lists` record at `record` holds.
async fn recorded_list(wallet: &Paykit, record: &[u8; 32]) -> String {
    let held = wallet.app().shared_record(record).await.unwrap().unwrap();
    let value: Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    let message = &value["list"]["plaintext"]["json"];
    let message: Value = match message.as_str() {
        Some(text) => {
            serde_json::from_slice(&pubky_switch_core::enc::unb64(text).unwrap()).unwrap()
        }
        None => message.clone(),
    };
    message["msg_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn test_local_state_late_held_list_placement_keeps_a_newer_held_list() {
    // The placement of a held list v3 is kept to send again; meanwhile a newer list v4 is
    // held over the record in its place, as the receive step holds one. The kept placement
    // carries no delete of the hold, so when it lands v4 stays held, and is placed after it,
    // never lost.
    use pubky_switch_client::testing::Fault;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let target = lists_of(&w, &shop.app_id());
    let record = wallet.app().record_key(&target).unwrap();
    let readable = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 3))
        .await;
    wallet.receive(None).await.unwrap();
    let v3 = wallet.held_lists().await.unwrap().remove(0);
    assert_eq!(v3.version, list_version(1, 3));
    wallet
        .app_mut()
        .put_record(&target, &readable, Vec::new())
        .await
        .unwrap();
    w.alice.switch().fail_next(Fault::Unanswered {
        path: "/ops".into(),
        after: 0,
    });
    let _ = wallet.receive(None).await;
    let v4 = shop_list_at(&w, list_version(1, 4));
    let v4_id = crate::restore::kept_msg_id(&v4);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: format!("lh/{}", b64(&record)).into_bytes(),
                value: serde_json::to_vec(&v4).unwrap(),
            }],
        )
        .await
        .unwrap();
    // The kept placement is sent again and applied; the next placement takes v4.
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(recorded_list(&wallet, &record).await, v4_id);
    assert!(wallet.held_lists().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_local_state_journal_expiry_without_an_allowance_is_corrupt() {
    // An Allowance's expiry is journaled only with the Allowance it reserved under; an entry
    // carrying one without it is a corrupt store, never read as a payment.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let (key, raw) = wallet
        .app()
        .protocol_scan("paykit", b"j/")
        .await
        .unwrap()
        .remove(0);
    let mut entry: Value = serde_json::from_slice(&raw).unwrap();
    assert!(entry["d"]["allowance"].is_null());
    entry["d"]["allowance_expires_at_ms"] = json!(1_234);
    put(&mut wallet, std::str::from_utf8(&key).unwrap(), &entry).await;
    let error = wallet.resume_payments().await.unwrap_err();
    assert!(corrupt(&error), "{error:?}");
}

#[tokio::test]
async fn test_local_state_held_withdrawal_survives_an_older_in_place_restore() {
    // The wallet holds bob's list and is backed up. The list record then stops reading, and
    // bob withdraws the list: the withdrawal is held over the record. Restored in place from
    // the older backup, the installation's held withdrawal stays: once the record reads
    // again with the older list, the withdrawal is placed over it, and nothing is paid to
    // the withdrawn list (Paykit wire 5.4).
    let w = World::new();
    let storage = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&storage)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let target = lists_of(&w, &shop.app_id());
    let record = wallet.app().record_key(&target).unwrap();
    let readable = wallet
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let backup = wallet.backup().await.unwrap();
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    w.list_to_alice_with(
        &mut shop,
        &[wallet.app_id()],
        list_version(1, 2),
        std::collections::BTreeMap::new(),
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    wallet.app_mut().flush_cursor().await.unwrap();
    drop(wallet);
    let mut wallet = paykit_sdk::Paykit::restore(
        w.env(&w.alice, storage),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    wallet.receive(None).await.unwrap();
    wallet.reconcile().await.unwrap();
    wallet
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    // The record reads again, with the older list.
    wallet
        .app_mut()
        .put_record(&target, &readable, Vec::new())
        .await
        .unwrap();
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let paid = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome;
    assert!(
        !matches!(paid, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "paid a withdrawn list: {paid:?}"
    );
    // A list bob sends later is paid to as usual.
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 3))
        .await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let paid = wallet
        .pay_without_request(paykit_sdk::UnrequestedPayment::new(
            w.bob_id(),
            crate::common::btc("1"),
        ))
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_local_state_list_held_over_its_record_not_paid_to() {
    // Bob's list is in the record, and a newer list of his, a withdrawal, is held over it, as
    // when the record didn't read when it came and its placement hasn't landed yet. While a
    // newer list is held over the record, the payee app's list isn't paid to: nothing goes to
    // the older list the withdrawal replaces (Paykit wire 5.4). A hold the record already
    // holds, placed but not yet cleared, stops nothing.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    let held = format!("lh/{}", b64(&record));
    put(&mut wallet, &held, &shop_list_at(&w, list_version(1, 2))).await;
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::WaitingForList { .. })
        ),
        "paid the list a held withdrawal replaces: {refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());

    put(&mut wallet, &held, &shop_list_at(&w, list_version(1, 1))).await;
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_local_state_list_held_over_a_divergent_record_refused_as_divergent() {
    // A newer list of bob's is held over his payment_lists record, which is divergent: the
    // held list waits for the user to confirm the record, so the payment is refused naming
    // it, for the user to confirm, never as waiting for a list (Paykit spec 6).
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&record)),
        &shop_list_at(&w, list_version(1, 2)),
    )
    .await;
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::Divergent { records })
                if records.iter().any(|r| r.record == record)
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

/// Alice's wallet on `base` holding the shop's list at `installed`, with a later list of the
/// shop's at `held` held over its record, as when the record didn't read when it came.
async fn shop_list_held_over(
    w: &World,
    base: Arc<dyn StateStore>,
    installed: paykit_lib::ListVersion,
    held: paykit_lib::ListVersion,
) -> (Paykit, Paykit) {
    let mut wallet = w.wallet_on("wallet", base).await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], installed)
        .await;
    wallet.receive(None).await.unwrap();
    let record = wallet
        .app()
        .record_key(&lists_of(w, &shop.app_id()))
        .unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&record)),
        &shop_list_at(w, held),
    )
    .await;
    (wallet, shop)
}

#[tokio::test]
async fn test_local_state_list_request_names_the_held_list_not_the_installed_one() {
    // The shop's list 1/1 is installed and its 1/2 held over the record: a payment asks the
    // shop for its current list naming 1/2, the pair the wallet holds, whether the payment
    // names the shop's app or not (Paykit wire 4.14).
    for targeted in [false, true] {
        let w = World::new();
        let (mut wallet, shop) = shop_list_held_over(
            &w,
            crate::common::store(),
            list_version(1, 1),
            list_version(1, 2),
        )
        .await;
        let options = if targeted {
            PayOptions::default().with_list_app(shop.app_id())
        } else {
            PayOptions::default()
        };
        let refused = wallet
            .pay_without_request(
                UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(options),
            )
            .await
            .outcome
            .unwrap_err();
        assert!(
            matches!(
                refused.refusal(),
                Some(paykit_sdk::Refusal::WaitingForList { .. })
            ),
            "{refused:?}"
        );
        assert_eq!(
            crate::destinations::list_requests_sent(&w, &wallet).await,
            [(Some(shop.app_id()), Some(list_version(1, 2)))],
            "targeted: {targeted}"
        );
    }
}

#[tokio::test]
async fn test_local_state_list_request_names_the_held_list_of_one_ahead_of_the_identity() {
    // The shop's installed list is of a generation the payee's identity.json doesn't reach
    // yet, and a later one is held over it: the request names the held pair (Paykit wire
    // 4.13, 4.14).
    let w = World::new();
    let (mut wallet, shop) = shop_list_held_over(
        &w,
        crate::common::store(),
        list_version(9, 1),
        list_version(9, 2),
    )
    .await;
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(shop.app_id()), Some(list_version(9, 2)))]
    );
}

#[tokio::test]
async fn test_local_state_list_request_asks_the_held_app_beside_a_paused_one() {
    // The shop's list 1/1 is installed with its 1/2 held over it, and the seller wallet's
    // list is paused as divergent: the payment waits for the user's confirmation of the
    // paused record, and the shop is still asked, naming 1/2 (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    let mut seller = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    seller.app_mut().invite(&w.alice_id()).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    crate::destinations::list_from(
        &w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 1),
        "btc-lightning-bolt11",
        "lnbc1wallet",
    );
    wallet.receive(None).await.unwrap();
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&record)),
        &shop_list_at(&w, list_version(1, 2)),
    )
    .await;
    let paused = wallet
        .app()
        .record_key(&lists_of(&w, &seller.app_id()))
        .unwrap();
    crate::divergent::set_divergent(&base, &wallet, &paused, true);
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::Divergent { .. })
        ),
        "{refused:?}"
    );
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(shop.app_id()), Some(list_version(1, 2)))]
    );
}

#[tokio::test]
async fn test_local_state_list_request_asks_the_other_publishers_beside_a_held_app() {
    // The shop's only installed list 1/1 is replaced by a withdrawal 1/2 held over its
    // record, and the seller wallet, a current app of bob's, has sent no list: the shop is
    // asked naming 1/2, and the seller wallet is asked too, so a usable list can come
    // (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let base = crate::common::store();
    let mut seller = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let (mut wallet, shop) = shop_list_held_over(
        &w,
        Arc::clone(&base),
        list_version(1, 1),
        list_version(1, 2),
    )
    .await;
    seller.app_mut().invite(&w.alice_id()).await.unwrap();
    let refused = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::WaitingForList { .. })
        ),
        "{refused:?}"
    );
    let asks = crate::destinations::list_requests_sent(&w, &wallet).await;
    assert!(
        asks.contains(&(Some(shop.app_id()), Some(list_version(1, 2)))),
        "{asks:?}"
    );
    // A request the seller wallet may answer: one naming no app, or naming it.
    assert!(
        asks.iter()
            .any(|(app, _)| app.is_none() || *app == Some(seller.app_id())),
        "{asks:?}"
    );
    assert!(crate::destinations::asked_for_a_list(&mut seller).await);
}

#[tokio::test]
async fn test_local_state_list_request_names_a_list_held_over_an_unreadable_record() {
    // The shop's record doesn't read, and its list 1/2 is held over it: a payment asks the
    // shop naming 1/2, whether it names the shop's app or not, and pays nothing to it
    // (Paykit wire 4.14, 5.4).
    for targeted in [false, true] {
        let w = World::new();
        let mut wallet = w.wallet("wallet").await;
        let mut shop = w.shop().await;
        w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
            .await;
        wallet.receive(None).await.unwrap();
        let target = lists_of(&w, &shop.app_id());
        let record = wallet.app().record_key(&target).unwrap();
        wallet
            .app_mut()
            .put_record(&target, b"{}", Vec::new())
            .await
            .unwrap();
        put(
            &mut wallet,
            &format!("lh/{}", b64(&record)),
            &shop_list_at(&w, list_version(1, 2)),
        )
        .await;
        let options = if targeted {
            PayOptions::default().with_list_app(shop.app_id())
        } else {
            PayOptions::default()
        };
        let paid = wallet
            .pay_without_request(
                UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")).with_options(options),
            )
            .await
            .outcome;
        assert!(!matches!(paid, Ok(PayOutcome::Paid { .. })), "{paid:?}");
        assert_eq!(
            crate::destinations::list_requests_sent(&w, &wallet).await,
            [(Some(shop.app_id()), Some(list_version(1, 2)))],
            "targeted: {targeted}"
        );
    }
}

/// Alice's wallet on `base` holding the shop's list 1/2 over the shop's record, which
/// doesn't read, with bob's seller wallet a current app that has sent no list.
async fn only_a_held_list(w: &World, base: Arc<dyn StateStore>) -> (Paykit, Paykit, Paykit) {
    only_a_held_list_at(w, base, |_| list_version(1, 2)).await
}

/// [`only_a_held_list`], the list held at the version `version` gives for the generation of
/// bob's identity.json once his apps have joined.
async fn only_a_held_list_at(
    w: &World,
    base: Arc<dyn StateStore>,
    version: fn(u64) -> paykit_lib::ListVersion,
) -> (Paykit, Paykit, Paykit) {
    let mut seller = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet_on("wallet", base).await;
    let mut shop = w.shop().await;
    seller.app_mut().invite(&w.alice_id()).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let target = lists_of(w, &shop.app_id());
    let record = wallet.app().record_key(&target).unwrap();
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&record)),
        &shop_list_at(
            w,
            version(w.homeserver.identity_doc(&w.bob_id()).unwrap().generation()),
        ),
    )
    .await;
    (wallet, shop, seller)
}

#[tokio::test]
async fn test_local_state_list_request_with_only_a_held_list_asks_the_other_publishers() {
    // The shop's only list is held over a record that doesn't read, and the seller wallet,
    // current, has sent none: the shop is asked naming 1/2 and the seller wallet is asked by
    // name with no pair (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let (mut wallet, shop, seller) = only_a_held_list(&w, crate::common::store()).await;
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    let asks = crate::destinations::list_requests_sent(&w, &wallet).await;
    assert!(
        asks.contains(&(Some(shop.app_id()), Some(list_version(1, 2)))),
        "{asks:?}"
    );
    assert!(asks.contains(&(Some(seller.app_id()), None)), "{asks:?}");
}

#[tokio::test]
async fn test_local_state_list_request_never_asks_an_app_whose_held_record_is_paused() {
    // The shop's list is held over a record that doesn't read and is divergent: the shop
    // isn't asked (Paykit spec 8.4).
    let w = World::new();
    let base = crate::common::store();
    let (mut wallet, shop, _seller) = only_a_held_list(&w, Arc::clone(&base)).await;
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    let asks = crate::destinations::list_requests_sent(&w, &wallet).await;
    assert!(
        !asks.iter().any(|(app, _)| *app == Some(shop.app_id())),
        "{asks:?}"
    );
}

#[tokio::test]
async fn test_local_state_damaged_held_list_of_another_payee_stops_no_payment() {
    // A held list kept under a key that names no record of bob's, or that doesn't read,
    // stops no payment to bob's list (Paykit wire 5.4).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    put(
        &mut wallet,
        &format!("lh/{}", b64(&[7; 32])),
        &shop_list_at(&w, list_version(1, 2)),
    )
    .await;
    let paid = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

/// [`only_a_held_list`], the shop's record paused as divergent.
async fn paused_held_list(w: &World) -> (Paykit, Paykit, Paykit) {
    paused_held_list_at(w, |_| list_version(1, 2)).await
}

/// [`paused_held_list`], the list held at the version [`only_a_held_list_at`] takes.
async fn paused_held_list_at(
    w: &World,
    version: fn(u64) -> paykit_lib::ListVersion,
) -> (Paykit, Paykit, Paykit) {
    let base = crate::common::store();
    let (wallet, shop, seller) = only_a_held_list_at(w, Arc::clone(&base), version).await;
    let record = wallet
        .app()
        .record_key(&lists_of(w, &shop.app_id()))
        .unwrap();
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    (wallet, shop, seller)
}

#[tokio::test]
async fn test_local_state_paused_held_publisher_alone_gets_no_request_naming_no_app() {
    // The shop's only list is held over its paused record: no request it can answer goes,
    // neither one naming it nor one naming no app, and the seller wallet, current with no
    // list, is asked by name (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let (mut wallet, _shop, seller) = paused_held_list(&w).await;
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(seller.app_id()), None)]
    );
}

#[tokio::test]
async fn test_local_state_paused_held_publisher_not_asked_beside_another_held_one() {
    // The shop's list is held over its paused record and the seller wallet's own list has
    // expired: the seller wallet is asked naming its pair, and the shop isn't asked at all
    // (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let (mut wallet, _shop, seller) = paused_held_list(&w).await;
    crate::destinations::list_from(
        &w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 1),
        "btc-lightning-bolt11",
        "lnbc1wallet",
    );
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(31 * 24 * crate::common::HOUR_MS);
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(seller.app_id()), Some(list_version(1, 1)))]
    );
}

#[tokio::test]
async fn test_local_state_payment_naming_a_paused_held_publisher_asks_nothing() {
    // A payment naming the shop, whose only list is held over its paused record, asks it
    // nothing (Paykit spec 8.4).
    let w = World::new();
    let (mut wallet, shop, _seller) = paused_held_list(&w).await;
    let refused = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), crate::common::btc("1"))
                .with_options(PayOptions::default().with_list_app(shop.app_id())),
        )
        .await
        .outcome
        .unwrap_err();
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        []
    );
    // The payment waits for the user's confirmation of the paused record, which it names,
    // not for a list nothing asks for (Paykit spec 8.4).
    let record = wallet
        .app()
        .record_key(&lists_of(&w, &shop.app_id()))
        .unwrap();
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::Divergent { records })
            if records.iter().map(|r| r.record).collect::<Vec<_>>() == [record]),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_local_state_paused_held_publisher_leaves_a_seller_with_an_uncounted_list_asked() {
    // The shop's list is held over its paused record, and the seller wallet's list was
    // signed under a certificate bob's identity.json no longer lists: the seller wallet is
    // asked by name, naming the pair held, and the shop isn't asked (Paykit spec 8.4, wire
    // 4.14).
    let w = World::new();
    let (mut wallet, _shop, seller) = paused_held_list(&w).await;
    crate::destinations::list_from(
        &w,
        "seller-wallet",
        wallet.app_id(),
        list_version(1, 1),
        "btc-lightning-bolt11",
        "lnbc1wallet",
    );
    wallet.receive(None).await.unwrap();
    let renewed = crate::certificate::renew_for(
        &w,
        &w.bob,
        "seller-wallet",
        seller.app_id(),
        &paykit_sdk::AppScopes::new().request().build().unwrap(),
        1_000,
    );
    w.bob
        .next_doc(|body| {
            for listed in &mut body.apps {
                if listed.app_id == seller.app_id() {
                    listed.cert = renewed.hash();
                }
            }
        })
        .unwrap();
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(seller.app_id()), Some(list_version(1, 1)))]
    );
}

#[tokio::test]
async fn test_local_state_paused_held_list_of_a_removed_app_stops_no_request() {
    // The shop's list is held over its paused record, but bob's identity.json has removed
    // the shop: its list plays no part, paused or not, and the request names no app, as
    // with no list held (Paykit spec 8.4, wire 4.14).
    let w = World::new();
    let (mut wallet, shop, _seller) = paused_held_list(&w).await;
    let now = w.clock.now_ms();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != shop.app_id());
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: shop.app_id(),
                distrust_from: now,
            });
        })
        .unwrap();
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(None, None)]
    );
}

#[tokio::test]
async fn test_local_state_paused_held_list_above_the_payees_generation_pauses_nothing() {
    // The shop's list, held over its paused record, is of a generation bob's identity.json
    // hasn't reached: it plays no part, paused or not, and the shop is asked naming the
    // pair held, as a readable list ahead would be (Paykit spec 8.4; wire 4.13, 4.14).
    let w = World::new();
    let (mut wallet, shop, seller) = paused_held_list_at(&w, |at| list_version(at + 1, 1)).await;
    let generation = w.homeserver.identity_doc(&w.bob_id()).unwrap().generation();
    let ahead = list_version(generation + 1, 1);
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    let mut asks = crate::destinations::list_requests_sent(&w, &wallet).await;
    asks.sort();
    let mut expected = vec![
        (Some(shop.app_id()), Some(ahead)),
        (Some(seller.app_id()), None),
    ];
    expected.sort();
    assert_eq!(asks, expected);
}

#[tokio::test]
async fn test_local_state_paused_held_list_above_the_generation_asks_the_newest_pair_kept() {
    // The shop's list held over its paused record is of a generation bob's identity.json
    // hasn't reached, and a newer one came after the pause and is kept for the user's
    // confirmation: the shop is asked naming the newer pair (Paykit spec 8.4; wire 4.13,
    // 4.14).
    let w = World::new();
    let (mut wallet, mut shop, _seller) =
        paused_held_list_at(&w, |at| list_version(at + 1, 1)).await;
    let generation = w.homeserver.identity_doc(&w.bob_id()).unwrap().generation();
    let newer = list_version(generation + 1, 2);
    w.list_to_alice(&mut shop, &[wallet.app_id()], newer).await;
    wallet.receive(None).await.unwrap();
    let _ = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), crate::common::btc("1"))
                .with_options(PayOptions::default().with_list_app(shop.app_id())),
        )
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(shop.app_id()), Some(newer))]
    );
}

#[tokio::test]
async fn test_local_state_payment_naming_a_paused_held_publisher_above_the_generation_names_its_pair(
) {
    // A payment naming the shop, whose list held over its paused record is of a generation
    // bob's identity.json hasn't reached, asks the shop naming the pair held (Paykit spec
    // 8.4; wire 4.13, 4.14).
    let w = World::new();
    let (mut wallet, shop, _seller) = paused_held_list_at(&w, |at| list_version(at + 1, 1)).await;
    let generation = w.homeserver.identity_doc(&w.bob_id()).unwrap().generation();
    let _ = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), crate::common::btc("1"))
                .with_options(PayOptions::default().with_list_app(shop.app_id())),
        )
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(shop.app_id()), Some(list_version(generation + 1, 1)))]
    );
}

#[tokio::test]
async fn test_local_state_list_kept_over_a_paused_record_stops_every_request_it_could_answer() {
    // The shop's record doesn't read and is paused before its next list comes: the list is
    // kept for the user's confirmation, and the shop is asked nothing, neither by name nor
    // by a request naming no app (Paykit spec 6, 8.4; wire 4.14).
    let w = World::new();
    let base = crate::common::store();
    let mut seller = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let mut shop = w.shop().await;
    seller.app_mut().invite(&w.alice_id()).await.unwrap();
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let target = lists_of(&w, &shop.app_id());
    let record = wallet.app().record_key(&target).unwrap();
    wallet
        .app_mut()
        .put_record(&target, b"{}", Vec::new())
        .await
        .unwrap();
    crate::divergent::set_divergent(&base, &wallet, &record, true);
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 2))
        .await;
    wallet.receive(None).await.unwrap();
    let _ = wallet
        .pay_without_request(UnrequestedPayment::new(w.bob_id(), crate::common::btc("1")))
        .await;
    assert_eq!(
        crate::destinations::list_requests_sent(&w, &wallet).await,
        [(Some(seller.app_id()), None)]
    );
}
