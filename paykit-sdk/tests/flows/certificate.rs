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
    // The records aren't listed under it, and can't be: a call that only reads is refused
    // as not now.
    let unlisted = wallet.requests().await.unwrap_err();
    assert!(is_not_now(&unlisted), "{unlisted:?}");
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
    // After the backup, the certificate is renewed for scopes Paykit refuses, and the app
    // takes it: the backup's and the current certificate differ in their permissions.
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
async fn test_certificate_renewal_dropping_pay_raises_a_notice_for_a_held_payment() {
    // A payment in flight when the app is renewed without `pay` waits, and the host is told
    // why, once: a later resume raises nothing new, and an acknowledged notice stays so.
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
    // Under `pay`, a payment in flight raises no notice.
    wallet.resume_payments().await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &reduced);
    wallet.receive(None).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::HeldWithoutPay);
    assert_eq!(notices[0].attempt_id, Some(attempt_id));
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
    wallet.resume_payments().await.unwrap();
    assert_eq!(wallet.notices().await.unwrap(), notices);
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    wallet.resume_payments().await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_certificate_acknowledged_held_payment_notice_raises_none_again_after_a_restore() {
    // The held payment's notice is acknowledged under the certificate without `pay`, then
    // the app is backed up and restored under it: a resume finds the acknowledged notice at
    // its key and raises nothing again.
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
    wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &reduced);
    wallet.receive(None).await.unwrap();
    wallet.resume_payments().await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let backup = paykit_sdk::PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let _ = restored.receive(None).await;
    let _ = restored.resume_payments().await;
    assert!(restored.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_certificate_renewal_bringing_pay_back_drops_the_held_payment_notice() {
    // Renewed without `pay` while a payment is in flight, then renewed with it again: the
    // payment finishes, and the notice that it was held without `pay` goes.
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
    wallet.resume_payments().await.unwrap();
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
    let again = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &reduced, &again);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), again.hash());
    w.rail.settle(&attempt_id, true);
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    assert!(wallet.notices().await.unwrap().is_empty());
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
    list(&w, me, &first, &reduced);
    wallet.receive(None).await.unwrap();
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
    list(&w, me, &first, &reduced);
    wallet.receive(None).await.unwrap();
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
/// read takes it. Returns the wallet, on `base`, and the renewed certificate.
async fn renewed_to_read_more_on(w: &World, base: Arc<dyn StateStore>) -> (Paykit, AppCert) {
    let (wallet, wider, _) = renewed_to_read_more_with_till(w, base).await;
    (wallet, wider)
}

/// [`renewed_to_read_more_on`], with the till that sent the request.
async fn renewed_to_read_more_with_till(
    w: &World,
    base: Arc<dyn StateStore>,
) -> (Paykit, AppCert, Paykit) {
    let mut wallet = w.wallet_on("wallet", base).await;
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
    (wallet, wider, till)
}

async fn renewed_to_read_more(w: &World) -> (Paykit, AppCert) {
    renewed_to_read_more_on(w, store()).await
}

/// The wallet's read takes the wider certificate, then fails delivering the record that
/// carries it, and the app is restarted on `base`.
async fn reloaded_after_a_failed_read(w: &World, base: &Arc<dyn StateStore>) -> Paykit {
    let (mut wallet, wider) = renewed_to_read_more_on(w, Arc::clone(base)).await;
    let me = wallet.app_id();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    assert!(wallet.receive(None).await.is_err());
    drop(wallet);
    let wallet = w
        .load_on_clock(&w.alice, Arc::clone(base), me, Arc::clone(&w.clock))
        .await;
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    wallet
}

fn is_not_now(error: &PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotNow(_)))
}

/// How many listing requests the client makes for `cert`: one per collection it reads,
/// each of one page here.
fn client_listing_requests(cert: &AppCert) -> usize {
    use pubky_switch_client::app::Protocol as _;
    paykit_sdk::PaykitProtocol::new()
        .collections()
        .iter()
        .filter(|collection| collection.readable_by(cert))
        .count()
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_lists_its_records_first() {
    // The wallet's copy lacks the till's record until the collection is listed: a call that
    // only reads is refused meanwhile, rather than show no requests sent. The read that
    // takes the certificate holds no scope key of it yet, so it doesn't list; the next one
    // fetches the keys, and the client lists before it reads the feed. A read whose
    // listing can't connect fails and leaves the records unlisted for the one after.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/records".into(),
            after: 0,
        });
    assert!(wallet.receive(None).await.is_err());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    // The client's listing goes through and Paykit's own, after the read, can't connect:
    // the read keeps its report, and says the records stay unlisted.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/records".into(),
            after: client_listing_requests(&wider),
        });
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.records_unlisted.is_some(), "{summary:?}");
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
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

#[tokio::test]
async fn test_certificate_renewal_reading_more_holds_reads_across_a_reload() {
    // The read that took the wider certificate failed, and the app restarted before any
    // listing: the records are still unlisted, so a call that only reads is refused.
    let w = World::new();
    let base = store();
    let wallet = reloaded_after_a_failed_read(&w, &base).await;
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_listed_after_a_reload() {
    let w = World::new();
    let base = store();
    let mut wallet = reloaded_after_a_failed_read(&w, &base).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_listing_lost_before_its_note_lists_again() {
    // The listing went through, and the store lost the write that keeps the certificate it
    // was made under, as a crash between the two commits would: the records count as
    // unlisted again, and the next read lists them once more.
    let w = World::new();
    let base = store();
    let (mut wallet, _wider) = renewed_to_read_more_on(&w, Arc::clone(&base)).await;
    let me = wallet.app_id();
    let held = wallet.app().protocol_get(b"lc").await.unwrap().unwrap();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: b"lc".to_vec(),
            value: held,
        }])
        .await
        .unwrap();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_store_without_a_listed_certificate_lists_once_then_keeps_it() {
    // A store that keeps no certificate listed under, as an earlier build wrote: a load
    // doesn't take the one held as listed, so the records count as unlisted until a read
    // lists them, which keeps it.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let me = wallet.app_id();
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Delete {
            key: b"lc".to_vec(),
        }])
        .await
        .unwrap();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    wallet.receive(None).await.unwrap();
    assert!(wallet.payee_activity().await.unwrap().is_empty());
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_some());
}

