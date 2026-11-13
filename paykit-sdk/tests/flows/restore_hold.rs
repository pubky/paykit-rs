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

/// The next `n` batches at alice's switch are refused for good.
fn refuse_writes(w: &World, n: usize) {
    for _ in 0..n {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
}

/// Bob's shop sends `request` again, as a re-send does.
async fn send_again(w: &World, shop: &mut Paykit, request: &RequestRef) {
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    let msg_id = sent
        .iter()
        .find(|s| s.plaintext.hash() == request.request_hash)
        .unwrap()
        .msg_id
        .clone();
    shop.app_mut().resend(&msg_id).await.unwrap();
}

/// `app` backed up, dropped and restored, as [`restored`] restores.
async fn restored_from(w: &World, mut app: Paykit) -> Paykit {
    let backup = app.backup().await.unwrap();
    let backup = PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    drop(app);
    restored(w, &backup).await
}

#[tokio::test]
async fn test_restore_hold_stop_kept_before_the_backup_stops_after_it() {
    // The payee's cancellation is kept, its placement refused, when the backup is taken:
    // the restore carries the stop, which refuses the request for good, confirmed or not.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    cancel(&mut shop, &request).await;
    refuse_writes(&w, 4);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app().protocol_scan(b"kc/").await.unwrap().len(), 1);
    let mut app = restored_from(&w, wallet).await;
    let refused = app.pay_request(&request, confirmed()).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Cancelled));
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_hold_stop_placed_before_the_backup_survives_the_request_sent_again() {
    // The cancellation is placed before the backup. After the restore the payee sends the
    // request again: the record keeps the stop.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    cancel(&mut shop, &request).await;
    wallet.receive(None).await.unwrap();
    let mut app = restored_from(&w, wallet).await;
    send_again(&w, &mut shop, &request).await;
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
async fn test_restore_hold_rejected_request_stays_rejected_after_it_is_sent_again() {
    // The user rejected the request before the backup; re-sent after the restore, it stays
    // rejected and is never paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    let mut app = restored_from(&w, wallet).await;
    send_again(&w, &mut shop, &request).await;
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Rejected
    );
    let refused = app.pay_request(&request, confirmed()).await.unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::Rejected));
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_hold_resolved_attempt_never_dispatched_again() {
    // The user resolved the attempt `failed` before the backup: a resume after the restore
    // dispatches nothing again, and the claim keeps the resolution.
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
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Failed,
            UserConfirmed::new(),
        )
        .await
        .unwrap();
    let mut app = restored_from(&w, wallet).await;
    assert!(app.resume_payments().await.unwrap().is_empty());
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        app.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    let read = app
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        read.view.attempt(&attempt_id.to_bytes()).unwrap().outcome,
        pubky_switch_core::claims::Outcome::Failed
    );
}

#[tokio::test]
async fn test_restore_hold_confirmation_is_asked_again_after_another_restore() {
    // A user's confirmation of a held request belongs to the restore it answered (`hc/` is
    // never backed up): each restore holds the open requests again until the payee re-sends
    // or the user confirms (Paykit spec 7.2, "What a restore loses").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let mut app = restored_from(&w, wallet).await;
    w.rail.next(paykit_sdk::rail::Behaviour::Refuse);
    let failed = app.pay_request(&request, confirmed()).await.unwrap();
    assert!(matches!(failed, PayOutcome::Failed { .. }), "{failed:?}");
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    let app = restored_from(&w, app).await;
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
}

/// A `missed` item at alice's switch: entries accepted in the last second were deleted before
/// the wallet read them (Switch spec 8.2).
fn missed(w: &World) {
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
}

/// Fail Paykit's own listing at the end of the next receive, after a `missed` item: the
/// client's listing after the item, of the wallet's two collections, goes through first. A
/// listing that fails inside the client's read fails the read.
fn fail_paykit_listing(w: &World) {
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "records?collection".into(),
            after: 2,
        });
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

