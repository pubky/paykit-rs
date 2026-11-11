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
const INVALID: &[u8] = b"{\"not\":\"an activity record\"}";

/// Alice's payer record of bob's request `request`.
fn payer_record(request: &RequestRef) -> RecordTarget {
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
struct NoSeenMarks {
    inner: MemoryStore,
    armed: AtomicBool,
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
    let PayOutcome::Pending { attempt_id } = wallet
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
async fn phone_writes(phone: &mut Paykit, request: &RequestRef, value: Option<&[u8]>) {
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
async fn record_value(wallet: &mut Paykit, request: &RequestRef) -> Option<serde_json::Value> {
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
    assert_eq!(resumed, vec![PayOutcome::RecordHeld { attempt_id }]);
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