#[tokio::test]
async fn test_certificate_passing_after_a_refused_load_unlisted_across_a_restart() {
    // A load under a certificate Paykit refuses keeps none listed under; a read takes a
    // renewal that passes and fails before Paykit lists; the app restarts. The records
    // still count as unlisted: the load doesn't take the passing certificate as listed.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let me = wallet.app_id();
    let first = wallet.app().cert().clone();
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &refused);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app().cert().hash(), refused.hash());
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Delete {
            key: b"lc".to_vec(),
        }])
        .await
        .unwrap();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    let fixed = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &refused, &fixed);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    assert!(wallet.receive(None).await.is_err());
    assert_eq!(wallet.app().cert().hash(), fixed.hash());
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_some());
}

#[tokio::test]
async fn test_certificate_renewal_reading_more_listing_refused_holds_reads_until_listed() {
    // The client's listing the wider certificate owes is refused for good: the read is
    // refused, and the records stay unlisted until a later read lists them.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/records".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let error = wallet.receive(None).await.unwrap_err();
    assert_eq!(
        error.code(),
        Some(pubky_switch_core::http::ErrorCode::PermissionMissing),
        "{error:?}"
    );
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    let refused = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.records_unlisted.is_none(), "{summary:?}");
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_restore_keeps_none_listed_under_until_a_listing() {
    // A restore keeps no certificate as listed under: its copy is empty until a listing
    // runs, so a call that only reads is refused until then. The first read lists, and that
    // listing keeps the certificate.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
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
    assert!(restored.app().protocol_get(b"lc").await.unwrap().is_none());
    let refused = restored.requests().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    restored.receive(None).await.unwrap();
    assert!(restored.app().protocol_get(b"lc").await.unwrap().is_some());
    assert!(restored.requests().await.unwrap().is_empty());
}

/// Alice's wallet, joined on `base` after her phone received bob's request and wrote its
/// record: nothing is read before the call returns. Returns the wallet and the request.
async fn joined_after_a_record_was_written(
    w: &World,
    base: Arc<dyn StateStore>,
) -> (Paykit, paykit_sdk::RequestRef) {
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    phone.receive(None).await.unwrap();
    assert_eq!(phone.requests().await.unwrap().len(), 1);
    let wallet = Paykit::join(
        w.env(&w.alice, base),
        PaykitConfig::default(),
        w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    (wallet, request)
}

/// Before any listing: a call that only reads is refused rather than show the empty copy,
/// and one that acts lists first, so it acts on the record another own app wrote.
async fn reads_and_acts_on_what_was_written(wallet: &mut Paykit, request: &paykit_sdk::RequestRef) {
    let refused = wallet.requests().await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    let refused = wallet.request(request).await.unwrap_err();
    assert!(is_not_now(&refused), "{refused:?}");
    wallet.reject_request(request, None).await.unwrap();
    assert_eq!(
        wallet.request(request).await.unwrap().status,
        paykit_sdk::RequestStatus::Rejected
    );
}

#[tokio::test]
async fn test_certificate_join_reads_nothing_from_its_copy_before_a_listing() {
    // Another own app wrote the request record before the join. Until a listing, the new
    // app's copy lacks it: a read is refused, and an action lists first and finds it.
    let w = World::new();
    let (mut wallet, request) = joined_after_a_record_was_written(&w, store()).await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    reads_and_acts_on_what_was_written(&mut wallet, &request).await;
}

#[tokio::test]
async fn test_certificate_join_reloaded_before_a_listing_reads_nothing_from_its_copy() {
    // As above, with the app reloaded before its first listing.
    let w = World::new();
    let base = store();
    let (wallet, request) = joined_after_a_record_was_written(&w, Arc::clone(&base)).await;
    let me = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    reads_and_acts_on_what_was_written(&mut wallet, &request).await;
}

#[tokio::test]
async fn test_certificate_join_first_receive_lists_before_taking_its_entries() {
    // Bob's receipt for a payment the phone made before the wallet joined comes to the new
    // wallet, whose copy lacks the payer record until a listing.
    let w = World::new();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    drop(phone);
    let mut wallet = Paykit::join(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    // Bob's shop reads alice's directory again, which lists the new wallet.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&sale).await.unwrap().is_some());
    // The client's first read lists before it reads the feed, so the receipt's record is
    // there when its entry comes: taken at that read, once, and the listing kept.
    let summary = wallet.receive(None).await.unwrap();
    assert!(received(&summary, "paykit.payment_receipt"), "{summary:?}");
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_some());
    for _ in 0..2 {
        let summary = wallet.receive(None).await.unwrap();
        assert!(!received(&summary, "paykit.payment_receipt"), "{summary:?}");
    }
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
}

#[tokio::test]
async fn test_certificate_stored_entry_the_app_cant_keep_stays_for_the_host() {
    // A request's step is refused for good, and the app is renewed without `pay` before
    // Paykit sees the event: no record of its own takes the request, so the event isn't
    // taken, and stays for the host.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut shop = w.shop().await;
    crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    let first = wallet.app().cert().clone();
    let reduced = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    list(&w, me, &first, &reduced);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let _ = wallet.app_mut().receive(None).await;
    let _ = wallet.app_mut().receive(None).await;
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    wallet.receive(None).await.unwrap();
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, pubky_switch_client::app::AppEvent::EffectRefused { .. })),
        "{events:?}"
    );
}

