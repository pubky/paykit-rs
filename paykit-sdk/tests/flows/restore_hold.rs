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

use crate::common::{draft, request_from, store, World};

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
