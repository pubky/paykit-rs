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
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::DAY_MS;
use serde_json::json;

use crate::common::{asked, draft, mark_senders, request_from, seed_marks, store, World};

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
        .outcome
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(entry))
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
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
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

/// The message of `kind` alice's wallet sent bob, as bob's apps store it.
async fn sent_reply(w: &World, wallet: &Paykit, kind: &str) -> StoredMessage {
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap();
    let mut of_kind = sent.iter().filter(|m| m.kind == kind);
    let reply = of_kind.next().unwrap();
    assert!(of_kind.next().is_none());
    StoredMessage::new(
        w.alice_id(),
        wallet.app().cert().signed(),
        reply.plaintext.clone(),
        None,
        &w.bob_id(),
    )
    .unwrap()
}

#[tokio::test]
async fn test_replies_their_record_already_holds_not_written_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    let acceptance = sent_reply(&w, &wallet, "paykit.payment_request_acceptance").await;
    let proof = sent_reply(&w, &wallet, "paykit.payment_proof").await;
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    // Another of bob's apps placed both replies long ago, and has since reflected 20 other
    // messages: the record's newest 16 msg_ids are of those.
    let copy = StoredMessage::new(
        w.bob_id(),
        shop.app().cert().signed(),
        request.plaintext().clone(),
        None,
        &w.alice_id(),
    )
    .unwrap();
    let mut record = ActivityRecord::new(ActivitySide::Payee, w.alice_id());
    record.request = Some(copy);
    record.response = Some(acceptance.clone());
    record.proof = Some(proof);
    let stored: Vec<String> = record
        .stored()
        .iter()
        .map(|(_, s)| s.message().msg_id.clone())
        .collect();
    let others: Vec<String> = (0..20)
        .map(|i| format!("123e4567-e89b-42d3-a456-4266141740{i:02}"))
        .collect();
    record.msg_ids = paykit_lib::records::bounded_msg_ids(&stored, &others);
    assert!(!record.msg_ids[stored.len()..].contains(&acceptance.message().msg_id));
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
        .submit_batch(OpBatch::for_protocol("paykit").entry(entry))
        .await
        .unwrap();
    let key = shop.app().record_key(&target).unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    // This app's kept copies of the same replies are taken now: the record holds them, so
    // nothing is written, and the copies go.
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(&value[..]));
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
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
    let (request, at) = unmatched_after_payment(w, wallet, shop, n).await;
    let received = shop.receive(None).await.unwrap();
    assert_eq!(
        received.resends,
        u64::from(n + 2 > paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER)
    );
    (request, at)
}

async fn unmatched_after_payment(
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
    (request, at)
}

/// The marks of senders whose replies were dropped, whole.
async fn mark_values(shop: &Paykit) -> Vec<serde_json::Value> {
    shop.app()
        .protocol_scan("paykit", b"pd/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| serde_json::from_slice::<serde_json::Value>(&value).unwrap()["d"].clone())
        .collect()
}

/// The marks of senders whose replies were dropped, as their earliest and latest times.
async fn marks(shop: &Paykit) -> Vec<(u64, u64)> {
    mark_values(shop)
        .await
        .iter()
        .map(|d| {
            (
                d["earliest_ms"].as_u64().unwrap(),
                d["latest_ms"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// [`kept_then_unmatched`], then the record is written. Returns the shop, the record key
/// and when the paid request's replies were accepted.
async fn recorded_after_unmatched(w: &World, n: usize) -> (Paykit, [u8; 32], u64) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, at) = kept_then_unmatched(w, &mut wallet, &mut shop, n).await;
    let dropped = n + 2 > paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let before = mark_values(&shop).await;
    assert_eq!(before.len(), usize::from(dropped));
    assert_eq!(
        asked(&shop, &w.alice_id()).await.len(),
        usize::from(dropped)
    );
    record_request(w, &mut shop, &request).await;
    let summary = shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert_eq!(
        summary.resends, 0,
        "receiving the record does not repeat the ask within the day"
    );
    // Asking doesn't change the mark's times.
    let after = mark_values(&shop).await;
    assert_eq!(after.len(), before.len());
    for (before, after) in before.iter().zip(&after) {
        assert_eq!(before["earliest_ms"], after["earliest_ms"]);
        assert_eq!(before["latest_ms"], after["latest_ms"]);
    }
    (shop, record, at)
}

#[tokio::test]
async fn test_replies_dropped_leave_one_mark_per_sender_with_the_earliest_and_the_latest() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Both of the paid request's replies are dropped, at the time they were accepted.
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (_, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(marks(&shop).await, vec![(at, at)]);
    // A reply dropped later, the earliest kept then, counts in the same mark with its own
    // time.
    w.clock.advance_ms(60_000);
    unmatched_replies(&w, &shop, 1);
    shop.receive(None).await.unwrap();
    assert_eq!(marks(&shop).await, vec![(at, at + 1)]);
}

#[tokio::test]
async fn test_replies_dropped_restore_holds_a_window_widened_after_its_last_ask() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (_, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
    assert_eq!(marks(&shop).await, vec![(at, at)]);

    // Another reply displaces one accepted after the original ask's covered drops.
    w.clock.advance_ms(60_000);
    unmatched_replies(&w, &shop, 1);
    shop.receive(None).await.unwrap();
    assert_eq!(marks(&shop).await, vec![(at, at + 1)]);
    let mut shop = shop_restored(&w, shop).await;
    w.clock.advance_ms(DAY_MS);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    let overlap = paykit_sdk::ResendSelection::default().with_accepted_from_ms(at);
    assert!(matches!(
        shop.request_resend(w.alice_id(), overlap)
            .await
            .unwrap_err()
            .refusal(),
        Some(paykit_sdk::Refusal::NotNow(_))
    ));
}

/// Bob's shop holds the paid request's replies, accepted at the time returned, and drops them
/// for the bound; then it drops alice's replies accepted 10 days later. Returns the request
/// and that first time.
async fn dropped_over_ten_days(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
) -> (SignedMessage, u64) {
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, at) = kept_then_unmatched(w, wallet, shop, n).await;
    // The 64 replies accepted then are dropped for 64 accepted 10 days later, and one of
    // those for a reply 20 days later.
    w.clock.advance_ms(10 * DAY_MS);
    let ten_days = w.clock.now_ms();
    unmatched_replies(w, shop, n);
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(10 * DAY_MS);
    unmatched_replies(w, shop, 1);
    shop.receive(None).await.unwrap();
    assert_eq!(marks(shop).await, vec![(at, ten_days)]);
    (request, at)
}

#[tokio::test]
async fn test_replies_dropped_mark_goes_30_days_after_its_latest_time() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, at) = dropped_over_ten_days(&w, &mut wallet, &mut shop).await;
    // 30 days after its earliest time, the mark still lives, and asks from that time.
    w.clock.set_ms(at + paykit_sdk::KEPT_MESSAGE_MS);
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None); 4]);
    // 30 days after its latest, it goes, and asks nothing more.
    w.clock
        .set_ms(at + 10 * DAY_MS + paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    assert!(marks(&shop).await.is_empty());
}

#[tokio::test]
async fn test_replies_dropped_mark_past_128_the_one_with_the_earliest_latest_time_goes() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // 128 marks held already, each with a latest time before alice's replies are dropped.
    let held = seed_marks(
        &mut shop,
        "pd/",
        paykit_sdk::MAX_DROPPED_MARKS,
        w.clock.now_ms() - 1_000,
    )
    .await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    let senders = mark_senders(&shop, "pd/").await;
    assert_eq!(senders.len(), paykit_sdk::MAX_DROPPED_MARKS);
    assert!(senders.contains(&w.alice_id()));
    assert!(!senders.contains(&held[0]));
    assert!(senders.contains(&held[1]));
    let lost: Vec<_> = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| {
            n.kind == paykit_sdk::NoticeKind::DroppedMessagesLost && n.counterparty == held[0]
        })
        .collect();
    assert_eq!(lost.len(), 1);
    assert!(lost[0].msg_id.is_none());
    shop.acknowledge_notice(&lost[0].id).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(!shop
        .notices()
        .await
        .unwrap()
        .iter()
        .any(|n| n.id == lost[0].id));
}