/// A payment in flight, its method hanging, when the wallet takes a certificate renewed to
/// read `request` too, whose scope keys no bundle brings: the records stay unlisted, and
/// can't be listed until the keys come. Returns the wallet, the shop, the request paid and a
/// second request, and the attempt.
async fn in_flight_under_a_wider_certificate_without_keys(
    w: &World,
) -> (
    Paykit,
    Paykit,
    paykit_sdk::RequestRef,
    paykit_sdk::RequestRef,
    paykit_lib::AttemptId,
) {
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
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&paid, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let old = wallet.app().cert().clone();
    let wider = renew(
        w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    (wallet, shop, paid, other, attempt_id)
}

#[tokio::test]
async fn test_certificate_wider_without_keys_finishes_a_dispatched_payment_once_listed() {
    // The method then completes. The certificate is another than the one the records were
    // listed under, so the payer's side counts as unlisted too, whichever side it widens:
    // one held between may have read less of it. The resume writes nothing while the keys
    // wait; once they come and the records are listed, it writes the outcome and sends the
    // proof once.
    let w = World::new();
    let (mut wallet, _shop, paid, _other, attempt_id) =
        in_flight_under_a_wider_certificate_without_keys(&w).await;
    let me = wallet.app_id();
    let wider = wallet.app().cert().clone();
    let unlisted = wallet.payee_activity().await.unwrap_err();
    assert!(is_not_now(&unlisted), "{unlisted:?}");
    w.rail.settle(&attempt_id, true);
    let before = w.router.log().len();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    store_bundle(&w, me, &wider);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let resumed = wallet.resume_payments().await.unwrap();
    let [paykit_sdk::PayOutcome::Paid {
        proof_msg_id: Some(proof),
        ..
    }] = resumed.as_slice()
    else {
        panic!("{resumed:?}");
    };
    assert!(
        matches!(
            wallet.message_delivery(proof).await.unwrap(),
            Some(paykit_sdk::MessageDelivery::Accepted { .. })
        ),
        "{:?}",
        wallet.message_delivery(proof).await
    );
    // The acceptance, released at the dispatch, and the proof: each once.
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), 2);
    assert_eq!(
        claim_done(&mut wallet).await,
        pubky_switch_core::claims::Outcome::Done
    );
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert!(wallet.request(&paid).await.is_ok());
}

/// The outcome of the wallet's one payment's attempt on its claim, read from the switch.
async fn claim_done(wallet: &mut Paykit) -> pubky_switch_core::claims::Outcome {
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    read.view.attempts[0].outcome
}

#[tokio::test]
async fn test_certificate_wider_without_keys_holds_a_new_payment_and_a_reconcile() {
    // A new payment waits for the keys, as a reconcile does: its payer pass reads the
    // obligations from records a listing can't open yet.
    let w = World::new();
    let (mut wallet, _shop, _paid, other, attempt_id) =
        in_flight_under_a_wider_certificate_without_keys(&w).await;
    let refused = wallet
        .pay_request(&other, paykit_sdk::PayOptions::default())
        .await
        .unwrap_err();
    assert!(waits_for_keys(&refused), "{refused:?}");
    let refused = wallet.reconcile().await.unwrap_err();
    assert!(waits_for_keys(&refused), "{refused:?}");
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let refused = wallet
        .resolve_attempt(
            &obligation,
            attempt_id,
            paykit_sdk::Resolution::Done,
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(waits_for_keys(&refused), "{refused:?}");
    // A list placed over a record the app can't open would drop its single-use marks.
    let refused = wallet
        .place_held_list_without_marks(
            &w.bob_id(),
            &[9; 16],
            pubky_switch_client::app::UserConfirmed::new(),
        )
        .await
        .unwrap_err();
    assert!(waits_for_keys(&refused), "{refused:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

/// The refusal while a wider certificate's scope keys are missing: not now, saying so.
fn waits_for_keys(error: &PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotNow(why)) if why.contains("scope keys"))
}

#[tokio::test]
async fn test_certificate_wider_without_the_pay_key_writes_nothing_and_journals_the_payment() {
    // A data epoch rotation besides: the wallet lacks the `pay` key too. The resume asks the
    // method's history only and journals the payment made; it writes nothing to the switch
    // and sends nothing until the keys come.
    let w = World::new();
    let (mut wallet, _shop, _paid, _other, attempt_id) =
        in_flight_under_a_wider_certificate_without_keys(&w).await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    let entries = w.bob.switch().entries_from(&w.alice_id());
    w.rail.settle(&attempt_id, true);
    let before = w.router.log().len();
    let resumed = wallet.resume_payments().await.unwrap();
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops") || line.contains("/claims"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
    assert_eq!(
        claim_done(&mut wallet).await,
        pubky_switch_core::claims::Outcome::Open
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), entries);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

/// The signer stores `cert`'s key bundle for app `me`, every data epoch up to the current.
fn store_bundle(w: &World, me: [u8; 16], cert: &AppCert) {
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    let parts = pubky_switch_core::records::seal_bundle_parts(
        w.alice.identity().key(),
        cert,
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
}

#[tokio::test]
async fn test_certificate_narrowed_then_widened_lists_again() {
    // Listed under `pay` and `request`, renewed to `pay`, and back: a record written while
    // narrowed is listed once the wider certificate's keys are held.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    let me = wallet.app_id();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
    let narrow = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &wider, &narrow);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), narrow.hash());
    let mut till = w
        .join(
            &w.alice,
            "till2",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    till.send_request(w.bob_id(), crate::common::draft("2", w.clock.now_ms()))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let again = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        3_000,
    );
    store_bundle(&w, me, &again);
    wallet.receive(None).await.unwrap();
    list(&w, me, &narrow, &again);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), again.hash());
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 2);
}

