//! A payer's replies that reach the payee before it holds the request record they name are
//! kept until it does, then placed (Switch spec 8.2, Paykit spec 8.1).

use std::sync::Arc;

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, Rail, RequestRef};
use pubky_switch_client::app::{
    Collection as SwitchCollection, OpBatch, OutgoingMessage, RecordTarget, SignedMessage,
};
use pubky_switch_client::clock::Clock;
use serde_json::json;

use crate::common::{draft, request_from, store, World};

/// Bob's shop signs a request to alice and sends it alone, without writing its record, as
/// another of bob's apps would before this one holds the record. Alice pays it: her
/// acceptance and proof reach the shop. Returns the request.
async fn paid_before_recorded(w: &World, wallet: &mut Paykit, shop: &mut Paykit) -> SignedMessage {
    paid_before_recorded_then(w, wallet, shop, async |_| {}).await
}

/// [`paid_before_recorded`], with `before` run before the shop reads the replies.
async fn paid_before_recorded_then(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    before: impl AsyncFnOnce(&mut Paykit),
) -> SignedMessage {
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let signed = shop
        .app()
        .sign_message(OutgoingMessage::new(
            w.alice_id(),
            "paykit.payment_request",
            body.as_object().cloned().unwrap(),
        ))
        .unwrap();
    shop.app_mut().queue_signed(signed.clone()).await.unwrap();
    shop.app_mut().flush_outbox().await.unwrap();
    wallet.receive(None).await.unwrap();
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: signed.hash(),
    };
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let held = shop.payee_activity().await.unwrap().len();
    before(shop).await;
    shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), held);
    signed
}

/// The shop's record of `request` is written, as the other app's batch would write it.
async fn record_request(w: &World, shop: &mut Paykit, request: &SignedMessage) {
    let copy = StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        request.plaintext().clone(),
        None,
        &w.alice_id(),
    )
    .unwrap();
    let mut record = ActivityRecord::new(ActivitySide::Payee, w.alice_id());
    record.msg_ids = vec![copy.message().msg_id.clone()];
    record.request = Some(copy);
    let target = RecordTarget::new(
        SwitchCollection::new(
            Collection::PayeeActivity.name(),
            Collection::PayeeActivity.policy(None),
        ),
        &natural_key::request_record(&w.bob_id(), &request.hash()),
    );
    let value = record.to_json().unwrap();
    let entry = shop
        .app_mut()
        .record_op(&target, Some(&value), Vec::new())
        .await
        .unwrap();
    shop.app_mut()
        .submit_batch(OpBatch::new().entry(entry))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_replies_kept_until_the_record_is_held_then_placed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    // A backup carries the kept replies.
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    shop.reconcile().await.unwrap();
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_replies_kept_no_longer_than_their_sender_keeps_them() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_REPLY_MS);
    // A read past the bound drops them; the record that comes later holds the request alone.
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::Requested);
}

#[tokio::test]
async fn test_replies_from_a_blocked_payer_not_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    let alice = w.alice_id();
    let request = paid_before_recorded_then(&w, &mut wallet, &mut shop, async |shop| {
        shop.app_mut().deny(&alice).await.unwrap();
    })
    .await;
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Requested
    );
}

/// Alice's wallet sends bob's shop `n` acceptances of requests bob never sent.
fn unmatched_replies(w: &World, shop: &Paykit, n: usize) {
    for i in 0..n {
        let refs = vec![[u8::try_from(i).unwrap(); 32]];
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_request_acceptance",
            refs,
            serde_json::Map::new(),
        );
    }
}

/// Bob's shop holds the acceptance and the proof of a request paid before it was recorded,
/// then `n` replies from alice naming records bob never sent. Returns the request and when
/// the paid request's replies were accepted.
async fn kept_then_unmatched(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    n: usize,
) -> (SignedMessage, u64) {
    let at = w.clock.now_ms();
    let request = paid_before_recorded(w, wallet, shop).await;
    // Accepted after the paid request's replies.
    w.clock.advance_ms(1);
    unmatched_replies(w, shop, n);
    shop.receive(None).await.unwrap();
    (request, at)
}

/// The marks of senders whose replies were dropped, with their times.
async fn marks(shop: &Paykit) -> Vec<u64> {
    shop.app()
        .protocol_scan(b"pd/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| {
            let read: serde_json::Value = serde_json::from_slice(&value).unwrap();
            read["d"].as_u64().unwrap()
        })
        .collect()
}

/// [`kept_then_unmatched`], then the record is written. Returns the shop and the record key.
async fn recorded_after_unmatched(w: &World, n: usize) -> (Paykit, [u8; 32]) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, _) = kept_then_unmatched(w, &mut wallet, &mut shop, n).await;
    record_request(w, &mut shop, &request).await;
    let summary = shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert_eq!(
        summary.resends,
        u64::from(n + 2 > paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER),
        "asked once only for a dropped reply"
    );
    // Asked, the mark goes.
    assert!(marks(&shop).await.is_empty());
    (shop, record)
}

#[tokio::test]
async fn test_replies_dropped_leave_one_mark_per_sender_at_the_earliest() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Both of the paid request's replies and four more are dropped: one mark, at the time
    // of the earliest dropped.
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER + 4;
    let (_, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(marks(&shop).await, vec![at]);
}

#[tokio::test]
async fn test_replies_dropped_mark_past_30_days_asks_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, _) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(marks(&shop).await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_REPLY_MS);
    shop.receive(None).await.unwrap();
    assert!(marks(&shop).await.is_empty());
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}

#[tokio::test]
async fn test_replies_dropped_asked_for_from_the_marks_time() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // An earlier payment of a request bob recorded: its replies were accepted before the
    // mark's time, and aren't asked for.
    let earlier = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&earlier, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, _) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    // Alice answers from the mark's time: the paid request's two replies come back, as
    // duplicates for now, and not the earlier payment's.
    wallet.receive(None).await.unwrap();
    let events = shop.receive(None).await.unwrap().report.events;
    let again = events
        .iter()
        .filter(|e| matches!(e, pubky_switch_client::app::AppEvent::Duplicate { .. }))
        .count();
    assert_eq!(again, 2, "{events:?}");
}

/// How many of the payer's acceptance and proof the shop's record at `record` holds.
async fn replies_held(shop: &Paykit, record: &[u8; 32]) -> usize {
    let held = shop.app().shared_record(record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    ["response", "proof"]
        .iter()
        .filter(|role| !value[**role].is_null())
        .count()
}

#[tokio::test]
async fn test_replies_kept_up_to_the_bound_per_sender() {
    let (shop, record) =
        recorded_after_unmatched(&World::new(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 2).await;
    assert_eq!(replies_held(&shop, &record).await, 2);
}

#[tokio::test]
async fn test_replies_past_the_bound_per_sender_drop_the_earliest_and_ask_once() {
    let w = World::new();
    // One of the paid request's replies, alice's earliest kept, is dropped for the 65th.
    let (mut shop, record) =
        recorded_after_unmatched(&w, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 1).await;
    assert_eq!(replies_held(&shop, &record).await, 1);
    // Asked once: a later read asks nothing more.
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}
