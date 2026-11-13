//! Restore and reconcile (Paykit spec 7.2, 7.3; Switch spec 12.4): a restored app makes no
//! attempt until it has reconciled, resolves its open attempts from the payment method's
//! history, and never pays twice.

use std::sync::Arc;

use paykit_sdk::rail::Behaviour;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, Paykit, PaykitBackup, PaykitConfig, Rail, Refusal,
    RequestStatus,
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
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
    let PayOutcome::Pending { attempt_id, .. } =
        wallet.pay_request(request, options).await.unwrap()
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
async fn test_restore_paid_attempt_whose_outcome_write_fails_is_written_at_a_resume() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let (attempt_id, backup) =
        paid_after_the_backup(&w, &mut wallet, &request, PayOptions::default()).await;
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    // The rebuilt payment's outcome batch can't connect.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let report = app.reconcile().await.unwrap();
    assert!(
        matches!(
            report.resolved.as_slice(),
            [PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{report:?}"
    );
    // The journal keeps it paid, with its proof, and a resume writes the outcome.
    let payments = app.payments().await.unwrap();
    assert_eq!(payments.len(), 1);
    assert_eq!(payments[0].stage, paykit_sdk::Stage::RailPaid);
    app.resume_payments().await.unwrap();
    assert_eq!(
        app.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    assert!(record_holds_proof(&app, &request).await);
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
    let options =
        PayOptions::default().with_endpoint(crate::common::endpoint("btc-bitcoin-p2wpkh"));
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

/// `good`'s layer with a notice that a message from bob was given up, `notice` its value.
fn with_given_up_notice(
    w: &World,
    good: &serde_json::Value,
    notice: serde_json::Value,
) -> serde_json::Value {
    let mut state = layer_of(good);
    let key = format!(
        "nt/{}/123e4567-e89b-42d3-a456-426614174000",
        pubky_switch_core::enc::b64(&w.bob_id())
    );
    state.push((key.into_bytes(), serde_json::json!({ "v": 1, "d": notice })));
    with_layer(good, state)
}

/// A kept notice that a message from bob was given up, its stored entry at local ID 4.
fn given_up_notice(w: &World) -> serde_json::Value {
    serde_json::json!({
        "kind": "message_given_up",
        "attempt_id": null,
        "msg_id": "123e4567-e89b-42d3-a456-426614174000",
        "counterparty": pubky_switch_core::enc::b64(&w.bob_id()),
        "request_hash": null,
        "raised_at_ms": 3,
        "acknowledged": false,
        "stored": { "stored_id": 4, "hash": pubky_switch_core::enc::b64(&[5; 32]) }
    })
}

#[tokio::test]
async fn test_restore_given_up_notice_with_its_entry_restored() {
    let w = World::new();
    let (good, _) = backup_with_a_payment(&w).await;
    let backup = with_given_up_notice(&w, &good, given_up_notice(&w));
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.notices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_restore_given_up_notice_without_its_entry_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut notice = given_up_notice(&w);
    notice.as_object_mut().unwrap().remove("stored");
    let target = store();
    let error = refused_restore(
        &w,
        with_given_up_notice(&w, &good, notice),
        Arc::clone(&target),
    )
    .await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, target, app_id).await;
}

#[tokio::test]
async fn test_restore_given_up_notice_acknowledged_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut notice = given_up_notice(&w);
    notice["acknowledged"] = serde_json::json!(true);
    let target = store();
    let error = refused_restore(
        &w,
        with_given_up_notice(&w, &good, notice),
        Arc::clone(&target),
    )
    .await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, target, app_id).await;
}