#[tokio::test]
async fn test_restore_hold_confirming_a_request_missing_from_a_copy_not_listed_refused_as_not_now()
{
    // After a `missed` item whose listing fails, bob's request is missing from the copy,
    // which may lack records: the confirmation is refused as not now, not as no request.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    fail_paykit_listing(&w);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    let refused = wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
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
    // The client lists right after the item, and Paykit's listing at the end of the receive
    // fails: the mark stays past the client's listing, and calls that only read are refused
    // until a listing of Paykit's own ends it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    fail_paykit_listing(&w);
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
    fail_paykit_listing(&w);
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

/// How many `resend_request`s `app` sent `to`.
async fn asks_to(app: &Paykit, to: &[u8; 32]) -> usize {
    crate::common::asked(app, to).await.len()
}

#[tokio::test]
async fn test_restore_hold_missed_item_asks_the_payee_of_an_open_request_another_app_recorded() {
    // Bob's request reaches only alice's phone, which records it; the wallet holds the
    // record but never received the request itself. After a `missed` item the wallet asks
    // bob to send again: he is the counterparty of an open request it holds (Paykit spec 9).
    // Bob shares his list with the phone first, so his switch admits alice's messages.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[phone.app_id()], w.clock.now_ms())
        .await;
    let request = signed_request(&w, "order-1");
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], &request);
    phone.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &request).await, RequestStatus::Open);
    assert!(wallet
        .app()
        .protocol_scan(b"peer/")
        .await
        .unwrap()
        .is_empty());
    missed(&w);
    wallet.receive(None).await.unwrap();
    let asked = crate::common::asked(&wallet, &w.bob_id()).await;
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(asked[0].0.is_some(), "{asked:?}");
}

#[tokio::test]
async fn test_restore_hold_missed_item_asks_no_counterparty_of_a_closed_request() {
    // The wallet rejected bob's only request: nothing of his is open, so a `missed` item asks
    // him nothing, though he has Paykit activity with the wallet.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    assert_eq!(wallet.app().protocol_scan(b"peer/").await.unwrap().len(), 1);
    missed(&w);
    wallet.receive(None).await.unwrap();
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, 0);
    // The mark is spent: a later receive asks nothing either.
    wallet.receive(None).await.unwrap();
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, 0);
}

#[tokio::test]
async fn test_restore_hold_missed_item_at_the_payee_asks_the_payer_of_an_open_request() {
    // The wallet paid bob's request; his shop holds its record, the proof received and no
    // receipt sent. A `missed` item at bob's switch asks alice to send again.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    w.bob.switch().push_missed(1, now - 1_000, now);
    shop.receive(None).await.unwrap();
    assert_eq!(asks_to(&shop, &w.alice_id()).await, 1);
}

#[tokio::test]
async fn test_restore_hold_missed_item_asks_wait_while_the_records_are_not_listed() {
    // Paykit's listing after the item fails: the copy may lack records, so the asks wait,
    // and go once a later receive lists them.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    fail_paykit_listing(&w);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, 0);
    wallet.receive(None).await.unwrap();
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, 1);
}