#[tokio::test]
async fn test_replies_dropped_mark_past_30_days_asks_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, _) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(marks(&shop).await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    shop.receive(None).await.unwrap();
    assert!(marks(&shop).await.is_empty());
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}

#[tokio::test]
async fn test_replies_dropped_asked_again_at_most_once_a_day_while_the_mark_lives() {
    let w = World::new();
    let (mut shop, _, at) =
        recorded_after_unmatched(&w, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER).await;
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    w.clock.advance_ms(DAY_MS - 1);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    w.clock.advance_ms(1);
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(
        asked(&shop, &w.alice_id()).await,
        vec![(Some(at), None), (Some(at), None)]
    );
    assert_eq!(marks(&shop).await, vec![(at, at)]);
}

/// The next delivery to alice's inbox is answered `code`.
fn refuse_next_delivery(w: &World, code: ErrorCode) {
    w.alice.switch().fail_next(Fault::Error {
        path: "inbox".into(),
        code,
        current: serde_json::Value::Null,
    });
}

#[tokio::test]
async fn test_replies_dropped_ask_not_delivered_tried_again_its_day_counted_from_delivery() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, at) = unmatched_after_payment(&w, &mut wallet, &mut shop, n).await;
    // The request isn't delivered now: it stays queued.
    refuse_next_delivery(&w, ErrorCode::Unavailable);
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    record_request(&w, &mut shop, &request).await;
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    // Tried again at the next receive, as the same request.
    w.clock.advance_ms(12 * 3_600_000);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
    // The day counts from its delivery, not from when it was queued.
    w.clock.advance_ms(12 * 3_600_000);
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    w.clock.advance_ms(12 * 3_600_000);
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(&shop, &w.alice_id()).await.len(), 2);
}

#[tokio::test]
async fn test_replies_dropped_first_ask_reopens_after_its_outbox_commit_failed() {
    use pubky_switch_client::store::{Fault as StoreFault, FaultStore};

    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let fault = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            fault.clone(),
        )
        .await;
    let (_, at) = unmatched_after_payment(
        &w,
        &mut wallet,
        &mut shop,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
    )
    .await;
    let id = shop.app_id();
    fault.fail_put_once(b"out/");
    shop.receive(None).await.unwrap_err();
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, fault.inner(), id, w.clock.clone())
        .await;
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
    assert_eq!(marks(&shop).await, vec![(at, at)]);
}

#[tokio::test]
async fn test_replies_dropped_floor_first_keep_can_retry_only_on_the_original_store() {
    for restore in [false, true] {
        let w = World::new();
        let (mut shop, store, from) = dropped_reply_behind_floor(&w).await;
        let app_id = shop.app_id();
        store.fail_put_once(b"p/paykit/rb/");
        shop.receive(None).await.unwrap_err();
        assert!(shop
            .app()
            .protocol_scan("paykit", b"rb/")
            .await
            .unwrap()
            .is_empty());
        let selected = shop.backup().await.unwrap();
        drop(shop);
        let mut shop = if restore {
            Paykit::restore(
                w.env(&w.bob, crate::common::store()),
                PaykitConfig::default(),
                &selected,
                w.rail.clone(),
                w.timer(),
            )
            .await
            .unwrap()
        } else {
            w.load_on_clock(&w.bob, store.inner(), app_id, w.clock.clone())
                .await
        };
        shop.receive(None).await.unwrap();
        shop.receive(None).await.unwrap();
        let asks: Vec<_> = asked(&shop, &w.alice_id())
            .await
            .into_iter()
            .filter(|(_, until)| until.is_none())
            .collect();
        if restore {
            assert!(asks.is_empty());
        } else {
            assert_eq!(asks, [(Some(from), None)]);
            assert!(!shop
                .app()
                .records_of("block_floor")
                .await
                .unwrap()
                .is_empty());
        }
    }
}

#[tokio::test]
async fn test_replies_dropped_applied_floor_reopens_the_same_original() {
    let w = World::new();
    let (mut shop, store, from) = dropped_reply_behind_floor(&w).await;
    let app_id = shop.app_id();
    store.fail_put_once(b"out/");
    // The native batch is kept, although this pass failed to commit its applied outbox item.
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    let mark = mark_values(&shop).await.pop().unwrap();
    let original = mark["queued"]["msg_id"].as_str().unwrap();
    let marker = shop.app().protocol_scan("paykit", b"rb/").await.unwrap();
    assert_eq!(marker.len(), 1);
    assert_eq!(marker[0].1.len(), 16);
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, store.inner(), app_id, w.clock.clone())
        .await;
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let asks: Vec<_> = asked(&shop, &w.alice_id())
        .await
        .into_iter()
        .filter(|(_, until)| until.is_none())
        .collect();
    assert_eq!(asks, [(Some(from), None)]);
    assert!(matches!(
        shop.app().delivery(original).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    assert!(!shop
        .app()
        .records_of("block_floor")
        .await
        .unwrap()
        .is_empty());
    assert!(shop
        .app()
        .protocol_scan("paykit", b"rb/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_replies_dropped_refused_original_floor_permits_a_new_ask() {
    let w = World::new();
    let (mut shop, store, from) = dropped_reply_behind_floor(&w).await;
    w.bob.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code: ErrorCode::Conflict,
        current: serde_json::Value::Null,
    });
    shop.receive(None).await.unwrap();
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    let mark = mark_values(&shop).await.pop().unwrap();
    let first = mark["queued"]["msg_id"].as_str().unwrap();
    let marker = shop.app().protocol_scan("paykit", b"rb/").await.unwrap();
    let batch = marker.first().unwrap().1.as_slice().try_into().unwrap();
    assert!(matches!(
        shop.app().kept_batch(batch).await.unwrap(),
        Some(pubky_switch_client::app::KeptBatchStatus::Dropped {
            reason: pubky_switch_client::app::KeptBatchDrop::Refused { .. },
            ..
        })
    ));
    let id = shop.app_id();
    drop(shop);
    let mut shop = w
        .load_on_clock(&w.bob, store.inner(), id, w.clock.clone())
        .await;
    shop.receive(None).await.unwrap();
    assert!(matches!(
        shop.app().delivery(first).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Failed { .. })
    ));
    let asks: Vec<_> = asked(&shop, &w.alice_id())
        .await
        .into_iter()
        .filter(|(_, until)| until.is_none())
        .collect();
    assert_eq!(asks, [(Some(from), None)]);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"rb/")
        .await
        .unwrap()
        .is_empty());
}

/// The seller has dropped a paid reply and separately dropped a later entry for a block.
/// Its first ask after the unblock must carry the floor raise.
async fn dropped_reply_behind_floor(
    w: &World,
) -> (Paykit, Arc<pubky_switch_client::store::FaultStore>, u64) {
    use pubky_switch_client::store::{Fault as StoreFault, FaultStore};
    let mut wallet = w.wallet("wallet").await;
    let storage = Arc::new(FaultStore::new(store(), StoreFault::None));
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new()
                .pay()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            storage.clone(),
        )
        .await;
    let (_, at) = unmatched_after_payment(
        w,
        &mut wallet,
        &mut shop,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
    )
    .await;
    shop.app_mut().receive(None).await.unwrap();
    assert_eq!(marks(&shop).await, [(at, at)]);
    unmatched_replies(w, &shop, 1);
    shop.block(&w.alice_id()).await.unwrap();
    shop.app_mut().receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"bd/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(mark_values(&shop).await[0]["queued"].is_null());
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    assert!(shop
        .app()
        .records_of("block_floor")
        .await
        .unwrap()
        .is_empty());
    shop.unblock(&w.alice_id()).await.unwrap();
    (shop, storage, at)
}