#[tokio::test]
async fn test_restore_acknowledged_notice_without_its_payment_refused() {
    // An acknowledged notice is kept only while the journal holds its payment.
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut state = layer_of(&good);
    let attempt = paykit_lib::AttemptId::from_bytes([9; 16]).to_text();
    let notice = serde_json::json!({
        "kind": "paid_after_failed",
        "attempt_id": attempt,
        "msg_id": null,
        "counterparty": pubky_switch_core::enc::b64(&w.bob_id()),
        "request_hash": null,
        "raised_at_ms": 3,
        "acknowledged": true
    });
    state.push((
        format!("nt/{attempt}/paid_after_failed").into_bytes(),
        serde_json::json!({ "v": 1, "d": notice }),
    ));
    let target = store();
    let error = refused_restore(&w, with_layer(&good, state), Arc::clone(&target)).await;
    assert!(is_refused(&error), "{error:?}");
    nothing_restored(&w, target, app_id).await;
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

/// `backup` with its certificate, and the app record carrying it, replaced by a new one of
/// the wallet's keys granting `scopes`, as the Switch takes it.
fn with_certificate(
    w: &World,
    backup: &serde_json::Value,
    scopes: &paykit_sdk::RequestedScopes,
) -> (serde_json::Value, pubky_switch_core::records::AppCert) {
    use pubky_switch_client::testing::{issue_cert, AppSpec};
    use pubky_switch_core::records::{AppRecord, AppRecordBody};
    let keys = w.alice.identity().app_keys("wallet");
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    let issued_at = w.clock.now_ms() - 1;
    let cert = issue_cert(
        w.alice.identity().key(),
        &keys,
        &AppSpec::new(&names).with_permissions(&scopes.permissions),
        issued_at,
    )
    .unwrap();
    let record = AppRecord::sign(&keys, AppRecordBody::new(&cert, vec![], issued_at)).unwrap();
    let mut out = backup.clone();
    out["cert"] = serde_json::to_value(cert.signed()).unwrap();
    out["record"] = serde_json::to_value(record.signed()).unwrap();
    (out, cert)
}

#[tokio::test]
async fn test_restore_certificate_paykit_refuses_writes_nothing() {
    use pubky_switch_client::store::{Fault, FaultStore, StateStore};
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    // `pay` with a switch permission its scopes don't derive: the Switch takes the
    // certificate, and Paykit refuses it (Paykit wire 3.1).
    let mut scopes = AppScopes::new().pay().build().unwrap();
    let extra = pubky_switch_core::scopes::Permission::ALL
        .into_iter()
        .find(|p| !scopes.permissions.contains(p))
        .unwrap();
    scopes.permissions.push(extra);
    // Each once, in the Switch's fixed order (Switch wire 3.1).
    let all = pubky_switch_core::scopes::Permission::ALL;
    scopes
        .permissions
        .sort_by_key(|p| all.iter().position(|q| q == p));
    let (refused, cert) = with_certificate(&w, &good, &scopes);
    assert!(paykit_lib::scopes::check_certificate(&cert).is_err());
    let target = store();
    let fault = Arc::new(FaultStore::new(Arc::clone(&target), Fault::None));
    let error = refused_restore(&w, refused, Arc::clone(&fault) as Arc<dyn StateStore>).await;
    assert!(
        matches!(
            &error,
            paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::InvalidData(why))
                if why.contains("the backup's certificate")
        ),
        "{error:?}"
    );
    // Refused before the restore's commit: nothing is written.
    assert_eq!(fault.commits(), 0);
    nothing_restored(&w, target, app_id).await;
    // The same backup with a certificate Paykit takes restores: the refusal is Paykit's.
    let (taken, _) = with_certificate(&w, &good, &AppScopes::new().pay().build().unwrap());
    let taken = PaykitBackup::from_bytes(&serde_json::to_vec(&taken).unwrap()).unwrap();
    restored(&w, &taken).await;
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
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
    // Refused before the restore's commit: nothing of either layer is in the store.
    nothing_restored(&w, target, app_id).await;
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
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(
            &request,
            PayOptions::default()
                .with_endpoint(crate::common::endpoint(paid))
                .with_list_app(shop.app_id()),
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

#[tokio::test]
async fn test_restore_unrebuildable_failure_stays_doubtful_then_paid_is_not_lost() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // The request offers two endpoints: the records can't tell which one was paid, so no
    // journal entry can be rebuilt for the attempt.
    let mut two = draft("1", w.clock.now_ms());
    if let paykit_lib::bodies::RequestDestination::Endpoints(endpoints) = &mut two.destination {
        endpoints.insert(
            crate::common::endpoint("btc-bitcoin-p2wpkh"),
            paykit_lib::PaymentEndpointPayload::new("bc1qrequest").unwrap(),
        );
    }
    let request = request_from(&mut shop, w.alice_id(), two).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(Behaviour::Hang);
    let PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("pending");
    };
    // The method reports a failure, and the device is lost.
    w.rail.settle(&attempt_id, false);
    drop(wallet);

    let mut app = restored(&w, &backup).await;
    let report = app.reconcile().await.unwrap();
    // Written `failed`, the claim would be free for a second payment while a late success
    // could never be reported: it stays doubtful (Paykit spec 7.2).
    assert_eq!(report.doubtful.len(), 1, "{report:?}");
    assert_eq!(report.doubtful[0].attempts, vec![attempt_id]);
    let obligation = report.doubtful[0].obligation;
    // The payment completes after all: the attempt still blocks a second payment.
    w.rail.complete_after_failure(&attempt_id);
    let again = app.reconcile().await.unwrap();
    assert_eq!(again.doubtful.len(), 1, "{again:?}");
    let claim = app
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.view.status,
        pubky_switch_core::claims::ClaimStatus::Attempting
    );
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert!(app
        .pay_request(&request, PayOptions::default())
        .await
        .is_err());
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// A message bob's app `app` signs for `to`, of `kind` with `body`, kept as a journal entry
/// keeps a request, with the certificate of bob's app `cert_app`.
pub(crate) fn kept_message(
    w: &World,
    app: &str,
    cert_app: &str,
    to: &pubky_switch_client::testing::TestSigner,
    kind: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    kept_message_with_refs(w, app, cert_app, to, kind, Vec::new(), body)
}

