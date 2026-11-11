//! The outcome's activity record (Paykit spec 7.2 step 3): `done` and the payer's activity
//! record's update go in one batch, and `done` never goes without it. A record deleted
//! between the attempt and the outcome is rebuilt from the journal; one whose value doesn't
//! read holds the outcome until it reads again or the user has it written again.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, RequestRef};
use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{KeyValue, MemoryStore, StateStore, WriteBatch};
use pubky_switch_core::claims::Outcome;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::common::{draft, request_from, World};

/// A value of the record that doesn't read as an activity record.
pub(crate) const INVALID: &[u8] = b"{\"not\":\"an activity record\"}";

/// Alice's payer record of bob's request `request`.
pub(crate) fn payer_record(request: &RequestRef) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&request.payee, &request.request_hash),
    )
}

/// A store that, once armed, shows no mark of what the app has seen in any record (the
/// client's `hw/` keys): an app that never took a value of the record that reads, so a
/// later value that doesn't read isn't refused as a regression of one it saw (Switch spec
/// 8.2, Paykit wire 5.3 "Seen roles"), and is held as the record's content.
#[derive(Default)]
pub(crate) struct NoSeenMarks {
    inner: MemoryStore,
    pub(crate) armed: AtomicBool,
    /// Every commit that queues a message (the client's `out/` items) fails, applying
    /// nothing.
    refuse_queue: AtomicBool,
}

impl StateStore for NoSeenMarks {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        if self.armed.load(Ordering::SeqCst) && key.starts_with(b"hw/") {
            return Ok(None);
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

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        let queues = batch.writes.iter().any(|write| {
            matches!(write, pubky_switch_client::store::Write::Put { key, .. } if key.starts_with(b"out/"))
        });
        if queues && self.refuse_queue.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }
}

/// Bob's request, paid by alice's wallet on `store` with the payment left in flight; her
/// phone, another own app holding `pay`, holds the request record too. Returns both apps,
/// the request and the attempt.
async fn in_flight_on(
    w: &World,
    store: Arc<dyn StateStore>,
) -> (Paykit, Paykit, RequestRef, AttemptId) {
    let mut wallet = w.wallet_on("wallet", store).await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    (wallet, phone, request, attempt_id)
}

async fn in_flight(w: &World) -> (Paykit, Paykit, RequestRef, AttemptId) {
    in_flight_on(w, crate::common::store()).await
}

/// The phone writes `value` at alice's payer record of `request`: `None` deletes it.
pub(crate) async fn phone_writes(phone: &mut Paykit, request: &RequestRef, value: Option<&[u8]>) {
    let op = phone
        .app_mut()
        .record_op(&payer_record(request), value, Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

/// The attempt's outcome on its claim, read from the switch.
async fn claim_outcome(wallet: &mut Paykit, attempt_id: &AttemptId) -> Outcome {
    let obligation = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.attempt_id == *attempt_id)
        .unwrap()
        .obligation;
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    read.view.attempt(&attempt_id.to_bytes()).unwrap().outcome
}

/// The payer record the switch holds, as JSON, read again from the switch.
pub(crate) async fn record_value(
    wallet: &mut Paykit,
    request: &RequestRef,
) -> Option<serde_json::Value> {
    let held = wallet
        .app_mut()
        .read_record(&payer_record(request))
        .await
        .unwrap()?;
    held.content
        .value()
        .and_then(|v| serde_json::from_slice(v).ok())
}

#[tokio::test]
async fn test_outcome_record_deleted_is_rebuilt_with_done() {
    let w = World::new();
    let (mut wallet, mut phone, request, attempt_id) = in_flight(&w).await;
    phone_writes(&mut phone, &request, None).await;

    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    // `done` went with the record, rebuilt from the journal: the request, the acceptance and
    // the proof.
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Done);
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["request"].is_null(), "{value}");
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
}

#[tokio::test]
async fn test_outcome_record_invalid_value_over_one_seen_is_written_over_with_done() {
    // The wallet saw the record read: the phone's value that doesn't read is refused as a
    // regression of it (Switch spec 8.2), the wallet keeps what it saw, and its outcome
    // builds on the refused write's version, so `done` goes with the record's update,
    // which replaces the value.
    let w = World::new();
    let (mut wallet, mut phone, request, attempt_id) = in_flight(&w).await;
    phone_writes(&mut phone, &request, Some(INVALID)).await;

    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Done);
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["request"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
}