#[tokio::test]
async fn test_replies_dropped_terminal_refusal_of_a_sealed_ask_keeps_the_window_held() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, _) = unmatched_after_payment(&w, &mut wallet, &mut shop, n).await;
    refuse_next_delivery(&w, ErrorCode::PermissionMissing);
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    record_request(&w, &mut shop, &request).await;
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    // Switch committed the envelope before the POST. A terminal refusal of that
    // try carries no assurance that the original was never accepted.
    let duties = shop.app_mut().unacknowledged_events().await.unwrap();
    assert!(duties.iter().any(|d| matches!(
        d,
        pubky_switch_client::app::AppEvent::DeliveryUnknown { .. }
    )));
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}

#[tokio::test]
async fn test_replies_dropped_marks_carried_through_a_restore_unreset() {
    let w = World::new();
    let (mut shop, _, at) =
        recorded_after_unmatched(&w, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER).await;
    let held = mark_values(&shop).await;
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
    assert_eq!(mark_values(&shop).await, held);
    // Asked again a day after the request before the restore, not at once.
    shop.reconcile().await.unwrap();
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    w.clock.advance_ms(DAY_MS);
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(marks(&shop).await, vec![(at, at)]);
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
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(60_000);
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, _) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    // The mark asks from the paid request's replies, which came after the earlier payment.
    assert_eq!(asked(&shop, &w.alice_id()).await.len(), 1);
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    // Alice answers from the mark's time: the paid request's two replies come back, each
    // taken as a new message, its pair forgotten when it was dropped, and not the earlier
    // payment's, which would come as duplicates.
    wallet.receive(None).await.unwrap();
    let events = shop.receive(None).await.unwrap().report.events;
    let count = |duplicate: bool| {
        events
            .iter()
            .filter(|e| match e {
                pubky_switch_client::app::AppEvent::Duplicate { .. } => duplicate,
                pubky_switch_client::app::AppEvent::MessageReceived { .. } => !duplicate,
                _ => false,
            })
            .count()
    };
    assert_eq!(count(false), 2, "{events:?}");
    assert_eq!(count(true), 0, "{events:?}");
}

#[tokio::test]
async fn test_replies_dropped_for_the_bound_taken_again_from_a_resend_and_placed() {
    // The paid request's acceptance and proof, alice's earliest kept replies, are dropped
    // for the bound by her 64 replies naming records bob never sent, and counted in her
    // mark. The shop asks her to re-send before the record opens. The same two messages come
    // again, are taken as never processed, and are placed in the record (Paykit spec 8.1,
    // "Replies before their record").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        n
    );
    assert_eq!(marks(&shop).await, vec![(at, at)]);
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert_eq!(replies_held(&shop, &record).await, 0);
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(replies_held(&shop, &record).await, 2);
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_replies_dropped_asked_at_once_while_a_request_of_their_sender_is_held() {
    // The shop held alice's request's record before her replies past the bound came: the
    // mark of the one dropped is due at once, though no record changes, and asks her.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    unmatched_replies(&w, &shop, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER + 1);
    shop.receive(None).await.unwrap();
    assert_eq!(mark_senders(&shop, "pd/").await, vec![w.alice_id()]);
    assert_eq!(asked(&shop, &w.alice_id()).await.len(), 1);
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
    let (shop, record, _) =
        recorded_after_unmatched(&World::new(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 2).await;
    assert_eq!(replies_held(&shop, &record).await, 2);
}

#[tokio::test]
async fn test_replies_past_the_bound_per_sender_drop_the_earliest_and_ask_for_it() {
    let w = World::new();
    // One of the paid request's replies, alice's earliest kept, is dropped for the 65th.
    let (mut shop, record, _) =
        recorded_after_unmatched(&w, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 1).await;
    assert_eq!(replies_held(&shop, &record).await, 1);
    // Asked once: a later read within the day asks nothing more.
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}

/// The signer stores bob's app `app`'s key bundle for `cert`, epochs `first` to current.
fn bob_bundle(w: &World, app: [u8; 16], cert: &pubky_switch_core::records::AppCert, first: u64) {
    use pubky_switch_client::app::Protocol as _;
    let epoch = w.bob.switch().identity_doc().unwrap().data_epoch();
    let protocol = paykit_sdk::PaykitProtocol::new();
    let parts = pubky_switch_core::records::seal_bundle_parts(
        w.bob.identity().key(),
        cert,
        first,
        epoch,
        &|scope| protocol.has_keys(scope),
    )
    .unwrap();
    for part in parts {
        w.bob
            .switch()
            .signer_op(pubky_switch_core::ops::OpKind::KeyBundlePut(
                pubky_switch_core::ops::KeyBundlePut {
                    target_app_id: app,
                    bundle: part.bundle,
                    first_epoch: part.first_epoch,
                    last_epoch: part.last_epoch,
                },
            ))
            .unwrap();
    }
}

/// `writer` writes the payee record of `request`, which `signer_cert`'s app signed.
async fn record_request_by(
    w: &World,
    writer: &mut Paykit,
    signer_cert: &pubky_switch_core::sig::SignedDoc,
    request: &SignedMessage,
) {
    let copy = StoredMessage::new(
        w.bob_id(),
        signer_cert,
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
    let entry = writer
        .app_mut()
        .record_op(&target, Some(&value), Vec::new())
        .await
        .unwrap();
    writer
        .app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(entry))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_replies_kept_over_a_record_sealed_at_a_key_not_held_given_up_with_the_notice() {
    // The payer's acceptance and proof reach the shop before it holds the request's record.
    // Another of bob's apps then writes the record at a new data epoch whose key the shop
    // doesn't hold: the shop holds the record, unopened. Past 30 days the kept replies are
    // given up, and the host told, as any message that waited for a scope key (Paykit spec
    // 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    let kept = shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .len();
    assert_eq!(kept, 2, "acceptance and proof kept");
    w.bob.next_doc(|body| body.data_epoch += 1).unwrap();
    let till_cert = till.app().cert().clone();
    bob_bundle(&w, till.app_id(), &till_cert, 1);
    till.receive(None).await.unwrap();
    till.receive(None).await.unwrap();
    let shop_cert = shop.app().cert().signed().clone();
    record_request_by(&w, &mut till, &shop_cert, &request).await;
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let key = shop
        .app()
        .record_key(&RecordTarget::new(
            SwitchCollection::new(
                Collection::PayeeActivity.name(),
                Collection::PayeeActivity.policy(None),
            ),
            &natural_key::request_record(&w.bob_id(), &request.hash()),
        ))
        .unwrap();
    let held = shop.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        held.content
    );
    // Within 30 days, both stay kept.
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(shop.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices
        .iter()
        .all(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
            && n.request_hash == Some(request.hash())));
    // No entry is stored for a kept reply: acknowledging its notice forgets none, and the
    // notice goes.
    let parked = shop.app_mut().parked_entries().await.unwrap().len();
    shop.acknowledge_notice(&notices[0].id).await.unwrap();
    assert_eq!(shop.app_mut().parked_entries().await.unwrap().len(), parked);
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"nt/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(shop.notices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_replies_kept_past_30_days_stay_while_the_records_are_not_listed() {
    // A kept reply past its 30 days when a `missed` item comes and Paykit's listing fails:
    // the absence of its record decides nothing then, so it stays; once Paykit lists, with
    // still no record, it goes (Paykit spec 8.1, "Records not listed yet").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    // The client lists the shop's collections right after the item; Paykit's own listing at
    // the end of the receive fails.
    w.bob.switch().fail_next(Fault::Unreachable {
        path: "records?collection".into(),
        after: crate::certificate::client_listing_requests(shop.app().cert()),
    });
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_none(), "{got:?}");
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert!(shop.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_replies_kept_from_another_identity_never_placed_nor_stop_the_receive() {
    // Carol cancels bob's request to alice before the shop holds its record: her reply is
    // kept beside alice's. Once the record is held, alice's are placed and carol's isn't, as
    // she isn't the record's counterparty; every receive goes on, and 30 days after its
    // acceptance her reply is given up, the host told.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _carol = w
        .join(
            &w.carol,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    let body = paykit_lib::bodies::RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_request_cancellation",
        vec![request.hash()],
        body,
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        3
    );
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(shop.notices().await.unwrap().is_empty());
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.carol_id()));
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_replies_with_their_record_in_one_read_placed() {
    // Bob's newly joined till sends alice a request and writes its record; alice pays it.
    // The shop reads the record and alice's acceptance and proof in one read: both replies
    // go into the record, and none is taken unplaced (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut till, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let got = shop.receive(None).await.unwrap();
    let records = got
        .report
        .events
        .iter()
        .filter(|e| matches!(e, pubky_switch_client::app::AppEvent::RecordChanged { .. }))
        .count();
    assert!(records >= 1, "{got:?}");
    assert_eq!(got.applied.len(), 2, "{got:?}");
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert!(shop
        .app()
        .protocol_scan("paykit", b"se/")
        .await
        .unwrap()
        .is_empty());
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_replies_kept_under_another_messages_key_refused_as_corrupt() {
    // A kept reply found under the key of another `msg_id` is a corrupt store: the receive
    // fails rather than place, drop or give it up (Switch spec 8.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    let (key, value) = shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .remove(0);
    let key = String::from_utf8(key).unwrap();
    let (prefix, _) = key.rsplit_once('/').unwrap();
    let moved = format!("{prefix}/123e4567-e89b-42d3-a456-426614174000");
    shop.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: moved.into_bytes(),
                value,
            }],
        )
        .await
        .unwrap();
    let failed = shop.receive(None).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn test_replies_kept_past_30_days_whose_write_fails_stay_kept() {
    // Past 30 days the record comes, and the shop's writes placing the replies don't reach
    // its switch: a failed write is no refusal, so the receive returns the error, the
    // replies stay kept, none given up, and are placed once the switch answers.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    w.clock.advance_ms(2 * DAY_MS);
    shop.app_mut().rotate().await.unwrap();
    shop.app_mut().flush_cursor().await.unwrap();
    w.bob.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    assert!(shop.receive(None).await.is_err());
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(shop.notices().await.unwrap().is_empty());
    shop.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
}

