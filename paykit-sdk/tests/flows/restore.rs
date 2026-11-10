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
        w.timer(),
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

/// Whether the payer's request record of `request` holds a proof (Paykit wire 5.3).
async fn record_holds_proof(app: &Paykit, request: &paykit_sdk::RequestRef) -> bool {
    let key = app.request(request).await.unwrap().record;
    let held = app.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    !value["proof"].is_null()
}

/// A payment of `request` made after `backup`, paid by the rail while its outcome was never
/// written: the device is lost, and the app restored from the backup reconciles.
async fn paid_after_the_backup(
    w: &World,
    wallet: &mut Paykit,
    request: &paykit_sdk::RequestRef,
    options: PayOptions,
) -> (paykit_lib::AttemptId, PaykitBackup) {
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet.pay_request(request, options).await.unwrap()
    else {
        panic!("pending");
    };
    w.rail.settle(&attempt_id, true);
    (attempt_id, backup)
}

#[tokio::test]
async fn test_restore_paid_attempt_after_the_backup_writes_and_sends_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let (attempt_id, backup) =
        paid_after_the_backup(&w, &mut wallet, &request, PayOptions::default()).await;
    drop(wallet);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(
        matches!(
            report.resolved.as_slice(),
            [PayOutcome::Paid {
                proof_msg_id: Some(_),
                ..
            }]
        ),
        "{report:?}"
    );
    // The payer's record holds the proof, which stops a second payment once the claim is
    // gone (Paykit spec 7.2), and the payee gets it and verifies it.
    assert!(record_holds_proof(&app, &request).await);
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].status, paykit_sdk::PayeeStatus::ProofReceived);
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_restore_paid_attempt_to_a_single_use_list_endpoint_sends_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    let mut any = draft("1", w.clock.now_ms());
    any.destination = paykit_lib::bodies::RequestDestination::Any;
    let request = request_from(&mut shop, w.alice_id(), any).await;
    wallet.receive(None).await.unwrap();
    // Its single-use mark names the attempt: the destination paid is known.
    let options = PayOptions {
        endpoint: Some(crate::common::endpoint("btc-bitcoin-p2wpkh")),
        ..PayOptions::default()
    };
    let (attempt_id, backup) = paid_after_the_backup(&w, &mut wallet, &request, options).await;
    drop(wallet);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(record_holds_proof(&app, &request).await);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_restore_paid_attempt_to_an_unknown_destination_stays_doubtful() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // The request offers two endpoints: the records can't tell which one was paid.
    let mut two = draft("1", w.clock.now_ms());
    if let paykit_lib::bodies::RequestDestination::Endpoints(endpoints) = &mut two.destination {
        endpoints.insert(
            crate::common::endpoint("btc-bitcoin-p2wpkh"),
            paykit_lib::PaymentEndpointPayload::new("bc1qrequest").unwrap(),
        );
    }
    let request = request_from(&mut shop, w.alice_id(), two).await;
    wallet.receive(None).await.unwrap();
    let (attempt_id, backup) =
        paid_after_the_backup(&w, &mut wallet, &request, PayOptions::default()).await;
    drop(wallet);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    // No proof can be written for it, so it isn't settled without one: it stays "may be
    // paid", for the user or a receipt.
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert_eq!(report.doubtful[0].attempts, vec![attempt_id]);
    assert!(!record_holds_proof(&app, &request).await);
    let claim = app
        .app_mut()
        .read_claim(&report.doubtful[0].obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.status,
        pubky_switch_core::claims::ClaimStatus::Attempting
    );
}

/// A backup of a wallet with one payment in flight, as JSON, to damage.
async fn backup_with_a_payment(w: &World) -> serde_json::Value {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.labelled_shop("shop1", "shop1").await;
    let labelled = draft("1", w.clock.now_ms());
    let request = request_from(&mut shop, w.alice_id(), labelled).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Restore from `backup`, damaged: the error, or a panic if it was taken.
async fn refused_restore(w: &World, backup: serde_json::Value) -> paykit_sdk::PaykitError {
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    match restored {
        Ok(_) => panic!("a damaged journal was restored"),
        Err(error) => error,
    }
}

fn is_corrupt(error: &paykit_sdk::PaykitError) -> bool {
    matches!(
        error,
        paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(
            pubky_switch_client::error::StoreError::Corrupt
        ))
    )
}

#[tokio::test]
async fn test_restore_corrupt_journal_refused() {
    let w = World::new();
    let good = backup_with_a_payment(&w).await;
    assert_eq!(good["journal"][0]["label"], "shop1");
    // Each damage reads as JSON of the right shape, and is refused for its meaning: a label
    // that isn't one, a destination source that isn't one of the three, a list version on a
    // request's own endpoint, and a payment reference dropped from a request's payment.
    let damages: [(&str, serde_json::Value); 4] = [
        ("label", serde_json::json!("Not A Label!")),
        ("source", serde_json::json!("elsewhere")),
        ("list_version", serde_json::json!(7)),
        ("payment_reference", serde_json::Value::Null),
    ];
    for (member, value) in damages {
        let mut damaged = good.clone();
        let entry = &mut damaged["journal"][0];
        match member {
            "source" | "list_version" => entry["destination"][member] = value,
            _ => entry[member] = value,
        }
        let error = refused_restore(&w, damaged).await;
        assert!(is_corrupt(&error), "{member}: {error:?}");
    }
}

#[tokio::test]
async fn test_restore_duplicate_journal_entry_refused() {
    let w = World::new();
    let mut backup = backup_with_a_payment(&w).await;
    let entry = backup["journal"][0].clone();
    backup["journal"].as_array_mut().unwrap().push(entry);
    let error = refused_restore(&w, backup).await;
    assert!(is_corrupt(&error), "{error:?}");
}

#[tokio::test]
async fn test_restore_failure_rebuilt_that_completed_after_all_sends_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    // Paid after the backup; the method reports a failure, and the device is lost before
    // the outcome is written.
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    w.rail.settle(&attempt_id, false);
    drop(wallet);

    // The restored app rebuilds the payment from its records and writes it `failed`.
    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Failed { .. }]),
        "{report:?}"
    );
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    // The payment completed after all: the owner finds it and sends its proof (Paykit spec
    // 7.2).
    w.rail.complete_after_failure(&attempt_id);
    let resumed = app.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [PayOutcome::Paid {
                proof_msg_id: Some(_),
                ..
            }]
        ),
        "{resumed:?}"
    );
    assert!(record_holds_proof(&app, &request).await);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales[0].attempt_id, Some(attempt_id));
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
}
