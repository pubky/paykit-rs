//! Public payments (Paykit spec 4, 8.4; Paykit wire 3.2): to a payee app's public endpoints,
//! read from its record on the Homeserver, only while the record is valid, current and
//! within its lifetime.

use paykit_sdk::app_records::{PublicAppFailureReason, PublicPaymentList};
use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, PaymentMode, Refusal, UnrequestedPayment};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::{app_record_path, AppRecord};
use serde_json::{json, Value};

use crate::common::{btc, endpoint, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// Bob's `shop` publishes its record on his Homeserver with `member` as its Paykit member,
/// expiring at `expires_at`, made now.
pub(crate) fn publish(w: &World, member: Value, expires_at: Option<u64>) -> Vec<u8> {
    publish_at(w, member, w.clock.now_ms(), expires_at)
}

/// As [`publish`], made at `updated_at`. Returns the bytes served.
fn publish_at(w: &World, member: Value, updated_at: u64, expires_at: Option<u64>) -> Vec<u8> {
    publish_app_at(w, "shop", member, updated_at, expires_at)
}

/// As [`publish_at`], for bob's app `app`.
pub(crate) fn publish_app_at(
    w: &World,
    app: &str,
    member: Value,
    updated_at: u64,
    expires_at: Option<u64>,
) -> Vec<u8> {
    let keys = w.bob.identity().app_keys(app);
    let app_id = keys.app_id_bytes();
    let held = w.bob.switch().app(&app_id).record.unwrap();
    let mut body = AppRecord::verify(&held, &w.bob_id())
        .unwrap()
        .body()
        .clone();
    let Value::Object(member) = member else {
        unreachable!()
    };
    body.protocols.insert("paykit".into(), member);
    body.updated_at = updated_at;
    body.expires_at = expires_at;
    let record = AppRecord::sign(&keys, body).unwrap();
    let bytes = serde_json::to_vec(record.signed()).unwrap();
    w.homeserver
        .put(w.bob_id(), &app_record_path(&app_id), bytes.clone());
    bytes
}

/// Bob's Homeserver serves `bytes` as the shop's record.
fn serve(w: &World, bytes: &[u8]) {
    let app_id = w.bob.identity().app_keys("shop").app_id_bytes();
    w.homeserver
        .put(w.bob_id(), &app_record_path(&app_id), bytes.to_vec());
}

/// A member with one public endpoint.
pub(crate) fn one_endpoint(id: &str, payload: &str) -> Value {
    json!({ "public_endpoints": { id: payload } })
}

/// The endpoint identifiers of bob's public endpoints, as the wallet reads them now.
async fn listed(wallet: &mut paykit_sdk::Paykit, w: &World) -> Vec<String> {
    wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .endpoints
        .into_iter()
        .map(|e| e.endpoint_id.as_str().to_owned())
        .collect()
}

/// Bob's public Payment List as the wallet reads it now.
async fn read(wallet: &mut paykit_sdk::Paykit, w: &World) -> PublicPaymentList {
    wallet.public_endpoints(&w.bob_id(), None).await.unwrap()
}

/// The endpoint identifiers of `list`.
fn ids(list: &PublicPaymentList) -> Vec<&str> {
    list.endpoints
        .iter()
        .map(|e| e.endpoint_id.as_str())
        .collect()
}

/// Whether `list` reports exactly `app`'s failures, in order, each `Refused` or `Expired`
/// naming its rule as `rules` give.
fn fails_with(list: &PublicPaymentList, app: [u8; 16], rules: &[&str]) -> bool {
    list.failures.len() == rules.len()
        && list.failures.iter().zip(rules).all(|(failure, rule)| {
            failure.app_id == app
                && match &failure.reason {
                    PublicAppFailureReason::Refused(text)
                    | PublicAppFailureReason::Expired(text) => text.contains(rule),
                    _ => false,
                }
        })
}

fn tip_jar() -> Value {
    json!({
        "capabilities": { "endpoint_identifiers": ["btc-lightning-bolt12"] },
        "public_endpoints": { "btc-lightning-bolt12": "lno1tips" }
    })
}

pub(crate) fn public_to_bob(w: &World) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc("0.001"))
        .with_options(PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new())))
}

