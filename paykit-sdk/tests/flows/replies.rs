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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 2);
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
        .submit_batch(OpBatch::new().entry(entry))
        .await
        .unwrap();
    let key = shop.app().record_key(&target).unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    // This app's kept copies of the same replies are taken now: the record holds them, so
    // nothing is written, and the copies go.
    shop.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.bob, &key), 1);
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
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
    let at = w.clock.now_ms();
    let request = paid_before_recorded(w, wallet, shop).await;
    // Accepted after the paid request's replies.
    w.clock.advance_ms(1);
    unmatched_replies(w, shop, n);
    shop.receive(None).await.unwrap();
    (request, at)
}

/// The marks of senders whose replies were dropped, whole.
async fn mark_values(shop: &Paykit) -> Vec<serde_json::Value> {
    shop.app()
        .protocol_scan(b"pd/")
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
    record_request(w, &mut shop, &request).await;
    let summary = shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert_eq!(
        summary.resends,
        u64::from(dropped),
        "asked once only for a dropped reply"
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
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
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
    let (request, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    // The request isn't delivered now: it stays queued.
    refuse_next_delivery(&w, ErrorCode::Unavailable);
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
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
async fn test_replies_dropped_ask_refused_for_good_asked_anew_at_the_next_receive() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let n = paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER;
    let (request, at) = kept_then_unmatched(&w, &mut wallet, &mut shop, n).await;
    refuse_next_delivery(&w, ErrorCode::PermissionMissing);
    record_request(&w, &mut shop, &request).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert!(asked(&shop, &w.alice_id()).await.is_empty());
    // Refused, it counts as not sent: asked anew, within the day.
    assert_eq!(shop.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(&shop, &w.alice_id()).await, vec![(Some(at), None)]);
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
        .submit_batch(OpBatch::new().entry(entry))
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
    let kept = shop.app().protocol_scan(b"pr/").await.unwrap().len();
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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 2);
    assert!(shop.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices
        .iter()
        .all(|n| n.kind == paykit_sdk::NoticeKind::MessageGivenUp
            && n.request_hash == Some(request.hash())));
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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 2);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS + DAY_MS);
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    w.bob.switch().fail_next(Fault::Error {
        path: "records?collection".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_none(), "{got:?}");
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
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
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 3);
    record_request(&w, &mut shop, &request).await;
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(shop.app().protocol_scan(b"pr/").await.unwrap().len(), 1);
    assert!(shop.notices().await.unwrap().is_empty());
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    shop.receive(None).await.unwrap();
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    let notices = shop.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, w.carol_id());
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
    assert!(shop.app().protocol_scan(b"pr/").await.unwrap().is_empty());
    assert!(shop.app().protocol_scan(b"se/").await.unwrap().is_empty());
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}