#[tokio::test]
async fn test_replies_kept_past_30_days_refused_by_the_switch_given_up_with_the_notice() {
    // Past 30 days the record comes, and the switch refuses for good the writes placing the
    // replies: that refusal is the record's, so the replies are given up, the host told.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    w.clock.advance_ms(2 * DAY_MS);
    shop.app_mut().rotate().await.unwrap();
    shop.app_mut().flush_cursor().await.unwrap();
    crate::common::refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices
        .iter()
        .all(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp));
}

#[tokio::test]
async fn test_replies_kept_past_30_days_over_quota_given_up_with_the_notice() {
    // As the write-fails test, but each write placing the replies is refused for good
    // (`429 quota_exceeded`), not unreachable, 31 days after their acceptance: a reply is kept
    // at most 30 days (Paykit spec 8.1), so both are given up, the host told.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    w.clock.advance_ms(2 * DAY_MS);
    shop.app_mut().rotate().await.unwrap();
    shop.app_mut().flush_cursor().await.unwrap();
    for _ in 0..2 {
        w.bob.switch().fail_next(Fault::Error {
            path: "/ops".into(),
            code: ErrorCode::QuotaExceeded,
            current: serde_json::Value::Null,
        });
    }
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices
        .iter()
        .all(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp));
}

#[tokio::test]
async fn test_replies_kept_wait_over_a_divergent_record_until_confirmed() {
    // The shop's record of the request comes, divergent after a provider loss: the replies
    // kept for it aren't written over it until the user confirms it (Paykit spec 6).
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&base),
        )
        .await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    record_request(&w, &mut shop, &request).await;
    let record = shop.payee_activity().await.unwrap()[0].record;
    crate::divergent::set_divergent(&base, &shop, &record, true);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, PayeeStatus::Requested);
    assert!(sales[0].divergent);
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    crate::divergent::confirm(&mut shop, &record).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_replies_kept_and_marks_dated_past_the_backup_bounded_at_a_restore() {
    // A backup carries kept replies and a mark of dropped replies dated two days past any
    // service time it verified: a restore bounds them by that time plus the clock tolerance,
    // as it bounds the cursor time, so they go 30 days on, not later.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    let ahead = w.clock.now_ms() + 2 * DAY_MS;
    let mut writes = Vec::new();
    for (key, value) in shop.app().protocol_scan("paykit", b"pr/").await.unwrap() {
        let mut kept: serde_json::Value = serde_json::from_slice(&value).unwrap();
        kept["d"]["accepted_at"] = json!(ahead);
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&kept).unwrap(),
        });
    }
    shop.app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
    seed_marks(&mut shop, "pd/", 1, ahead).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let bound = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    let kept = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(kept.len(), 2);
    for (_, value) in kept {
        let kept: serde_json::Value = serde_json::from_slice(&value).unwrap();
        assert!(
            kept["d"]["accepted_at"].as_u64().unwrap() <= bound,
            "{kept}"
        );
    }
    let marks = shop.app().protocol_scan("paykit", b"pd/").await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["latest_ms"].as_u64().unwrap() <= bound, "{mark}");
    assert!(
        mark["d"]["earliest_ms"].as_u64().unwrap() <= bound,
        "{mark}"
    );
}

/// `shop` backed up, dropped and restored from that backup.
async fn shop_restored(w: &World, mut shop: Paykit) -> Paykit {
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
    shop
}

/// Alice's wallet sends bob again every message it sent him, as answers to his asks do.
async fn send_all_again(w: &World, wallet: &mut Paykit) {
    for sent in wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
    {
        wallet.app_mut().resend(&sent.msg_id).await.unwrap();
    }
    wallet.flush().await.unwrap();
}

#[tokio::test]
async fn test_replies_given_up_stay_given_up_after_a_restore_and_a_resend() {
    // The replies are given up past 30 days, the switch refusing their writes. A restore
    // from a backup taken then, and alice sending them again, place nothing: a message
    // given up stays processed while its notice is kept (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    shop.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    w.clock.advance_ms(2 * DAY_MS);
    shop.app_mut().rotate().await.unwrap();
    shop.app_mut().flush_cursor().await.unwrap();
    crate::common::refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.notices().await.unwrap().len(), 2);
    let mut shop = shop_restored(&w, shop).await;
    send_all_again(&w, &mut wallet).await;
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        shop.notices()
            .await
            .unwrap()
            .iter()
            .filter(|notice| notice.kind == paykit_sdk::NoticeKind::MessageGivenUp)
            .count(),
        2
    );
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Requested
    );
}

#[tokio::test]
async fn test_replies_dropped_past_30_days_stay_dropped_after_a_restore() {
    // The replies' record never came: past 30 days they are dropped, with no notice. A
    // restore from a backup taken then keeps nothing of them.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let mut shop = shop_restored(&w, shop).await;
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    assert!(shop.notices().await.unwrap().is_empty());
}

/// Bob's shop holding his request record to alice as a value that opens but doesn't read as
/// an activity record, written by bob's till, with no pause.
struct PayeeUnread {
    shop: Paykit,
    till: Paykit,
    _wallet: Paykit,
    _carol: Paykit,
    request: RequestRef,
    record: [u8; 32],
    target: RecordTarget,
    good: Vec<u8>,
}

/// [`PayeeUnread`]: the shop keeps no mark of what it has seen in a record, so it holds the
/// value that doesn't read rather than refuse it as a regression.
async fn payee_unread(w: &World) -> PayeeUnread {
    let seen = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let base = Arc::clone(&seen) as Arc<dyn pubky_switch_client::store::StateStore>;
    let wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            base,
        )
        .await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let carol = w
        .join(
            &w.carol,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let record = shop.payee_activity().await.unwrap()[0].record;
    till.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let good = till
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    seen.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut s = PayeeUnread {
        shop,
        till,
        _wallet: wallet,
        _carol: carol,
        request,
        record,
        target,
        good,
    };
    s.till_writes(crate::outcome_record::INVALID).await;
    s.shop.receive(None).await.unwrap();
    assert_eq!(s.held().await, crate::outcome_record::INVALID);
    s
}