#[tokio::test]
async fn test_public_endpoint_paid_proven_and_credited() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Bob shares nothing privately; his shop's public endpoint is on his Homeserver. He
    // admits alice's proof as a conversation he starts.
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let listed = wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .endpoints;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].endpoint_id, endpoint("btc-lightning-bolt12"));
    assert_eq!(listed[0].app_id, shop.app_id());

    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id,
        endpoint("btc-lightning-bolt12")
    );
    // The payee credits it by its settlement and verifies it by its own method.
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop
        .verify_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_public_malformed_record_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    // A key of public_endpoints that isn't a Payment Endpoint Identifier (Paykit wire 1.4,
    // 3.2): the reader refuses the whole record.
    publish(
        &w,
        json!({ "public_endpoints": { "private": "lno1tips", "btc-lightning-bolt12": "lno1" } }),
        Some(w.clock.now_ms() + 30 * DAY_MS),
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert!(read.endpoints.is_empty());
    assert_eq!(read.failures.len(), 1, "{read:?}");
    assert_eq!(read.failures[0].app_id, shop.app_id());
    assert!(
        matches!(read.failures[0].reason, PublicAppFailureReason::Refused(_)),
        "{read:?}"
    );
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
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
async fn test_public_expired_or_stale_record_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    // Expired: past its own expires_at.
    publish(&w, tip_jar(), Some(w.clock.now_ms() + HOUR_MS));
    w.clock.advance_ms(HOUR_MS);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    // Stale: it signed an expiry a year ahead, but the identity trusts a record for 90 days
    // after its updated_at (Paykit spec 4, record age limit).
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 365 * DAY_MS));
    w.clock.advance_ms(90 * DAY_MS);
    // The payer's copy of bob's identity.json is renewed meanwhile, as it would be.
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_record_expired_by_the_service_time_alone_is_refused() {
    // The wallet's device clock runs ten seconds behind the service. Once the service time
    // it verified reaches the record's expiry, by its own expires_at and by its updated_at
    // plus the record age limit alike, the record has expired, and the refusal says the
    // device clock is behind (Paykit spec 7.2, check the destination).
    for age_limit in [false, true] {
        let w = World::new();
        let wallet_store = crate::common::store();
        let wallet = w
            .wallet_on("wallet", std::sync::Arc::clone(&wallet_store))
            .await;
        let app_id = wallet.app_id();
        drop(wallet);
        let _shop = w.shop().await;
        let expires = if age_limit {
            // A null expires_at lasts until updated_at plus the 90-day age limit.
            publish(&w, tip_jar(), None);
            w.clock.now_ms() + 90 * DAY_MS
        } else {
            publish(&w, tip_jar(), Some(w.clock.now_ms() + HOUR_MS));
            w.clock.now_ms() + HOUR_MS
        };
        w.clock.set_ms(expires);
        let behind =
            std::sync::Arc::new(pubky_switch_client::clock::TestClock::new(expires - 10_000));
        let mut wallet = w
            .load_on_clock(&w.alice, wallet_store, app_id, behind)
            .await;
        wallet.receive(None).await.unwrap();
        assert!(wallet
            .app()
            .highest_service_time()
            .is_some_and(|at| at >= expires));
        let refused = wallet
            .pay_without_request(public_to_bob(&w))
            .await
            .outcome
            .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
            "{age_limit}: {refused:?}"
        );
        assert!(
            format!("{refused:?}").contains("the device clock is behind"),
            "{age_limit}: {refused:?}"
        );
        assert!(w.rail.all_executions().is_empty(), "{age_limit}");
    }
}

#[tokio::test]
async fn test_public_record_without_public_endpoints_offers_none() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    // No record on the Homeserver at all, then one without public endpoints: neither offers
    // an endpoint, and neither is a failure.
    let none = PublicPaymentList::default();
    assert_eq!(
        wallet.public_endpoints(&w.bob_id(), None).await.unwrap(),
        none
    );
    publish(
        &w,
        json!({ "capabilities": { "endpoint_identifiers": [] } }),
        None,
    );
    assert_eq!(
        wallet.public_endpoints(&w.bob_id(), None).await.unwrap(),
        none
    );
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
async fn test_public_older_record_after_a_newer_one_not_used() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    let older = publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1old"),
        t0,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1new"),
        t0 + 1000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    // A stale copy is served again: an older record never replaces a newer one (Switch spec
    // 4.2), and the payment goes to the newer record's endpoint.
    serve(&w, &older);
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id,
        endpoint("btc-lightning-bolt12")
    );
}

#[tokio::test]
async fn test_public_conflicting_records_use_the_earlier_one_until_a_later_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1first"),
        t0,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1a"),
        t0 + 1000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    // Another body at the same updated_at: a conflict. Neither is used, it is told as the
    // shop's failure, and the wallet keeps using the earlier record it holds (Switch spec
    // 4.2): discovery and a payment agree.
    publish_at(
        &w,
        one_endpoint("btc-bitcoin-p2wpkh", "bc1qother"),
        t0 + 1000,
        expiry,
    );
    let list = read(&mut wallet, &w).await;
    assert_eq!(ids(&list), ["btc-lightning-bolt11"]);
    assert!(
        fails_with(&list, shop.app_id(), &["Switch spec 4.2"]),
        "{list:?}"
    );
    let paid = wallet.pay_without_request(public_to_bob(&w)).await;
    assert_eq!(paid.public_failures, list.failures);
    let outcome = paid.outcome.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id,
        endpoint("btc-lightning-bolt11")
    );
    // A strictly later record ends the conflict, and with it the failure.
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1b"),
        t0 + 2000,
        expiry,
    );
    let list = read(&mut wallet, &w).await;
    assert_eq!(ids(&list), ["btc-lightning-bolt12"]);
    assert!(list.failures.is_empty(), "{list:?}");
}