/// [`kept_message`], naming `refs`.
pub(crate) fn kept_message_with_refs(
    w: &World,
    app: &str,
    cert_app: &str,
    to: &pubky_switch_client::testing::TestSigner,
    kind: &str,
    refs: Vec<[u8; 32]>,
    body: serde_json::Value,
) -> serde_json::Value {
    let plaintext = w.sign_as(
        &w.bob,
        app,
        to,
        kind,
        refs,
        body.as_object().cloned().unwrap(),
    );
    let keys = w.bob.identity().app_keys(cert_app);
    let cert = w.bob.switch().app(&keys.app_id_bytes()).cert.unwrap();
    serde_json::json!({
        "sender": pubky_switch_core::enc::b64(&w.bob_id()),
        "cert": serde_json::to_value(cert.signed()).unwrap(),
        "plaintext": pubky_switch_core::enc::b64(&plaintext.encode()),
        "accepted_at": w.clock.now_ms(),
    })
}

fn another_request_body() -> serde_json::Value {
    serde_json::json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-2",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    })
}

/// `backup` refused onto an empty store, with no commit and nothing to load.
async fn refused_writing_nothing(
    w: &World,
    backup: serde_json::Value,
    app_id: [u8; 16],
    what: &str,
) {
    use pubky_switch_client::store::{Fault, FaultStore, StateStore};
    let target = store();
    let fault = Arc::new(FaultStore::new(Arc::clone(&target), Fault::None));
    let error = refused_restore(w, backup, Arc::clone(&fault) as Arc<dyn StateStore>).await;
    assert!(is_refused(&error), "{what}: {error:?}");
    assert_eq!(fault.commits(), 0, "{what}");
    nothing_restored(w, target, app_id).await;
}

#[tokio::test]
async fn test_restore_kept_request_that_doesnt_verify_writes_nothing() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    // Bob's plain shop, whose certificate another signature claims below.
    let _shop = w.shop().await;
    let layer = layer_of(&good);
    let journal = layer
        .iter()
        .position(|(k, _)| k.starts_with(b"j/"))
        .unwrap();
    let kept = layer[journal].1["d"]["request"].clone();
    let own_cert = good["cert"].clone();
    let another = |kind: &str, to: &pubky_switch_client::testing::TestSigner, cert_app: &str| {
        kept_message(&w, "shop1", cert_app, to, kind, another_request_body())
    };
    let mut sender = kept.clone();
    sender["sender"] = serde_json::Value::String(pubky_switch_core::enc::b64(&w.alice_id()));
    let mut certificate = kept.clone();
    certificate["cert"] = own_cert;
    // Each reads, and fails one check a use of the request would make: another request
    // than the one paid (its hash), one addressed to bob, a message of another kind, a
    // signature by another key than the certificate's, a sender the certificate isn't
    // for, and a certificate not the sender's.
    let rejection = kept_message(
        &w,
        "shop1",
        "shop1",
        &w.alice,
        "paykit.payment_request_rejection",
        serde_json::json!({ "reason": null, "label": "shop1" }),
    );
    let cases: [(&str, serde_json::Value); 6] = [
        (
            "another request",
            another("paykit.payment_request", &w.alice, "shop1"),
        ),
        (
            "addressed to bob",
            another("paykit.payment_request", &w.bob, "shop1"),
        ),
        ("another kind", rejection),
        (
            "another key",
            another("paykit.payment_request", &w.alice, "shop"),
        ),
        ("sender", sender),
        ("certificate", certificate),
    ];
    for (what, request) in cases {
        let mut state = layer.clone();
        state[journal].1["d"]["request"] = request;
        refused_writing_nothing(&w, with_layer(&good, state), app_id, what).await;
    }
    // The entry's own members against the request it keeps, which still verifies: another
    // amount, payment reference or label; a carrier addressed to bob; and an acceptance
    // that isn't one of the request, a message of another kind signed by bob.
    let to_bob = kept_message(
        &w,
        "shop1",
        "shop1",
        &w.bob,
        "paykit.payment_request",
        another_request_body(),
    );
    let members: [(&str, &str, serde_json::Value); 4] = [
        (
            "amount",
            "amount",
            serde_json::json!({ "value": "2", "asset": "btc" }),
        ),
        (
            "payment reference",
            "payment_reference",
            serde_json::json!("order-other"),
        ),
        ("label", "label", serde_json::json!("shop2")),
        ("carrier", "carrier", to_bob),
    ];
    for (what, member, value) in members {
        let mut state = layer.clone();
        let entry = &mut state[journal].1["d"];
        assert!(entry.get(member).is_some(), "{what}: no {member}");
        entry[member] = value;
        refused_writing_nothing(&w, with_layer(&good, state), app_id, what).await;
    }
    // An acceptance the wallet signed, with its `msg_id` kept, but of another kind: the
    // wallet's rejection of the request.
    let request_hash: [u8; 32] = pubky_switch_core::enc::unb64_array(
        layer[journal].1["d"]["request_hash"].as_str().unwrap(),
    )
    .unwrap();
    let wallet = w.alice.identity().app_keys("wallet");
    let rejection = pubky_switch_core::message::Message::new(
        "paykit.payment_request_rejection",
        pubky_switch_core::message::Sender::of_app(w.alice_id(), &wallet),
        w.bob_id(),
        w.clock.now_ms(),
        vec![request_hash],
        serde_json::json!({ "reason": null, "label": "shop1" })
            .as_object()
            .cloned()
            .unwrap(),
    )
    .unwrap();
    let mut state = layer.clone();
    let entry = &mut state[journal].1["d"];
    assert!(entry.get("acceptance_plaintext").is_some());
    entry["acceptance_plaintext"] = serde_json::Value::String(pubky_switch_core::enc::b64(
        &rejection.sign(&wallet).unwrap().encode(),
    ));
    entry["acceptance_msg_id"] = serde_json::Value::String(rejection.msg_id.clone());
    refused_writing_nothing(&w, with_layer(&good, state), app_id, "acceptance").await;
}

