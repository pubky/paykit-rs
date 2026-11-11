//! The app's own certificate by Paykit's checks (Paykit wire 3.1): a Paykit App never runs
//! under a certificate Paykit refuses, whether it comes with the approval at a join, with a
//! backup at a restore, or as a renewal the client takes later.

use std::sync::Arc;

use paykit_sdk::{
    AppScopes, Paykit, PaykitBackup, PaykitConfig, PaykitError, Rail, Refusal, RequestedScopes,
};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_client::testing::{issue_cert, AppSpec};
use pubky_switch_core::ops::OpKind;
use pubky_switch_core::records::{AppCert, ListedApp, RenewedCert};
use pubky_switch_core::scopes::Permission;
use serde_json::json;

use crate::common::{store, World};

/// `pay` with one switch permission its scopes don't derive: the Switch takes such a
/// certificate, and Paykit refuses it (Paykit wire 3.1).
fn refused_scopes() -> RequestedScopes {
    let mut scopes = AppScopes::new().pay().build().unwrap();
    let extra = Permission::ALL
        .into_iter()
        .find(|p| !scopes.permissions.contains(p))
        .unwrap();
    scopes.permissions.push(extra);
    // Each once, in the Switch's fixed order (Switch wire 3.1).
    scopes
        .permissions
        .sort_by_key(|p| Permission::ALL.iter().position(|q| q == p));
    scopes
}

fn is_cert_refused(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Refused(Refusal::CertificateRefused(_)))
}

/// Alice's signer renews her `wallet`'s certificate for `scopes`, issued `after_ms` from now,
/// and stores it at her switch (Switch wire 3.8). Not listed yet.
fn renew(w: &World, app_id: [u8; 16], scopes: &RequestedScopes, after_ms: u64) -> AppCert {
    let keys = w.alice.identity().app_keys("wallet");
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    let renewed = issue_cert(
        w.alice.identity().key(),
        &keys,
        &AppSpec::new(&names).with_permissions(&scopes.permissions),
        w.clock.now_ms() + after_ms,
    )
    .unwrap();
    w.alice
        .switch()
        .signer_op(OpKind::CertPut {
            target_app_id: app_id,
            cert: renewed.signed().clone(),
        })
        .unwrap();
    renewed
}

/// Alice's next `identity.json` lists `renewed` for the app, renewing `old`.
fn list(w: &World, app_id: [u8; 16], old: &AppCert, renewed: &AppCert) {
    w.alice
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != app_id);
            body.apps.push(ListedApp {
                app_id,
                cert: renewed.hash(),
            });
            body.renewed.push(RenewedCert {
                app_id,
                cert: old.hash(),
                until: None,
            });
        })
        .unwrap();
}

/// Bob's shop sends alice's `wallet` a payment request, entered at her switch directly.
fn request_to(w: &World, wallet: [u8; 16]) {
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet],
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
}

#[tokio::test]
async fn test_certificate_join_paykit_refuses_writes_and_sends_nothing() {
    let w = World::new();
    let request = w.approve(&w.alice, "wallet", &refused_scopes());
    let app_id = request.keys.app_id_bytes();
    let base = store();
    let fault = Arc::new(FaultStore::new(Arc::clone(&base), Fault::None));
    let tip = w.alice.switch().tip_seq();
    let joined = Paykit::join(
        w.env(&w.alice, Arc::clone(&fault) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        request,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    let error = joined.expect_err("a certificate Paykit refuses was joined with");
    assert!(is_cert_refused(&error), "{error:?}");
    // Refused before the client's join: no commit, and no registration at the switch.
    assert_eq!(fault.commits(), 0);
    assert_eq!(w.alice.switch().tip_seq(), tip);
    assert!(w.alice.switch().app(&app_id).cert.is_none());
    let loaded = Paykit::load(
        w.env(&w.alice, base),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(loaded.is_err(), "the join wrote the app's state");
}

#[tokio::test]
async fn test_certificate_renewal_paykit_refuses_holds_entries_and_refuses_calls() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    // A renewal Paykit refuses, seen, then listed and taken: the read says the certificate
    // fails.
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &refused);
    let error = wallet.receive(None).await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    assert_eq!(wallet.app().cert().hash(), refused.hash());
    // A request that arrives under it is held, not processed and not dropped.
    request_to(&w, me);
    let error = wallet.receive(None).await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    assert!(wallet.requests().await.unwrap().is_empty());
    // No Paykit call that writes or sends runs under it.
    let error = wallet.flush().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    let error = wallet.resume_payments().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    // A renewal that passes replaces it: the held request is processed then.
    let fixed = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &refused, &fixed);
    // The listing is read after the held request, which is held once more; the read takes
    // the certificate, and the next one processes the request.
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), fixed.hash());
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        !summary
            .report
            .events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageDropped { .. })),
        "{:?}",
        summary.report.events
    );
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_restore_then_renewal_paykit_refuses_takes_no_action() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let backup = wallet.backup().await.unwrap();
    // After the backup, the certificate is renewed for scopes Paykit refuses, and the app
    // takes it: the backup's and the current certificate differ in their permissions.
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &refused);
    let error = wallet.receive(None).await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    assert_eq!(wallet.app().cert().hash(), refused.hash());
    drop(wallet);
    // The restore checks the backup's certificate, which passes, and restores.
    let backup = PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().cert().hash(), old.hash());
    // Its first read takes the renewed certificate the `identity.json` lists, and from then
    // on it takes no Paykit action under it.
    let error = restored.receive(None).await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    assert_eq!(restored.app().cert().hash(), refused.hash());
    let error = restored.reconcile().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    let error = restored.flush().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
}
