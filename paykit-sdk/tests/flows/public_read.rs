//! Reading a payee's public endpoints (Paykit spec 4): each app's failure is that app's, the
//! other apps' usable endpoints stay available, and the read is bounded over all the apps.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use paykit_sdk::app_records::{
    PublicAppFailureReason, PublicPaymentList, MAX_PUBLIC_ENDPOINTS, MAX_PUBLIC_PAYLOAD_BYTES,
};
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaymentMode, Refusal};
use pubky_switch_client::app::{HomeserverReader, UserConfirmed};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_client::error::{StoreError, TransportError};
use pubky_switch_client::testing::TestHomeserver;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::records::{app_record_path, IDENTITY_PATH};
use serde_json::{json, Map, Value};

use crate::common::{btc, request_from, store, World, HOUR_MS};
use crate::public::{one_endpoint, public_to_bob, publish_app_at};

const DAY_MS: u64 = 24 * HOUR_MS;
const MIB: usize = 1024 * 1024;

#[tokio::test]
async fn test_public_read_labelled_only_app_cannot_offer_an_unlabelled_destination() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.labelled_shop("hosted", "shop1").await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish_now(
        &w,
        "hosted",
        one_endpoint("btc-lightning-bolt12", "lno1labelled"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.outcome;
    assert!(
        read.endpoints.is_empty() && w.rail.all_executions().is_empty(),
        "offered={read:?}; payment={outcome:?}; executions={:?}",
        w.rail.all_executions()
    );
    assert_eq!(read.failures.len(), 1);
    assert_eq!(read.failures[0].app_id, shop.app_id());
    assert!(matches!(
        read.failures[0].reason,
        PublicAppFailureReason::Refused(_)
    ));
    assert!(matches!(
        outcome.unwrap_err().refusal(),
        Some(Refusal::NoDestination(_))
    ));
    assert!(wallet.payments().await.unwrap().is_empty());
}

/// The world's Homeserver, but for the app records it was told it can't serve now, and the
/// one whose read it was told takes a while on the world's clock.
struct FailingRecords {
    inner: Arc<TestHomeserver>,
    failing: Mutex<Vec<String>>,
    on_read: Mutex<BTreeMap<String, Box<dyn FnOnce() + Send>>>,
}

impl FailingRecords {
    /// The record of `app_id` can't be read from now on.
    fn fail(&self, app_id: &[u8; 16]) {
        self.failing.lock().unwrap().push(app_record_path(app_id));
    }

    /// Every record can be read again.
    fn heal(&self) {
        self.failing.lock().unwrap().clear();
    }

    /// Run a change during the next read of this app's record.
    fn on_read(&self, app_id: &[u8; 16], change: impl FnOnce() + Send + 'static) {
        self.on_read
            .lock()
            .unwrap()
            .insert(app_record_path(app_id), Box::new(change));
    }
}

impl HomeserverReader for FailingRecords {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> BoxFuture<'_, Result<Option<Vec<u8>>, TransportError>> {
        if self.failing.lock().unwrap().contains(&path) {
            return Box::pin(async { Err(TransportError::Connect("unreachable".into())) });
        }
        if let Some(change) = self.on_read.lock().unwrap().remove(&path) {
            change();
        }
        self.inner.get(identity, path)
    }
}

/// Alice's wallet, reading Homeservers through a [`FailingRecords`].
async fn wallet_reading_through(w: &World) -> (Paykit, Arc<FailingRecords>) {
    wallet_reading_through_on(w, |base| base).await
}

/// As [`wallet_reading_through`], its store `wrap` of a fresh one.
async fn wallet_reading_through_on(
    w: &World,
    wrap: impl FnOnce(
        Arc<dyn pubky_switch_client::store::StateStore>,
    ) -> Arc<dyn pubky_switch_client::store::StateStore>,
) -> (Paykit, Arc<FailingRecords>) {
    let base = store();
    let app_id = w.wallet_on("wallet", Arc::clone(&base)).await.app_id();
    let base = wrap(base);
    let reads = Arc::new(FailingRecords {
        inner: Arc::clone(&w.homeserver),
        failing: Mutex::new(Vec::new()),
        on_read: Mutex::new(BTreeMap::new()),
    });
    let wallet = w
        .load_with_homeserver(
            &w.alice,
            base,
            app_id,
            Arc::clone(&reads) as Arc<dyn HomeserverReader>,
        )
        .await;
    (wallet, reads)
}

