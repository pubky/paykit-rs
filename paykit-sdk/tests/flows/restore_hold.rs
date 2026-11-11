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
    PayOptions {
        possibly_cancelled_confirmed: Some(UserConfirmed::new()),
        ..PayOptions::default()
    }
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
async fn test_restore_hold_spares_a_request_first_received_after_the_reconciliation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let before = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    // A request bob sends once the restored app has reconciled can't have been cancelled
    // before the backup: it isn't held, while the one from before still is.
    w.clock.advance_ms(60_000);
    let after = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    app.receive(None).await.unwrap();
    assert_eq!(
        app.request(&after).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(
        app.request(&before).await.unwrap().status,
        RequestStatus::Held
    );
    let paid = app
        .pay_request(&after, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
}
