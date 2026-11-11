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
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let _shop = w.shop().await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    // A renewal Paykit refuses, seen, then listed and taken: the read says the certificate
    // fails.
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &refused);
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        matches!(
            summary.certificate_refused,
            Some(Refusal::CertificateRefused(_))
        ),
        "{summary:?}"
    );
    assert_eq!(wallet.app().cert().hash(), refused.hash());
    // The app still loads, and the host has a notice to show while it lasts.
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(matches!(
        wallet.certificate_refused(),
        Some(Refusal::CertificateRefused(_))
    ));
    // A request that arrives under it is held, not processed and not dropped, and the
    // client's report of the read still reaches the host.
    request_to(&w, me);
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        matches!(
            summary.certificate_refused,
            Some(Refusal::CertificateRefused(_))
        ),
        "{summary:?}"
    );
    // Each read reports to the host: from the client's third try at the held entry, its
    // `StoreFailing`.
    let mut events = summary.report.events;
    for _ in 0..pubky_switch_client::app::RETRY_EVENT_READS {
        let summary = wallet.receive(None).await.unwrap();
        assert!(summary.certificate_refused.is_some());
        events.extend(summary.report.events);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::StoreFailing { .. })),
        "{events:?}"
    );
    assert!(wallet.requests().await.unwrap().is_empty());
    // No Paykit call that writes or sends runs under it.
    let error = wallet.flush().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    let error = wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap_err();
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
    assert_eq!(wallet.certificate_refused(), None);
}

#[tokio::test]
async fn test_certificate_restore_then_renewal_paykit_refuses_takes_no_action() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let backup = wallet.backup().await.unwrap();
    // After the backup, the certificate is renewed for scopes Paykit refuses and listed:
    // the backup's and the current certificate differ in their permissions. The wallet
    // stops before it takes it, so its record never carries it, and the restored app reads
    // its `cert.put`.
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &refused);
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
    let summary = restored.receive(None).await.unwrap();
    assert!(
        matches!(
            summary.certificate_refused,
            Some(Refusal::CertificateRefused(_))
        ),
        "{summary:?}"
    );
    assert_eq!(restored.app().cert().hash(), refused.hash());
    let error = restored.reconcile().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    let error = restored.flush().await.unwrap_err();
    assert!(is_cert_refused(&error), "{error:?}");
    assert!(restored.certificate_refused().is_some());
}

#[tokio::test]
async fn test_certificate_renewal_paykit_refuses_reads_no_lists() {
    // A wallet holding `contacts` reads its lists at each receive once they are due. Renewed
    // with `contacts` but without the `lists.read` it needs, a certificate the Switch takes
    // and Paykit refuses, it reads no list and writes no local refresh time.
    let w = World::new();
    let scopes = AppScopes::new().pay().contacts().build().unwrap();
    let recording = Arc::new(Recording::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            scopes.clone(),
            PaykitConfig::default(),
            Arc::clone(&recording) as Arc<dyn StateStore>,
        )
        .await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let mut refused = scopes;
    refused.permissions.retain(|p| *p != Permission::ListsRead);
    let renewed = renew(&w, me, &refused, 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &renewed);
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.certificate_refused.is_some());
    // The lists are due again: under the refused certificate, no list is asked for.
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS + 1);
    let before = w.router.log().len();
    recording.keys.lock().unwrap().clear();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.certificate_refused.is_some());
    let refreshed: Vec<String> = recording
        .keys
        .lock()
        .unwrap()
        .iter()
        .filter(|key| key.ends_with("/lr"))
        .cloned()
        .collect();
    assert!(refreshed.is_empty(), "{refreshed:?}");
    let asked: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/allow"))
        .cloned()
        .collect();
    assert!(asked.is_empty(), "{asked:?}");
}

/// A store that records the key of every write it commits.
#[derive(Default)]
struct Recording {
    inner: pubky_switch_client::store::MemoryStore,
    keys: std::sync::Mutex<Vec<String>>,
}