/// `value` as Paykit's local state keeps it.
pub(crate) fn kept(value: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "v": 1, "d": value })
}

#[tokio::test]
async fn test_restore_layer_state_that_doesnt_verify_or_read_writes_nothing() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let layer = layer_of(&good);
    let entry = layer
        .iter()
        .find(|(k, _)| k.starts_with(b"j/"))
        .map(|(_, v)| v["d"].clone())
        .unwrap();
    let request = entry["request"].clone();
    let request_hash: [u8; 32] =
        pubky_switch_core::enc::unb64_array(entry["request_hash"].as_str().unwrap()).unwrap();
    let key_of = |prefix: &str| format!("{prefix}{}", pubky_switch_core::enc::b64(&[7u8; 32]));
    // Of the right kind, verified, but kept under a key that isn't its record's: a payee's
    // cancellation and a payee's list.
    let cancellation = kept_message_with_refs(
        &w,
        "shop1",
        "shop1",
        &w.alice,
        "paykit.payment_request_cancellation",
        vec![request_hash],
        serde_json::json!({ "reason": null }),
    );
    let list = kept_message(
        &w,
        "shop1",
        "shop1",
        &w.alice,
        "paykit.private_payment_list",
        serde_json::Value::Object(
            paykit_lib::bodies::PrivatePaymentList {
                version: 1,
                payment_endpoints: std::collections::BTreeMap::new(),
                expires_at: w.clock.now_ms() + 86_400_000,
            }
            .to_body()
            .unwrap(),
        ),
    );
    let uuid = "123e4567-e89b-42d3-a456-426614174000";
    let other_uuid = "223e4567-e89b-42d3-a456-426614174000";
    let answer_key = format!("ans/00000000000000000001/{uuid}");
    let answer = |cert: serde_json::Value, body: serde_json::Value, msg_id: &str| {
        serde_json::json!({
            "seq": 1,
            "msg_id": msg_id,
            "requester": pubky_switch_core::enc::b64(&w.alice_id()),
            "cert": cert,
            "body": body,
        })
    };
    let good_body = serde_json::json!({ "msg_ids": [uuid] });
    let own_cert = good["cert"].clone();
    // A payee app's records seen: a certificate, not a record, and a record of bob's
    // under the key of another of his apps.
    let shop1 = w.bob.identity().app_keys("shop1");
    let shop1_cert = w.bob.switch().app(&shop1.app_id_bytes()).cert.unwrap();
    let record = pubky_switch_core::records::AppRecord::sign(
        &shop1,
        pubky_switch_core::records::AppRecordBody::new(&shop1_cert, vec![], w.clock.now_ms()),
    )
    .unwrap();
    let seen_key = |app: [u8; 16]| {
        format!(
            "ar/{}/{}",
            pubky_switch_core::enc::b64(&w.bob_id()),
            pubky_switch_core::enc::b64(&app)
        )
    };
    let record_json = serde_json::to_value(record.signed()).unwrap();
    let list_ask = |payee: [u8; 32]| {
        serde_json::json!({
            "payee": pubky_switch_core::enc::b64(&payee),
            "app_id": null,
            "held_version": null,
            "asked_at_ms": 1,
        })
    };
    let cases: [(&str, String, serde_json::Value); 18] = [
        (
            "kept cancellation of another kind",
            key_of("kc/"),
            kept(request.clone()),
        ),
        (
            "kept cancellation under another key",
            key_of("kc/"),
            kept(cancellation),
        ),
        (
            "held list of another kind",
            key_of("lh/"),
            kept(request.clone()),
        ),
        ("held list under another key", key_of("lh/"), kept(list)),
        (
            "queued answer of another's certificate",
            answer_key.clone(),
            kept(answer(request["cert"].clone(), good_body.clone(), uuid)),
        ),
        (
            "queued answer with a malformed body",
            answer_key.clone(),
            kept(answer(
                own_cert.clone(),
                serde_json::json!({ "bogus": 1 }),
                uuid,
            )),
        ),
        (
            "queued answer under another key",
            answer_key.clone(),
            kept(answer(own_cert.clone(), good_body.clone(), other_uuid)),
        ),
        (
            "malformed key",
            "peer/not-base64".into(),
            kept(serde_json::json!(1)),
        ),
        ("unknown key", "zz/1".into(), kept(serde_json::json!(1))),
        (
            "journal entry under another attempt's key",
            format!("j/{}", paykit_lib::AttemptId::generate().unwrap().to_text()),
            kept(entry.clone()),
        ),
        (
            "queued ask under another sender's key",
            format!("{}/{:020}", key_of("ask/"), 5),
            kept(serde_json::json!({
                "sender": pubky_switch_core::enc::b64(&w.bob_id()),
                "from_ms": 5,
                "until_ms": null,
            })),
        ),
        (
            "list ask under another payee's key",
            key_of("lq/"),
            kept(list_ask(w.bob_id())),
        ),
        (
            "kept side under another message's key",
            format!("side/{other_uuid}"),
            kept(serde_json::json!({ "msg_id": uuid, "side": "payer" })),
        ),
        (
            "missed time other than its key's",
            format!("missed/{:020}", 5),
            kept(serde_json::json!(0)),
        ),
        (
            "seen records holding a certificate",
            seen_key(shop1.app_id_bytes()),
            kept(serde_json::json!({ "records": [request["cert"].clone()] })),
        ),
        (
            "seen records of another app",
            seen_key([9; 16]),
            kept(serde_json::json!({ "records": [record_json.clone()] })),
        ),
        (
            "lists read time",
            "lr".into(),
            kept(serde_json::json!("soon")),
        ),
        (
            "queued ask",
            key_of("ask/"),
            kept(serde_json::json!({ "sender": 1 })),
        ),
    ];
    for (what, key, value) in cases {
        let mut state = layer.clone();
        state.push((key.into_bytes(), value));
        refused_writing_nothing(&w, with_layer(&good, state), app_id, what).await;
    }
    // A record of bob's, under its own app's key, restores.
    let mut state = layer.clone();
    state.push((
        seen_key(shop1.app_id_bytes()).into_bytes(),
        kept(serde_json::json!({ "records": [record_json] })),
    ));
    // A queued answer bound to its key restores too.
    state.push((
        answer_key.into_bytes(),
        kept(answer(own_cert, good_body, uuid)),
    ));
    // So do a list ask and a kept side under their own keys.
    state.push((
        format!("lq/{}", pubky_switch_core::enc::b64(&w.bob_id())).into_bytes(),
        kept(list_ask(w.bob_id())),
    ));
    state.push((
        format!("side/{uuid}").into_bytes(),
        kept(serde_json::json!({ "msg_id": uuid, "side": "payer" })),
    ));
    // A missed item holding the time its key names restores too.
    state.push((
        format!("missed/{:020}", 5).into_bytes(),
        kept(serde_json::json!(5)),
    ));
    let backup = with_layer(&good, state);
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    restored(&w, &backup).await;
}