#[tokio::test]
async fn test_public_conflict_sides_leave_the_history_together() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    // Every updated_at is a conflict, more of them than the history keeps: no side of an
    // older one is ever the earlier record a later conflict falls back to.
    for i in 0..34u64 {
        publish_at(
            &w,
            one_endpoint("btc-lightning-bolt11", &format!("lnbc1a{i}")),
            t0 + i,
            expiry,
        );
        listed(&mut wallet, &w).await;
        publish_at(
            &w,
            one_endpoint("btc-lightning-bolt12", &format!("lno1b{i}")),
            t0 + i,
            expiry,
        );
        let list = read(&mut wallet, &w).await;
        assert!(list.endpoints.is_empty(), "pair {i}: {list:?}");
    }
}

#[tokio::test]
async fn test_public_older_record_served_again_answered_from_the_newer_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let t0 = w.clock.now_ms();
    let older = publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1old"),
        t0,
        Some(t0 + 2 * HOUR_MS),
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1new"),
        t0 + 1000,
        Some(t0 + 30 * DAY_MS),
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    // The older record, now expired, is served again: it is superseded, never the app's
    // record again (Switch spec 4.2), and the newer one, checked as any record used, is.
    w.clock.advance_ms(3 * HOUR_MS);
    serve(&w, &older);
    let list = read(&mut wallet, &w).await;
    assert_eq!(ids(&list), ["btc-lightning-bolt12"]);
    assert!(list.failures.is_empty(), "{list:?}");
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id,
        endpoint("btc-lightning-bolt12")
    );
}

#[tokio::test]
async fn test_public_record_comparison_survives_a_reload() {
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let _shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    let older = publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1old"),
        t0,
        expiry,
    );
    listed(&mut wallet, &w).await;
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1new"),
        t0 + 1000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    drop(wallet);
    // Restarted, the wallet is served the older copy: what it saw is in its store.
    serve(&w, &older);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            std::sync::Arc::clone(&base),
            app_id,
            std::sync::Arc::clone(&w.clock),
        )
        .await;
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
}

/// Alice's wallet, its requests to bob's switch going through a slow switch, which doesn't
/// answer its public read of bob's identity.json until released, and a request from bob's
/// `shop` it holds. Bob's copy held is old, so paying looks it up again.
async fn beside_a_slow_switch(
    w: &World,
    shop: &mut paykit_sdk::Paykit,
) -> (
    paykit_sdk::Paykit,
    std::sync::Arc<crate::common::SlowSwitch>,
    paykit_sdk::RequestRef,
) {
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let request = crate::common::request_from(
        shop,
        w.alice_id(),
        crate::common::draft("1", w.clock.now_ms()),
    )
    .await;
    wallet.receive(None).await.unwrap();
    drop(wallet);
    w.clock.advance_ms(11 * 60_000);
    let slow = std::sync::Arc::new(crate::common::SlowSwitch::new(
        std::sync::Arc::clone(&w.router),
        crate::common::BOB_ORIGIN,
    ));
    let wallet = w
        .load_with_transport(
            &w.alice,
            base,
            app_id,
            std::sync::Arc::clone(&slow)
                as std::sync::Arc<dyn pubky_switch_client::transport::Transport>,
        )
        .await;
    (wallet, slow, request)
}

/// `call`, which must finish in real time whatever the slow switch does.
async fn bounded<T>(call: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(10), call)
        .await
        .expect("the payment waited on the payee's switch past its grace")
}