#[tokio::test]
async fn test_outcome_record_invalid_value_holds_done_until_repaired() {
    // The wallet holds no value of the record it saw read: the phone's value that doesn't
    // read is the content it holds.
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, mut phone, request, attempt_id) =
        in_flight_on(&w, Arc::clone(&store) as Arc<dyn StateStore>).await;
    store.armed.store(true, Ordering::SeqCst);
    phone_writes(&mut phone, &request, Some(INVALID)).await;

    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordHeld { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    // Nothing was written: the attempt stays open, the journal keeps the proof, and the
    // host is told.
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Open);
    let payment = wallet.payments().await.unwrap().remove(0);
    assert!(payment.record_held);
    assert_eq!(payment.stage, paykit_sdk::Stage::RailPaid);
    assert_eq!(w.rail.executions(&attempt_id), 1);

    // The user has the record written again: `done` goes with it.
    let repaired = wallet.repair_activity_record(&attempt_id).await.unwrap();
    assert!(matches!(repaired, PayOutcome::Paid { .. }), "{repaired:?}");
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Done);
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["proof"].is_null(), "{value}");
    assert!(!wallet.payments().await.unwrap()[0].record_held);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

// ---------------------------------------------------------------------------------------
// The user's `done` (Paykit spec 7.3) goes with the record too

/// The user resolves `attempt_id` as paid.
async fn resolve_done(wallet: &mut Paykit, attempt_id: AttemptId) -> paykit_sdk::Result<()> {
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
}

#[tokio::test]
async fn test_outcome_record_user_done_with_the_record_deleted_rebuilds_it() {
    let w = World::new();
    let (mut wallet, mut phone, request, attempt_id) = in_flight(&w).await;
    phone_writes(&mut phone, &request, None).await;
    wallet.receive(None).await.unwrap();

    resolve_done(&mut wallet, attempt_id).await.unwrap();
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Done);
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["request"].is_null(), "{value}");
    assert_eq!(value["status"], "paid", "{value}");
}

#[tokio::test]
async fn test_outcome_record_user_done_over_an_invalid_value_holds() {
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, mut phone, request, attempt_id) =
        in_flight_on(&w, Arc::clone(&store) as Arc<dyn StateStore>).await;
    store.armed.store(true, Ordering::SeqCst);
    phone_writes(&mut phone, &request, Some(INVALID)).await;
    wallet.receive(None).await.unwrap();

    let refused = resolve_done(&mut wallet, attempt_id).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::RecordHeld(_))),
        "{refused:?}"
    );
    // Nothing was written: the attempt stays open, and so does the journal entry.
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Open);
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
}

#[tokio::test]
async fn test_outcome_record_user_done_of_a_payment_made_places_its_proof() {
    // The method reported the payment made, and its outcome waits on a record that doesn't
    // read; the record is then deleted, and the user resolves it as paid: the outcome is
    // written with the rebuilt record and the proof.
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, mut phone, request, attempt_id) =
        in_flight_on(&w, Arc::clone(&store) as Arc<dyn StateStore>).await;
    store.armed.store(true, Ordering::SeqCst);
    phone_writes(&mut phone, &request, Some(INVALID)).await;
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordHeld { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    phone.receive(None).await.unwrap();
    phone_writes(&mut phone, &request, None).await;
    wallet.receive(None).await.unwrap();

    resolve_done(&mut wallet, attempt_id).await.unwrap();
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Done);
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["proof"].is_null(), "{value}");
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Finished);
    assert!(!payment.record_held);
}

#[tokio::test]
async fn test_outcome_record_user_cant_fail_a_payment_the_method_reported_made() {
    // The method reported the payment made, and its outcome waits on a record that doesn't
    // read: the user can't resolve it `failed` or `unknown`, which would free the claim for
    // a second payment, and the attempt stays open with its proof journaled.
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, mut phone, request, attempt_id) =
        in_flight_on(&w, Arc::clone(&store) as Arc<dyn StateStore>).await;
    store.armed.store(true, Ordering::SeqCst);
    phone_writes(&mut phone, &request, Some(INVALID)).await;
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordHeld { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    for resolution in [
        paykit_sdk::Resolution::Failed,
        paykit_sdk::Resolution::Unknown,
    ] {
        let refused = wallet
            .resolve_attempt(
                &obligation,
                attempt_id,
                resolution,
                pubky_switch_client::app::UserConfirmed::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            refused.refusal(),
            Some(&paykit_sdk::Refusal::AlreadyPaid),
            "{resolution:?}"
        );
    }
    assert_eq!(claim_outcome(&mut wallet, &attempt_id).await, Outcome::Open);
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
}