#[tokio::test]
async fn test_restore_cursor_time_ahead_of_the_backup_is_clamped() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    drop(wallet);
    // The backup's cursor time is a year ahead of anything it verified.
    let mut state = layer_of(&backup);
    let cursor = state
        .iter_mut()
        .find(|(k, _)| k.as_slice() == b"ct")
        .expect("the backup keeps its cursor time");
    cursor.1 = kept(serde_json::json!(
        w.clock.now_ms() + 365 * 24 * crate::common::HOUR_MS
    ));
    let backup = paykit_sdk::PaykitBackup::from_bytes(
        &serde_json::to_vec(&with_layer(&backup, state)).unwrap(),
    )
    .unwrap();
    let app = restored(&w, &backup).await;
    // Clamped to the backup's verified time plus the clock tolerance, and the re-sends start
    // from there, not from the future.
    let bound = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    let read = |bytes: Vec<u8>| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
    let cursor = read(app.app().protocol_get(b"ct").await.unwrap().unwrap());
    assert!(cursor["d"].as_u64().unwrap() <= bound, "{cursor}");
    let hold = read(app.app().protocol_get(b"rh").await.unwrap().unwrap());
    assert!(
        hold["d"]["resend_from_ms"].as_u64().unwrap() <= w.clock.now_ms(),
        "{hold}"
    );
}

/// Bob's shop's record of `request`.
fn payee_record(request: &paykit_sdk::RequestRef) -> pubky_switch_client::app::RecordTarget {
    use paykit_lib::collections::Collection;
    let collection = Collection::PayeeActivity;
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::request_record(&request.payee, &request.request_hash),
    )
}

/// The value of the record at `target`, as `app` reads it.
async fn value_of(
    app: &mut Paykit,
    target: &pubky_switch_client::app::RecordTarget,
) -> serde_json::Value {
    let held = app.app_mut().read_record(target).await.unwrap().unwrap();
    serde_json::from_slice(held.content.value().unwrap()).unwrap()
}

/// How many acceptances `shop` takes at its next receive.
async fn acceptances_taken(shop: &mut Paykit) -> usize {
    let got = shop.receive(None).await.unwrap();
    got.report
        .events
        .iter()
        .filter(|e| {
            matches!(e, pubky_switch_client::app::AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.payment_request_acceptance")
        })
        .count()
}

/// Alice's wallet pays bob's request, which stays in flight, its acceptance released; the
/// wallet's backup is taken then, the device is lost, and a restore reconciles.
/// `delivered` says whether the acceptance reached bob before the backup.
async fn restored_with_a_released_acceptance(
    delivered: bool,
) -> (World, Paykit, Paykit, paykit_sdk::RequestRef) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    if !delivered {
        // Bob's switch doesn't answer the delivery: the acceptance stays in the outbox.
        w.bob
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/inbox".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
    }
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let outcome = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Pending { .. }), "{outcome:?}");
    assert_eq!(
        acceptances_taken(&mut shop).await,
        usize::from(delivered),
        "before the backup"
    );
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored(&w, &backup).await;
    app.reconcile().await.unwrap();
    (w, app, shop, request)
}

