//! The hold after a restore (Paykit spec 7.2, "What a restore loses"), one-time branch of
//! the conformance case `restore_stale_backup_cancellation_stop`: a backup can predate the
//! payee's cancellation of a request it holds open, so every restore holds every open
//! request until the payee's cancellation stops it for good, or the user confirms it.

use std::sync::Arc;

use paykit_sdk::{
    PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig, Rail, Refusal, RequestRef,
    RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, store, World, HOUR_MS};
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;

async fn restored(w: &World, backup: &PaykitBackup) -> Paykit {
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    app
}

fn confirmed() -> PayOptions {
    PayOptions::default().with_possibly_cancelled_confirmed(UserConfirmed::new())
}

/// The record of bob's shop app the wallet holds for `request`.
async fn cancel(shop: &mut Paykit, request: &RequestRef) {
    let record = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap()
        .record;
    shop.cancel_sent_request(&record, None).await.unwrap();
}

#[tokio::test]
async fn test_restore_hold_cancellation_resent_after_the_restore_stops_for_good() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let mut backup = wallet.backup().await.unwrap();
    // The payee cancels after the backup; the device that got it is lost before it could
    // write it into the record: its placements are refused.
    cancel(&mut shop, &request).await;
    for _ in 0..4 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
    let _ = wallet.receive(None).await;
    backup = PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    // The restore asked bob to re-send from its backup's cursor time, and bob answers;
    // the cancellation stops the request for good (Paykit spec 7.2, 8.1).
    let answered = shop.receive(None).await.unwrap();
    assert!(answered.resends >= 1, "no re-send request reached bob");
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    let refused = app.pay_request(&request, confirmed()).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_hold_asks_sent_when_reconcile_mode_ends_without_reconciling() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // The host ends reconcile mode at once: the counterparty is still asked to re-send,
    // once (Paykit spec 7.2).
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    let answered = shop.receive(None).await.unwrap();
    assert!(answered.resends >= 1, "no re-send request reached bob");
    app.receive(None).await.unwrap();
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
}

#[tokio::test]
async fn test_restore_hold_kept_when_the_confirmed_payment_is_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    // The user confirms, but the payment is refused before any attempt: the request has no
    // such endpoint. The confirmation ends nothing.
    let refused = app
        .pay_request(
            &request,
            confirmed().with_endpoint(crate::common::endpoint("btc-no-such-endpoint")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    let refused = app
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::HeldAfterRestore));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_hold_payee_silent_held_until_the_user_confirms() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // Bob never answers the re-send request.
    let mut app = restored(&w, &backup).await;
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    let refused = app
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::HeldAfterRestore));
    assert!(w.rail.all_executions().is_empty());
    // Once the user has seen the payee may have cancelled it, a manual payment goes on.
    let paid = app.pay_request(&request, confirmed()).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_restore_hold_backup_after_the_stop_keeps_it_and_holds_the_rest() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let r1 = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let mut r3_draft = draft("3", w.clock.now_ms());
    r3_draft.payment_reference = paykit_lib::PaymentReference::new("order-3").unwrap();
    let r3 = request_from(&mut shop, w.alice_id(), r3_draft).await;
    wallet.receive(None).await.unwrap();
    // R1 is cancelled before the backup: the backup holds the stop.
    cancel(&mut shop, &r1).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    // R1 stays stopped, whatever the user confirms.
    let refused = app.pay_request(&r1, confirmed()).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    // R3 is held until the user confirms it.
    let refused = app
        .pay_request(&r3, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::HeldAfterRestore));
    let paid = app.pay_request(&r3, confirmed()).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_restore_hold_another_apps_record_write_stops_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let mut other_draft = draft("2", w.clock.now_ms());
    other_draft.payment_reference = paykit_lib::PaymentReference::new("order-2").unwrap();
    let other = request_from(&mut shop, w.alice_id(), other_draft).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // The phone gets bob's cancellation of the first and writes it into its record.
    cancel(&mut shop, &request).await;
    phone.receive(None).await.unwrap();
    let mut app = restored(&w, &backup).await;
    // The record the phone wrote stops it: no confirmation overrides it.
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    let refused = app.pay_request(&request, confirmed()).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    // The other is held, as every open request after a restore.
    assert_eq!(
        app.request(&other).await.unwrap().status,
        RequestStatus::Held
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_hold_holds_a_request_first_seen_after_the_restore_until_confirmed() {
    // No time the app can see proves a request new: one bob sends after the restore is held
    // like the others, and a manual payment needs the user's confirmation (Paykit spec 7.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    w.clock.advance_ms(60_000);
    let after = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&after).await.unwrap().status,
        RequestStatus::Held
    );
    let refused = app
        .pay_request(&after, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::HeldAfterRestore));
    let paid = app.pay_request(&after, confirmed()).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_restore_hold_keeps_a_request_created_before_and_resent_after() {
    // The adversarial re-send: a request bob made before the restore, which the lost device
    // never placed, comes only after the reconciliation, with a later acceptance time: it
    // is held.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let _shop = w.shop().await;
    let body = serde_json::json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let made = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
    let mut app = restored(&w, &backup).await;
    w.clock.advance_ms(60_000);
    w.deliver(&w.bob, "shop", &w.alice, &[app.app_id()], &made);
    app.receive(None).await.unwrap();
    let requests = app.requests().await.unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].status, RequestStatus::Held);
}

/// A `missed` item at alice's switch: entries accepted in the last second were deleted before
/// the wallet read them (Switch spec 8.2).
fn missed(w: &World) {
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
}