/// Bob's apps `names` joined, each holding `paykit_request` and admitting alice's proofs,
/// in the order bob's `identity.json` lists them.
async fn bob_apps(w: &World, names: &[&'static str]) -> Vec<(&'static str, [u8; 16])> {
    let mut apps = Vec::new();
    for name in names {
        let mut app = w
            .join(
                &w.bob,
                name,
                paykit_sdk::AppScopes::new().request().build().unwrap(),
            )
            .await;
        app.app_mut().invite(&w.alice_id()).await.unwrap();
        apps.push((*name, app.app_id()));
    }
    let doc = w.homeserver.identity_doc(&w.bob_id()).unwrap();
    let order: Vec<[u8; 16]> = doc.body().apps.iter().map(|a| a.app_id).collect();
    apps.sort_by_key(|(_, id)| order.iter().position(|o| o == id).unwrap());
    apps
}

/// A member with `count` public endpoints, each a payload of `len` bytes.
fn many(count: usize, len: usize) -> Value {
    let endpoints: Map<String, Value> = (0..count)
        .map(|i| (format!("btc-e{i}"), Value::String("x".repeat(len))))
        .collect();
    json!({ "public_endpoints": endpoints })
}

/// Bob's app `name` publishes `member`, made now, expiring in 30 days.
fn publish_now(w: &World, name: &str, member: Value) {
    let now = w.clock.now_ms();
    publish_app_at(w, name, member, now, Some(now + 30 * DAY_MS));
}

/// The apps whose endpoints the read offers, in order, each once.
fn offering(read: &PublicPaymentList) -> Vec<[u8; 16]> {
    let mut apps: Vec<[u8; 16]> = Vec::new();
    for endpoint in &read.endpoints {
        if apps.last() != Some(&endpoint.app_id) {
            apps.push(endpoint.app_id);
        }
    }
    apps
}

#[tokio::test]
async fn test_public_read_one_app_failure_keeps_the_others() {
    let w = World::new();
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let (first, second) = (apps[0], apps[1]);
    publish_now(
        &w,
        first.0,
        one_endpoint("btc-lightning-bolt12", "lno1first"),
    );
    publish_now(
        &w,
        second.0,
        one_endpoint("btc-lightning-bolt11", "lnbc1second"),
    );
    // The first app's record can't be read now: that is its failure, and the second app's
    // endpoint stays available.
    reads.fail(&first.1);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [second.1], "{read:?}");
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, first.1);
    assert!(
        matches!(
            read.failures[0].reason,
            PublicAppFailureReason::Homeserver(_)
        ),
        "{read:?}"
    );
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
    // Neither can be read: no public destination, and the refusal names each app's failure.
    reads.fail(&second.1);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert_eq!(text.matches("; app ").count(), 2, "{text}");
    assert!(text.contains("homeserver"), "{text}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_read_refused_or_expired_app_reported_others_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet", "kiosk"]).await;
    let now = w.clock.now_ms();
    // The first app's record expires in an hour; the second serves a record that doesn't
    // verify; the third's is good for 30 days.
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1soon"),
        now,
        Some(now + HOUR_MS),
    );
    w.homeserver
        .put(w.bob_id(), &app_record_path(&apps[1].1), b"{}".to_vec());
    publish_now(
        &w,
        apps[2].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1good"),
    );
    w.clock.advance_ms(HOUR_MS);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[2].1], "{read:?}");
    let failures: Vec<([u8; 16], &PublicAppFailureReason)> = read
        .failures
        .iter()
        .map(|f| (f.app_id, &f.reason))
        .collect();
    assert_eq!(failures.len(), 2, "{read:?}");
    assert_eq!(failures[0].0, apps[0].1);
    assert!(
        matches!(failures[0].1, PublicAppFailureReason::Expired(_)),
        "{read:?}"
    );
    assert_eq!(failures[1].0, apps[1].1);
    assert!(
        matches!(failures[1].1, PublicAppFailureReason::Refused(_)),
        "{read:?}"
    );
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The third app's record expires as well: nothing is left, and the refusal names the
    // failures.
    w.clock.advance_ms(30 * DAY_MS);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(
        text.contains("expired") && text.contains("refused"),
        "{text}"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_read_one_time_judges_every_app() {
    let w = World::new();
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    // Bob's shop fills the read's bound with a record that expires in an hour.
    let shop = bob_apps(&w, &["shop"]).await[0];
    let now = w.clock.now_ms();
    publish_app_at(
        &w,
        shop.0,
        many(MAX_PUBLIC_ENDPOINTS, 8),
        now,
        Some(now + HOUR_MS),
    );
    // The Homeserver keeps serving the identity.json before the seller wallet joined, and bob's switch
    // the newer one listing it after the shop: the seller wallet's record is read after the shop's,
    // and that read takes the hour.
    let older = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    let seller_wallet = bob_apps(&w, &["seller-wallet"]).await[0];
    publish_now(
        &w,
        seller_wallet.0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, older);
    let clock = Arc::clone(&w.clock);
    reads.on_read(&seller_wallet.1, move || clock.advance_ms(HOUR_MS));
    let looked_up = w.homeserver.identity_reads(&w.bob_id());
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    // The copy of identity.json taken first is over 10 minutes old once the records are
    // in: it is looked up again before any app is judged.
    assert!(
        w.homeserver.identity_reads(&w.bob_id()) >= looked_up + 2,
        "{looked_up}"
    );
    // Both are judged at the read's one time: the shop has expired, and the bound goes to
    // the seller wallet.
    assert_eq!(offering(&read), [seller_wallet.1], "{read:?}");
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, shop.1);
    assert!(
        matches!(read.failures[0].reason, PublicAppFailureReason::Expired(_)),
        "{read:?}"
    );
}

#[tokio::test]
async fn test_public_read_identity_source_covers_both_ends_of_every_app_read() {
    // The second app read moves the clock back inside the observation and changes
    // the first app's offer. Both ends of every GET must fit before its bytes are used.
    for (start_offset, end_offset, expected) in [
        (-60_000, 0, "lnbc1current"),
        (0, -60_000, "lnbc1current"),
        (0, 599_999, "lnbc1before"),
        (0, 600_000, "lnbc1current"),
        (0, 0, "lnbc1before"),
    ] {
        let w = Arc::new(World::new());
        let (mut wallet, reads) = wallet_reading_through(&w).await;
        let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
        let (name, first_app) = apps[0];
        let observed = w.clock.now_ms();
        publish_app_at(
            &w,
            name,
            one_endpoint("btc-lightning-bolt11", "lnbc1before"),
            observed,
            Some(observed + DAY_MS),
        );
        let initial = wallet
            .app_mut()
            .identity_of(&w.bob_id(), true)
            .await
            .unwrap();
        assert_eq!(initial.read_at_ms, Some(observed));
        w.homeserver.remove(w.bob_id(), IDENTITY_PATH);
        let start = observed.checked_add_signed(start_offset).unwrap();
        let end = observed.checked_add_signed(end_offset).unwrap();
        w.clock.set_ms(start);
        let started = wallet
            .app_mut()
            .begin_identity_lookup(&w.bob_id(), false)
            .await
            .unwrap();
        assert_eq!(started.identity.doc.hash(), initial.doc.hash());
        assert_eq!(started.identity.read_at_ms, Some(observed));
        assert_eq!(w.clock.now_ms(), start);
        drop(started);
        let clock = Arc::clone(&w.clock);
        reads.on_read(&first_app, move || clock.set_ms(end));
        let changed = Arc::clone(&w);
        reads.on_read(&apps[1].1, move || {
            changed.clock.set_ms(observed + 1);
            publish_app_at(
                &changed,
                name,
                one_endpoint("btc-lightning-bolt11", "lnbc1current"),
                observed + 1,
                Some(observed + DAY_MS),
            );
        });
        let discovery = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        let shown = discovery
            .endpoints
            .iter()
            .find(|endpoint| endpoint.app_id == first_app)
            .unwrap();
        assert_eq!(
            shown.payload.as_str(),
            expected,
            "first GET started at {start_offset:+} ms and ended at {end_offset:+} ms from observation"
        );
    }
}