impl PayeeUnread {
    /// Bob's till writes `value` at the request record.
    async fn till_writes(&mut self, value: &[u8]) {
        self.till.receive(None).await.unwrap();
        let op = self
            .till
            .app_mut()
            .record_op(&self.target, Some(value), Vec::new())
            .await
            .unwrap();
        self.till
            .app_mut()
            .submit_batch(OpBatch::for_protocol("paykit").entry(op))
            .await
            .unwrap();
    }

    /// The value the shop holds at the request record.
    async fn held(&self) -> Vec<u8> {
        self.shop
            .app()
            .shared_record(&self.record)
            .await
            .unwrap()
            .unwrap()
            .content
            .value()
            .unwrap()
            .to_vec()
    }

    /// The shop's status of the request.
    async fn status(&self) -> PayeeStatus {
        self.shop
            .payee_activity()
            .await
            .unwrap()
            .into_iter()
            .find(|a| a.request_hash == Some(self.request.request_hash))
            .unwrap()
            .status
    }
}

/// A payer's acceptance, under no label.
fn acceptance_body() -> serde_json::Map<String, serde_json::Value> {
    json!({ "label": null }).as_object().cloned().unwrap()
}

#[tokio::test]
async fn test_replies_over_an_unread_record_another_identitys_period_proof_writes_nothing() {
    // Carol's proof naming bob's request and a Billing Period reaches the shop, which can't
    // read the request record. A record whose counterparty can't be told is never written
    // over: the value stays, and names no one new.
    let w = World::new();
    let mut s = payee_unread(&w).await;
    let now = w.clock.now_ms();
    let body = paykit_lib::bodies::PaymentProof {
        attempt_id: paykit_lib::AttemptId::generate().unwrap(),
        billing_period: Some(paykit_lib::periods::PeriodRef {
            starts_at: now,
            ends_at: now + crate::common::HOUR_MS,
        }),
        payment_endpoint_identifier: crate::common::endpoint("btc-lightning-bolt11"),
        payment_app_id: s.shop.app_id(),
        list_version: None,
        amount: None,
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: json!({ "type": "x-fake" }).as_object().cloned().unwrap(),
    }
    .to_body(1)
    .unwrap();
    w.inject(
        &w.carol,
        "wallet",
        &w.bob,
        &[s.shop.app_id()],
        "paykit.payment_proof",
        vec![s.request.request_hash],
        body,
    );
    s.shop.receive(None).await.unwrap();
    let carol = pubky_switch_core::enc::pubky_id(&w.carol_id());
    let held = String::from_utf8_lossy(&s.held().await).into_owned();
    assert!(
        !held.contains(&carol),
        "the request record now names carol: {held}"
    );
    assert_eq!(
        held,
        String::from_utf8_lossy(crate::outcome_record::INVALID)
    );
}

#[tokio::test]
async fn test_replies_over_an_unread_record_counterpartys_acceptance_placed_once_it_reads() {
    // Alice's acceptance reaches the shop while it can't read the request record: it is kept
    // as a reply before its record, never dropped, and placed once the record reads naming
    // alice.
    let w = World::new();
    let mut s = payee_unread(&w).await;
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[s.shop.app_id()],
        "paykit.payment_request_acceptance",
        vec![s.request.request_hash],
        acceptance_body(),
    );
    s.shop.receive(None).await.unwrap();
    assert_eq!(
        s.shop
            .app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        1
    );
    let good = s.good.clone();
    s.till_writes(&good).await;
    s.shop.receive(None).await.unwrap();
    s.shop.receive(None).await.unwrap();
    assert_eq!(s.status().await, PayeeStatus::Accepted);
    assert!(s
        .shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
}

/// A proof of a payment without a request from `from`, with app `app`, of the fake rail's
/// settlement `tx`, to bob's shop `shop`.
fn proof_of_settlement(
    w: &World,
    from: &pubky_switch_client::testing::TestSigner,
    app: &str,
    shop: &Paykit,
    tx: &str,
) -> String {
    let body = paykit_lib::bodies::PaymentProof {
        attempt_id: paykit_lib::AttemptId::generate().unwrap(),
        billing_period: None,
        payment_endpoint_identifier: crate::common::endpoint("btc-lightning-bolt12"),
        payment_app_id: shop.app_id(),
        list_version: None,
        amount: Some(crate::common::btc("0.0015")),
        allowance_id: None,
        label: None,
        payment_reference: None,
        proof: json!({ "rail": "fake", "tx": tx })
            .as_object()
            .cloned()
            .unwrap(),
    }
    .to_body(0)
    .unwrap();
    w.inject(
        from,
        app,
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_proof",
        Vec::new(),
        body,
    )
}

#[tokio::test]
async fn test_replies_proof_over_an_unread_settlement_record_writes_nothing() {
    // Carol's proof of a payment without a request opens the shop's record of its
    // settlement; bob's till then writes there a value that doesn't read. Proofs of the same
    // settlement, alice's and carol's own, aren't written over that value: the record can't
    // show its counterparty, and its key doesn't bind the sender, so each is kept as a reply
    // before its record, none held on the feed. Once the record reads again, naming carol,
    // carol's is written and alice's goes, never written.
    let w = World::new();
    let seen = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let base = Arc::clone(&seen) as Arc<dyn pubky_switch_client::store::StateStore>;
    let _wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            base,
        )
        .await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let _carol = w
        .join(
            &w.carol,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    proof_of_settlement(&w, &w.carol, "wallet", &shop, "t1");
    shop.receive(None).await.unwrap();
    let collection = Collection::PayeeActivity;
    let target = RecordTarget::new(
        SwitchCollection::new(collection.name(), collection.policy(None)),
        &natural_key::payee_payment_record(
            &crate::common::endpoint("btc-lightning-bolt12"),
            b"fake:t1",
        ),
    );
    let record = till.app().record_key(&target).unwrap();
    let good = shop
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    seen.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    till.receive(None).await.unwrap();
    let op = till
        .app_mut()
        .record_op(&target, Some(crate::outcome_record::INVALID), Vec::new())
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let from_alice = proof_of_settlement(&w, &w.alice, "wallet", &shop, "t1");
    w.clock.advance_ms(1);
    let from_carol = proof_of_settlement(&w, &w.carol, "wallet", &shop, "t1");
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(crate::outcome_record::INVALID));
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
    till.receive(None).await.unwrap();
    let op = till
        .app_mut()
        .record_op(&target, Some(&good), Vec::new())
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value = String::from_utf8_lossy(held.content.value().unwrap()).into_owned();
    assert!(value.contains(&from_carol), "{value}");
    assert!(!value.contains(&from_alice), "{value}");
}

