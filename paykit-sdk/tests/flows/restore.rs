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

/// A backup of a wallet with one payment in flight, as JSON, to damage, and the wallet's
/// App ID.
async fn backup_with_a_payment(w: &World) -> (serde_json::Value, [u8; 16]) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.labelled_shop("shop1", "shop1").await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.rail.next(Behaviour::Hang);
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    (serde_json::from_slice(&bytes).unwrap(), wallet.app_id())
}

/// Paykit's layer of a backup: its local state, each value as Paykit keeps it.
fn layer_of(backup: &serde_json::Value) -> Vec<(Vec<u8>, serde_json::Value)> {
    let data = backup["protocol"]["data"].as_str().unwrap();
    let bytes = pubky_switch_core::enc::unb64(data).unwrap();
    pubky_switch_client::app::decode_local_state(&bytes)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k, serde_json::from_slice(&v).unwrap()))
        .collect()
}

/// `backup` with Paykit's layer replaced by `state`.
fn with_layer(
    backup: &serde_json::Value,
    state: Vec<(Vec<u8>, serde_json::Value)>,
) -> serde_json::Value {
    let state: Vec<(Vec<u8>, Vec<u8>)> = state
        .into_iter()
        .map(|(k, v)| (k, serde_json::to_vec(&v).unwrap()))
        .collect();
    let mut out = backup.clone();
    out["protocol"]["data"] = serde_json::Value::String(pubky_switch_core::enc::b64(
        &pubky_switch_client::app::encode_local_state(&state),
    ));
    out
}

/// Restore from `backup` on `store`: the error, or a panic if it was taken.
async fn refused_restore(
    w: &World,
    backup: serde_json::Value,
    store: Arc<dyn pubky_switch_client::store::StateStore>,
) -> paykit_sdk::PaykitError {
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    match restored {
        Ok(_) => panic!("a refused backup was restored"),
        Err(error) => error,
    }
}

/// Nothing of the app is in `store`: it doesn't load.
async fn nothing_restored(
    w: &World,
    store: Arc<dyn pubky_switch_client::store::StateStore>,
    app_id: [u8; 16],
) {
    let loaded = Paykit::load(
        w.env(&w.alice, store),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(loaded.is_err(), "the app's state was written");
}

fn is_refused(error: &paykit_sdk::PaykitError) -> bool {
    matches!(
        error,
        paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::InvalidData(_))
    )
}

#[tokio::test]
async fn test_restore_corrupt_journal_refused_and_nothing_written() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let layer = layer_of(&good);
    let journal = layer
        .iter()
        .position(|(k, _)| k.starts_with(b"j/"))
        .expect("the layer holds the journal");
    assert_eq!(layer[journal].1["d"]["label"], "shop1");
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
        let mut state = layer.clone();
        let entry = &mut state[journal].1["d"];
        match member {
            "source" | "list_version" => entry["destination"][member] = value,
            _ => entry[member] = value,
        }
        let target = store();
        let error = refused_restore(&w, with_layer(&good, state), Arc::clone(&target)).await;
        assert!(is_refused(&error), "{member}: {error:?}");
        // The journal is checked before anything is written: neither layer is.
        nothing_restored(&w, target, app_id).await;
    }
}

#[tokio::test]
async fn test_restore_two_open_entries_for_one_obligation_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut state = layer_of(&good);
    let (_, entry) = state
        .iter()
        .find(|(k, _)| k.starts_with(b"j/"))
        .cloned()
        .unwrap();
    let other = paykit_lib::AttemptId::generate().unwrap().to_text();
    let mut copy = entry;
    copy["d"]["attempt_id"] = serde_json::Value::String(other.clone());
    state.push((format!("j/{other}").into_bytes(), copy));
    let target = store();
    let error = refused_restore(&w, with_layer(&good, state), Arc::clone(&target)).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, target, app_id).await;
}

#[tokio::test]
async fn test_restore_another_protocols_layer_refused() {
    let w = World::new();
    let (mut backup, app_id) = backup_with_a_payment(&w).await;
    backup["protocol"]["name"] = serde_json::Value::String("chat".into());
    let target = store();
    let error = refused_restore(&w, backup, Arc::clone(&target)).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, target, app_id).await;
}