impl StateStore for Recording {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        let mut keys = self.keys.lock().unwrap();
        for write in &batch.writes {
            let key = match write {
                pubky_switch_client::store::Write::Put { key, .. }
                | pubky_switch_client::store::Write::Delete { key, .. } => key,
                pubky_switch_client::store::Write::DeletePrefix { prefix, .. } => prefix,
            };
            keys.push(String::from_utf8_lossy(key).into_owned());
        }
        drop(keys);
        self.inner.commit(batch)
    }
}

#[tokio::test]
async fn test_certificate_renewal_dropping_pay_keeps_finished_payments_backed_up() {
    // A request is paid, its acceptance signed under the certificate that held `pay`. The
    // app is then renewed to `contacts` only. The finished payment is history: the backup
    // still takes it, and a restore shows it finished.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let old = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
}

#[tokio::test]
async fn test_certificate_renewal_dropping_pay_keeps_a_dispatched_payment() {
    // The payment is in flight, its acceptance released, when the app is renewed to
    // `contacts`. Without `pay` the app can neither write nor read payer records, so its
    // outcome waits, the proof kept in the journal; the backup and a restore still take
    // the entry. Renewed to `pay` again, the outcome is written, the acceptance placed under
    // the certificate it was signed under.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    drop(restored);
    // `pay` again: the outcome goes, with the acceptance signed under the first certificate.
    let regranted = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &reduced, &regranted);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), regranted.hash());
    wallet.resume_payments().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    let record = wallet.request(&request).await.unwrap().record;
    let held = wallet.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_certificate_issued_ahead_of_the_apps_clock_keeps_its_acceptance_backed_up() {
    // The signer's clock runs ahead of the app's: a renewal is issued a minute "later" than
    // the app's clock reads, and the app signs an acceptance under it at once. No reader
    // judges a certificate by its issue time, so neither does the journal: the backup takes
    // the payment.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let ahead = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 60_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &ahead);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), ahead.hash());
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    assert!(ahead.issued_at() > w.clock.now_ms());
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
}

#[tokio::test]
async fn test_certificate_restored_lost_acceptance_not_signed_again_without_pay() {
    // In flight, its acceptance released, the payment is backed up while the app holds
    // `pay`; the app is then renewed to `contacts`. A restore of that backup on a fresh
    // store, with no outbox, finds the payment paid and its acceptance unknown to the
    // client, so lost: no new one is signed under a certificate without `pay`, and the
    // entry waits, backed up and restored as it is. Renewed to `pay` again, the outcome is
    // written once, with the acceptance and the proof.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let backup = wallet.backup().await.unwrap();
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    // The renewal is listed and the wallet stops before it takes it: its record never
    // carries it, so the restored app reads its `cert.put` and takes it.
    list(&w, me, &first, &reduced);
    drop(wallet);
    w.rail.settle(&attempt_id, true);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), reduced.hash());
    for _ in 0..2 {
        let resumed = restored.resume_payments().await.unwrap();
        assert!(
            matches!(
                resumed.as_slice(),
                [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
            ),
            "{resumed:?}"
        );
        assert_eq!(resumed[0].attempt_id(), attempt_id);
    }
    assert_eq!(
        restored.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    // The backup still takes the entry, and a restore of it reads.
    let again = restored.backup().await.unwrap();
    let copy = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &again,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        copy.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    drop(copy);
    let regranted = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = restored.receive(None).await;
    list(&w, me, &reduced, &regranted);
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), regranted.hash());
    let paid = restored.resume_payments().await.unwrap();
    assert!(
        matches!(paid.as_slice(), [paykit_sdk::PayOutcome::Paid { .. }]),
        "{paid:?}"
    );
    let record = restored.request(&request).await.unwrap().record;
    let held = restored
        .app()
        .shared_record(&record)
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(!value["response"].is_null(), "{value}");
    assert!(!value["proof"].is_null(), "{value}");
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_certificate_acceptance_kept_under_an_expired_certificate_backs_up() {
    // Paid under the first certificate; renewed (still `pay`); the clock passes the first
    // certificate's expiry. The acceptance kept under the expired one still backs up.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let old = wallet.app().cert().clone();
    let renewed = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &renewed);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), renewed.hash());
    let past = old.expires_at() - w.clock.now_ms() + 1;
    assert!(renewed.expires_at() > old.expires_at() + 1);
    w.clock.advance_ms(past);
    assert!(old.expired_at(w.clock.now_ms()) && !renewed.expired_at(w.clock.now_ms()));
    let backup = wallet.backup().await.unwrap();
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
}

