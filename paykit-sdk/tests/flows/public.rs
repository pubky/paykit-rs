//! Public payments (Paykit spec 4, 8.4; Paykit wire 3.2): to a payee app's public endpoints,
//! read from its record on the Homeserver, only while the record is valid, current and
//! within its lifetime.

use paykit_sdk::{PayOptions, PayOutcome, PayeeStatus, PaymentMode, Refusal, UnrequestedPayment};
use pubky_switch_client::app::UserConfirmed;
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::{app_record_path, AppRecord};
use serde_json::{json, Value};

use crate::common::{btc, endpoint, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// Bob's `shop` publishes its record on his Homeserver with `member` as its Paykit member,
/// expiring at `expires_at`, made now.
fn publish(w: &World, member: Value, expires_at: Option<u64>) -> Vec<u8> {
    publish_at(w, member, w.clock.now_ms(), expires_at)
}

/// As [`publish`], made at `updated_at`. Returns the bytes served.
fn publish_at(w: &World, member: Value, updated_at: u64, expires_at: Option<u64>) -> Vec<u8> {
    let keys = w.bob.identity().app_keys("shop");
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
    serve(w, &bytes);
    bytes
}

/// Bob's Homeserver serves `bytes` as the shop's record.
fn serve(w: &World, bytes: &[u8]) {
    let app_id = w.bob.identity().app_keys("shop").app_id_bytes();
    w.homeserver
        .put(w.bob_id(), &app_record_path(&app_id), bytes.to_vec());
}

/// A member with one public endpoint.
fn one_endpoint(id: &str, payload: &str) -> Value {
    json!({ "public_endpoints": { id: payload } })
}

/// The endpoint identifiers of bob's public endpoints, as the wallet reads them now.
async fn listed(wallet: &mut paykit_sdk::Paykit, w: &World) -> Vec<String> {
    wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.endpoint_id.as_str().to_owned())
        .collect()
}

fn tip_jar() -> Value {
    json!({
        "capabilities": { "endpoint_identifiers": ["btc-lightning-bolt12"] },
        "public_endpoints": { "btc-lightning-bolt12": "lno1tips" }
    })
}

fn public_to_bob(w: &World) -> UnrequestedPayment {
    UnrequestedPayment {
        payee: w.bob_id(),
        amount: btc("0.001"),
        options: PayOptions {
            mode: PaymentMode::Public(UserConfirmed::new()),
            ..PayOptions::default()
        },
    }
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
    let listed = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].endpoint_id, endpoint("btc-lightning-bolt12"));
    assert_eq!(listed[0].app_id, shop.app_id());

    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
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
    let _shop = w.shop().await;
    // A key of public_endpoints that isn't a Payment Endpoint Identifier (Paykit wire 1.4,
    // 3.2): the reader refuses the whole record.
    publish(
        &w,
        json!({ "public_endpoints": { "private": "lno1tips", "btc-lightning-bolt12": "lno1" } }),
        Some(w.clock.now_ms() + 30 * DAY_MS),
    );
    assert!(wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .is_empty());
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
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
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_public_record_without_public_endpoints_offers_none() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let _shop = w.shop().await;
    // No record on the Homeserver at all, then one without public endpoints.
    assert!(wallet
        .public_endpoints(&w.bob_id(), None)
        .await
        .unwrap()
        .is_empty());
    publish(
        &w,
        json!({ "capabilities": { "endpoint_identifiers": [] } }),
        None,
    );
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
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
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
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
    // Another body at the same updated_at: a conflict. Neither is used; the wallet goes on
    // with the earlier record it holds (Switch spec 4.2).
    publish_at(
        &w,
        one_endpoint("btc-bitcoin-p2wpkh", "bc1qother"),
        t0 + 1000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt11"]);
    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    assert!(matches!(outcome, PayOutcome::Paid { .. }), "{outcome:?}");
    assert_eq!(
        wallet.payments().await.unwrap()[0].endpoint_id,
        endpoint("btc-lightning-bolt11")
    );
    // A strictly later record ends the conflict.
    publish_at(
        &w,
        one_endpoint("btc-lightning-bolt12", "lno1b"),
        t0 + 2000,
        expiry,
    );
    assert_eq!(listed(&mut wallet, &w).await, ["btc-lightning-bolt12"]);
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
