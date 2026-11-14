//! Records written by apps of other versions (Paykit wire 5, 5.3): a writer keeps every member
//! it doesn't know as it read it.

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{PayOptions, PayOutcome, Paykit, RequestRef};
use pubky_switch_client::app::{OpBatch, RecordTarget};
use pubky_switch_client::clock::Clock;
use serde_json::{json, Value};

use crate::common::{draft, request_from, World};
use crate::outcome_record::payer_record;

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