#[tokio::test]
async fn test_public_slow_switch_waited_on_only_through_its_grace() {
    let w = World::new();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let (mut wallet, slow, _) = beside_a_slow_switch(&w, &mut shop).await;
    // The switch doesn't answer within the grace after the Homeserver's reads: the user is
    // asked to confirm, and nothing is written (Paykit spec 4, 14; Switch spec 4.3).
    let unconfirmed = public_to_bob(&w)
        .with_options(PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new())));
    let refused = bounded(wallet.pay_without_request(unconfirmed.clone()))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::IdentityUnconfirmed));
    assert!(wallet.payments().await.unwrap().is_empty());
    // Its answer, arriving later, is still taken: the next payment goes on without asking.
    slow.release();
    let outcome = bounded(wallet.pay_without_request(unconfirmed))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_public_identity_source_starts_a_new_lookup_after_rollback() {
    let w = World::new();
    let mut shop = w.shop().await;
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let (mut wallet, _slow, _) = beside_a_slow_switch(&w, &mut shop).await;
    let began = w.clock.now_ms();
    let first = bounded(wallet.public_endpoints(&w.bob_id(), None))
        .await
        .unwrap_err();
    assert_eq!(first.refusal(), Some(&Refusal::IdentityUnconfirmed));

    // The first switch lookup is still pending when the clock moves behind its
    // start. A fresh Homeserver read and a fresh grace can serve this confirmed call.
    w.clock.set_ms(began - 60_000);
    let reads = w.homeserver.identity_reads(&w.bob_id());
    let list = bounded(wallet.public_endpoints(&w.bob_id(), Some(UserConfirmed::new())))
        .await
        .unwrap();
    assert_eq!(ids(&list), ["btc-lightning-bolt12"]);
    assert!(w.homeserver.identity_reads(&w.bob_id()) > reads);
}

#[tokio::test]
async fn test_public_automatic_payment_waits_for_a_pending_switch() {
    use std::future::{poll_fn, Future};
    use std::sync::Arc;
    use std::task::Poll;
    let w = World::new();
    let saved = crate::common::store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&saved)).await;
    let app_id = wallet.app_id();
    let mut shop = w.shop().await;
    let mut draft = crate::common::draft("1", w.clock.now_ms());
    draft.destination = paykit_lib::bodies::RequestDestination::Any;
    let request = crate::common::request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    drop(wallet);
    w.clock.advance_ms(11 * 60_000);
    publish(&w, tip_jar(), Some(w.clock.now_ms() + DAY_MS));
    let slow = Arc::new(crate::common::SlowSwitch::new(
        Arc::clone(&w.router),
        crate::common::BOB_ORIGIN,
    ));
    let mut wallet = w
        .load_with_transport(
            &w.alice,
            saved,
            app_id,
            Arc::clone(&slow) as Arc<dyn pubky_switch_client::transport::Transport>,
        )
        .await;
    let mut payment =
        Box::pin(wallet.pay_request_automatically(&request, public_to_bob(&w).options));
    bounded(poll_fn(|cx| {
        assert!(
            payment.as_mut().poll(cx).is_pending(),
            "the payment stopped before the switch answered"
        );
        if slow.has_waiter() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }))
    .await;
    assert!(w.rail.all_executions().is_empty());
    slow.release();
    let outcome = bounded(payment).await.outcome.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_public_slow_switch_paid_once_the_user_confirms() {
    let w = World::new();
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let (mut wallet, _slow, _) = beside_a_slow_switch(&w, &mut shop).await;
    let mut payment = public_to_bob(&w);
    payment.options.unconfirmed_identity_confirmed = Some(UserConfirmed::new());
    let outcome = bounded(wallet.pay_without_request(payment))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_identity_source_stale_service_requires_user_confirmation() {
    use pubky_switch_client::app::SwitchCopy;
    use pubky_switch_client::clock::TestClock;
    use pubky_switch_client::testing::START_MS;
    use std::sync::Arc;

    let w = World::on_clocks(
        Arc::new(TestClock::new(START_MS)),
        Arc::new(TestClock::new(START_MS)),
    );
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut draft = crate::common::draft("1", w.clock.now_ms());
    draft.destination = paykit_lib::bodies::RequestDestination::Any;
    let request = crate::common::request_from(&mut shop, w.alice_id(), draft).await;
    wallet.receive(None).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));

    // Bob's switch still signs with its old clock. Only his Homeserver supplies a
    // current observation of the same document to the wallet.
    w.clock.advance_ms(11 * 60_000 + 1);
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.switch_copy, SwitchCopy::Stale);
    assert_eq!(source.homeserver_read_ms, Some(w.clock.now_ms()));
    let public = public_to_bob(&w).options;
    let confirmed = public
        .clone()
        .with_unconfirmed_identity_confirmed(UserConfirmed::new());
    let refused = wallet
        .pay_request_automatically(&request, confirmed.clone())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{refused:?}"
    );
    let refused = wallet
        .pay_request(&request, public)
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::IdentityUnconfirmed));
    assert!(w.rail.all_executions().is_empty());
    assert!(wallet.payments().await.unwrap().is_empty());
    let paid = wallet
        .pay_request(&request, confirmed)
        .await
        .outcome
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_failed_lookup_needs_the_exact_homeserver_version_even_with_consent() {
    use pubky_switch_client::app::SwitchCopy;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let higher = w.bob_directory_version(&mut wallet).await;
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.doc.hash(), higher.hash());
    assert_eq!(source.switch_copy, SwitchCopy::Read);
    assert!(source.switch_read_ms.is_some());
    assert_eq!(source.homeserver_read_ms, None);

    w.router.remove(crate::common::BOB_ORIGIN);
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.switch_copy, SwitchCopy::Unavailable);
    assert_eq!(source.homeserver_read_ms, None);
    let mut payment = public_to_bob(&w);
    payment.options.unconfirmed_identity_confirmed = Some(UserConfirmed::new());
    let refused = wallet
        .pay_without_request(payment.clone())
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::IdentityUnavailable(_))),
        "{refused:?}"
    );
    assert!(wallet.payments().await.unwrap().is_empty());
    assert!(w.rail.all_executions().is_empty());

    w.homeserver.publish(&higher).unwrap();
    let source = wallet
        .app_mut()
        .identity_of(&w.bob_id(), true)
        .await
        .unwrap();
    assert_eq!(source.switch_copy, SwitchCopy::Unavailable);
    assert_eq!(source.homeserver_read_ms, Some(w.clock.now_ms()));
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::IdentityUnconfirmed));
    assert!(wallet.payments().await.unwrap().is_empty());
    let paid = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(w.rail.all_executions().len(), 1);
}