#[tokio::test]
async fn test_restore_released_acceptance_never_delivered_is_sent_once() {
    let (w, mut app, mut shop, request) = restored_with_a_released_acceptance(false).await;
    // The restored client didn't know it, and the record holds none: a new one went.
    assert_eq!(acceptances_taken(&mut shop).await, 1);
    let record = value_of(&mut shop, &payee_record(&request)).await;
    assert!(!record["response"].is_null(), "{record}");
    // A second resume finds the new one known to the client and sends nothing.
    app.resume_payments().await.unwrap();
    assert_eq!(acceptances_taken(&mut shop).await, 0);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_restore_released_acceptance_delivered_before_the_backup_changes_only_msg_ids() {
    // The first acceptance reached bob before the backup: his record holds it.
    let (w, mut app, mut shop, request) = restored_with_a_released_acceptance(true).await;
    // The restored client can't tell, so a second acceptance went with a new `msg_id`.
    // Bob keeps his first acceptance and his status; his record notes the second `msg_id`:
    // one record write.
    let before = shop.payee_activity().await.unwrap()[0].status.clone();
    // The record as bob holds it before taking the second acceptance.
    let response_before = value_of(&mut shop, &payee_record(&request)).await;
    assert_eq!(acceptances_taken(&mut shop).await, 1);
    let after = value_of(&mut shop, &payee_record(&request)).await;
    assert_eq!(after["response"], response_before["response"], "{after}");
    let ids = |v: &serde_json::Value| v["msg_ids"].as_array().map_or(0, Vec::len);
    assert_eq!(ids(&after), ids(&response_before) + 1, "{after}");
    assert_eq!(shop.payee_activity().await.unwrap()[0].status, before);
    app.resume_payments().await.unwrap();
    assert_eq!(acceptances_taken(&mut shop).await, 0);
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_restore_records_listed_before_any_call_reads_them() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
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
    // The client's copy is empty until the records are listed: a view of them is not now,
    // rather than empty.
    let refused = app.requests().await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    // A resume lists them first.
    app.resume_payments().await.unwrap();
    assert_eq!(app.requests().await.unwrap().len(), 1);
}

/// Alice's wallet restored bare from `backup`: nothing read or listed yet.
async fn restored_bare(w: &World, backup: &paykit_sdk::PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_restore_bare_then_resume_sends_no_acceptance_the_record_holds() {
    // The backup is taken while the payment is in flight; the lost device then finishes it,
    // the record holding the acceptance and the proof. A bare restore that resumes first
    // lists the records, finds the acceptance placed, and sends no other.
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
        panic!("in flight");
    };
    assert_eq!(acceptances_taken(&mut shop).await, 1);
    let backup = wallet.backup().await.unwrap();
    w.rail.settle(&attempt_id, true);
    wallet.resume_payments().await.unwrap();
    assert!(record_holds_proof(&wallet, &request).await);
    drop(wallet);
    let mut app = restored_bare(&w, &backup).await;
    app.resume_payments().await.unwrap();
    assert_eq!(acceptances_taken(&mut shop).await, 0);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_restore_cursor_time_left_unclamped_by_a_crash_is_clamped_at_load() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let base = store();
    let mut app = Paykit::restore(
        w.env(&w.alice, Arc::clone(&base)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let app_id = app.app_id();
    // As a crash after the restore's commit and before its clamp would leave it: the cursor
    // time and the hold's re-send time a year ahead.
    let ahead = w.clock.now_ms() + 365 * 24 * crate::common::HOUR_MS;
    let hold = app.app().protocol_get(b"rh").await.unwrap().unwrap();
    let mut hold: serde_json::Value = serde_json::from_slice(&hold).unwrap();
    hold["d"]["resend_from_ms"] = serde_json::json!(ahead);
    app.app_mut()
        .protocol_commit(&[
            pubky_switch_client::app::LocalWrite::Put {
                key: b"ct".to_vec(),
                value: serde_json::to_vec(&kept(serde_json::json!(ahead))).unwrap(),
            },
            pubky_switch_client::app::LocalWrite::Put {
                key: b"rh".to_vec(),
                value: serde_json::to_vec(&hold).unwrap(),
            },
        ])
        .await
        .unwrap();
    drop(app);
    let app = w
        .load_on_clock(&w.alice, Arc::clone(&base), app_id, Arc::clone(&w.clock))
        .await;
    let read = |bytes: Vec<u8>| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
    let bound = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    let cursor = read(app.app().protocol_get(b"ct").await.unwrap().unwrap());
    assert!(cursor["d"].as_u64().unwrap() <= bound, "{cursor}");
    let hold = read(app.app().protocol_get(b"rh").await.unwrap().unwrap());
    assert!(
        hold["d"]["resend_from_ms"].as_u64().unwrap() <= w.clock.now_ms(),
        "{hold}"
    );
}

#[tokio::test]
async fn test_restore_records_mark_kept_by_a_stopped_read() {
    // The wallet is removed after its backup. Restored, its reads stop (the client returns
    // before, or without finishing, its listing): the mark stays, and a call that decides
    // from a record lists first and fails rather than read the empty copy as "gone".
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let wallet_id = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let now = w.clock.now_ms();
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != wallet_id);
            body.removed.push(pubky_switch_core::records::RemovedApp {
                app_id: wallet_id,
                distrust_from: now,
            });
        })
        .unwrap();
    w.clock.advance_ms(1000);
    let mut app = restored_bare(&w, &backup).await;
    for _ in 0..2 {
        let read = app.receive(None).await.unwrap();
        assert!(read.report.stopped, "{:?}", read.report);
    }
    assert!(app.app().protocol_get(b"nl").await.unwrap().is_some());
    assert!(app.resume_payments().await.is_err());
    let refused = app.requests().await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_restore_asks_start_bounded_where_they_are_queued() {
    // The hold's re-send start is a year ahead, as a load under a refused certificate,
    // then a renewal, would leave it unclamped: the asks queued still start no later than
    // the verified time.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut app = restored_bare(&w, &backup).await;
    let ahead = w.clock.now_ms() + 365 * 24 * crate::common::HOUR_MS;
    let hold = app.app().protocol_get(b"rh").await.unwrap().unwrap();
    let mut hold: serde_json::Value = serde_json::from_slice(&hold).unwrap();
    hold["d"]["resend_from_ms"] = serde_json::json!(ahead);
    app.app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: b"rh".to_vec(),
            value: serde_json::to_vec(&hold).unwrap(),
        }])
        .await
        .unwrap();
    // Queued, not yet sent: bob's switch doesn't answer.
    for _ in 0..4 {
        w.bob
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/inbox".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
    }
    app.reconcile().await.unwrap();
    let hold = app.app().protocol_get(b"rh").await.unwrap().unwrap();
    let hold: serde_json::Value = serde_json::from_slice(&hold).unwrap();
    assert!(
        hold["d"]["resend_from_ms"].as_u64().unwrap() <= w.clock.now_ms(),
        "{hold}"
    );
}