#[tokio::test]
async fn test_restore_hold_missed_item_holds_an_open_request_until_the_user_confirms() {
    // A payee's cancellation may be among the entries a `missed` item tells of: every open
    // request is held as after a restore (Paykit spec 7.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    missed(&w);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::HeldAfterRestore));
    assert!(w.rail.all_executions().is_empty());
    let paid = wallet.pay_request(&request, confirmed()).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}

#[tokio::test]
async fn test_restore_hold_missed_item_holds_a_repeat_until_the_user_confirms() {
    // A payment is in flight on a method that is idempotent and keeps no history, so a
    // resume repeats it. The user confirmed the request before; then a `missed` item comes:
    // that confirmation releases nothing, and the repeat is withheld, the attempt left open,
    // until the user confirms again.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    missed(&w);
    wallet.receive(None).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [PayOutcome::Uncertain { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    // Repeated: the method is called again for the same attempt.
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
}

#[tokio::test]
async fn test_restore_hold_confirming_a_request_not_held_refused_as_not_found() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let unknown = RequestRef {
        payee: w.bob_id(),
        request_hash: [9; 32],
    };
    let refused = wallet
        .confirm_possibly_cancelled(&unknown, UserConfirmed::new())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::NotFound));
}

/// Whether `wallet` keeps the mark of records not listed since a `missed` item.
async fn missed_unlisted(wallet: &Paykit) -> bool {
    wallet
        .app()
        .protocol_scan(b"ml")
        .await
        .unwrap()
        .iter()
        .any(|(key, _)| key == b"ml")
}

#[tokio::test]
async fn test_restore_hold_missed_item_absence_decides_nothing_until_the_listing() {
    // A request comes after a `missed` item in one read: its record's absence from the copy
    // decides nothing, since the client lists again only at its next read. The entry is held;
    // Paykit's own listing at the end of the receive ends the mark, and the next read takes
    // the request (Paykit spec 8.1, "Records not listed yet").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert!(wallet.requests().await.unwrap().is_empty());
    assert!(!missed_unlisted(&wallet).await);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
}

#[tokio::test]
async fn test_restore_hold_missed_item_mark_stays_until_paykit_lists() {
    // Paykit's listing at the end of the receive fails: the mark stays past the client's own
    // listing at the next read's start, and calls that only read are refused until a listing
    // of Paykit's own ends it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.is_empty(), "{got:?}");
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert!(missed_unlisted(&wallet).await);
    let refused = wallet.requests().await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    wallet.receive(None).await.unwrap();
    assert!(!missed_unlisted(&wallet).await);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_restore_hold_missed_item_keeps_a_stop_past_30_days_until_paykit_lists() {
    // A stop kept for a request the wallet holds no record of is past its 30 days when a
    // `missed` item comes, and Paykit's listing fails: the absence decides nothing, so the
    // stop stays until a listing of Paykit's own, and goes then (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let stops = async |wallet: &Paykit| wallet.app().protocol_scan(b"kc/").await.unwrap().len();
    let body = paykit_lib::bodies::RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request_cancellation",
        vec![[7; 32]],
        body,
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, 1);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + pubky_switch_core::records::DAY_MS);
    missed(&w);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(stops(&wallet).await, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(stops(&wallet).await, 0);
}

/// Bob's request of `reference`, signed now by his shop for alice, not sent yet.
fn signed_request(w: &World, reference: &str) -> pubky_switch_core::message::MessagePlaintext {
    let body = serde_json::json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": reference,
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    let serde_json::Value::Object(body) = body else {
        unreachable!()
    };
    w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request",
        vec![],
        body,
    )
}

/// The status the wallet shows for bob's request `plaintext`.
async fn status_of(
    w: &World,
    wallet: &Paykit,
    plaintext: &pubky_switch_core::message::MessagePlaintext,
) -> RequestStatus {
    let request = RequestRef {
        payee: w.bob_id(),
        request_hash: plaintext.hash(),
    };
    wallet.request(&request).await.unwrap().status
}

#[tokio::test]
async fn test_restore_hold_missed_item_holds_a_request_created_before_it_seen_after() {
    // A request bob created before a `missed` item reaches the wallet only after it: its
    // cancellation may have been among the entries missed, so it is held (Paykit spec 9).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let request = signed_request(&w, "order-1");
    missed(&w);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(HOUR_MS);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &request).await, RequestStatus::Held);
}

#[tokio::test]
async fn test_restore_hold_missed_item_holds_no_request_created_after_it() {
    // A request bob created past the item's last acceptance time plus the clock tolerance
    // can't have had its cancellation among the entries missed: it isn't held. One created
    // within the tolerance is.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    missed(&w);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(CLOCK_TOLERANCE_MS - 1_000);
    let within = signed_request(&w, "order-1");
    w.clock.advance_ms(2_000);
    let after = signed_request(&w, "order-2");
    for request in [&within, &after] {
        w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], request);
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &within).await, RequestStatus::Held);
    assert_eq!(status_of(&w, &wallet, &after).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_restore_hold_missed_item_holds_a_request_held_at_it_whatever_its_creation_time() {
    // A request the wallet held when the item came stays held, even one whose payee's clock
    // ran ahead, its creation time later than the item's.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let now = w.clock.now_ms();
    w.clock.set_ms(now + pubky_switch_core::records::DAY_MS);
    let ahead = signed_request(&w, "order-1");
    w.clock.set_ms(now);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &ahead);
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &ahead).await, RequestStatus::Open);
    missed(&w);
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &ahead).await, RequestStatus::Held);
}