#[tokio::test]
async fn test_public_read_identity_source_rereads_a_directory_only_pass() {
    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let (name, first) = apps[0];
    publish_now(
        &w,
        name,
        one_endpoint("btc-lightning-bolt11", "lnbc1before"),
    );
    let higher = w.bob_directory_version(&mut wallet).await;
    let started = wallet
        .app_mut()
        .begin_identity_lookup(&w.bob_id(), false)
        .await
        .unwrap();
    assert_eq!(started.identity.doc.hash(), higher.hash());
    assert_eq!(started.identity.read_at_ms, None);
    // Records and the switch read run together. The second app's read happens after the
    // first app's old bytes have returned. The switch then qualifies the very same hash.
    let changed = Arc::clone(&w);
    reads.on_read(&apps[1].1, move || {
        changed.clock.advance_ms(1);
        publish_now(
            &changed,
            name,
            one_endpoint("btc-lightning-bolt11", "lnbc1current"),
        );
    });
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    let endpoint = read
        .endpoints
        .iter()
        .find(|endpoint| endpoint.app_id == first)
        .unwrap();
    assert_eq!(endpoint.payload.as_str(), "lnbc1current");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_read_failed_lookup_time_does_not_shorten_the_homeserver_observation() {
    use pubky_switch_client::app::SwitchCopy;
    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let (name, first) = apps[0];
    let observed = w.clock.now_ms();
    publish_now(&w, name, one_endpoint("btc-lightning-bolt11", "lnbc1first"));
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.homeserver_read_ms, Some(observed));
    w.homeserver.remove(w.bob_id(), IDENTITY_PATH);
    w.router.remove(crate::common::BOB_ORIGIN);
    w.clock.set_ms(observed + 4 * 60_000);
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.switch_copy, SwitchCopy::Unavailable);
    assert_eq!(source.switch_lookup_ms, w.clock.now_ms());
    assert_eq!(source.homeserver_read_ms, Some(observed));

    let changed = Arc::clone(&w);
    reads.on_read(&apps[1].1, move || {
        changed.clock.set_ms(observed + 2 * 60_000);
        publish_now(
            &changed,
            name,
            one_endpoint("btc-lightning-bolt11", "lnbc1later"),
        );
    });
    // Both GETs are inside the exact Homeserver observation. The failed attempt
    // supplies no currency of its own and cannot require a repeat of the first GET.
    let read = wallet
        .public_endpoints(&w.bob_id(), Some(UserConfirmed::new()))
        .await
        .unwrap();
    let endpoint = read.endpoints.iter().find(|e| e.app_id == first).unwrap();
    assert_eq!(endpoint.payload.as_str(), "lnbc1first");
}

#[tokio::test]
async fn test_public_read_identity_source_rereads_after_rollback_past_either_observation() {
    use pubky_switch_client::testing::START_MS;
    use pubky_switch_core::records::IdentityDoc;

    for later_homeserver in [true, false] {
        let base = START_MS;
        let bob_clock = Arc::new(TestClock::new(base));
        let w = Arc::new(World::on_clocks(
            Arc::new(TestClock::new(base)),
            Arc::clone(&bob_clock),
        ));
        let (mut wallet, reads) = wallet_reading_through(&w).await;
        let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
        let (name, first) = apps[0];
        let expiry = Some(base + DAY_MS);
        publish_app_at(
            &w,
            name,
            one_endpoint("btc-lightning-bolt11", "lnbc1before"),
            base,
            expiry,
        );
        // A higher version known only from the Homeserver. A later switch lookup
        // can complete without supplying that version again.
        let mut body = w.bob.body();
        body.version += 1;
        let higher = IdentityDoc::sign(w.bob.identity().key(), body).unwrap();
        w.homeserver.publish(&higher).unwrap();
        wallet
            .app_mut()
            .identity_of(&w.bob_id(), true)
            .await
            .unwrap();
        let later = base + 4 * 60_000;
        w.clock.set_ms(later);
        if !later_homeserver {
            bob_clock.set_ms(later);
            w.homeserver.remove(w.bob_id(), IDENTITY_PATH);
        }
        let source = wallet
            .app_mut()
            .identity_of(&w.bob_id(), true)
            .await
            .unwrap();
        let (version_at, lookup_at) = if later_homeserver {
            (later, base)
        } else {
            (base, later)
        };
        assert_eq!(
            (source.read_at_ms, source.switch_lookup_ms),
            (Some(version_at), lookup_at)
        );
        let changed = Arc::clone(&w);
        reads.on_read(&apps[1].1, move || {
            changed.clock.set_ms(base + 2 * 60_000);
            changed.homeserver.publish(&higher).unwrap();
            publish_app_at(
                &changed,
                name,
                one_endpoint("btc-lightning-bolt11", "lnbc1after"),
                base + 1,
                expiry,
            );
        });
        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        let endpoint = read
            .endpoints
            .iter()
            .find(|endpoint| endpoint.app_id == first)
            .unwrap();
        assert_eq!(
            endpoint.payload.as_str(),
            "lnbc1after",
            "later Homeserver {later_homeserver}"
        );
    }
}

