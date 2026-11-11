//! Records written by apps of other versions (Paykit wire 5, 5.3): a writer keeps every member
//! it doesn't know as it read it, and reads `unrequested` without `amount`.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::AttemptId;
use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, RequestRef, UnrequestedPayment};
use pubky_switch_client::app::{Collection as SwitchCollection, OpBatch, RecordTarget};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::common::{btc, draft, endpoint, request_from, World};
use crate::outcome_record::{payer_record, NoSeenMarks};

/// Alice's payer record of her payment without a request `attempt_id`.
fn unrequested_record(w: &World, attempt_id: &AttemptId) -> RecordTarget {
    let collection = Collection::PayerActivity;
    RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::payer_payment_record(&w.alice_id(), attempt_id),
    )
}

/// The value at `target`, read again from the switch by `app`.
async fn value_at(app: &mut Paykit, target: &RecordTarget) -> Value {
    let held = app.app_mut().read_record(target).await.unwrap().unwrap();
    serde_json::from_slice(held.content.value().unwrap()).unwrap()
}

/// The phone, an own app of another version, reads the record at `target`, changes its value
/// with `change` and writes it back.
async fn phone_rewrites(
    phone: &mut Paykit,
    target: &RecordTarget,
    change: impl FnOnce(&mut Value),
) {
    let mut value = value_at(phone, target).await;
    change(&mut value);
    let bytes = serde_json::to_vec(&value).unwrap();
    let op = phone
        .app_mut()
        .record_op(target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    phone
        .app_mut()
        .submit_batch(OpBatch::new().entry(op))
        .await
        .unwrap();
}

/// Bob's request at alice's wallet and phone, which both hold its record.
async fn requested(w: &World) -> (Paykit, Paykit, RequestRef) {
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    (wallet, phone, request)
}

#[tokio::test]
async fn test_mixed_versions_payment_keeps_members_its_writer_does_not_know() {
    let w = World::new();
    let (mut wallet, mut phone, request) = requested(&w).await;
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // A later version's app adds members this one doesn't know, and a manual-only mark.
    let target = payer_record(&request);
    let note = json!({ "by": "a later version", "n": 2 });
    phone_rewrites(&mut phone, &target, |v| {
        v["future_member"] = note.clone();
        v["request"]["relayed_by"] = json!("bob's relay");
        v["manual_only"] = json!(true);
    })
    .await;

    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    // The wallet's outcome wrote the record again, with the proof, and kept them all.
    let value = value_at(&mut wallet, &target).await;
    assert!(!value["proof"].is_null(), "{value}");
    assert_eq!(value["future_member"], note, "{value}");
    assert_eq!(value["request"]["relayed_by"], "bob's relay", "{value}");
    assert_eq!(value["manual_only"], true, "{value}");
}

#[tokio::test]
async fn test_mixed_versions_unrequested_without_amount_reads() {
    let w = World::new();
    // The wallet keeps no mark of the values it saw, so it acts on the value the phone
    // writes only if that value reads.
    let store = Arc::new(NoSeenMarks::default());
    let mut wallet = w
        .wallet_on("wallet", Arc::clone(&store) as Arc<dyn StateStore>)
        .await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.0015")).with_options(
                PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")),
            ),
        )
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // The attempt's batch wrote the amount with the destination.
    let target = unrequested_record(&w, &attempt_id);
    let value = value_at(&mut phone, &target).await;
    assert_eq!(
        value["unrequested"]["amount"],
        json!({ "asset": "btc", "value": "0.0015" }),
        "{value}"
    );
    // An app that doesn't know the member writes the record without it.
    store.armed.store(true, Ordering::SeqCst);
    phone_rewrites(&mut phone, &target, |v| {
        v["unrequested"].as_object_mut().unwrap().remove("amount");
    })
    .await;

    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    let value = value_at(&mut wallet, &target).await;
    assert!(!value["proof"].is_null(), "{value}");
    assert!(!value["unrequested"].is_null(), "{value}");
}