/// A payment made whose acceptance could never be queued, its record held over a value
/// that doesn't read: the wallet on `store`, the phone, the request and the attempt.
async fn made_without_its_acceptance(
    w: &World,
    store: &Arc<NoSeenMarks>,
) -> (Paykit, Paykit, RequestRef, AttemptId) {
    store.refuse_queue.store(true, Ordering::SeqCst);
    let (mut wallet, mut phone, request, attempt_id) =
        in_flight_on(w, Arc::clone(store) as Arc<dyn StateStore>).await;
    store.armed.store(true, Ordering::SeqCst);
    phone_writes(&mut phone, &request, Some(INVALID)).await;
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::RecordHeld { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    store.refuse_queue.store(false, Ordering::SeqCst);
    (wallet, phone, request, attempt_id)
}

#[tokio::test]
async fn test_outcome_record_repair_sends_the_acceptance_still_owed() {
    // Every try to queue the acceptance failed, and the outcome waited on the record: the
    // repair that settles the outcome sends the acceptance and places it with the proof.
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, _phone, request, attempt_id) = made_without_its_acceptance(&w, &store).await;
    let repaired = wallet.repair_activity_record(&attempt_id).await.unwrap();
    assert!(matches!(repaired, PayOutcome::Paid { .. }), "{repaired:?}");
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
}

#[tokio::test]
async fn test_outcome_record_user_done_sends_the_acceptance_still_owed() {
    // The same, settled by the user's `done` once the record is deleted and rebuilt.
    let w = World::new();
    let store = Arc::new(NoSeenMarks::default());
    let (mut wallet, mut phone, request, attempt_id) =
        made_without_its_acceptance(&w, &store).await;
    phone.receive(None).await.unwrap();
    phone_writes(&mut phone, &request, None).await;
    wallet.receive(None).await.unwrap();
    resolve_done(&mut wallet, attempt_id).await.unwrap();
    let value = record_value(&mut wallet, &request).await.unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
}

/// Pay `attempt_id` through, with the outcome batch's first try failing as `first` says and
/// the switch refusing it for good when it is sent again: the resume reports the payment
/// made and its outcome unwritten.
async fn outcome_refused_when_sent_again(
    w: &World,
    wallet: &mut Paykit,
    attempt_id: &AttemptId,
    first: pubky_switch_client::testing::Fault,
) {
    w.rail.settle(attempt_id, true);
    let switch = w.alice.switch();
    switch.fail_next(first);
    switch.fail_next(pubky_switch_client::testing::Fault::Error {
        path: "/ops".into(),
        code: pubky_switch_core::http::ErrorCode::Conflict,
        current: serde_json::Value::Null,
    });
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
}

/// The entry is where the refused batch left it, and a resume under `pay` writes the outcome
/// once: `done`, the acceptance and the proof, paying nothing again.
async fn outcome_written_again(
    w: &World,
    wallet: &mut Paykit,
    request: &RequestRef,
    attempt_id: &AttemptId,
) {
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(claim_outcome(wallet, attempt_id).await, Outcome::Done);
    let value = record_value(wallet, request).await.unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
    assert_eq!(w.rail.executions(attempt_id), 1);
}

#[tokio::test]
async fn test_outcome_record_batch_refused_when_sent_again_is_written_again() {
    // The outcome's batch (`done`, the record and the proof) can't connect and is kept. The
    // resume's last flush sends it again and the switch refuses it for good, so the client
    // drops it with its local writes, the journal's move to `Finished` included, and reports
    // it (`BatchRefused`).
    let w = World::new();
    let (mut wallet, _phone, request, attempt_id) = in_flight(&w).await;
    let unreachable = pubky_switch_client::testing::Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    };
    outcome_refused_when_sent_again(&w, &mut wallet, &attempt_id, unreachable).await;
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, pubky_switch_client::app::AppEvent::BatchRefused { .. })),
        "{summary:?}"
    );
    outcome_written_again(&w, &mut wallet, &request, &attempt_id).await;
}

#[tokio::test]
async fn test_outcome_record_kept_after_a_refusal_about_the_caller_stays_unwritten() {
    // The outcome's batch is signed and kept, and the service time read before its first
    // send is refused `403 not_current`. The batch still goes once the app is current, so
    // the entry isn't finished as unreportable: when the switch then refuses the batch for
    // good, the entry is still there for a resume.
    let w = World::new();
    let (mut wallet, _phone, request, attempt_id) = in_flight(&w).await;
    w.clock.advance_ms(11 * 60_000);
    let not_current = pubky_switch_client::testing::Fault::Error {
        path: "/feed".into(),
        code: pubky_switch_core::http::ErrorCode::NotCurrent,
        current: serde_json::Value::Null,
    };
    outcome_refused_when_sent_again(&w, &mut wallet, &attempt_id, not_current).await;
    outcome_written_again(&w, &mut wallet, &request, &attempt_id).await;
}