#[tokio::test]
async fn test_public_read_identity_source_rechecks_after_a_newly_listed_apps_read() {
    use pubky_switch_core::records::RemovedApp;
    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    bob_apps(&w, &["shop"]).await;
    let first_doc = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    let seller_wallet = bob_apps(&w, &["seller-wallet"]).await[0];
    publish_now(
        &w,
        seller_wallet.0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, first_doc);
    let before = w.clock.now_ms();
    w.clock.advance_ms(4 * 60_000);
    // The switch adds the seller wallet to the second pass. Its record returns after the
    // clock moves behind that lookup. A new lookup now shows the seller wallet's removal.
    let changed = Arc::clone(&w);
    reads.on_read(&seller_wallet.1, move || {
        changed.clock.set_ms(before + 60_000);
        changed
            .bob
            .next_doc(|body| {
                body.apps.retain(|a| a.app_id != seller_wallet.1);
                body.removed.push(RemovedApp {
                    app_id: seller_wallet.1,
                    distrust_from: changed.clock.now_ms(),
                });
            })
            .unwrap();
    });
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert!(reads.on_read.lock().unwrap().is_empty());
    assert!(!offering(&read).contains(&seller_wallet.1), "{read:?}");
}

#[tokio::test]
async fn test_public_read_fetches_the_renewed_apps_record() {
    read_during_renewal(false).await;
}

#[tokio::test]
async fn test_public_read_fetches_the_replacement_after_an_open_renewal_closes() {
    read_during_renewal(true).await;
}

async fn read_during_renewal(already_listed: bool) {
    use pubky_switch_client::testing::{issue_cert, AppSpec};
    use pubky_switch_core::ops::OpKind;
    use pubky_switch_core::records::{AppRecord, RenewedCert};

    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let (name, id) = apps[0];
    publish_now(&w, name, one_endpoint("btc-lightning-bolt11", "lnbc1old"));
    let keys = w.bob.identity().app_keys(name);
    let held = w.bob.switch().app(&id).record.unwrap();
    let old = AppRecord::verify(&held, &w.bob_id()).unwrap();
    paykit_lib::scopes::check_certificate(old.cert()).unwrap();
    let scopes: Vec<_> = old.cert().scopes().iter().map(String::as_str).collect();
    let spec = AppSpec::new(&scopes).with_permissions(old.cert().permissions());
    w.clock.advance_ms(1);
    let renewed = issue_cert(w.bob.identity().key(), &keys, &spec, w.clock.now_ms()).unwrap();
    paykit_lib::scopes::check_certificate(&renewed).unwrap();
    let mut replacement = old.body().clone();
    replacement.cert = renewed.signed().clone();
    let list_renewal = move |w: &World| {
        w.bob
            .switch()
            .signer_op(OpKind::CertPut {
                target_app_id: id,
                cert: renewed.signed().clone(),
            })
            .unwrap();
        w.bob
            .next_doc(|body| {
                body.apps.iter_mut().find(|a| a.app_id == id).unwrap().cert = renewed.hash();
                body.renewed.push(RenewedCert {
                    app_id: id,
                    cert: old.cert().hash(),
                    until: None,
                });
            })
            .unwrap();
    };
    if already_listed {
        list_renewal(&w);
    }
    // The first app's record has been read. While the wallet waits for the second app,
    // Bob publishes a replacement and closes the renewal of the earlier certificate.
    let world = Arc::clone(&w);
    reads.on_read(&apps[1].1, move || {
        world.clock.advance_ms(HOUR_MS);
        if !already_listed {
            list_renewal(&world);
        }
        let now = world.clock.now_ms();
        replacement.updated_at = now;
        replacement.expires_at = Some(now + 30 * DAY_MS);
        let member = one_endpoint("btc-lightning-bolt11", "lnbc1renewed");
        replacement
            .protocols
            .insert("paykit".into(), member.as_object().unwrap().clone());
        let current = AppRecord::sign(&keys, replacement).unwrap();
        world.homeserver.put(
            world.bob_id(),
            &app_record_path(&id),
            serde_json::to_vec(current.signed()).unwrap(),
        );
        world
            .bob
            .next_doc(|body| {
                body.close_renewals(&id, now);
            })
            .unwrap();
    });
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [id], "{read:?}");
    assert_eq!(read.endpoints[0].payload.as_str(), "lnbc1renewed");
    assert!(read.failures.is_empty(), "{read:?}");
}

#[tokio::test]
async fn test_public_read_refreshes_after_a_newly_listed_apps_slow_read() {
    use pubky_switch_core::records::RemovedApp;

    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    bob_apps(&w, &["shop"]).await;
    let first_doc = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    let seller_wallet = bob_apps(&w, &["seller-wallet"]).await[0];
    publish_now(
        &w,
        seller_wallet.0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, first_doc);

    // The switch lists the seller wallet. While its record is read a kiosk joins, so the next
    // identity lookup must also collect the kiosk's record.
    let kiosk = w.bob.identity().app_keys("kiosk").app_id_bytes();
    let world = Arc::clone(&w);
    reads.on_read(&seller_wallet.1, move || {
        world.clock.advance_ms(HOUR_MS);
        let scopes = paykit_sdk::AppScopes::new().request().build().unwrap();
        world.approve(&world.bob, "kiosk", &scopes);
    });
    // That read outlasts the new identity too. The final document no longer lists the seller wallet.
    let world = Arc::clone(&w);
    reads.on_read(&kiosk, move || {
        world.clock.advance_ms(HOUR_MS);
        world
            .bob
            .next_doc(|body| {
                body.apps.retain(|listed| listed.app_id != seller_wallet.1);
                body.removed.push(RemovedApp {
                    app_id: seller_wallet.1,
                    distrust_from: world.clock.now_ms(),
                });
            })
            .unwrap();
    });
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert!(reads.on_read.lock().unwrap().is_empty());
    assert!(!offering(&read).contains(&seller_wallet.1), "{read:?}");
}

