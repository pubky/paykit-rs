//! Restore and reconcile (Paykit spec 7.2, 7.3; Switch spec 12.4): a restored app makes no
//! attempt until it has reconciled, resolves its open attempts from the payment method's
//! history, and never pays twice.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig, Rail, Refusal, RequestStatus,
};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;

use crate::common::{draft, request_from, store, World};

async fn restored(w: &World, backup: &paykit_sdk::PaykitBackup) -> Paykit {
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    app
}

#[tokio::test]
async fn test_restore_reconciles_a_payment_made_after_the_backup() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();

    // Paid after the backup, then the device is lost before the outcome is written.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    w.rail.settle(&attempt_id, true);
    drop(wallet);

    let mut app = restored(
        &w,
        &PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap(),
    )
    .await;
    // Reconcile mode: no attempt before reconciliation.
    let refused = app
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );

    let report = app.reconcile().await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
    let refused = app
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::AlreadyPaid));
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_restore_resolves_a_restored_journal_entry_and_sends_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // The backup holds the journal with the payment in flight.
    let backup = wallet.backup().await.unwrap();
    w.rail.settle(&attempt_id, true);
    drop(wallet);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{report:?}"
    );
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::ProofReceived
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_restore_unresolvable_attempt_stays_doubtful() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    drop(wallet);
    // The method's history lost the dispatch: nothing can tell whether it paid.
    w.rail.forget_history(&attempt_id);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert_eq!(report.doubtful[0].attempts, vec![attempt_id]);
    // The user, who has seen "may already be paid", resolves it as paid.
    app.resolve_attempt(
        &report.doubtful[0].obligation,
        attempt_id,
        paykit_sdk::Resolution::Done,
        UserConfirmed::new(),
    )
    .await
    .unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert_eq!(
        app.request(&request).await.unwrap().status,
        RequestStatus::Paid
    );
}