#[tokio::test]
async fn test_certificate_renewal_dropping_pay_makes_no_rail_call() {
    // A dispatch's answer is lost, so a resume would repeat it on an idempotent method. The
    // app is renewed to `contacts` first: no path calls the payment method again, the
    // attempt stays open, and nothing moves.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let mut declaration = Rail::declaration(&*w.rail);
    declaration.history = false;
    declaration.idempotent = true;
    declaration.idempotency_retention_ms = Some(7 * 24 * crate::common::HOUR_MS);
    w.rail.set_declaration(declaration);
    w.rail.next(paykit_sdk::rail::Behaviour::LoseAnswer);
    let outcome = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await;
    let attempt_id = wallet.payments().await.unwrap()[0].attempt_id;
    assert!(
        !matches!(outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{outcome:?}"
    );
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    let old = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    let _ = wallet.resume_payments().await;
    let _ = wallet.reconcile().await;
    let _ = wallet.resume_payments().await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 1, "a rail call without pay");
    assert_ne!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
}

#[tokio::test]
async fn test_certificate_renewal_dropping_pay_resumes_without_reading_claims() {
    // A payment in flight, then a renewal to `contacts`. The service refuses a claim read
    // without `claims.read`, as the scripted switch is made to here: a resume, and a
    // reconcile, read no claim and make no payment, and every entry waits.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let old = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    // The method keeps no history of it and repeats idempotently: with `pay`, a resume
    // would read the claim to judge a repeat.
    w.rail.forget_history(&attempt_id);
    let mut declaration = Rail::declaration(&*w.rail);
    declaration.history = false;
    declaration.idempotent = true;
    declaration.idempotency_retention_ms = Some(7 * 24 * crate::common::HOUR_MS);
    w.rail.set_declaration(declaration);
    for _ in 0..8 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/claims".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    assert_eq!(resumed[0].attempt_id(), attempt_id);
    wallet.reconcile().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    // The method's "not found" decides nothing: the dispatch stays open in the journal, so
    // the claim isn't freed for a second payment once `pay` is back.
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Paying
    );
}

#[tokio::test]
async fn test_certificate_renewal_dropping_pay_rechecks_no_failed_payment() {
    // Reported failed by the method, the payment is finished, asked about again while the
    // method's history keeps it. Renewed to `contacts`, a resume asks nothing and reports
    // nothing, though the payment completed after all; renewed to `pay` again, it is
    // reported once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let outcome = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    w.rail.complete_after_failure(&attempt_id);
    let old = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &old, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    assert_eq!(wallet.resume_payments().await.unwrap(), Vec::new());
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Finished
    );
    let regranted = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &reduced, &regranted);
    wallet.receive(None).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

/// Whether `error` is the refusal while no reconcile under `pay` has run the payer pass
/// since a restore.
fn payer_pass_owed(error: &PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotNow(reason)) if reason.contains("Paykit spec 7.3"))
}