#[tokio::test]
async fn test_public_read_refuses_when_the_directory_keeps_expiring() {
    let w = Arc::new(World::new());
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    bob_apps(&w, &["shop"]).await;
    let first_doc = w.homeserver.file(w.bob_id(), IDENTITY_PATH).unwrap();
    bob_apps(&w, &["seller-wallet"]).await;
    w.homeserver.put(w.bob_id(), IDENTITY_PATH, first_doc);
    let names = [
        "seller-wallet",
        "kiosk",
        "counter",
        "desk",
        "terminal",
        "office",
    ];
    for pair in names.windows(2) {
        let id = w.bob.identity().app_keys(pair[0]).app_id_bytes();
        let next = pair[1];
        let world = Arc::clone(&w);
        reads.on_read(&id, move || {
            world.clock.advance_ms(HOUR_MS);
            let scopes = paykit_sdk::AppScopes::new().request().build().unwrap();
            world.approve(&world.bob, next, &scopes);
        });
    }
    let lookups = w.homeserver.identity_reads(&w.bob_id());
    let err = wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{err:?}"
    );
    assert!(w.homeserver.identity_reads(&w.bob_id()) - lookups <= 4);
}

#[tokio::test]
async fn test_public_read_endpoint_bound_reports_an_app_that_does_not_fit() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet", "kiosk"]).await;
    // The second app's endpoints don't fit after the first's; the third's fill what is left
    // exactly.
    let rest = 56;
    publish_now(&w, apps[0].0, many(MAX_PUBLIC_ENDPOINTS - rest, 8));
    publish_now(&w, apps[1].0, many(rest + 1, 8));
    publish_now(&w, apps[2].0, many(rest, 8));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), MAX_PUBLIC_ENDPOINTS);
    assert_eq!(offering(&read), [apps[0].1, apps[2].1]);
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, apps[1].1);
    assert_eq!(read.failures[0].reason, PublicAppFailureReason::OverBound);
}

#[tokio::test]
async fn test_public_read_payload_bound_reports_an_app_that_does_not_fit() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet", "kiosk"]).await;
    // 3 MiB, then 2 MiB that don't fit, then 1 MiB that fills the 4 MiB exactly.
    publish_now(&w, apps[0].0, many(1, MAX_PUBLIC_PAYLOAD_BYTES - MIB));
    publish_now(&w, apps[1].0, many(1, 2 * MIB));
    publish_now(&w, apps[2].0, many(1, MIB));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[2].1]);
    let bytes: usize = read
        .endpoints
        .iter()
        .map(|e| e.payload.as_str().len())
        .sum();
    assert_eq!(bytes, MAX_PUBLIC_PAYLOAD_BYTES);
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, apps[1].1);
    assert_eq!(read.failures[0].reason, PublicAppFailureReason::OverBound);
}

#[tokio::test]
async fn test_public_read_refused_record_gives_no_endpoints_of_the_held_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    publish_now(&w, apps[0].0, many(1, 8));
    publish_now(&w, apps[1].0, many(2, 8));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    // The seller wallet's newer record is refused: that is its failure, once, and the record it holds
    // gives no endpoints, so it takes none of the room the shop's newer record leaves.
    w.clock.advance_ms(1000);
    publish_now(&w, apps[0].0, many(MAX_PUBLIC_ENDPOINTS - 1, 8));
    publish_now(
        &w,
        apps[1].0,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1]);
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, apps[1].1);
    assert!(
        matches!(&read.failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("Paykit wire 3.2")),
        "{read:?}"
    );
}

#[tokio::test]
async fn test_public_read_discovery_and_payment_agree_on_a_failed_app() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let seller_wallet = bob_apps(&w, &["seller-wallet"]).await;
    let apps = [("shop", shop.app_id()), seller_wallet[0]];
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1shop"),
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    // The shop's newer record is refused: discovery gives the shop's failure and only the
    // seller wallet's endpoint, not the shop's record kept.
    w.clock.advance_ms(1000);
    publish_now(
        &w,
        apps[0].0,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, apps[0].1);
    // A request requiring the shop has no public destination, as discovery shows.
    let draft = paykit_sdk::RequestDraft::new(
        btc("1"),
        paykit_lib::PaymentReference::new("order-1").unwrap(),
        paykit_lib::bodies::RequestDestination::RequiredApp(apps[0].1),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + DAY_MS);
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let public = || PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new()));
    let refused = wallet
        .pay_request(&request, public())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(text.contains("Paykit wire 3.2"), "{text}");
    assert!(w.rail.all_executions().is_empty());
    // A payment to any app pays the seller wallet's endpoint, the one discovery offers, and tells
    // the shop's failure with it.
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    assert_eq!(paid.public_failures, read.failures);
    let outcome = paid.outcome.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
}

#[tokio::test]
async fn test_public_read_required_app_in_conflict_pays_the_earlier_record() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_app_at(
        &w,
        "shop",
        one_endpoint("btc-lightning-bolt11", "lnbc1a"),
        t0,
        expiry,
    );
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    publish_app_at(
        &w,
        "shop",
        one_endpoint("btc-lightning-bolt12", "lno1b"),
        t0 + 1000,
        expiry,
    );
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    // Two bodies at one updated_at: the shop keeps the earlier record it holds (Switch spec
    // 4.2), so a request requiring it pays that record's endpoint and tells the conflict.
    publish_app_at(
        &w,
        "shop",
        one_endpoint("btc-bitcoin-p2wpkh", "bc1q"),
        t0 + 1000,
        expiry,
    );
    let draft = paykit_sdk::RequestDraft::new(
        btc("1"),
        paykit_lib::PaymentReference::new("order-1").unwrap(),
        paykit_lib::bodies::RequestDestination::RequiredApp(shop.app_id()),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + DAY_MS);
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let public = PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new()));
    let paid = wallet.pay_request(&request, public).await;
    let outcome = paid.outcome.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
    let failures = &paid.public_failures;
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        matches!(&failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("Switch spec 4.2")),
        "{failures:?}"
    );
}