/// How the wallet comes to hold a narrower certificate than the one listed under, before it
/// is renewed back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Narrowed {
    /// The read takes it, and the store fails Paykit's write of its state after it.
    StateWriteFails,
    /// The read takes it, and Paykit refuses it.
    Refused,
    /// As [`Narrowed::StateWriteFails`], and the app stops in the read that takes the wider
    /// one, before Paykit's write of its state after it: the app is loaded again.
    Stopped,
}

/// The wallet, listed under `pay` and `request`, is narrowed as `narrowed` says, in the read
/// that also brings a request alice's till sent meanwhile, which the narrower certificate
/// doesn't read. It is renewed back to `pay` and `request` as the data epoch rotates, and
/// bob's acceptance comes in the read that takes it, before any listing: the copy lacks the
/// request's record, so the acceptance is held, not taken as a reply to a request the wallet
/// didn't send, and stays held while the keys wait. Once they come and the records are
/// listed, it goes into the record, once.
async fn narrowed_then_renewed_back(narrowed: Narrowed) {
    let w = World::new();
    let base = store();
    let fault = Arc::new(FaultStore::new(Arc::clone(&base), Fault::None));
    let (mut wallet, wider) =
        renewed_to_read_more_on(&w, Arc::clone(&fault) as Arc<dyn StateStore>).await;
    let me = wallet.app_id();
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
    let mut payer = w
        .join(&w.bob, "bwallet", AppScopes::new().pay().build().unwrap())
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut till = w
        .join(
            &w.alice,
            "till2",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    // Bob pays a first request of the till while the wallet reads `request`: his wallet
    // reads alice's directory, which lists the wallet under the wider certificate, and
    // keeps that copy for the rest of the flow.
    let first = till
        .send_request(w.bob_id(), crate::common::draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    wallet.receive(None).await.unwrap();
    payer.receive(None).await.unwrap();
    let paid = payer
        .pay_request(&first, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    wallet.receive(None).await.unwrap();
    let scopes = match narrowed {
        Narrowed::Refused => refused_scopes(),
        _ => AppScopes::new().pay().build().unwrap(),
    };
    let narrow = renew(&w, me, &scopes, 2_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &wider, &narrow);
    let request = till
        .send_request(w.bob_id(), crate::common::draft("2", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    drop(till);
    if narrowed == Narrowed::Refused {
        wallet.receive(None).await.unwrap();
        assert!(wallet.certificate_refused().is_some());
    } else {
        fault.refuse_puts_as_too_large_once(b"p/lc");
        assert!(wallet.receive(None).await.is_err());
    }
    assert_eq!(wallet.app().cert().hash(), narrow.hash());
    let again = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        3_000,
    );
    list(&w, me, &narrow, &again);
    // The data epoch rotates too, and no bundle brings its keys: the client can't list the
    // records until they come, so the copy still lacks the request's record at later reads.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    payer.receive(None).await.unwrap();
    let paid = payer
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let acceptance = "paykit.payment_request_acceptance";
    if narrowed == Narrowed::Stopped {
        fault.refuse_puts_as_too_large_once(b"p/lc");
        let read = wallet.receive(None).await;
        assert!(
            read.as_ref()
                .map_or(true, |summary| !received(summary, acceptance)),
            "{read:?}"
        );
        drop(wallet);
        wallet = w
            .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
            .await;
    } else {
        let summary = wallet.receive(None).await.unwrap();
        assert!(!received(&summary, acceptance), "{summary:?}");
    }
    assert_eq!(wallet.app().cert().hash(), again.hash());
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    assert!(!received(&summary, acceptance), "{summary:?}");
    store_bundle(&w, me, &again);
    let mut acceptances = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        acceptances += usize::from(received(&summary, acceptance));
    }
    assert_eq!(acceptances, 1);
    let sales = wallet.payee_activity().await.unwrap();
    let sale = sales
        .iter()
        .find(|sale| sale.request_hash == Some(request.request_hash))
        .unwrap();
    assert_ne!(sale.status, paykit_sdk::PayeeStatus::Requested, "{sales:?}");
}

#[tokio::test]
async fn test_certificate_narrowed_with_its_state_write_failed_then_renewed_back_holds_a_reply() {
    narrowed_then_renewed_back(Narrowed::StateWriteFails).await;
}

#[tokio::test]
async fn test_certificate_narrowed_to_a_refused_one_then_renewed_back_holds_a_reply() {
    narrowed_then_renewed_back(Narrowed::Refused).await;
}

#[tokio::test]
async fn test_certificate_narrowed_then_stopped_taking_the_wider_one_holds_a_reply_after_a_load() {
    narrowed_then_renewed_back(Narrowed::Stopped).await;
}

#[tokio::test]
async fn test_certificate_narrowed_on_one_side_widened_on_another_then_back_holds_a_receipt() {
    // `request` reads the payee's side, whose keys wait, and no longer the payer's.
    narrowed_on_the_payer_side_then_back(AppScopes::new().request().build().unwrap()).await;
}

#[tokio::test]
async fn test_certificate_narrowed_to_allowance_then_back_holds_a_receipt() {
    // `allowance` reads none of the payer's side.
    narrowed_on_the_payer_side_then_back(AppScopes::new().allowance().build().unwrap()).await;
}

/// The wallet, listed under `pay`, is renewed to `narrowed`, which doesn't read the payer's
/// side. Bob's request comes, and the phone pays it: the payer record is written while the
/// wallet can't read it. The wallet is renewed back to `pay`, and bob's receipt comes in the
/// read that takes it. Nothing on the payer's side was listed meanwhile: the receipt's entry
/// is held, and goes into the record, once, after the listing.
async fn narrowed_on_the_payer_side_then_back(narrowed: RequestedScopes) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let first = wallet.app().cert().clone();
    let mixed = renew(&w, me, &narrowed, 1_000);
    // The narrowed certificate's keys are stored, so the listing it owes isn't left waiting,
    // which would hold the feed and the renewal back behind it.
    store_bundle(&w, me, &mixed);
    wallet.receive(None).await.unwrap();
    list(&w, me, &first, &mixed);
    // The shop reads alice's directory while it still lists the wallet under `pay`, so its
    // receipt goes to the wallet too.
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), mixed.hash());
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    drop(phone);
    let back = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &mixed, &back);
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&sale).await.unwrap().is_some());
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), back.hash());
    assert!(!received(&summary, "paykit.payment_receipt"), "{summary:?}");
    let mut receipts = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        receipts += usize::from(received(&summary, "paykit.payment_receipt"));
    }
    assert_eq!(receipts, 1);
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
}