#[tokio::test]
async fn test_restore_hold_missed_item_deleted_record_decides_nothing_until_the_listing() {
    // The phone deleted the wallet's record of bob's request. A `missed` item comes, Paykit's
    // listing fails, and bob's cancellation of the request follows in the same read: the
    // delete the copy holds decides nothing while the copy may lack records, so the
    // cancellation is held, then taken once Paykit lists (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, None).await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    let body = paykit_lib::bodies::RequestReply {
        reason: None,
        label: None,
    }
    .to_body()
    .unwrap();
    let cancellation = w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request_cancellation",
        vec![request.request_hash],
        body,
    );
    fail_paykit_listing(&w);
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_some(), "{got:?}");
    assert!(!got.applied.contains(&cancellation), "{got:?}");
    assert!(wallet.app().protocol_scan(b"kc/").await.unwrap().is_empty());
    // The next receive's read still holds it, and its end lists; the one after takes it.
    let got = wallet.receive(None).await.unwrap();
    assert!(got.records_unlisted.is_none(), "{got:?}");
    let got = wallet.receive(None).await.unwrap();
    assert!(got.applied.contains(&cancellation), "{got:?}");
    assert_eq!(wallet.app().protocol_scan(b"kc/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_restore_hold_missed_item_mark_naming_another_time_refused_as_corrupt() {
    // A `missed` item's mark whose value isn't the time its key names is a corrupt store:
    // the receive fails rather than ask from the wrong time and drop the mark (Paykit spec 9).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let key = format!("mi/{:020}", 5);
    let value = serde_json::to_vec(&serde_json::json!({ "v": 1, "d": 0 })).unwrap();
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: key.clone().into_bytes(),
            value,
        }])
        .await
        .unwrap();
    let failed = wallet.receive(None).await.unwrap_err();
    assert!(crate::common::is_corrupt(&failed), "{failed:?}");
    assert!(crate::common::asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(wallet.app().protocol_scan(b"mi/").await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_restore_hold_missed_item_confirmed_is_held_again_by_a_restore() {
    // The user confirmed a request a `missed` item held. A restore from a backup taken then
    // holds it again, as every restore holds every open request (Paykit spec 7.2): a
    // confirmation belongs to the hold it ended, and a backup carries none.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    wallet.receive(None).await.unwrap();
    wallet
        .confirm_possibly_cancelled(&request, UserConfirmed::new())
        .await
        .unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    let mut wallet = restored_from(&w, wallet).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Held
    );
}

#[tokio::test]
async fn test_restore_hold_missed_item_asks_sent_not_sent_again_by_a_restore() {
    // A `missed` item's ask to bob went, and its mark with it. A restore from a backup taken
    // then asks bob from the restore's own time as its reconcile ends, and never the item's
    // ask again.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, 1);
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
    // The restore's own ask goes as its reconcile ends.
    let mut wallet = restored_from(&w, wallet).await;
    let before = asks_to(&wallet, &w.bob_id()).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(asks_to(&wallet, &w.bob_id()).await, before);
    assert!(wallet.app().protocol_scan(b"mi/").await.unwrap().is_empty());
    assert!(wallet
        .app()
        .protocol_scan(b"ask/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_restore_hold_missed_item_listing_failing_in_the_read_fails_the_receive() {
    // The client lists right after a `missed` item, inside its read, before it hands over
    // any later entry (Switch spec 8.2): a listing that fails there fails the read, and the
    // receive returns its error, the request after the item not taken. The next receive
    // lists first and takes it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    missed(&w);
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "records?collection".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    assert!(wallet.receive(None).await.is_err());
    // The item was passed: the records count as not listed by Paykit since.
    assert!(wallet.app().protocol_get(b"ml").await.unwrap().is_some());
    let mut applied = Vec::new();
    for _ in 0..2 {
        applied.extend(wallet.receive(None).await.unwrap().applied);
    }
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert!(wallet.app().protocol_get(b"ml").await.unwrap().is_none());
    assert!(wallet.request(&request).await.is_ok());
}

#[tokio::test]
async fn test_restore_hold_missed_item_bounded_by_its_last_acceptance_time() {
    // The item's last acceptance time is an hour before the read: a request bob created
    // since, past that time plus the clock tolerance, can't have had its cancellation among
    // the entries missed and isn't held, though the read's service time is later. One
    // created at the item's last time is held.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let now = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, now - 2 * HOUR_MS, now - HOUR_MS);
    wallet.receive(None).await.unwrap();
    let after = signed_request(&w, "order-1");
    w.clock.set_ms(now - HOUR_MS);
    let within = signed_request(&w, "order-2");
    w.clock.set_ms(now);
    for request in [&within, &after] {
        w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], request);
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &within).await, RequestStatus::Held);
    assert_eq!(status_of(&w, &wallet, &after).await, RequestStatus::Open);
}

#[tokio::test]
async fn test_restore_hold_request_sent_again_over_its_seen_deleted_record_changes_nothing() {
    // The wallet took bob's request; alice's phone then deletes its record. Past the 30
    // days the wallet keeps the request as processed, the same request comes again: the
    // wallet saw the record's messages, so it takes the request as one for the record it
    // saw and writes nothing over the delete (Paykit spec 8.1, "Records not listed yet").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let _shop = w.shop().await;
    let request = signed_request(&w, "order-1");
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    wallet.receive(None).await.unwrap();
    assert_eq!(status_of(&w, &wallet, &request).await, RequestStatus::Open);
    phone.receive(None).await.unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.hash()),
    );
    phone
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    w.clock.advance_ms(31 * pubky_switch_core::records::DAY_MS);
    wallet.receive(None).await.unwrap();
    let written = w.record_writes(&w.alice, &key);
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &request);
    let got = wallet.receive(None).await.unwrap();
    assert_eq!(got.applied.len(), 1, "{got:?}");
    assert_eq!(w.record_writes(&w.alice, &key), written);
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
}