#[tokio::test]
async fn test_public_read_failures_told_with_every_payment_result() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1shop"),
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    w.clock.advance_ms(1000);
    publish_now(
        &w,
        apps[0].0,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
    );
    let shop_failed = |failures: &[paykit_sdk::app_records::PublicAppFailure]| {
        failures.len() == 1 && failures[0].app_id == apps[0].1
    };
    // The payment method fails the payment: the outcome isn't `Paid`, and the shop's
    // failure is still told.
    w.rail.next(paykit_sdk::rail::Behaviour::Fail);
    let failed = wallet.pay_without_request(public_to_bob(&w)).await;
    assert!(
        matches!(failed.outcome, Ok(PayOutcome::Failed { .. })),
        "{failed:?}"
    );
    assert!(shop_failed(&failed.public_failures));
    // The payer's switch can't be reached for the attempt: an error, and the shop's
    // failure is still told.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let errored = wallet.pay_without_request(public_to_bob(&w)).await;
    assert!(errored.outcome.is_err(), "{errored:?}");
    assert!(shop_failed(&errored.public_failures));
    // A payment that goes through tells it too.
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    assert!(
        matches!(paid.outcome, Ok(PayOutcome::Paid { .. })),
        "{paid:?}"
    );
    assert!(shop_failed(&paid.public_failures));
}

#[tokio::test]
async fn test_public_read_required_app_cached_record_not_paid_over_a_refused_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish_now(&w, "shop", one_endpoint("btc-lightning-bolt12", "lno1shop"));
    // The wallet keeps the shop's record holding the endpoint.
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1);
    w.clock.advance_ms(1000);
    publish_now(
        &w,
        "shop",
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
    );
    let draft = paykit_sdk::RequestDraft::new(
        btc("1"),
        paykit_lib::PaymentReference::new("order-1").unwrap(),
        paykit_lib::bodies::RequestDestination::RequiredApp(shop.app_id()),
    )
    .with_proposal_expires_at_ms(w.clock.now_ms() + DAY_MS);
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    let public = PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new()));
    let refused = wallet
        .pay_request(&request, public)
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_public_read_payment_tells_a_conflict_it_first_finds() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1a"),
        t0,
        expiry,
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    // The wallet's first payment reads both records and pays one.
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let first_attempt = outcome.attempt_id();
    // Another body of the shop's record at the same updated_at: the next payment is the
    // first read to find the conflict, pays the seller wallet, and tells the conflict.
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1b"),
        t0,
        expiry,
    );
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    let public_failures = &paid.public_failures;
    assert!(
        matches!(paid.outcome, Ok(PayOutcome::Paid { .. })),
        "{paid:?}"
    );
    let second_attempt = paid.outcome.as_ref().unwrap().attempt_id();
    assert_ne!(first_attempt, second_attempt);
    assert_eq!(public_failures.len(), 1, "{public_failures:?}");
    assert_eq!(public_failures[0].app_id, apps[0].1);
    assert!(
        matches!(&public_failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("Switch spec 4.2")),
        "{public_failures:?}"
    );
    let payments = wallet.payments().await.unwrap();
    assert_eq!(payments.len(), 2);
    let second = payments
        .iter()
        .find(|payment| payment.attempt_id == second_attempt)
        .unwrap();
    assert_eq!(second.endpoint_id.as_str(), "btc-lightning-bolt11");
    assert_eq!(
        w.rail.all_executions(),
        BTreeMap::from([(first_attempt, 1), (second_attempt, 1)])
    );
}

#[tokio::test]
async fn test_public_read_withdrawal_served_after_a_clock_rollback_is_used() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let app = ("shop", shop.app_id());
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    // The wallet keeps T0, offering A, then T2, an hour later, offering A and B.
    let a_and_b = json!({ "public_endpoints": {
        "btc-lightning-bolt12": "lno1a",
        "btc-lightning-bolt11": "lnbc1b"
    } });
    publish_app_at(
        &w,
        app.0,
        one_endpoint("btc-lightning-bolt12", "lno1a"),
        t0,
        expiry,
    );
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    w.clock.set_ms(t0 + HOUR_MS);
    publish_app_at(&w, app.0, a_and_b, t0 + HOUR_MS, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 2);
    // The wallet's clock goes back below T2, which the view sets aside, and the Homeserver
    // serves T1, which withdraws A: T1 is used, not T0, and A is never offered.
    w.clock.set_ms(t0 + 20 * 60_000);
    publish_app_at(
        &w,
        app.0,
        one_endpoint("btc-lightning-bolt11", "lnbc1b"),
        t0 + 10 * 60_000,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    let ids: Vec<&str> = read
        .endpoints
        .iter()
        .map(|e| e.endpoint_id.as_str())
        .collect();
    assert_eq!(ids, ["btc-lightning-bolt11"], "{read:?}");
    assert!(read.failures.is_empty(), "{read:?}");
    // A payment pays from the same record discovery shows.
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
    // The clock catches up after a reload. Two bodies at T2 must fall back to T1.
    let wallet_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, storage, wallet_id, Arc::clone(&w.clock))
        .await;
    w.clock.set_ms(t0 + HOUR_MS);
    publish_app_at(
        &w,
        app.0,
        one_endpoint("btc-onchain-address", "bc1qother"),
        t0 + HOUR_MS,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert!(matches!(
        read.failures[0].reason,
        PublicAppFailureReason::Refused(_)
    ));
    assert_eq!(read.endpoints.len(), 1, "{read:?}");
    assert_eq!(
        read.endpoints[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
}

#[tokio::test]
async fn test_public_read_large_records_kept_per_app_within_the_store_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    // The shop renews a 3 MiB record several times: more together than the store takes in
    // one value, so the oldest are dropped, and both apps stay readable.
    for _ in 0..6 {
        w.clock.advance_ms(1000);
        publish_now(&w, apps[0].0, many(1, 3 * MIB));
        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
        assert!(read.failures.is_empty(), "{:?}", read.failures);
    }
    // The shop's newer nine MiB of payloads exceed both the history and read limits.
    w.clock.advance_ms(1000);
    publish_now(&w, apps[0].0, many(3, 3 * MIB));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    assert_eq!(read.failures.len(), 1, "{:?}", read.failures);
    assert_eq!(read.failures[0].app_id, apps[0].1);
    assert_eq!(read.failures[0].reason, PublicAppFailureReason::OverBound);
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    assert_eq!(paid.public_failures, read.failures);
    assert!(matches!(paid.outcome.unwrap(), PayOutcome::Paid { .. }));
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_read_unretained_record_reports_its_remaining_endpoint_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet", "kiosk"]).await;
    publish_now(&w, apps[0].0, many(MAX_PUBLIC_ENDPOINTS - 1, 8));
    publish_now(
        &w,
        apps[2].0,
        one_endpoint("btc-lightning-bolt12", "lno1kiosk"),
    );

    // The unknown signed member takes history space but contributes no endpoint payload.
    // One endpoint would fit in the read. Two would exceed the slot the shop leaves.
    for count in [1, 2] {
        w.clock.advance_ms(1000);
        let mut member = many(count, 8);
        member["description"] = Value::String("a".repeat(9 * MIB));
        publish_now(&w, apps[1].0, member);

        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        assert_eq!(offering(&read), [apps[0].1, apps[2].1]);
        assert_eq!(read.endpoints.len(), MAX_PUBLIC_ENDPOINTS);
        assert_eq!(read.failures.len(), 1, "{:?}", read.failures);
        assert_eq!(read.failures[0].app_id, apps[1].1);
        let reason = &read.failures[0].reason;
        if count == 1 {
            assert!(
                matches!(reason, PublicAppFailureReason::Refused(rule)
                if rule.contains("a reader keeps")),
                "{reason:?}"
            );
        } else {
            assert_eq!(reason, &PublicAppFailureReason::OverBound);
        }
    }
}

#[tokio::test]
async fn test_public_read_unretained_conflicting_or_expired_record_is_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let original_time = w.clock.now_ms();
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1shop"),
        original_time,
        Some(original_time + DAY_MS),
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);

    w.clock.advance_ms(1000);
    // The first body conflicts with the shop's retained record. The second is newer but
    // has expired. Neither can supply usable public endpoints, whatever their size.
    for (updated, expires) in [
        (original_time, original_time + DAY_MS),
        (original_time + 1, original_time + 2),
    ] {
        publish_app_at(&w, apps[0].0, many(3, 3 * MIB), updated, Some(expires));
        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        assert_eq!(offering(&read), [apps[1].1]);
        assert_eq!(read.failures.len(), 1, "{:?}", read.failures);
        assert_eq!(read.failures[0].app_id, apps[0].1);
        assert!(
            matches!(&read.failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("a reader keeps")),
            "updated {updated}, expires {expires}, time {original_time}: {:?}",
            read.failures
        );
    }
}