/// The restore of `backup`, a backup's JSON, on a fresh store: the app, or the refusal.
async fn restored_or_refused(
    w: &World,
    backup: serde_json::Value,
) -> Result<Paykit, paykit_sdk::PaykitError> {
    let backup = PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
}

/// `good` with its journaled payment's kept acceptance certificate replaced by `cert`, or
/// removed with `None`.
fn with_acceptance_cert(
    good: &serde_json::Value,
    cert: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut layer = layer_of(good);
    let entry = layer
        .iter_mut()
        .find(|(k, _)| k.starts_with(b"j/"))
        .map(|(_, v)| v)
        .unwrap();
    assert!(!entry["d"]["acceptance_plaintext"].is_null());
    assert!(!entry["d"]["acceptance_cert"].is_null());
    match cert {
        Some(c) => entry["d"]["acceptance_cert"] = c,
        None => {
            entry["d"]
                .as_object_mut()
                .unwrap()
                .remove("acceptance_cert");
        }
    }
    with_layer(good, layer)
}

#[tokio::test]
async fn test_restore_acceptance_without_a_kept_certificate_read_under_the_current() {
    let w = World::new();
    let (good, _app_id) = backup_with_a_payment(&w).await;
    let r = restored_or_refused(&w, with_acceptance_cert(&good, None)).await;
    assert!(r.is_ok(), "{:?}", r.err());
}

#[tokio::test]
async fn test_restore_acceptance_certificate_of_another_own_app_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let keys = w.alice.identity().app_keys("phone");
    let other = pubky_switch_client::testing::issue_cert(
        w.alice.identity().key(),
        &keys,
        &app_spec(&AppScopes::new().pay().build().unwrap()),
        w.clock.now_ms(),
    )
    .unwrap();
    refused_writing_nothing(
        &w,
        with_acceptance_cert(&good, Some(serde_json::to_value(other.signed()).unwrap())),
        app_id,
        "other own app",
    )
    .await;
}

#[tokio::test]
async fn test_restore_acceptance_certificate_of_another_identity_refused() {
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let keys = w.bob.identity().app_keys("wallet");
    let other = pubky_switch_client::testing::issue_cert(
        w.bob.identity().key(),
        &keys,
        &app_spec(&AppScopes::new().pay().build().unwrap()),
        w.clock.now_ms(),
    )
    .unwrap();
    refused_writing_nothing(
        &w,
        with_acceptance_cert(&good, Some(serde_json::to_value(other.signed()).unwrap())),
        app_id,
        "other identity",
    )
    .await;
}