#[tokio::test]
async fn test_restore_failed_commit_leaves_neither_layer() {
    use pubky_switch_client::store::{Fault, FaultStore};
    let w = World::new();
    let (backup, app_id) = backup_with_a_payment(&w).await;
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    // A commit fails at each point of the restore in turn: either the restore fails and
    // neither layer is in the store, or it succeeds with both, never the client's state
    // without the journal.
    for n in 1..=4 {
        let base = store();
        let faulty = Arc::new(FaultStore::new(Arc::clone(&base), Fault::FailBefore(n)));
        let restored = Paykit::restore(
            w.env(
                &w.alice,
                faulty as Arc<dyn pubky_switch_client::store::StateStore>,
            ),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await;
        let failed = restored.is_err();
        drop(restored);
        let loaded = Paykit::load(
            w.env(&w.alice, base),
            PaykitConfig::default(),
            app_id,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await;
        match loaded {
            Err(_) => assert!(failed, "commit {n}: a restore that succeeded didn't load"),
            Ok(app) => assert_eq!(
                app.payments().await.unwrap().len(),
                1,
                "commit {n}: the app's state was written without its journal"
            ),
        }
    }
}

#[tokio::test]
async fn test_restore_installs_both_layers_loadable() {
    let w = World::new();
    let (backup, app_id) = backup_with_a_payment(&w).await;
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    let base = store();
    let app = Paykit::restore(
        w.env(&w.alice, Arc::clone(&base)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    drop(app);
    // Both layers are in the store: the app loads, with its journal, marked restored.
    let loaded = Paykit::load(
        w.env(&w.alice, base),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let payments = loaded.payments().await.unwrap();
    assert_eq!(payments.len(), 1);
    assert!(payments[0].restored);
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

#[tokio::test]
async fn test_restore_journal_entry_naming_another_obligation_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut state = layer_of(&good);
    let journal = state
        .iter()
        .position(|(k, _)| k.starts_with(b"j/"))
        .unwrap();
    // The entry's claim key names another obligation than its request's (Paykit spec 7.1).
    state[journal].1["d"]["obligation"] =
        serde_json::Value::String(pubky_switch_core::enc::b64(&[7; 32]));
    let target = store();
    let backup =
        PaykitBackup::from_bytes(&serde_json::to_vec(&with_layer(&good, state)).unwrap()).unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, Arc::clone(&target)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(
        restored.is_err(),
        "a journal naming another obligation was restored"
    );
    // Nor is the entry used afterwards: the journal refuses to read.
    let loaded = Paykit::load(
        w.env(&w.alice, target),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    if let Ok(app) = loaded {
        assert!(app.payments().await.is_err());
    }
}

// ---------------------------------------------------------------------------------------
// Rebuilding a list destination from its single-use marks

/// Bob's app `app` sends alice's wallet a list with a single-use on-chain endpoint and a
/// reusable Lightning offer.
fn list_from(w: &World, app: &str, wallet: [u8; 16], version: u64) {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-bitcoin-p2wpkh"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(&format!("bc1q{app}")).unwrap(),
            single_use: true,
        },
    );
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt12"),
        paykit_lib::bodies::ListEndpoint {
            payload: paykit_lib::PaymentEndpointPayload::new(&format!("lno1{app}")).unwrap(),
            single_use: false,
        },
    );
    let body = paykit_lib::bodies::PrivatePaymentList {
        version,
        payment_endpoints: endpoints,
        expires_at: w.clock.now_ms() + 30 * 24 * crate::common::HOUR_MS,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        app,
        &w.alice,
        &[wallet],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
}

/// Rewrite, through `wallet`, its `payment_lists` record of bob's app `app_id`, its
/// single-use marks replaced by `used`: what a restore finds there.
async fn rewrite_marks(wallet: &mut Paykit, w: &World, app_id: [u8; 16], used: serde_json::Value) {
    use paykit_lib::collections::Collection;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            Collection::PaymentLists.name(),
            Collection::PaymentLists.policy(None),
        ),
        &paykit_lib::derive::natural_key::payment_list(&w.bob_id(), &app_id),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    value["used"] = used;
    let bytes = serde_json::to_vec(&value).unwrap();
    let entry = wallet
        .app_mut()
        .record_op(&target, Some(&bytes), Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::new().entry(entry))
        .await
        .unwrap();
}

fn mark(version: u64, endpoint: &str, attempt_id: paykit_lib::AttemptId) -> serde_json::Value {
    serde_json::json!({ "version": version, "endpoint": endpoint, "attempt_id": attempt_id.to_text() })
}

/// A wallet with lists from bob's `shop` and `other`, a backup taken before paying
/// `destination`'s request, which is paid after it through the shop's list endpoint `paid`,
/// and the marks then rewritten by `marks`. Returns the restored app's report and the
/// request.
async fn reconcile_with_marks(
    destination: impl Fn([u8; 16]) -> paykit_lib::bodies::RequestDestination,
    accepted: Option<Vec<&str>>,
    paid: &str,
    marks: impl Fn([u8; 16], [u8; 16], u64, paykit_lib::AttemptId) -> Vec<([u8; 16], serde_json::Value)>,
) -> (World, paykit_sdk::ReconcileReport, paykit_sdk::RequestRef) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let other = w.labelled_shop("other", "other").await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let version = w.clock.now_ms();
    list_from(&w, "shop", wallet.app_id(), version);
    list_from(&w, "other", wallet.app_id(), version);
    let mut any = draft("1", w.clock.now_ms());
    any.destination = destination(shop.app_id());
    any.accepted_endpoint_identifiers =
        accepted.map(|ids| ids.into_iter().map(crate::common::endpoint).collect());
    let request = request_from(&mut shop, w.alice_id(), any).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id } = wallet
        .pay_request(
            &request,
            PayOptions {
                endpoint: Some(crate::common::endpoint(paid)),
                list_app: Some(shop.app_id()),
                ..PayOptions::default()
            },
        )
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    w.rail.settle(&attempt_id, true);
    for (app_id, used) in marks(shop.app_id(), other.app_id(), version, attempt_id) {
        rewrite_marks(&mut wallet, &w, app_id, used).await;
    }
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    (w, report, request)
}

fn stayed_doubtful(report: &paykit_sdk::ReconcileReport) {
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert!(report.resolved.is_empty(), "{report:?}");
}

#[tokio::test]
async fn test_restore_mark_in_another_app_than_the_required_one_stays_doubtful() {
    // The request requires the shop's list; the only mark naming the attempt is in another
    // app's: the destination can't be told (Paykit wire 4.5).
    let (_w, report, _) = reconcile_with_marks(
        paykit_lib::bodies::RequestDestination::RequiredApp,
        None,
        "btc-bitcoin-p2wpkh",
        |shop, other, version, attempt| {
            vec![
                (shop, serde_json::json!([])),
                (
                    other,
                    serde_json::json!([mark(version, "btc-bitcoin-p2wpkh", attempt)]),
                ),
            ]
        },
    )
    .await;
    stayed_doubtful(&report);
}

#[tokio::test]
async fn test_restore_mark_of_an_identifier_the_request_refuses_stays_doubtful() {
    // The request accepts Lightning only, and is paid through the reusable offer, which
    // leaves no mark; a mark naming the attempt names an on-chain endpoint it refuses.
    let (_w, report, _) = reconcile_with_marks(
        |_| paykit_lib::bodies::RequestDestination::Any,
        Some(vec!["btc-lightning-bolt12"]),
        "btc-lightning-bolt12",
        |shop, _, version, attempt| {
            vec![(
                shop,
                serde_json::json!([mark(version, "btc-bitcoin-p2wpkh", attempt)]),
            )]
        },
    )
    .await;
    stayed_doubtful(&report);
}

#[tokio::test]
async fn test_restore_several_marks_for_one_attempt_stay_doubtful() {
    // Two lists both mark an endpoint used by the attempt: which was paid can't be told.
    let (_w, report, _) = reconcile_with_marks(
        |_| paykit_lib::bodies::RequestDestination::Any,
        None,
        "btc-bitcoin-p2wpkh",
        |_, other, version, attempt| {
            vec![(
                other,
                serde_json::json!([mark(version, "btc-bitcoin-p2wpkh", attempt)]),
            )]
        },
    )
    .await;
    stayed_doubtful(&report);
}

#[tokio::test]
async fn test_restore_one_allowed_mark_rebuilds_the_payment() {
    let (_w, report, _) = reconcile_with_marks(
        paykit_lib::bodies::RequestDestination::RequiredApp,
        Some(vec!["btc-bitcoin-p2wpkh"]),
        "btc-bitcoin-p2wpkh",
        |_, _, _, _| Vec::new(),
    )
    .await;
    assert!(report.doubtful.is_empty(), "{report:?}");
    assert!(
        matches!(report.resolved.as_slice(), [PayOutcome::Paid { .. }]),
        "{report:?}"
    );
}