#[tokio::test]
async fn test_public_read_unretained_conflict_still_refused_after_a_reload() {
    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let original_time = w.clock.now_ms();
    let expiry = Some(original_time + DAY_MS);
    let original = one_endpoint("btc-lightning-bolt11", "lnbc1shop");
    publish_app_at(&w, apps[0].0, original.clone(), original_time, expiry);
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);

    // A second body at the original time conflicts with it and is too large to keep.
    w.clock.advance_ms(1000);
    publish_app_at(&w, apps[0].0, many(3, 3 * MIB), original_time, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);

    // After a reload the Homeserver serves the original body again. The conflict seen before
    // still holds it: neither body is used.
    let wallet_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, storage, wallet_id, Arc::clone(&w.clock))
        .await;
    publish_app_at(&w, apps[0].0, original, original_time, expiry);
    // Refused naming the time a later record must pass.
    let floor = "after that time can be used";
    let at = format!("{original_time} ms");
    for _ in 0..2 {
        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        assert_eq!(offering(&read), [apps[1].1], "{read:?}");
        assert_eq!(read.failures.len(), 1, "{:?}", read.failures);
        assert_eq!(read.failures[0].app_id, apps[0].1);
        assert!(
            matches!(&read.failures[0].reason, PublicAppFailureReason::Refused(rule)
                if rule.contains("a reader keeps") && rule.contains(&at) && rule.contains(floor)),
            "{:?}",
            read.failures
        );
    }

    // A strictly later record ends the hold.
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-onchain-address", "bc1qlater"),
        original_time + 2000,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    assert!(read.failures.is_empty(), "{:?}", read.failures);
}

/// Whether the read refuses `app` alone for a group the reader couldn't keep.
fn held_for_an_unkept_group(read: &PublicPaymentList, app: [u8; 16]) -> bool {
    read.failures.len() == 1
        && read.failures[0].app_id == app
        && matches!(&read.failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("a reader keeps"))
}

#[tokio::test]
async fn test_public_read_record_at_an_unkept_groups_time_stays_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let start = w.clock.now_ms();
    let expiry = Some(start + DAY_MS);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1shop"),
        start,
        expiry,
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);

    // A later group too large to keep, then a small body at that same time: the small one
    // may conflict with the bodies not kept, so it is never used.
    w.clock.advance_ms(1000);
    let group = start + 1000;
    publish_app_at(&w, apps[0].0, many(3, 3 * MIB), group, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-onchain-address", "bc1qsame"),
        group,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1], "{read:?}");
    assert!(
        held_for_an_unkept_group(&read, apps[0].1),
        "{:?}",
        read.failures
    );
}

#[tokio::test]
async fn test_public_read_earlier_unkept_group_keeps_the_later_hold() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let start = w.clock.now_ms();
    let expiry = Some(start + DAY_MS);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1shop"),
        start,
        expiry,
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);

    // A group too large to keep, then an earlier one too large to keep: the hold stays at
    // the later group, so a small body at the later time is still not used.
    w.clock.advance_ms(2000);
    publish_app_at(&w, apps[0].0, many(3, 3 * MIB), start + 2000, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    publish_app_at(&w, apps[0].0, many(3, 3 * MIB), start + 1000, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-onchain-address", "bc1qsame"),
        start + 2000,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1], "{read:?}");
    assert!(
        held_for_an_unkept_group(&read, apps[0].1),
        "{:?}",
        read.failures
    );
}