#[tokio::test]
async fn test_restore_kept_certificate_without_its_acceptance_refused() {
    // A kept certificate with the acceptance plaintext removed: check() refuses.
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let mut layer = layer_of(&good);
    let entry = layer
        .iter_mut()
        .find(|(k, _)| k.starts_with(b"j/"))
        .map(|(_, v)| v)
        .unwrap();
    entry["d"]["acceptance_plaintext"] = serde_json::Value::Null;
    entry["d"]["acceptance_msg_id"] = serde_json::Value::Null;
    refused_writing_nothing(
        &w,
        with_layer(&good, layer),
        app_id,
        "cert without plaintext",
    )
    .await;
}

/// The app spec a certificate for `scopes` is issued with.
fn app_spec(scopes: &paykit_sdk::RequestedScopes) -> pubky_switch_client::testing::AppSpec {
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    pubky_switch_client::testing::AppSpec::new(&names).with_permissions(&scopes.permissions)
}

#[tokio::test]
async fn test_restore_acceptance_under_a_certificate_without_pay_refused() {
    // A certificate holding only `contacts`, with that scope's own permissions, so it
    // verifies and only the check of the sender's scopes can refuse it.
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let keys = w.alice.identity().app_keys("wallet");
    let c = pubky_switch_client::testing::issue_cert(
        w.alice.identity().key(),
        &keys,
        &app_spec(&AppScopes::new().contacts().build().unwrap()),
        w.clock.now_ms(),
    )
    .unwrap();
    paykit_lib::scopes::verify_certificate(c.signed(), &w.alice_id()).expect("verifies");
    refused_writing_nothing(
        &w,
        with_acceptance_cert(&good, Some(serde_json::to_value(c.signed()).unwrap())),
        app_id,
        "contacts verified",
    )
    .await;
}

#[tokio::test]
async fn test_restore_acceptance_of_another_own_app_refused() {
    // Both the plaintext and the kept certificate replaced by another own app's ("phone",
    // `pay`): a genuine acceptance of the same request, signed by the phone. Only the
    // App ID check refuses it.
    use pubky_switch_core::message::{Message, MessagePlaintext, Sender};
    let w = World::new();
    let (good, app_id) = backup_with_a_payment(&w).await;
    let keys = w.alice.identity().app_keys("phone");
    let phone = pubky_switch_client::testing::issue_cert(
        w.alice.identity().key(),
        &keys,
        &app_spec(&AppScopes::new().pay().build().unwrap()),
        w.clock.now_ms(),
    )
    .unwrap();
    let mut layer = layer_of(&good);
    let entry = layer
        .iter_mut()
        .find(|(k, _)| k.starts_with(b"j/"))
        .map(|(_, v)| v)
        .unwrap();
    let pt = pubky_switch_core::enc::unb64(entry["d"]["acceptance_plaintext"].as_str().unwrap())
        .unwrap();
    let pt = MessagePlaintext::decode(&pt).unwrap();
    let json: serde_json::Value = serde_json::from_slice(pt.json()).unwrap();
    let refs = json["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let b = pubky_switch_core::enc::unb64(r.as_str().unwrap()).unwrap();
            <[u8; 32]>::try_from(b.as_slice()).unwrap()
        })
        .collect();
    let payee: [u8; 32] = w.bob_id();
    let msg = Message {
        kind: json["kind"].as_str().unwrap().to_owned(),
        msg_id: entry["d"]["acceptance_msg_id"].as_str().unwrap().to_owned(),
        created_at_ms: w.clock.now_ms(),
        from: Sender {
            identity: w.alice_id(),
            app_id: keys.app_id_bytes(),
        },
        to: payee,
        refs,
        body: json["body"].as_object().cloned().unwrap(),
    };
    let signed = msg.sign(&keys).unwrap();
    entry["d"]["acceptance_plaintext"] =
        serde_json::Value::String(pubky_switch_core::enc::b64(&signed.encode()));
    entry["d"]["acceptance_cert"] = serde_json::to_value(phone.signed()).unwrap();
    refused_writing_nothing(
        &w,
        with_layer(&good, layer),
        app_id,
        "another own app's acceptance",
    )
    .await;
}

#[tokio::test]
async fn test_restore_unrequested_entry_naming_another_payments_record_refused() {
    // Two payments without a request; in the backup, the first's entry names the second's
    // record. A restore binds it as every runtime read does, and refuses it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    for value in ["0.0015", "0.0025"] {
        let payment = paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc(value))
            .with_options(
                PayOptions::default()
                    .with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
            );
        let paid = wallet.pay_without_request(payment).await.unwrap();
        assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    }
    let bytes = wallet.backup().await.unwrap().to_bytes().unwrap();
    let good: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let mut state = layer_of(&good);
    let journal: Vec<usize> = (0..state.len())
        .filter(|i| state[*i].0.starts_with(b"j/"))
        .collect();
    assert_eq!(journal.len(), 2);
    let other = state[journal[1]].1["d"]["natural_key"].clone();
    assert_ne!(state[journal[0]].1["d"]["natural_key"], other);
    state[journal[0]].1["d"]["natural_key"] = other;
    refused_writing_nothing(&w, with_layer(&good, state), wallet.app_id(), "record").await;
}