#[tokio::test]
async fn test_certificate_reconcile_waits_for_the_scope_keys_then_lists() {
    // A reconcile before the wider certificate's keys lists nothing and keeps nothing as
    // listed: once the keys come, a read lists the records.
    let w = World::new();
    let (mut wallet, wider) = renewed_to_read_more(&w).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    let refused = wallet.reconcile().await.unwrap_err();
    assert!(waits_for_keys(&refused), "{refused:?}");
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.payee_activity().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_certificate_none_listed_under_a_refused_one_lists_once_one_passes() {
    // A store an earlier build wrote keeps no certificate listed under, and loads under one
    // Paykit refuses, so it keeps none. Once a renewal passes, its read lists the records.
    let w = World::new();
    let base = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&base)).await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &refused);
    wallet.receive(None).await.unwrap();
    assert!(wallet.certificate_refused().is_some());
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Delete {
            key: b"lc".to_vec(),
        }])
        .await
        .unwrap();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&base), me, Arc::clone(&w.clock))
        .await;
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    let fixed = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    wallet.receive(None).await.unwrap();
    list(&w, me, &refused, &fixed);
    let before = w.router.log().len();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), fixed.hash());
    assert!(
        w.router.log()[before..]
            .iter()
            .any(|line| line.contains("/records")),
        "{:?}",
        &w.router.log()[before..]
    );
    let listed = wallet.app().protocol_get(b"lc").await.unwrap().unwrap();
    assert!(!listed.is_empty());
}

#[tokio::test]
async fn test_certificate_wider_on_the_payer_side_writes_nothing_while_keys_wait() {
    // In flight, the wallet is narrowed, then renewed to `pay` and `request` with no
    // bundle: it holds the `pay` key from before, but the new certificate reads more of the
    // payer's side than the one listed under: its records, through `pay`, after either, and
    // its claims too after `contacts` (`allowance` reads claims). The resume journals the
    // payment made
    // and writes nothing, rather than build on a copy that may lack what was written
    // meanwhile.
    payer_side_widened(AppScopes::new().contacts().build().unwrap(), false).await;
    payer_side_widened(AppScopes::new().allowance().build().unwrap(), false).await;
}

#[tokio::test]
async fn test_certificate_wider_on_the_payer_side_places_no_kept_cancellation() {
    // As above, and bob cancels the request meanwhile: the wallet keeps his cancellation,
    // and neither its receive nor the resume places it in the request record, which it
    // builds on, while the payer's side waits.
    payer_side_widened(AppScopes::new().contacts().build().unwrap(), true).await;
}

/// The wallet pays bob's request, the payment in flight; it is narrowed to `narrowed`, then
/// renewed to `pay` and `request` with no bundle. With `cancelled`, bob cancels the request
/// once the wider certificate is taken. The method then reports it paid, and the resume
/// writes nothing.
async fn payer_side_widened(narrowed: RequestedScopes, cancelled: bool) {
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
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&paid, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    let first = wallet.app().cert().clone();
    let narrow = renew(&w, me, &narrowed, 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &narrow);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), narrow.hash());
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        2_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &narrow, &wider);
    wallet.receive(None).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    w.rail.settle(&attempt_id, true);
    if cancelled {
        shop.receive(None).await.unwrap();
        let record = shop.payee_activity().await.unwrap()[0].record;
        shop.cancel_sent_request(&record, None).await.unwrap();
        let before = w.router.log().len();
        wallet.receive(None).await.unwrap();
        let writes: Vec<String> = w.router.log()[before..]
            .iter()
            .filter(|line| line.contains("alice-switch") && line.contains("/ops"))
            .cloned()
            .collect();
        assert!(writes.is_empty(), "{writes:?}");
    }
    let before = w.router.log().len();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::RailPaid
    );
}