#[tokio::test]
async fn test_public_unconfirmed_payee_paid_only_once_the_user_confirms() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    // Bob's switch can't be reached: a public endpoint needs its confirmation, or the
    // user's (Switch spec 4.3).
    w.router.remove(crate::common::BOB_ORIGIN);
    w.clock.advance_ms(11 * 60_000);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&Refusal::IdentityUnconfirmed));
    let mut payment = public_to_bob(&w);
    payment.options.unconfirmed_identity_confirmed = Some(UserConfirmed::new());
    let outcome = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_public_request_endpoint_beside_a_slow_switch_not_waited_on() {
    let w = World::new();
    let mut shop = w.shop().await;
    // A request's own endpoint is a verified message's: the switch's answer isn't waited on
    // (Paykit spec 7.2).
    let (mut wallet, _slow, request) = beside_a_slow_switch(&w, &mut shop).await;
    let outcome = bounded(wallet.pay_request(&request, PayOptions::default()))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
}

#[tokio::test]
async fn test_public_record_of_an_app_only_the_switch_lists_read() {
    let w = World::new();
    // Bob's identity.json lists another app first; the Homeserver keeps serving that copy
    // after the shop joins, while his switch serves the newer one listing the shop.
    let _other = w.labelled_shop("other", "other").await;
    let older = w
        .homeserver
        .file(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH)
        .unwrap();
    let _shop = w.shop().await;
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    w.homeserver
        .put(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH, older);
    // A wallet that never looked bob up: the first lookup starts from the older copy.
    let mut phone = w
        .join(
            &w.alice,
            "phone",
            paykit_sdk::AppScopes::new().pay().build().unwrap(),
        )
        .await;
    assert_eq!(listed(&mut phone, &w).await, ["btc-lightning-bolt12"]);
}

#[tokio::test]
async fn test_public_malformed_newer_record_is_the_apps_record_and_unusable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    let valid = publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1ok"),
        t0,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    // A newer record whose Paykit member doesn't read: it is the app's newest record (Switch
    // spec 4.2), and its endpoints are unusable (Paykit wire 3.2): the shop's failure, with
    // no endpoints from the valid record kept (Paykit spec 4).
    publish_at(
        &w,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
        t0 + 1000,
        expiry,
    );
    let list = read(&mut wallet, &w).await;
    assert!(list.endpoints.is_empty(), "{list:?}");
    assert!(
        fails_with(&list, shop.app_id(), &["Paykit wire 3.2"]),
        "{list:?}"
    );
    // The valid record served again is older than the newest seen: it never comes back, and
    // the shop still fails.
    serve(&w, &valid);
    let list = read(&mut wallet, &w).await;
    assert!(list.endpoints.is_empty(), "{list:?}");
    assert!(
        fails_with(&list, shop.app_id(), &["Paykit wire 3.2"]),
        "{list:?}"
    );
}

