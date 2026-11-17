//! Reading a payee's public endpoints (Paykit spec 4): each app's failure is that app's, the
//! other apps' usable endpoints stay available, and the read is bounded over all the apps.

use std::sync::{Arc, Mutex};

use paykit_sdk::app_records::{
    PublicAppFailureReason, PublicPaymentList, MAX_PUBLIC_ENDPOINTS, MAX_PUBLIC_PAYLOAD_BYTES,
};
use paykit_sdk::{PayOptions, PayOutcome, Paykit, PaymentMode, Refusal};
use pubky_switch_client::app::{HomeserverReader, UserConfirmed};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::TransportError;
use pubky_switch_client::testing::TestHomeserver;
use pubky_switch_client::transport::BoxFuture;
use pubky_switch_core::records::app_record_path;
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
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await;
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

/// The world's Homeserver, but for the app records it was told it can't serve now.
struct FailingRecords {
    inner: Arc<TestHomeserver>,
    failing: Mutex<Vec<String>>,
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
        self.inner.get(identity, path)
    }
}

/// Alice's wallet, reading Homeservers through a [`FailingRecords`].
async fn wallet_reading_through(w: &World) -> (Paykit, Arc<FailingRecords>) {
    let base = store();
    let app_id = w.wallet_on("wallet", Arc::clone(&base)).await.app_id();
    let reads = Arc::new(FailingRecords {
        inner: Arc::clone(&w.homeserver),
        failing: Mutex::new(Vec::new()),
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
    let apps = bob_apps(&w, &["shop", "till"]).await;
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
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
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
    let apps = bob_apps(&w, &["shop", "till", "kiosk"]).await;
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
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // The third app's record expires as well: nothing is left, and the refusal names the
    // failures.
    w.clock.advance_ms(30 * DAY_MS);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
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
async fn test_public_read_endpoint_bound_reports_an_app_that_does_not_fit() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "till", "kiosk"]).await;
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
    let apps = bob_apps(&w, &["shop", "till", "kiosk"]).await;
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
    let apps = bob_apps(&w, &["shop", "till"]).await;
    publish_now(&w, apps[0].0, many(1, 8));
    publish_now(&w, apps[1].0, many(2, 8));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    // The till's newer record is refused: that is its failure, once, and the record it holds
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
    let till = bob_apps(&w, &["till"]).await;
    let apps = [("shop", shop.app_id()), till[0]];
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1shop"),
    );
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1till"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[0].1, apps[1].1]);
    // The shop's newer record is refused: discovery gives the shop's failure and only the
    // till's endpoint, not the shop's record kept.
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
    let refused = wallet.pay_request(&request, public()).await.unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(text.contains("Paykit wire 3.2"), "{text}");
    assert!(w.rail.all_executions().is_empty());
    // A payment to any app pays the till's endpoint, the one discovery offers, and tells
    // the shop's failure with it.
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    let PayOutcome::Paid {
        public_failures, ..
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(public_failures, &read.failures);
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
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
    let refused = wallet.pay_request(&request, public).await.unwrap_err();
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
    let apps = bob_apps(&w, &["shop", "till"]).await;
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
        one_endpoint("btc-lightning-bolt11", "lnbc1till"),
    );
    // The wallet's first payment reads both records and pays one.
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // Another body of the shop's record at the same updated_at: the next payment is the
    // first read to find the conflict, pays the till, and tells the conflict.
    publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1b"),
        t0,
        expiry,
    );
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    let PayOutcome::Paid {
        public_failures, ..
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(public_failures.len(), 1, "{public_failures:?}");
    assert_eq!(public_failures[0].app_id, apps[0].1);
    assert!(
        matches!(&public_failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("Switch spec 4.2")),
        "{public_failures:?}"
    );
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt11"
    );
}

#[tokio::test]
async fn test_public_read_records_ahead_after_a_clock_rollback_not_a_conflict() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop"]).await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    let older = publish_app_at(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1"),
        t0,
        expiry,
    );
    // The wallet sees only a record an hour later.
    w.clock.set_ms(t0 + HOUR_MS);
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt12", "lno1later"),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1);
    // The wallet's clock goes back and the older record is served: the record seen is ahead
    // of the clock, and that is the failure, not a conflict. Nothing is offered.
    w.clock.set_ms(t0 + 10 * 60_000);
    w.homeserver
        .put(w.bob_id(), &app_record_path(&apps[0].1), older);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert!(read.endpoints.is_empty(), "{read:?}");
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert!(
        matches!(&read.failures[0].reason, PublicAppFailureReason::Expired(rule)
            if rule.contains("newest record seen was made ahead")),
        "{read:?}"
    );
}

#[tokio::test]
async fn test_public_read_large_records_kept_per_app_within_the_store_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let apps = bob_apps(&w, &["shop", "till"]).await;
    publish_now(
        &w,
        apps[1].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1till"),
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
    // A record the reader can't keep at all is the shop's failure; the till is still read.
    w.clock.advance_ms(1000);
    publish_now(&w, apps[0].0, many(3, 3 * MIB));
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(offering(&read), [apps[1].1]);
    assert_eq!(read.failures.len(), 1, "{:?}", read.failures);
    assert_eq!(read.failures[0].app_id, apps[0].1);
    assert!(
        matches!(&read.failures[0].reason, PublicAppFailureReason::Refused(rule)
            if rule.contains("a reader keeps")),
        "{:?}",
        read.failures
    );
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
    let apps = bob_apps(&w, &["till"]).await;
    publish_now(&w, "shop", one_endpoint("btc-lightning-bolt12", "lno1shop"));
    publish_now(
        &w,
        apps[0].0,
        one_endpoint("btc-lightning-bolt11", "lnbc1till"),
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
    // The request requires the shop, whose record can't be read now: the till's endpoint is
    // fine, but only the shop's record is read, so there is no public destination.
    let draft = requiring(w.clock.now_ms());
    let request = request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    reads.fail(&shop.app_id());
    let refused = wallet.pay_request(&request, public()).await.unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert_eq!(text.matches("; app ").count(), 1, "{text}");
    assert!(text.contains("homeserver"), "{text}");
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    // Once the shop's record reads again, its endpoint is paid, never the till's.
    reads.heal();
    let outcome = wallet.pay_request(&request, public()).await.unwrap();
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
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
}