/// A store that moves the world's clock on at the first read, once armed, of the records a
/// reader keeps of a payee app (`ar/`).
struct ClockOnSeenRead {
    inner: Arc<dyn pubky_switch_client::store::StateStore>,
    clock: Arc<TestClock>,
    advance_ms: u64,
    armed: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for ClockOnSeenRead {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        if key.windows(4).any(|w| w == b"/ar/")
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.clock.advance_ms(self.advance_ms);
        }
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, StoreError> {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(&self, batch: pubky_switch_client::store::WriteBatch) -> Result<(), StoreError> {
        self.inner.commit(batch)
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, StoreError> {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, StoreError> {
        self.inner.commit_if(ns, owner, batch)
    }
}

#[tokio::test]
async fn test_public_read_time_taken_once_before_any_app_is_judged() {
    for (advance_ms, source_still_current) in [(2 * 60_000, true), (2 * HOUR_MS, false)] {
        let w = World::new();
        let seen = Arc::new(std::sync::OnceLock::<Arc<ClockOnSeenRead>>::new());
        let clock = Arc::clone(&w.clock);
        let kept = Arc::clone(&seen);
        let (mut wallet, _reads) = wallet_reading_through_on(&w, move |base| {
            let store = Arc::new(ClockOnSeenRead {
                inner: base,
                clock,
                advance_ms,
                armed: std::sync::atomic::AtomicBool::new(false),
            });
            let _ = kept.set(Arc::clone(&store));
            store
        })
        .await;
        let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
        let now = w.clock.now_ms();
        publish_now(
            &w,
            apps[0].0,
            one_endpoint("btc-lightning-bolt12", "lno1shop"),
        );
        // The seller wallet's record expires in a minute.
        publish_app_at(
            &w,
            apps[1].0,
            one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
            now,
            Some(now + 60_000),
        );
        // Both apps are judged at the same time. If the store read outlasts the identity
        // observation, the next pass must judge both again with current authority.
        seen.get()
            .unwrap()
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        if source_still_current {
            assert_eq!(offering(&read), [apps[0].1, apps[1].1], "{read:?}");
            assert!(read.failures.is_empty(), "{read:?}");
        } else {
            assert_eq!(offering(&read), [apps[0].1], "{read:?}");
            assert!(
                matches!(read.failures.as_slice(), [failure] if failure.app_id == apps[1].1
            && matches!(failure.reason, PublicAppFailureReason::Expired(_))),
                "{read:?}"
            );
        }
    }
}

#[tokio::test]
async fn test_public_read_failures_found_before_an_error_come_with_it() {
    let w = World::new();
    let fault = Arc::new(std::sync::OnceLock::<
        Arc<pubky_switch_client::store::FaultStore>,
    >::new());
    let kept = Arc::clone(&fault);
    let (mut wallet, reads) = wallet_reading_through_on(&w, move |base| {
        let store = Arc::new(pubky_switch_client::store::FaultStore::new(
            base,
            pubky_switch_client::store::Fault::None,
        ));
        let _ = kept.set(Arc::clone(&store));
        store
    })
    .await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1shop"),
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    // The shop's record can't be read, then keeping the seller wallet's record fails: the payment
    // stops with the store's error, and the shop's failure comes with it.
    reads.fail(&apps[0].1);
    fault.get().unwrap().fail_put_once(b"p/paykit/ar/");
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    assert!(paid.outcome.is_err(), "{paid:?}");
    assert_eq!(paid.public_failures.len(), 1, "{paid:?}");
    assert_eq!(paid.public_failures[0].app_id, apps[0].1);
    assert!(
        matches!(
            paid.public_failures[0].reason,
            PublicAppFailureReason::Homeserver(_)
        ),
        "{paid:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_read_required_app_failure_leaves_no_public_destination() {
    let w = World::new();
    let (mut wallet, reads) = wallet_reading_through(&w).await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let apps = bob_apps(&w, &["seller-wallet"]).await;
    publish_now(&w, "shop", one_endpoint("btc-lightning-bolt12", "lno1shop"));
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1wallet"),
    );
    let requiring = |now: u64| {
        paykit_sdk::RequestDraft::new(
            btc("1"),
            paykit_lib::PaymentReference::new("order-1").unwrap(),
            paykit_lib::bodies::RequestDestination::RequiredApp(shop.app_id()),
        )
        .with_proposal_expires_at_ms(now + DAY_MS)
    };
    let public = || PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new()));
    // The request requires the shop, whose record can't be read now: the seller wallet's endpoint is
    // fine, but only the shop's record is read, so there is no public destination.
    let draft = requiring(w.clock.now_ms());
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    reads.fail(&shop.app_id());
    let refused = wallet
        .pay_request(&request, public())
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert_eq!(text.matches("; app ").count(), 1, "{text}");
    assert!(text.contains("homeserver"), "{text}");
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    // Once the shop's record reads again, its endpoint is paid, never the seller wallet's.
    reads.heal();
    let outcome = wallet
        .pay_request(&request, public())
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
}

#[tokio::test]
async fn test_public_read_app_without_record_has_none_whatever_was_seen() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop"]).await;
    publish_now(&w, "shop", one_endpoint("btc-lightning-bolt12", "lno1shop"));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1]);
    // The record is gone from the Homeserver: the app has no public endpoints, though its
    // record was seen, and it isn't a failure.
    w.homeserver
        .remove(w.bob_id(), &app_record_path(&apps[0].1));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read, PublicPaymentList::default());
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_public_read_unretained_conflict_member_never_the_fallback_of_a_later_conflict() {
    // The shop's record has a second body at its time too large to keep, so that time's
    // group is unknown. A later record replaces it, and then conflicts with a body at its
    // own time: the earlier record, which may itself be in conflict, is never the record
    // used meanwhile (Switch spec 4.2).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "seller-wallet"]).await;
    let original_time = w.clock.now_ms();
    let expiry = Some(original_time + DAY_MS);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1shop"),
        original_time,
        expiry,
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    w.clock.advance_ms(1000);
    publish_app_at(&w, apps[0].0, many(3, 3 * MIB), original_time, expiry);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    // A later record is kept and used; then a body at its own time conflicts with it.
    let later = w.clock.now_ms();
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-bitcoin-p2wpkh", "bc1qlater"),
        later,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-bitcoin-p2tr", "bc1plater"),
        later,
        expiry,
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1], "{read:?}");
}