#[tokio::test]
async fn test_public_equal_time_malformed_record_is_a_conflict_across_a_reload() {
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1old"),
        t0,
        expiry,
    );
    listed(&mut wallet, &w).await;
    let valid = publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1new"),
        t0 + 1000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
    // Another body at the same updated_at, its Paykit member malformed: still two bodies at
    // one updated_at, a conflict (Switch wire 3.2). The earlier record is used meanwhile.
    publish_at(
        &w,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
        t0 + 1000,
        expiry,
    );
    let conflict = |list: &PublicPaymentList| {
        ids(list) == ["btc-lightning-bolt11"]
            && fails_with(list, shop.app_id(), &["Switch spec 4.2"])
    };
    let list = read(&mut wallet, &w).await;
    assert!(conflict(&list), "{list:?}");
    // Restarted and served the valid side again: the conflict was kept, and stays until a
    // strictly later record.
    drop(wallet);
    serve(&w, &valid);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            std::sync::Arc::clone(&base),
            app_id,
            std::sync::Arc::clone(&w.clock),
        )
        .await;
    let list = read(&mut wallet, &w).await;
    assert!(conflict(&list), "{list:?}");
}

#[tokio::test]
async fn test_public_record_ahead_of_the_clock_gives_no_endpoints_of_the_held_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1ok"),
        t0,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    // A record made a day ahead of the wallet's clock takes no part, and is the shop's
    // failure, with no endpoints from the record kept (Paykit spec 4).
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1ahead"),
        t0 + DAY_MS,
        expiry,
    );
    let list = read(&mut wallet, &w).await;
    assert!(list.endpoints.is_empty(), "{list:?}");
    assert!(
        fails_with(&list, shop.app_id(), &["ahead of the payer's clock"]),
        "{list:?}"
    );
}

#[tokio::test]
async fn test_public_refused_record_reported_over_a_held_record_without_endpoints() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    publish_at(&w, json!({ "public_endpoints": {} }), t0, expiry);
    assert_eq!(read(&mut wallet, &w).await, PublicPaymentList::default());
    // The record held offers no endpoints, and nothing of it fails; the newer one served is
    // refused, which is the shop's failure all the same (Paykit spec 4).
    publish_at(
        &w,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
        t0 + 1000,
        expiry,
    );
    let list = read(&mut wallet, &w).await;
    assert!(list.endpoints.is_empty(), "{list:?}");
    assert!(
        fails_with(&list, shop.app_id(), &["Paykit wire 3.2"]),
        "{list:?}"
    );
    // A payment finds no destination, and its refusal names that failure.
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    let Some(Refusal::NoDestination(text)) = refused.refusal() else {
        panic!("{refused:?}");
    };
    assert!(text.contains("Paykit wire 3.2"), "{text}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_refused_record_reported_once_over_an_expired_held_one() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let shop = w.shop().await;
    let t0 = w.clock.now_ms();
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt11", "lnbc1ok"),
        t0,
        Some(t0 + 2 * HOUR_MS),
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    // Past the held record's expires_at, a newer record served is refused: that is the
    // shop's one failure, and the record kept isn't judged.
    w.clock.advance_ms(3 * HOUR_MS);
    publish_at(
        &w,
        json!({ "public_endpoints": { "btc-lightning-bolt12": 7 } }),
        t0 + 1000,
        Some(t0 + 30 * DAY_MS),
    );
    let list = read(&mut wallet, &w).await;
    assert!(list.endpoints.is_empty(), "{list:?}");
    assert!(
        fails_with(&list, shop.app_id(), &["Paykit wire 3.2"]),
        "{list:?}"
    );
}