/// How many acceptances `shop` takes at its next read.
async fn acceptances_received(shop: &mut Paykit) -> usize {
    shop.receive(None)
        .await
        .unwrap()
        .report
        .events
        .iter()
        .filter(|e| {
            matches!(e, AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.payment_request_acceptance")
        })
        .count()
}

#[tokio::test]
async fn test_certificate_restore_unlisted_with_payee_keys_awaited_resumes_nothing() {
    // The backup holds a payment in flight, which the wallet then finishes; the wallet is
    // renewed to read `request` too. The restored app's first read takes that renewal from
    // its record, then reads no feed: its copy counts as unlisted since the restore, and the
    // listing waits for the payee's keys. The resume treats the payer's side as waiting too:
    // it journals what the method says and sends nothing, no acceptance again among it.
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
    assert_eq!(acceptances_received(&mut shop).await, 1);
    let backup = wallet.backup().await.unwrap();
    w.rail.settle(&attempt_id, true);
    wallet.resume_payments().await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
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
    let before = w.router.log().len();
    let summary = restored.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    assert!(
        !w.router.log()[before..]
            .iter()
            .any(|line| line.contains("/feed")),
        "{:?}",
        w.router.log()
    );
    assert_eq!(restored.app().cert().hash(), wider.hash());
    let before = w.router.log().len();
    let resumed = restored.resume_payments().await.unwrap();
    assert!(
        matches!(
            resumed.as_slice(),
            [paykit_sdk::PayOutcome::PaidOutcomeUnwritten { .. }]
        ),
        "{resumed:?}"
    );
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    restored.app_mut().flush_outbox().await.unwrap();
    assert_eq!(acceptances_received(&mut shop).await, 0);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_certificate_payee_side_widening_with_a_data_epoch_rotation_writes_nothing() {
    // The attempt is open, nothing dispatched. The wallet is renewed to read `request` too,
    // with no bundle, and the data epoch rotates: it reads no more of the payer's side, but
    // lacks the current `pay` key. The payer's side waits: the resume writes no `failed`
    // and leaves the attempt as it is.
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
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let kept = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap_err();
    assert!(kept.kept_batch().is_some(), "{kept:?}");
    wallet.flush().await.unwrap();
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Attempted
    );
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = w.router.log().len();
    let resumed = wallet.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Pending { .. }]),
        "{resumed:?}"
    );
    let writes: Vec<String> = w.router.log()[before..]
        .iter()
        .filter(|line| line.contains("/ops"))
        .cloned()
        .collect();
    assert!(writes.is_empty(), "{writes:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].stage,
        paykit_sdk::Stage::Attempted
    );
    assert!(w.rail.all_executions().is_empty());
}

/// A transport to alice's switch that applies `op`, signed by `keys`, just before the
/// listing request after the first `skip`: so the SDK's own listing, after the client's,
/// is what takes that write.
struct WriteBeforeSdkListing {
    inner: Arc<pubky_switch_client::testing::Router>,
    switch: Arc<pubky_switch_client::testing::ScriptedSwitch>,
    keys: pubky_switch_core::keys::AppKeys,
    op: std::sync::Mutex<Option<OpKind>>,
    skip: usize,
    listed: std::sync::atomic::AtomicUsize,
}

impl pubky_switch_client::transport::Transport for WriteBeforeSdkListing {
    fn send(
        &self,
        request: pubky_switch_client::transport::Request,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<
            pubky_switch_client::transport::Response,
            pubky_switch_client::error::TransportError,
        >,
    > {
        use std::sync::atomic::Ordering;
        let path = request.path_and_query.split('?').next().unwrap_or_default();
        if path.ends_with("/records") && self.listed.fetch_add(1, Ordering::SeqCst) == self.skip {
            if let Some(op) = self.op.lock().unwrap().take() {
                self.switch.app_op(&self.keys, op).unwrap();
            }
        }
        Box::pin(async move { self.inner.send(request).await })
    }
}

#[tokio::test]
async fn test_certificate_duty_event_raised_by_the_sdks_listing_reaches_a_report() {
    // The till rewrites its request record with a member it doesn't know, over the bound on
    // what a reader keeps of a record it has seen: a reader that takes the write raises
    // `RecordUncheckable`, a duty event. The SDK's listing after the read takes it.
    let w = World::new();
    let base = store();
    let (wallet, wider, mut till) = renewed_to_read_more_with_till(&w, Arc::clone(&base)).await;
    let me = wallet.app_id();
    drop(wallet);
    let sale = till.payee_activity().await.unwrap().remove(0);
    let held = till
        .app()
        .shared_record(&sale.record)
        .await
        .unwrap()
        .unwrap();
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().unwrap()).unwrap();
    value["padding"] = json!("x".repeat(pubky_switch_client::app::MAX_SEEN_MARK_BYTES + 1));
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayeeActivity.name(),
            paykit_lib::collections::Collection::PayeeActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(
            &w.alice_id(),
            &sale.request_hash.unwrap(),
        ),
    );
    assert_eq!(till.app().record_key(&target).unwrap(), sale.record);
    // The till's own client refuses a write over the bound, so the write is built small and
    // sealed again with the large value, as another writer would send it.
    let (_, mut op) = till
        .app_mut()
        .record_op(&target, held.content.value(), Vec::new())
        .await
        .unwrap();
    let OpKind::RecordPut(put) = &mut op else {
        panic!("a record put");
    };
    let epoch = till.app().data_epoch();
    let scope = &target.collection.policy.read_scope;
    put.value_sealed = pubky_switch_core::sealed::SealedValue::seal(
        &till.app().scope_key(epoch, scope).unwrap(),
        &w.alice_id(),
        epoch,
        scope,
        &sale.record,
        &serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
    let transport = Arc::new(WriteBeforeSdkListing {
        inner: Arc::clone(&w.router),
        switch: Arc::clone(w.alice.switch()),
        keys: w.alice.identity().app_keys("till"),
        op: std::sync::Mutex::new(Some(op)),
        skip: client_listing_requests(&wider),
        listed: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut wallet = w
        .load_with_transport(&w.alice, Arc::clone(&base), me, transport)
        .await;
    let mut events = Vec::new();
    for _ in 0..3 {
        events.extend(wallet.receive(None).await.unwrap().report.events);
    }
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::RecordUncheckable { .. })),
        "{events:?}"
    );
}