#[tokio::test]
async fn test_certificate_restore_without_pay_attempts_nothing_until_a_reconcile_under_pay() {
    // Paid after the backup, then the app is renewed to `contacts` and its device lost. The
    // restored app reconciles without `pay`, so without the payer pass, and the host ends
    // reconcile mode. Renewed to `pay` again, no payment goes until a reconcile runs the
    // pass: it finds the payment made after the backup, and the other request is paid once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    let paid = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    let other = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("2", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&paid, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    w.rail.settle(&attempt_id, true);
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    // The renewal is listed and the wallet stops before it takes it: its record never
    // carries it, so the restored app reads its `cert.put` and takes it.
    list(&w, me, &first, &reduced);
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), reduced.hash());
    let report = restored.reconcile().await.unwrap();
    assert!(report.payer_skipped, "{report:?}");
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    let regranted = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = restored.receive(None).await;
    list(&w, me, &reduced, &regranted);
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().cert().hash(), regranted.hash());
    let confirmed = || {
        paykit_sdk::PayOptions::default()
            .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new())
    };
    for request in [&paid, &other] {
        let refused = restored
            .pay_request(request, confirmed())
            .await
            .unwrap_err();
        assert!(payer_pass_owed(&refused), "{refused:?}");
    }
    assert_eq!(w.rail.all_executions().len(), 1);
    // A reconcile under `pay` runs the pass: the payment made after the backup is found and
    // reported, and payments go again.
    let report = restored.reconcile().await.unwrap();
    assert!(!report.payer_skipped, "{report:?}");
    assert!(
        matches!(report.resolved.as_slice(), [paykit_sdk::PayOutcome::Paid { attempt_id: a, .. }] if *a == attempt_id),
        "{report:?}"
    );
    assert!(restored.pay_request(&paid, confirmed()).await.is_err());
    w.rail.next(paykit_sdk::rail::Behaviour::Succeed);
    let outcome = restored.pay_request(&other, confirmed()).await.unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 2);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_certificate_restore_ended_without_reconcile_pays_nothing_until_one() {
    // Restored with `pay`, the host ends reconcile mode without reconciling: no payment goes
    // until a reconcile has run the payer pass, and then one goes once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    let confirmed = || {
        paykit_sdk::PayOptions::default()
            .with_possibly_cancelled_confirmed(pubky_switch_client::app::UserConfirmed::new())
    };
    let refused = restored
        .pay_request(&request, confirmed())
        .await
        .unwrap_err();
    assert!(payer_pass_owed(&refused), "{refused:?}");
    assert!(w.rail.all_executions().is_empty());
    let report = restored.reconcile().await.unwrap();
    assert!(!report.payer_skipped, "{report:?}");
    w.rail.next(paykit_sdk::rail::Behaviour::Succeed);
    let outcome = restored.pay_request(&request, confirmed()).await.unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_certificate_restored_payee_ends_reconcile_and_requests() {
    // A shop holds no `pay`: its reconcile skips the payer pass, which it never needs, and
    // ending reconcile mode lets it work as a payee at once.
    let w = World::new();
    let mut shop = w.shop().await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let report = restored.reconcile().await.unwrap();
    assert!(report.payer_skipped, "{report:?}");
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    let mut wallet = w.wallet("wallet").await;
    let request = crate::common::request_from(
        &mut restored,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&request).await.is_ok());
}

/// Alice's till, holding `request`, sent bob a request: its `payee_activity` record is
/// alice's. Her wallet holds `pay` only and reads none of them. Its certificate is then
/// renewed to hold `request` too, with the scope keys it needs, and listed: the wallet's next
/// read takes it. Returns the wallet and the renewed certificate.
async fn renewed_to_read_more(w: &World) -> (Paykit, AppCert) {
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut till = w
        .join(
            &w.alice,
            "till",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    till.send_request(w.bob_id(), crate::common::draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.payee_activity().await.unwrap().is_empty());
    let old = wallet.app().cert().clone();
    let wider = renew(
        w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    // The signer stores a bundle with the scope keys the renewed certificate needs.
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    let parts = pubky_switch_core::records::seal_bundle_parts(
        w.alice.identity().key(),
        &wider,
        1,
        epoch,
        &|scope| crate::common::spec().has_keys(scope),
    )
    .unwrap();
    for part in parts {
        w.alice
            .switch()
            .signer_op(OpKind::KeyBundlePut(pubky_switch_core::ops::KeyBundlePut {
                target_app_id: me,
                bundle: part.bundle,
                first_epoch: part.first_epoch,
                last_epoch: part.last_epoch,
            }))
            .unwrap();
    }
    wallet.receive(None).await.unwrap();
    list(w, me, &old, &wider);
    (wallet, wider)
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_lists_its_records_first() {
    // The wallet's copy lacks the till's record until the collection is listed: a call that
    // only reads is refused meanwhile, rather than show no requests sent, and a read lists
    // it.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    // The read that takes the certificate can't list the collections now.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/records".into(),
            after: 0,
        });
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    // The next read lists them: the till's request shows.
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_taken_by_a_failed_read_holds_reads() {
    // The read takes the certificate, then fails delivering the record that carries it:
    // the SDK hasn't followed the certificate yet, and a call that only reads is refused
    // all the same.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    assert!(wallet.receive(None).await.is_err());
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}