#[tokio::test]
async fn test_replies_proof_over_an_unread_settlement_record_from_a_blocked_sender_only_counted() {
    // Bob's shop, holding `paykit_contacts`, holds its record of alice's payment t1 as a value that
    // doesn't read. Carol, blocked, sends proofs of t1. The spam rule: a blocked sender only
    // counts and keeps nothing, the client's stored entries included, then or 31 days on.
    // The feed goes on: alice's later proof of t2 is credited meanwhile.
    let w = World::new();
    let seen = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let base = Arc::clone(&seen) as Arc<dyn pubky_switch_client::store::StateStore>;
    let _wallet = w.wallet("wallet").await;
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
            base,
        )
        .await;
    let mut till = w
        .join(
            &w.bob,
            "till",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let _carol = w
        .join(
            &w.carol,
            "wallet",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    proof_of_settlement(&w, &w.alice, "wallet", &shop, "t1");
    shop.receive(None).await.unwrap();
    let target = |tx: &[u8]| {
        RecordTarget::new(
            SwitchCollection::new(
                Collection::PayeeActivity.name(),
                Collection::PayeeActivity.policy(None),
            ),
            &natural_key::payee_payment_record(
                &crate::common::endpoint("btc-lightning-bolt12"),
                tx,
            ),
        )
    };
    seen.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    till.receive(None).await.unwrap();
    let op = till
        .app_mut()
        .record_op(
            &target(b"fake:t1"),
            Some(crate::outcome_record::INVALID),
            Vec::new(),
        )
        .await
        .unwrap();
    till.app_mut()
        .submit_batch(OpBatch::for_protocol("paykit").entry(op))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    shop.block(&w.carol_id()).await.unwrap();
    for _ in 0..3 {
        proof_of_settlement(&w, &w.carol, "wallet", &shop, "t1");
    }
    w.clock.advance_ms(1);
    proof_of_settlement(&w, &w.alice, "wallet", &shop, "t2");
    shop.receive(None).await.unwrap();
    let t2 = shop.app().record_key(&target(b"fake:t2")).unwrap();
    let t2_held = shop.app().shared_record(&t2).await.unwrap().is_some();
    let counted = shop.spam_count(&w.carol_id()).await.unwrap();
    w.clock.advance_ms(DAY_MS + 1);
    shop.receive(None).await.unwrap();
    let parked_day = shop.app_mut().parked_entries().await.unwrap().len();
    w.clock.advance_ms(31 * DAY_MS);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let parked_month = shop.app_mut().parked_entries().await.unwrap().len();
    assert_eq!(
        (t2_held, counted, parked_day, parked_month),
        (true, 3, 0, 0),
        "(t2 credited, carol's proofs only counted, stored entries after a day, after 31 days)"
    );
}

/// Alice's reply of `kind` to the request `request`, signed by her app `wallet` under
/// `msg_id`, sent to bob's `shop`.
fn replies_with_id(
    w: &World,
    shop: &Paykit,
    kind: &str,
    request: [u8; 32],
    body: serde_json::Value,
    msg_id: &str,
) {
    let keys = w.alice.identity().app_keys("wallet");
    let cert = w.alice.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let mut message = pubky_switch_core::message::Message::new(
        kind,
        pubky_switch_core::message::Sender::of_app(w.alice_id(), &keys),
        w.bob_id(),
        w.clock.now_ms(),
        vec![request],
        body.as_object().cloned().unwrap(),
    )
    .unwrap();
    message.msg_id = msg_id.to_owned();
    let plaintext = message.sign(&keys).unwrap();
    w.bob
        .switch()
        .send_plaintext(w.alice_id(), &cert, &plaintext, &[shop.app_id()])
        .unwrap();
}

#[tokio::test]
async fn test_replies_kept_of_one_millisecond_placed_in_acceptance_order() {
    // Alice's rejection of a request, then her acceptance of it, reach the shop in one
    // millisecond before it holds the request's record, the rejection under the `msg_id`
    // that sorts last. Once the record is held, the reply the inbox accepted first is the
    // record's response: the request is rejected, whatever the `msg_id`s (Paykit spec 8.1).
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let request = shop
        .app()
        .sign_message(OutgoingMessage::new(
            w.alice_id(),
            "paykit.payment_request",
            body.as_object().cloned().unwrap(),
        ))
        .unwrap();
    w.clock.advance_ms(1);
    replies_with_id(
        &w,
        &shop,
        "paykit.payment_request_rejection",
        request.hash(),
        json!({ "reason": null, "label": null }),
        "f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e1f",
    );
    replies_with_id(
        &w,
        &shop,
        "paykit.payment_request_acceptance",
        request.hash(),
        json!({ "label": null }),
        "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30",
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"pr/")
            .await
            .unwrap()
            .len(),
        2
    );
    record_request(&w, &mut shop, &request).await;
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    assert!(shop
        .app()
        .protocol_scan("paykit", b"pr/")
        .await
        .unwrap()
        .is_empty());
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::Rejected);
}

#[tokio::test]
async fn test_replies_stored_of_one_millisecond_placed_in_acceptance_order() {
    // Alice's rejection of a request the shop holds, then her acceptance of it, come in one
    // millisecond, the rejection under the `msg_id` that sorts last, and the switch refuses
    // the record write of each for good: both entries are stored. The shop writes them by
    // later writes of its own in the order its inbox accepted them, so the request is
    // rejected, whatever the `msg_id`s (Paykit spec 8.1).
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    w.clock.advance_ms(1);
    replies_with_id(
        &w,
        &shop,
        "paykit.payment_request_rejection",
        request.request_hash,
        json!({ "reason": null, "label": null }),
        "f0e1d2c3-b4a5-4697-8879-6a5b4c3d2e1f",
    );
    replies_with_id(
        &w,
        &shop,
        "paykit.payment_request_acceptance",
        request.request_hash,
        json!({ "label": null }),
        "0d6f5b9e-3c1a-4e8b-9f2d-7a4c1e5b8d30",
    );
    crate::common::refuse_writes(&w.bob, 4);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.app()
            .protocol_scan("paykit", b"se/")
            .await
            .unwrap()
            .len(),
        2
    );
    for _ in 0..2 {
        shop.receive(None).await.unwrap();
    }
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::Rejected);
}

#[tokio::test]
async fn test_replies_dropped_before_their_record_are_asked_after_a_live_block_ends() {
    dropped_before_record_after_unblock(false).await;
}

#[tokio::test]
async fn test_replies_dropped_before_their_record_hold_an_unidentified_ask_after_restore() {
    dropped_before_record_after_unblock(true).await;
}