/// A read processes bob's request, whose step's write the switch refuses for good, and then
/// takes a renewal Paykit refuses: the receive reports the stored entry's duty event under
/// the refused certificate. With `host_acks`, the host acknowledges up to the last event
/// that report showed. Then a renewal that passes is taken, and the request is written.
async fn stored_entry_reported_under_a_refused_certificate(host_acks: bool) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    wallet.receive(None).await.unwrap();
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    list(&w, me, &old, &refused);
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.certificate_refused.is_some(), "{summary:?}");
    let events = summary.report.events;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::EffectRefused { .. })),
        "{events:?}"
    );
    if host_acks {
        let last = events.iter().filter_map(AppEvent::id).next_back().unwrap();
        wallet.app_mut().ack_events(last).await.unwrap();
    }
    let fixed = renew(&w, me, &AppScopes::new().pay().build().unwrap(), 2_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &refused, &fixed);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(wallet.certificate_refused(), None);
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::Open
    );
}

#[tokio::test]
async fn test_certificate_refused_stored_entry_written_once_a_renewal_passes() {
    stored_entry_reported_under_a_refused_certificate(false).await;
}

#[tokio::test]
async fn test_certificate_refused_stored_entry_kept_though_the_host_acknowledges_its_event() {
    stored_entry_reported_under_a_refused_certificate(true).await;
}

/// Whether `summary` shows a message of `kind` received, its effect applied.
fn received(summary: &paykit_sdk::ReceiveSummary, kind: &str) -> bool {
    summary
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageReceived { kind: k, .. } if k == kind))
}

#[tokio::test]
async fn test_certificate_receipt_for_a_record_not_listed_yet_is_held_until_listed() {
    // The wallet is narrowed to `contacts`. Meanwhile bob's request comes, and the phone
    // pays it: the payer record is written while the wallet can't read it. The wallet is
    // renewed to `pay` and `request` with no bundle, so its records can't be listed, and
    // bob's receipt comes. The record's absence from the wallet's copy decides nothing:
    // the receipt's entry is held, the cursor before it. Once the keys come and the records
    // are listed, the receipt goes into the record, once, and the claim stays settled once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let first = wallet.app().cert().clone();
    let narrow = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &narrow);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), narrow.hash());
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    phone.receive(None).await.unwrap();
    let paid = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    let paykit_sdk::PayOutcome::Paid { attempt_id, .. } = paid else {
        panic!("{paid:?}");
    };
    let obligation = phone.payments().await.unwrap()[0].obligation;
    drop(phone);
    wallet.receive(None).await.unwrap();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        2_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &narrow, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = wallet.receive(None).await.unwrap().report.cursor;
    // Bob's shop reads alice's directory again, which lists the wallet under its new
    // certificate, so the receipt goes to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&sale).await.unwrap().is_some());
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_receipt"), "{summary:?}");
    assert_eq!(summary.report.cursor, before);
    let target = crate::outcome_record::payer_record(&request);
    let key = wallet.app().record_key(&target).unwrap();
    assert!(wallet.app().shared_record(&key).await.unwrap().is_none());
    store_bundle(&w, me, &wider);
    let mut receipts = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        receipts += usize::from(received(&summary, "paykit.payment_receipt"));
    }
    assert_eq!(receipts, 1);
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
    let claim = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap()
        .view;
    assert_eq!(
        claim.status,
        pubky_switch_core::claims::ClaimStatus::Settled
    );
    let attempt = claim.attempt(&attempt_id.to_bytes()).unwrap();
    assert_eq!(attempt.outcome, pubky_switch_core::claims::Outcome::Done);
    assert_eq!(w.rail.executions(&attempt_id), 1);
}