#[tokio::test]
async fn test_public_conflict_kept_past_the_history_bound_and_a_reload() {
    let w = World::new();
    let base = crate::common::store();
    let mut wallet = w.wallet_on("wallet", std::sync::Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    let _shop = w.shop().await;
    let t0 = w.clock.now_ms();
    let expiry = Some(t0 + 30 * DAY_MS);
    // More renewals than the history keeps, then a conflict at the newest updated_at.
    for i in 0..70u64 {
        publish_at(
            &w,
            one_endpoint("btc-lightning-bolt11", &format!("lnbc1r{i}")),
            t0 + i,
            expiry,
        );
        listed(&mut wallet, &w).await;
    }
    publish_at(
        &w,
        one_endpoint("btc-bitcoin-p2wpkh", "bc1qother"),
        t0 + 69,
        expiry,
    );
    // Neither side of the conflict is used: the wallet falls back to the earlier record.
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    // Many more bodies at that updated_at keep the conflict as it is, and a reload too: the
    // rejected endpoint never becomes usable.
    for i in 0..70u64 {
        publish_at(
            &w,
            one_endpoint("btc-bitcoin-p2tr", &format!("bc1p{i}")),
            t0 + 69,
            expiry,
        );
        assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    }
    drop(wallet);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            std::sync::Arc::clone(&base),
            app_id,
            std::sync::Arc::clone(&w.clock),
        )
        .await;
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    publish_at(
        &w,
        one_endpoint("btc-bitcoin-p2wpkh", "bc1qsecond"),
        t0 + 69,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
}

#[tokio::test]
async fn test_public_endpoint_the_payment_method_doesnt_pay_refused() {
    // Bob's public endpoint is Lightning; alice's method pays only on-chain: refused as
    // unsupported, nothing paid.
    let w = World::new();
    w.rail
        .set_endpoint_identifiers(vec![endpoint("btc-bitcoin-p2wpkh")]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(&w, tip_jar(), Some(w.clock.now_ms() + 30 * DAY_MS));
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(paykit_sdk::Refusal::UnsupportedEndpoint(_))
        ),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_endpoints_pay_the_payment_methods_first() {
    // Bob publishes two Lightning endpoints; the method prefers the one that comes second
    // by identifier.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        endpoint("btc-lightning-bolt12"),
        endpoint("btc-lightning-bolt11"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    publish(
        &w,
        json!({ "public_endpoints": {
            "btc-lightning-bolt11": "lnbc1pub",
            "btc-lightning-bolt12": "lno1pub"
        } }),
        Some(w.clock.now_ms() + 30 * DAY_MS),
    );
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
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
async fn test_public_endpoints_of_every_app_in_the_payment_methods_order() {
    // Bob's public Payment List is both apps' endpoints together: the method's first choice
    // is paid though only the later app, by App ID, offers it, and the earlier one offers
    // its second.
    let w = World::new();
    w.rail.set_endpoint_identifiers(vec![
        endpoint("btc-lightning-bolt12"),
        endpoint("btc-lightning-bolt11"),
    ]);
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let (earlier, later) = if shop.app_id() < seller_wallet.app_id() {
        ("shop", "seller-wallet")
    } else {
        ("seller-wallet", "shop")
    };
    let now = w.clock.now_ms();
    let expires_at = Some(now + 30 * DAY_MS);
    publish_app_at(
        &w,
        earlier,
        one_endpoint("btc-lightning-bolt11", "lnbc1earlier"),
        now,
        expires_at,
    );
    publish_app_at(
        &w,
        later,
        one_endpoint("btc-lightning-bolt12", "lno1later"),
        now,
        expires_at,
    );
    assert_eq!(
        listed(&mut wallet, &w).await.len(),
        2,
        "both apps' endpoints are listed"
    );
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    // Only the later app offers it.
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id.as_str(),
        "btc-lightning-bolt12"
    );
    // Both apps' endpoints were handed to the method at once, by App ID.
    let handed: Vec<String> = w
        .rail
        .selections()
        .last()
        .unwrap()
        .0
        .iter()
        .map(|c| format!("{} {} {}", c.id.get(), c.endpoint_id, c.payload.as_str()))
        .collect();
    assert_eq!(
        handed,
        [
            "0 btc-lightning-bolt11 lnbc1earlier",
            "1 btc-lightning-bolt12 lno1later"
        ]
    );
}

#[tokio::test]
async fn test_public_same_identifier_from_two_apps_pays_the_first_by_app_id() {
    // Bob's shop and second wallet both offer `btc-lightning-bolt12`, each its own payload. The
    // method is handed the endpoints by App ID (Paykit spec 4), and the fake rail's default
    // takes the first it is handed: the app with the lower App ID is paid.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    seller_wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    let expires = Some(w.clock.now_ms() + 30 * DAY_MS);
    let now = w.clock.now_ms();
    publish_app_at(
        &w,
        "shop",
        one_endpoint("btc-lightning-bolt12", "lno1shop"),
        now,
        expires,
    );
    publish_app_at(
        &w,
        "seller-wallet",
        one_endpoint("btc-lightning-bolt12", "lno1wallet"),
        now,
        expires,
    );
    let (first, payload) = if shop.app_id() < seller_wallet.app_id() {
        (shop.app_id(), "lno1shop")
    } else {
        (seller_wallet.app_id(), "lno1wallet")
    };
    let offered = wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .endpoints;
    assert_eq!(offered.len(), 2, "{offered:?}");

    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    let (handed, _) = w.rail.selections().pop().unwrap();
    let payloads: Vec<&str> = handed.iter().map(|c| c.payload.as_str()).collect();
    assert_eq!(payloads[0], payload, "{payloads:?}");
    let records = wallet.app().records_of("payer_activity").await.unwrap();
    assert_eq!(records.len(), 1);
    let value: Value = serde_json::from_slice(records[0].content.value().unwrap()).unwrap();
    assert_eq!(value["unrequested"]["payload"], payload, "{value}");
    assert_eq!(
        value["unrequested"]["app_id"],
        pubky_switch_core::enc::zbase32_encode(&first),
        "{value}"
    );
}

#[tokio::test]
async fn test_public_record_past_its_age_limit_on_the_device_clock_names_no_clock_behind() {
    // The record's updated_at plus the 90-day age limit has passed on the device clock too;
    // only its later expires_at is passed by the service's time alone. The device clock
    // shows the record expired, so the refusal says nothing of a clock behind (Paykit spec
    // 7.2, check the destination).
    let w = World::new();
    let wallet_store = crate::common::store();
    let wallet = w
        .wallet_on("wallet", std::sync::Arc::clone(&wallet_store))
        .await;
    let app_id = wallet.app_id();
    drop(wallet);
    let _shop = w.shop().await;
    let expires = w.clock.now_ms() + 90 * DAY_MS + HOUR_MS;
    publish(&w, tip_jar(), Some(expires));
    w.clock.set_ms(expires);
    let behind = std::sync::Arc::new(pubky_switch_client::clock::TestClock::new(expires - 10_000));
    let mut wallet = w
        .load_on_clock(&w.alice, wallet_store, app_id, behind)
        .await;
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    let shown = format!("{refused:?}");
    assert!(shown.contains("record age limit"), "{shown}");
    assert!(!shown.contains("the device clock is behind"), "{shown}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_record_read_again_before_dispatch_keeps_the_clock_note() {
    // The device clock runs 10 s behind the service. Bob's identity.json and record are read
    // 10 minutes 20 s before the record's age limit, and the payment is planned 9 minutes
    // later on that copy. Just after the attempt is recorded both clocks reach the limit: the
    // copy is no longer fresh, the record is read again before dispatch, and only the
    // service's time has reached the limit, so the refusal says the device clock is behind
    // (Paykit spec 7.2, check the destination).
    let w = World::new();
    let base = std::sync::Arc::new(crate::period_payments::StageStore::default());
    let wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&base)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let app_id = wallet.app_id();
    drop(wallet);
    let _shop = w.shop().await;
    publish(&w, tip_jar(), None);
    let expires = w.clock.now_ms() + 90 * DAY_MS;
    let read = expires - 10 * 60_000 - 20_000;
    w.clock.set_ms(read);
    let device = std::sync::Arc::new(pubky_switch_client::clock::TestClock::new(read - 10_000));
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            std::sync::Arc::clone(&base)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
            app_id,
            std::sync::Arc::clone(&device),
        )
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.public_endpoints(&w.bob_id(), None).await.is_ok());
    w.clock.set_ms(read + 9 * 60_000);
    device.set_ms(read + 9 * 60_000 - 10_000);
    let service = std::sync::Arc::clone(&w.clock);
    let moved = std::sync::Arc::clone(&device);
    base.on_stage("recorded", move || {
        service.set_ms(expires);
        moved.set_ms(expires - 10_000);
    });
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::AttemptFailed(inner)) if matches!(&**inner, Refusal::Destination(_))
        ),
        "{refused:?}"
    );
    assert!(
        format!("{refused:?}").contains("the device clock is behind"),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_record_past_its_age_limit_at_dispatch_names_no_clock_behind() {
    // The record's age limit falls 20 s before its expires_at, and the device clock runs
    // 10 s behind the service. Planned a minute before either, just after the attempt is
    // recorded the device clock passes the age limit and only the service's time passes
    // expires_at. The device clock shows the record expired, so the refusal before dispatch
    // says nothing of a clock behind (Paykit spec 4, 7.2).
    let w = World::new();
    let base = std::sync::Arc::new(crate::period_payments::StageStore::default());
    let wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&base)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let app_id = wallet.app_id();
    drop(wallet);
    let _shop = w.shop().await;
    let age_limit = w.clock.now_ms() + 90 * DAY_MS;
    publish(&w, tip_jar(), Some(age_limit + 20_000));
    w.clock.set_ms(age_limit - 60_000);
    let device = std::sync::Arc::new(pubky_switch_client::clock::TestClock::new(
        age_limit - 70_000,
    ));
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            std::sync::Arc::clone(&base)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
            app_id,
            std::sync::Arc::clone(&device),
        )
        .await;
    wallet.receive(None).await.unwrap();
    let service = std::sync::Arc::clone(&w.clock);
    base.on_stage("recorded", move || {
        service.set_ms(age_limit + 25_000);
        device.set_ms(age_limit + 15_000);
    });
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap_err();
    assert!(
        matches!(
            refused.refusal(),
            Some(Refusal::AttemptFailed(inner)) if matches!(&**inner, Refusal::Destination(_))
        ),
        "{refused:?}"
    );
    let shown = format!("{refused:?}");
    assert!(shown.contains("record age limit"), "{shown}");
    assert!(!shown.contains("the device clock is behind"), "{shown}");
    assert!(w.rail.all_executions().is_empty());
}