async fn dropped_before_record_after_unblock(restore: bool) {
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
    let payer = w.alice_id();
    let accepted = w.clock.now_ms();
    let request = paid_before_recorded(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(1);
    unmatched_replies(&w, &shop, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER);
    // Process the drop, then block before the SDK first tries to ask.
    shop.app_mut().receive(None).await.unwrap();
    assert_eq!(marks(&shop).await, vec![(accepted, accepted)]);
    shop.block(&payer).await.unwrap();
    if restore {
        let backup = shop.backup().await.unwrap();
        // The original installation delivered the ask, lost its reply and gave up that ID.
        shop.unblock(&payer).await.unwrap();
        w.alice.switch().fail_next(Fault::LoseAnswer {
            path: "inbox".into(),
        });
        shop.receive(None).await.unwrap();
        let original = shop
            .app()
            .queued_messages("paykit")
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.kind == "paykit.resend_request")
            .unwrap()
            .msg_id;
        let seen = wallet.app_mut().receive(None).await.unwrap();
        assert!(seen.events.iter().any(|e| matches!(e,
            pubky_switch_client::app::AppEvent::MessageReceived {msg_id, ..} if msg_id == &original)));
        assert!(shop
            .app_mut()
            .withdraw_message("paykit", &original)
            .await
            .unwrap());
        assert!(shop.app_mut().unacknowledged_events().await.unwrap().iter().any(|e| matches!(e,
            pubky_switch_client::app::AppEvent::DeliveryUnknown {msg_id, ..} if msg_id == &original)));
        shop.block(&payer).await.unwrap();
        drop(shop);
        shop = Paykit::restore(
            w.env(&w.bob, store()),
            PaykitConfig::default(),
            &backup,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
    }
    shop.receive(None).await.unwrap();
    shop.app_mut().flush_outbox().await.unwrap();
    assert!(asked(&shop, &payer).await.is_empty());
    assert!(shop.payee_activity().await.unwrap().is_empty());
    shop.unblock(&payer).await.unwrap();
    shop.receive(None).await.unwrap();
    let reply_asks = asked(&shop, &payer)
        .await
        .into_iter()
        .filter(|(_, until)| until.is_none())
        .collect::<Vec<_>>();
    if restore {
        assert!(reply_asks.is_empty());
        let overlap = paykit_sdk::ResendSelection::default().with_accepted_from_ms(accepted);
        assert!(matches!(
            shop.request_resend(payer, overlap)
                .await
                .unwrap_err()
                .refusal(),
            Some(paykit_sdk::Refusal::NotNow(_))
        ));
        let earlier = paykit_sdk::ResendSelection::default()
            .with_accepted_from_ms(accepted - 1)
            .with_accepted_until_ms(accepted - 1);
        let id = shop.request_resend(payer, earlier).await.unwrap();
        assert!(matches!(
            shop.app().delivery(&id).await.unwrap(),
            Some(pubky_switch_client::app::Delivery::Accepted { .. })
        ));
        let body = paykit_lib::bodies::ResendRequest {
            accepted_from: Some(accepted),
            ..Default::default()
        };
        let raw = shop
            .app()
            .sign_message(OutgoingMessage::new(
                payer,
                "paykit.resend_request",
                body.to_body().unwrap(),
            ))
            .unwrap();
        let id = shop.app_mut().queue_signed(raw).await.unwrap();
        shop.app_mut().flush_outbox().await.unwrap();
        assert!(matches!(
            shop.app().delivery(&id).await.unwrap(),
            Some(pubky_switch_client::app::Delivery::Queued { .. })
        ));
        // The counterparty can still return the exact proof. It settles the sale alone.
        record_request(&w, &mut shop, &request).await;
        let proof = wallet
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.kind == "paykit.payment_proof")
            .unwrap();
        wallet.app_mut().resend(&proof.msg_id).await.unwrap();
        shop.receive(None).await.unwrap();
        assert_eq!(
            shop.payee_activity().await.unwrap()[0].status,
            PayeeStatus::ProofReceived
        );
        let overlapping = paykit_sdk::ResendSelection::default().with_msg_ids(vec![proof.msg_id]);
        assert!(matches!(
            shop.request_resend(payer, overlapping)
                .await
                .unwrap_err()
                .refusal(),
            Some(paykit_sdk::Refusal::NotNow(_))
        ));

        let mut carol = w
            .join(
                &w.carol,
                "other-wallet",
                paykit_sdk::AppScopes::new()
                    .pay()
                    .manage_contacts()
                    .build()
                    .unwrap(),
            )
            .await;
        carol.app_mut().allow_connect(&w.bob_id()).await.unwrap();
        let unrelated = shop
            .request_resend(
                w.carol_id(),
                paykit_sdk::ResendSelection::default().with_accepted_from_ms(accepted),
            )
            .await
            .unwrap();
        assert!(matches!(
            shop.app().delivery(&unrelated).await.unwrap(),
            Some(pubky_switch_client::app::Delivery::Accepted { .. })
        ));
        w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 1);
        shop.receive(None).await.unwrap();
        assert!(marks(&shop).await.is_empty());
        let lost: Vec<_> = shop
            .notices()
            .await
            .unwrap()
            .into_iter()
            .filter(|notice| {
                notice.kind == paykit_sdk::NoticeKind::DroppedMessagesLost
                    && notice.counterparty == payer
            })
            .collect();
        assert_eq!(lost.len(), 1);
        assert_eq!(
            (lost[0].lost_from_ms, lost[0].lost_until_ms),
            (Some(accepted), Some(accepted))
        );
        shop.acknowledge_notice(&lost[0].id).await.unwrap();
        shop.receive(None).await.unwrap();
        assert!(!shop
            .notices()
            .await
            .unwrap()
            .iter()
            .any(|n| n.kind == paykit_sdk::NoticeKind::DroppedMessagesLost));
        let overlapping = paykit_sdk::ResendSelection::default().with_accepted_from_ms(accepted);
        assert!(matches!(
            shop.request_resend(payer, overlapping)
                .await
                .unwrap_err()
                .refusal(),
            Some(paykit_sdk::Refusal::NotNow(_))
        ));
        assert_eq!(w.rail.all_executions().len(), 1);
        return;
    }
    assert_eq!(reply_asks, vec![(Some(accepted), None)]);
    wallet.receive(None).await.unwrap();
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_replies_dropped_restore_in_place_uses_the_identified_first_delivery() {
    for accepted in [true, false] {
        let w = World::new();
        let payer = w.alice_id();
        let storage = store();
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
                Arc::clone(&storage),
            )
            .await;
        let (_sale, from) = unmatched_after_payment(
            &w,
            &mut wallet,
            &mut shop,
            paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
        )
        .await;
        shop.app_mut().receive(None).await.unwrap();
        shop.block(&payer).await.unwrap();
        let older = shop.backup().await.unwrap();
        shop.unblock(&payer).await.unwrap();
        if !accepted {
            shop.app_mut().forget_directory(&payer).await.unwrap();
            w.alice.switch().fail_next(Fault::Unreachable {
                path: "directory".into(),
                after: 0,
            });
        }
        shop.receive(None).await.unwrap();
        if accepted {
            assert_eq!(asked(&shop, &payer).await, vec![(Some(from), None)]);
        } else {
            let id = shop
                .app()
                .queued_messages("paykit")
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.kind == "paykit.resend_request")
                .unwrap()
                .msg_id;
            assert!(shop
                .app_mut()
                .withdraw_message("paykit", &id)
                .await
                .unwrap());
            assert!(shop.app_mut().unacknowledged_events().await.unwrap().iter().any(|e| matches!(e,
                pubky_switch_client::app::AppEvent::MessageUndelivered {msg_id, resend:false,..} if msg_id == &id)));
        }
        shop.block(&payer).await.unwrap();
        shop.receive(None).await.unwrap();
        drop(shop);
        let mut shop = Paykit::restore(
            w.env(&w.bob, storage),
            PaykitConfig::default(),
            &older,
            w.rail.clone(),
            w.timer(),
        )
        .await
        .unwrap();
        shop.receive(None).await.unwrap();
        shop.unblock(&payer).await.unwrap();
        shop.receive(None).await.unwrap();
        let open = |asks: Vec<(Option<u64>, Option<u64>)>| {
            asks.into_iter()
                .filter(|(_, until)| until.is_none())
                .collect::<Vec<_>>()
        };
        if accepted {
            assert!(open(asked(&shop, &payer).await).is_empty());
            w.clock.advance_ms(DAY_MS);
            shop.receive(None).await.unwrap();
        }
        assert_eq!(open(asked(&shop, &payer).await), vec![(Some(from), None)]);
    }
}

fn loss_notice(sender: &[u8; 32], from: u64, until: u64) -> (Vec<u8>, serde_json::Value) {
    let sender = pubky_switch_core::enc::b64(sender);
    (
        format!("nt/{sender}/dropped-pd-{from:020}").into_bytes(),
        json!({"v":1,"d":{
            "kind":"dropped_messages_lost","counterparty":sender,"lost":[from,until],
            "raised_at_ms":until,"acknowledged":false
        }}),
    )
}

fn retired_windows(
    sender: &[u8; 32],
    times: std::ops::RangeInclusive<u64>,
) -> (Vec<u8>, serde_json::Value) {
    let retired: Vec<_> = times
        .map(|from| json!({"from_ms":from,"until_ms":from+1,"raised_ms":from+2}))
        .collect();
    (
        format!("ra/{}", pubky_switch_core::enc::b64(sender)).into_bytes(),
        json!({"v":1,"d":{"unidentified_from":null,"originals":[],"retired":retired}}),
    )
}