#[tokio::test]
async fn test_certificate_proof_for_a_payee_record_not_listed_yet_is_held_until_listed() {
    // Alice's till sends bob a request; its payee record is written while the wallet, which
    // holds `pay` only, can't read it. The wallet is renewed to read `request` too, with no
    // bundle, and bob pays. The record's absence from the wallet's copy decides nothing: the
    // proof's entry is held, not taken as a reply to a request the wallet didn't send.
    // Once the keys come and the records are listed, the proof goes into the record, once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut payer = w
        .join(&w.bob, "bwallet", AppScopes::new().pay().build().unwrap())
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut till = w
        .join(
            &w.alice,
            "till",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = till
        .send_request(w.bob_id(), crate::common::draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    drop(till);
    wallet.receive(None).await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    let before = wallet.receive(None).await.unwrap().report.cursor;
    // Bob's wallet reads alice's directory again, which lists the wallet under its new
    // certificate, so the acceptance and the proof go to it too.
    w.clock.advance_ms(11 * 60_000);
    payer.receive(None).await.unwrap();
    let paid = payer
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_proof"), "{summary:?}");
    assert_eq!(summary.report.cursor, before);
    store_bundle(&w, me, &wider);
    let mut proofs = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        proofs += usize::from(received(&summary, "paykit.payment_proof"));
    }
    assert_eq!(proofs, 1);
    let sales = wallet.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, paykit_sdk::PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_certificate_restore_taking_a_wider_one_without_keys_stays_unlisted() {
    // After the backup, the wallet is renewed to read `request` too, and alice's till sends
    // bob a request. The restored app takes the renewal from its record at its first read,
    // whose scope keys it lacks, so the client skips the restore's listing: the restore's
    // mark stays, and bob's proof, whose payee record the copy lacks, is held. Once the keys
    // come and a read lists, the mark goes and the proof is taken, once.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut payer = w
        .join(&w.bob, "bwallet", AppScopes::new().pay().build().unwrap())
        .await;
    payer.app_mut().invite(&w.alice_id()).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let mut till = w
        .join(
            &w.alice,
            "till",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = till
        .send_request(w.bob_id(), crate::common::draft("1", w.clock.now_ms()))
        .await
        .unwrap()
        .request
        .unwrap();
    drop(till);
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
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
    let before = restored.receive(None).await.unwrap().report.cursor;
    assert_eq!(restored.app().cert().hash(), wider.hash());
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_some());
    // Bob's wallet reads alice's directory again, which lists the wallet under its new
    // certificate, so the proof goes to it too.
    w.clock.advance_ms(11 * 60_000);
    payer.receive(None).await.unwrap();
    let paid = payer
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let summary = restored.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_proof"), "{summary:?}");
    assert_eq!(summary.report.cursor, before);
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_some());
    store_bundle(&w, me, &wider);
    let mut proofs = 0;
    for _ in 0..3 {
        let summary = restored.receive(None).await.unwrap();
        proofs += usize::from(received(&summary, "paykit.payment_proof"));
    }
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_none());
    assert_eq!(proofs, 1);
    let sales = restored.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, paykit_sdk::PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_certificate_restore_taking_one_without_a_key_no_listing_needs_is_listed() {
    // The renewal after the backup adds `allowance`, whose key the restored app lacks but
    // which reads none of the records the client lists: the client lists at the restore's
    // first read all the same, so the restore's mark goes and the requests show.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    let old = wallet.app().cert().clone();
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().allowance().build().unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
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
    assert_eq!(restored.app().cert().hash(), wider.hash());
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_none());
    assert!(restored.requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_certificate_restore_taking_a_labelled_one_without_keys_keeps_the_mark() {
    // As above, with a labelled selling scope: the client lists a member of
    // `payee_activity` under it, so without its key the restore's listing waits and the
    // mark stays, until the keys come and a read lists.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let backup = wallet.backup().await.unwrap();
    let old = wallet.app().cert().clone();
    let label = paykit_lib::Label::parse("shop").unwrap();
    let wider = renew(
        &w,
        me,
        &AppScopes::new()
            .pay()
            .request_label(&label)
            .build()
            .unwrap(),
        1_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &old, &wider);
    wallet.receive(None).await.unwrap();
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
    assert_eq!(restored.app().cert().hash(), wider.hash());
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_some());
    store_bundle(&w, me, &wider);
    restored.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert!(restored.app().protocol_get(b"nl").await.unwrap().is_none());
}

#[tokio::test]
async fn test_certificate_late_receipt_for_a_record_not_listed_yet_is_held_then_taken_once() {
    // The wallet is narrowed to `contacts` while the phone pays bob's request: the method
    // reports a failure, `failed` is written, and its history then shows the payment made.
    // Past the claim's window the wallet is renewed to `pay` with no bundle, so its copy lacks
    // the payer record, and bob's receipt comes. The record's absence decides nothing: the
    // receipt is held, raising nothing. Once the keys come and the records are listed, it is
    // taken once. The wallet neither made the payment nor saw its claim, which the switch
    // no longer holds: it can't tell the payment was marked failed, and raises nothing.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let me = wallet.app_id();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let first = wallet.app().cert().clone();
    let narrow = renew(&w, me, &AppScopes::new().contacts().build().unwrap(), 1_000);
    let _ = wallet.receive(None).await;
    list(&w, me, &first, &narrow);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), narrow.hash());
    let request = crate::common::request_from(
        &mut shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    phone.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let paykit_sdk::PayOutcome::Failed { attempt_id, .. } = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("failed");
    };
    w.rail.complete_after_failure(&attempt_id);
    let resumed = phone.resume_payments().await.unwrap();
    assert!(
        matches!(resumed.as_slice(), [paykit_sdk::PayOutcome::Paid { .. }]),
        "{resumed:?}"
    );
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(32 * pubky_switch_core::records::DAY_MS);
    let wider = renew(
        &w,
        me,
        &AppScopes::new().pay().request().build().unwrap(),
        2_000,
    );
    wallet.receive(None).await.unwrap();
    list(&w, me, &narrow, &wider);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), wider.hash());
    assert!(summary.scope_keys_awaited, "{summary:?}");
    // Bob's shop reads alice's directory again, which lists the wallet under its new
    // certificate, so the receipt goes to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&sale).await.unwrap().is_some());
    let summary = wallet.receive(None).await.unwrap();
    assert!(!received(&summary, "paykit.payment_receipt"), "{summary:?}");
    assert!(wallet.notices().await.unwrap().is_empty());
    let key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    assert!(wallet.app().shared_record(&key).await.unwrap().is_none());
    store_bundle(&w, me, &wider);
    let mut receipts = 0;
    for _ in 0..3 {
        let summary = wallet.receive(None).await.unwrap();
        receipts += usize::from(received(&summary, "paykit.payment_receipt"));
    }
    assert_eq!(receipts, 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    let _ = attempt_id;
    let value = crate::outcome_record::record_value(&mut wallet, &request)
        .await
        .unwrap();
    assert!(!value["receipt"].is_null(), "{value}");
}

#[tokio::test]
async fn test_certificate_refused_before_any_listing_of_paykit_reads_nothing() {
    // A new wallet's first read takes a renewal Paykit refuses: Paykit lists nothing under
    // it and keeps no certificate as listed under, so a call that only reads is refused
    // rather than answer from a copy it never saw listed.
    let w = World::new();
    let mut wallet = Paykit::join(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let me = wallet.app_id();
    let first = wallet.app().cert().clone();
    let refused = renew(&w, me, &refused_scopes(), 1_000);
    list(&w, me, &first, &refused);
    let _ = wallet.receive(None).await;
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app().cert().hash(), refused.hash());
    assert!(wallet.app().protocol_get(b"lc").await.unwrap().is_none());
    let error = wallet.requests().await.unwrap_err();
    assert!(is_not_now(&error), "{error:?}");
}