async fn keep_loss_fixture(app: &mut Paykit, keys: Vec<(Vec<u8>, serde_json::Value)>) {
    let writes = keys
        .into_iter()
        .map(|(key, value)| pubky_switch_client::app::LocalWrite::Put {
            key,
            value: serde_json::to_vec(&value).unwrap(),
        })
        .collect::<Vec<_>>();
    app.app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_reply_loss_capacity_keeps_the_rotated_input_until_acknowledgement() {
    use pubky_switch_client::app::{AppEvent, ParkReason};
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    unmatched_replies(&w, &shop, paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER);
    shop.receive(None).await.unwrap();
    let prior = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(prior.len(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER);
    let first = w.clock.now_ms() - 1000;
    let marks = seed_marks(&mut shop, "pd/", paykit_sdk::MAX_DROPPED_MARKS, first).await;
    let peer = w.carol_id();
    let mut losses = (1..=63)
        .map(|n| loss_notice(&peer, n, n))
        .collect::<Vec<_>>();
    losses.push(retired_windows(&peer, 64..=64));
    keep_loss_fixture(&mut shop, losses).await;

    let id = w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.payment_request_acceptance",
        vec![[255; 32]],
        serde_json::Map::new(),
    );
    let read = shop.app_mut().receive(None).await.unwrap();
    assert!(read.events.iter().any(|e| matches!(
        e,
        AppEvent::Parked {
            reason: ParkReason::AwaitingAdmission,
            ..
        }
    )));
    assert_eq!(
        shop.app().protocol_scan("paykit", b"pr/").await.unwrap(),
        prior
    );
    assert!(mark_senders(&shop, "pd/").await.contains(&marks[0]));
    assert!(!shop.receive(None).await.unwrap().applied.contains(&id));
    let shown = shop.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    let ready = shown.iter().find(|n| n.lost_from_ms == Some(64)).unwrap();
    assert_eq!(ready.lost_until_ms, Some(65));
    shop.acknowledge_notice(&ready.id).await.unwrap();
    assert!(shop.receive(None).await.unwrap().applied.contains(&id));
    let held = shop.app().protocol_scan("paykit", b"pr/").await.unwrap();
    assert_eq!(held.len(), prior.len());
    assert_ne!(held, prior);
    let senders = mark_senders(&shop, "pd/").await;
    assert!(senders.contains(&w.alice_id()) && !senders.contains(&marks[0]));
    let shown = shop.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    assert!(shown
        .iter()
        .any(|n| n.counterparty == marks[0] && n.lost_from_ms == Some(first)));
}

#[tokio::test]
async fn test_reply_loss_restore_counts_overlapping_windows_and_preserves_the_longest_notice() {
    use crate::restore::{layer_of, with_layer};
    use paykit_sdk::PaykitBackup;
    let w = World::new();
    let mut shop = w.shop().await;
    let original = shop.backup().await.unwrap();
    let backup: serde_json::Value = serde_json::from_slice(&original.to_bytes().unwrap()).unwrap();
    let peer = w.alice_id();
    let mut selected = layer_of(&backup);
    selected.push(retired_windows(&peer, 1..=33));
    selected.extend((33..=64).map(|n| loss_notice(&peer, n, n + 5)));
    let encoded = |rows| {
        PaykitBackup::from_bytes(&serde_json::to_vec(&with_layer(&backup, rows)).unwrap()).unwrap()
    };
    let good = encoded(selected.clone());
    selected.push(loss_notice(&peer, 65, 65));
    let too_many = encoded(selected);
    let mut extra = layer_of(&backup);
    extra.push(loss_notice(&peer, 65, 65));
    let new_only = encoded(extra);
    let restore = |storage, backup| {
        Paykit::restore(
            w.env(&w.bob, storage),
            Default::default(),
            backup,
            w.rail.clone(),
            w.timer(),
        )
    };
    assert!(restore(store(), &too_many).await.is_err());

    let mut wider = layer_of(&backup);
    wider.push(retired_windows(&peer, 33..=33));
    wider.extend((1..=64).map(|n| loss_notice(&peer, n, n)));
    let wider = encoded(wider);
    let mut pending = restore(store(), &wider).await.unwrap();
    pending.receive(None).await.unwrap();
    let shown = pending.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    assert!(shown
        .iter()
        .any(|n| n.lost_from_ms == Some(33) && n.lost_until_ms == Some(34)));

    let storage = store();
    let restored = restore(storage.clone(), &good).await.unwrap();
    assert_eq!(restored.notices().await.unwrap().len(), 32);
    let unchanged = restored.app().protocol_scan("paykit", b"").await.unwrap();
    drop(restored);
    assert!(restore(storage.clone(), &new_only).await.is_err());
    let mut restored = w
        .load_on_clock(&w.bob, storage, shop.app_id(), w.clock.clone())
        .await;
    assert_eq!(
        restored.app().protocol_scan("paykit", b"").await.unwrap(),
        unchanged
    );
    restored.receive(None).await.unwrap();
    let shown = restored.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    let overlap = shown.iter().find(|n| n.lost_from_ms == Some(33)).unwrap();
    assert_eq!(overlap.lost_until_ms, Some(38));
    restored.acknowledge_notice(&overlap.id).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.notices().await.unwrap().len(), 63);
}

#[tokio::test]
async fn test_reply_loss_acknowledgement_before_the_first_restored_receive_keeps_only_a_wider_check(
) {
    use crate::restore::{layer_of, with_layer};
    use paykit_sdk::PaykitBackup;
    let w = World::new();
    let mut shop = w.shop().await;
    let backup: serde_json::Value =
        serde_json::from_slice(&shop.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let peer = w.alice_id();
    let mut selected = layer_of(&backup);
    let other = w.carol_id();
    selected.push(retired_windows(&peer, 1..=1));
    selected.push(retired_windows(&other, 2..=2));
    let chosen =
        PaykitBackup::from_bytes(&serde_json::to_vec(&with_layer(&backup, selected)).unwrap())
            .unwrap();
    let storage = store();
    let mut restored = Paykit::restore(
        w.env(&w.bob, storage.clone()),
        Default::default(),
        &chosen,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    keep_loss_fixture(
        &mut restored,
        vec![loss_notice(&peer, 1, 2), loss_notice(&other, 2, 2)],
    )
    .await;
    drop(restored);
    let mut restored = Paykit::restore(
        w.env(&w.bob, storage),
        Default::default(),
        &chosen,
        w.rail.clone(),
        w.timer(),
    )
    .await
    .unwrap();
    assert!(restored
        .app()
        .protocol_get(
            "paykit",
            format!("ra/{}", pubky_switch_core::enc::b64(&peer)).as_bytes()
        )
        .await
        .unwrap()
        .is_none());
    let visible = restored.notices().await.unwrap();
    assert_eq!(visible.len(), 2);
    for notice in visible {
        restored.acknowledge_notice(&notice.id).await.unwrap();
    }
    restored.receive(None).await.unwrap();
    let shown = restored.notices().await.unwrap();
    assert_eq!(shown.len(), 1);
    assert_eq!(
        (shown[0].lost_from_ms, shown[0].lost_until_ms),
        (Some(2), Some(3))
    );
}

#[tokio::test]
async fn test_reply_loss_expiry_keeps_an_unreported_mark_and_widens_an_existing_notice() {
    let w = World::new();
    let mut shop = w.shop().await;
    let from = w.clock.now_ms();
    let senders = seed_marks(&mut shop, "pd/", 2, from).await;
    let first = format!("pd/{}", pubky_switch_core::enc::b64(&senders[0]));
    let bytes = shop
        .app()
        .protocol_get("paykit", first.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mut mark: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    mark["d"]["latest_ms"] = json!(from + 4);
    let mut rows = (1..64)
        .map(|n| loss_notice(&w.alice_id(), n, n))
        .collect::<Vec<_>>();
    rows.push((first.into_bytes(), mark));
    rows.push(loss_notice(&senders[0], from, from));
    keep_loss_fixture(&mut shop, rows).await;
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(mark_senders(&shop, "pd/").await, [senders[1]]);
    let shown = shop.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    let widened = shown.iter().find(|n| n.counterparty == senders[0]).unwrap();
    assert_eq!(widened.lost_until_ms, Some(from + 4));
    shop.acknowledge_notice(&widened.id).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(mark_senders(&shop, "pd/").await.is_empty());
    let shown = shop.notices().await.unwrap();
    assert_eq!(shown.len(), 64);
    assert!(shown
        .iter()
        .any(|n| n.counterparty == senders[1] && n.lost_from_ms == Some(from + 1)));
}
