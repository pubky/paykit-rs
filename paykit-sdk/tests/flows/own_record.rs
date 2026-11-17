//! The app's own record (Paykit spec 4, Paykit wire 3.2): its public endpoints, published by
//! a payee app that publishes its record, with `expires_at` and renewal past half its
//! lifetime, and the capabilities of a paying app.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use paykit_lib::app_record::PaykitMember;
use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, PaykitError, PaymentMode,
    Refusal, UnrequestedPayment, RECORD_LIFETIME_MS,
};
use pubky_switch_client::app::{AppConfig, UserConfirmed};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::{app_record_path, AppRecord};

use crate::common::{btc, endpoint, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

/// A config whose app publishes its record on the Homeserver.
fn publishing() -> PaykitConfig {
    PaykitConfig::default().with_app(AppConfig {
        publishes_record: true,
        ..AppConfig::default()
    })
}

/// Bob's shop, holding `paykit_request`, publishing its record.
async fn publishing_shop(w: &World) -> Paykit {
    w.join_with(
        &w.bob,
        "shop",
        AppScopes::new().request().build().unwrap(),
        publishing(),
        store(),
    )
    .await
}

fn payload(text: &str) -> PaymentEndpointPayload {
    PaymentEndpointPayload::new(text).unwrap()
}

/// The record `app` holds now, verified.
fn held(app: &Paykit, identity: [u8; 32]) -> AppRecord {
    AppRecord::verify(app.app().record(), &identity).unwrap()
}

/// The record of bob's shop on his Homeserver, verified.
fn on_homeserver(w: &World, shop: &Paykit) -> Option<AppRecord> {
    let bytes = w
        .homeserver
        .file(w.bob_id(), &app_record_path(&shop.app_id()))?;
    let signed = serde_json::from_slice(&bytes).unwrap();
    Some(AppRecord::verify(&signed, &w.bob_id()).unwrap())
}

fn public_to_bob(w: &World) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc("0.001"))
        .with_options(PayOptions::default().with_mode(PaymentMode::Public(UserConfirmed::new())))
}

#[tokio::test]
async fn test_own_record_public_endpoint_published_paid_and_credited() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = publishing_shop(&w).await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    // Delivered to bob's switch and written to his Homeserver, in the same bytes.
    let published = on_homeserver(&w, &shop).expect("published");
    assert_eq!(published.signed(), shop.app().record());
    let member = PaykitMember::read(&published).unwrap();
    assert_eq!(
        member.public_endpoints,
        BTreeMap::from([(endpoint("btc-lightning-bolt12"), payload("lno1tips"))])
    );
    assert_eq!(
        shop.own_public_endpoints().unwrap(),
        member.public_endpoints
    );

    let outcome = wallet.pay_without_request(public_to_bob(&w)).await.unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
}

#[tokio::test]
async fn test_own_record_public_endpoint_removed_no_longer_paid() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    shop.remove_public_endpoint(&id).await.unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    // The record published carries no public endpoint, and no `expires_at` (Paykit wire
    // 3.2): it has nothing to expire.
    let published = on_homeserver(&w, &shop).unwrap();
    assert!(PaykitMember::read(&published)
        .unwrap()
        .public_endpoints
        .is_empty());
    assert_eq!(published.expires_at(), None);
    let refused = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, PaykitError::Refused(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    // Removing one the record doesn't hold signs nothing.
    let before = shop.app().record().clone();
    shop.remove_public_endpoint(&id).await.unwrap();
    assert_eq!(shop.app().record(), &before);
}

#[tokio::test]
async fn test_own_record_public_endpoints_refused_without_paykit_request() {
    let w = World::new();
    // Alice's wallet holds only `paykit_pay`: no proof of a payment to her would reach a
    // payee app of hers.
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().build().unwrap(),
            publishing(),
            store(),
        )
        .await;
    let before = wallet.app().record().clone();
    let refused = wallet
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, PaykitError::Refused(Refusal::ScopeMissing(s)) if s == "paykit_request"),
        "{refused:?}"
    );
    assert_eq!(wallet.app().record(), &before);
}

#[tokio::test]
async fn test_own_record_public_endpoints_refused_at_an_app_that_doesnt_publish() {
    let w = World::new();
    let mut shop = w.shop().await;
    let before = shop.app().record().clone();
    let refused = shop
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, PaykitError::Refused(Refusal::Usage(_))),
        "{refused:?}"
    );
    assert_eq!(shop.app().record(), &before);
    assert!(on_homeserver(&w, &shop).is_none());
}

#[tokio::test]
async fn test_own_record_public_endpoints_past_the_read_bound_refused() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let over: BTreeMap<_, _> = (0..=paykit_sdk::MAX_OWN_PUBLIC_ENDPOINTS)
        .map(|i| (endpoint(&format!("btc-lightning-x{i}")), payload("lno1")))
        .collect();
    let before = shop.app().record().clone();
    let refused = shop.set_public_endpoints(over).await.unwrap_err();
    assert!(
        matches!(refused, PaykitError::Refused(Refusal::Usage(_))),
        "{refused:?}"
    );
    assert_eq!(shop.app().record(), &before);
}

#[tokio::test]
async fn test_own_record_expiry_set_with_public_endpoints() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let record = held(&shop, w.bob_id());
    // 30 days, under the default record age limit of 90 days.
    let expires_at = record.expires_at().expect("expires_at");
    assert!(expires_at > w.clock.now_ms());
    assert!(expires_at <= record.updated_at() + RECORD_LIFETIME_MS);
    assert!(expires_at + HOUR_MS > record.updated_at() + RECORD_LIFETIME_MS);
}

#[tokio::test]
async fn test_own_record_renewed_past_half_its_lifetime_only() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    // With more than half its 30 days left, a receive signs nothing.
    w.clock.advance_ms(14 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(held(&shop, w.bob_id()).signed(), first.signed());
    // Past half, it is renewed: a later `updated_at` and `expires_at`, the same endpoints,
    // published.
    w.clock.advance_ms(2 * DAY_MS);
    shop.receive(None).await.unwrap();
    let renewed = held(&shop, w.bob_id());
    assert!(renewed.updated_at() > first.updated_at());
    assert!(renewed.expires_at().unwrap() > first.expires_at().unwrap());
    assert_eq!(
        PaykitMember::read(&renewed).unwrap(),
        PaykitMember::read(&first).unwrap()
    );
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), renewed.signed());
}

#[tokio::test]
async fn test_own_record_without_public_endpoints_not_renewed() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let first = shop.app().record().clone();
    w.clock.advance_ms(60 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), &first);
}

#[tokio::test]
async fn test_own_record_capabilities_name_what_the_payment_method_pays() {
    let w = World::new();
    let wallet = w.wallet("wallet").await;
    // At a paying app, the record lists what the payment method pays once a receive has
    // brought it up to date (the join's receive did).
    let listed = PaykitMember::read(&held(&wallet, w.alice_id()))
        .unwrap()
        .endpoint_identifiers;
    let mut paid = paykit_sdk::Rail::endpoint_identifiers(&*w.rail);
    paid.sort();
    assert!(!paid.is_empty());
    assert_eq!(listed, paid);
    // A payee app without `paykit_pay` lists none, and so carries no Paykit member at all.
    let shop = w.shop().await;
    let shop_record = held(&shop, w.bob_id());
    assert!(shop_record.body().protocol("paykit").is_none());
}

#[tokio::test]
async fn test_own_record_disabling_publication_clears_only_the_held_endpoints() {
    let w = World::new();
    let base = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            std::sync::Arc::clone(&base),
        )
        .await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let app_id = shop.app_id();
    drop(shop);
    // Loaded again without publishing: adding is refused, removing isn't, and the next
    // receive clears the held endpoints without changing the Homeserver copy.
    let mut shop = w
        .load_with(
            &w.bob,
            std::sync::Arc::clone(&base),
            app_id,
            std::sync::Arc::clone(&w.clock),
            PaykitConfig::default(),
        )
        .await;
    let other = endpoint("btc-onchain-address");
    let refused = shop
        .set_public_endpoint(other, payload("bc1q"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused, PaykitError::Refused(Refusal::Usage(_))),
        "{refused:?}"
    );
    assert_eq!(shop.own_public_endpoints().unwrap().len(), 1);
    shop.receive(None).await.unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert_eq!(held(&shop, w.bob_id()).expires_at(), None);
    let published = on_homeserver(&w, &shop).unwrap();
    assert_eq!(
        PaykitMember::read(&published)
            .unwrap()
            .public_endpoints
            .get(&id),
        Some(&payload("lno1tips"))
    );
    assert!(published.expires_at().is_some());
}

#[tokio::test]
async fn test_own_record_capabilities_updated_at_once_on_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let only = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![only.clone()]);
    wallet.update_own_record().await.unwrap();
    let listed = PaykitMember::read(&held(&wallet, w.alice_id()))
        .unwrap()
        .endpoint_identifiers;
    assert_eq!(listed, vec![only]);
}

#[tokio::test]
async fn test_own_record_oversized_capabilities_refuse_the_update() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let before = wallet.app().record().clone();
    let oversized = (0..10_000)
        .map(|i| endpoint(&format!("btc-{i:057}")))
        .collect();
    w.rail.set_endpoint_identifiers(oversized);
    let error = wallet.update_own_record().await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Refused(
                pubky_switch_client::app::Refusal::Usage(_)
            ))
        ),
        "{error:?}"
    );
    assert_eq!(wallet.app().record(), &before);
    let supported = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    wallet.update_own_record().await.unwrap();
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported]
    );
}

#[tokio::test]
async fn test_own_record_oversized_capabilities_refuse_the_receive() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let before = wallet.app().record().clone();
    let oversized = (0..10_000)
        .map(|i| endpoint(&format!("btc-{i:057}")))
        .collect();
    w.rail.set_endpoint_identifiers(oversized);
    let error = wallet.receive(None).await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Refused(
                pubky_switch_client::app::Refusal::Usage(_)
            ))
        ),
        "{error:?}"
    );
    assert_eq!(wallet.app().record(), &before);
    let supported = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported]
    );
}

#[tokio::test]
async fn test_own_record_failed_update_stays_owed_until_delivery() {
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().build().unwrap(),
            publishing(),
            store(),
        )
        .await;
    let path = app_record_path(&wallet.app_id());
    let published = w.homeserver.file(w.alice_id(), &path).unwrap();
    let supported = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let error = wallet.update_own_record().await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Transport(_))
        ),
        "{error:?}"
    );
    let pending = wallet.app().record().clone();
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported]
    );
    assert_eq!(w.homeserver.file(w.alice_id(), &path), Some(published));
    wallet.receive(None).await.unwrap();
    let bytes = w.homeserver.file(w.alice_id(), &path).unwrap();
    let signed = serde_json::from_slice(&bytes).unwrap();
    let delivered = AppRecord::verify(&signed, &w.alice_id()).unwrap();
    assert_eq!(delivered.signed(), &pending);
}

#[tokio::test]
async fn test_own_record_unchanged_endpoints_keep_the_signed_record() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let before = shop.app().record().clone();
    w.clock.advance_ms(HOUR_MS);
    shop.set_public_endpoint(id, payload("lno1tips"))
        .await
        .unwrap();
    assert_eq!(shop.app().record(), &before);
    shop.remove_public_endpoint(&endpoint("btc-onchain-address"))
        .await
        .unwrap();
    assert_eq!(shop.app().record(), &before);
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &before);
}

#[tokio::test]
async fn test_own_record_failed_publication_retries_the_newest_record_after_restart() {
    let w = World::new();
    let base = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            std::sync::Arc::clone(&base),
        )
        .await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1first"))
        .await
        .unwrap();
    let first = on_homeserver(&w, &shop).unwrap();
    w.homeserver.fail_writes(1, false);
    let publication = shop
        .set_public_endpoint(id.clone(), payload("lno1second"))
        .await
        .unwrap();
    assert!(
        matches!(
            publication,
            pubky_switch_client::app::Publication::Owed { .. }
        ),
        "{publication:?}"
    );
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), first.signed());
    let newest = shop.app().record().clone();
    let app_id = shop.app_id();
    drop(shop);
    let mut shop = w
        .load_with(
            &w.bob,
            base,
            app_id,
            std::sync::Arc::clone(&w.clock),
            publishing(),
        )
        .await;
    shop.receive(None).await.unwrap();
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &newest);
    assert_eq!(
        shop.own_public_endpoints().unwrap().get(&id),
        Some(&payload("lno1second"))
    );
}

#[tokio::test]
async fn test_own_record_withdrawal_waits_for_delivery_before_publication() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let published = on_homeserver(&w, &shop).unwrap();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    let error = shop.remove_public_endpoint(&id).await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Transport(_))
        ),
        "{error:?}"
    );
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        published.signed()
    );
    let pending = shop.app().record().clone();
    shop.receive(None).await.unwrap();
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &pending);
    assert!(PaykitMember::read(&on_homeserver(&w, &shop).unwrap())
        .unwrap()
        .public_endpoints
        .is_empty());
}

#[tokio::test]
async fn test_own_record_half_lifetime_boundary() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    let halfway = first.updated_at() + (first.expires_at().unwrap() - first.updated_at()) / 2;
    w.clock.set_ms(halfway);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), first.signed());
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    assert!(held(&shop, w.bob_id()).updated_at() > first.updated_at());
}

#[derive(Clone, Copy, Debug)]
enum RecordChange {
    Set,
    Replace,
    Remove,
    Capabilities,
}

async fn change_record(shop: &mut Paykit, change: RecordChange) -> Result<(), PaykitError> {
    let id = endpoint("btc-lightning-bolt12");
    match change {
        RecordChange::Set => shop
            .set_public_endpoint(id, payload("lno1changed"))
            .await
            .map(|_| ()),
        RecordChange::Replace => shop
            .set_public_endpoints(BTreeMap::from([(id, payload("lno1changed"))]))
            .await
            .map(|_| ()),
        RecordChange::Remove => shop.remove_public_endpoint(&id).await.map(|_| ()),
        RecordChange::Capabilities => shop.update_own_record().await,
    }
}

#[tokio::test]
async fn test_own_record_restored_changes_wait_for_confirmed_first_read() {
    use crate::common::ScriptedIdentityReads;
    use std::sync::Arc;
    for change in [
        RecordChange::Set,
        RecordChange::Replace,
        RecordChange::Remove,
        RecordChange::Capabilities,
    ] {
        let w = World::new();
        let mut shop = w
            .join_with(
                &w.bob,
                "shop",
                AppScopes::new().request().pay().build().unwrap(),
                publishing(),
                store(),
            )
            .await;
        shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
            .await
            .unwrap();
        let backup = shop.backup().await.unwrap();
        let reads = Arc::new(ScriptedIdentityReads::new(
            Arc::clone(&w.homeserver),
            Arc::clone(&w.clock),
        ));
        let mut env = w.env(&w.bob, store());
        env.homeserver = Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>;
        let mut restored = Paykit::restore(
            env,
            publishing(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        let before = restored.app().record().clone();
        let published = on_homeserver(&w, &restored).unwrap();
        w.rail
            .set_endpoint_identifiers(vec![endpoint("btc-onchain-address")]);
        reads.then(w.bob_id(), 0, true);
        let error = change_record(&mut restored, change)
            .await
            .expect_err("the first read has no confirmed identity");
        assert!(
            matches!(error.refusal(), Some(Refusal::IdentityUnavailable(_))),
            "{change:?}: {error:?}"
        );
        assert_eq!(restored.app().record(), &before, "{change:?}");
        assert_eq!(
            on_homeserver(&w, &restored).unwrap().signed(),
            published.signed(),
            "{change:?}"
        );
        assert_eq!(restored.app().restore_held_cursor(), None);
        change_record(&mut restored, change).await.unwrap();
        assert!(restored.app().restore_held_cursor().is_some(), "{change:?}");
        assert_ne!(restored.app().record(), &before, "{change:?}");
        assert_eq!(
            on_homeserver(&w, &restored).unwrap().signed(),
            restored.app().record(),
            "{change:?}"
        );
    }
}

#[tokio::test]
async fn test_own_record_restored_upkeep_waits_for_a_completed_read() {
    use std::sync::Arc;
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let before = restored.app().record().clone();
    w.clock.advance_ms(16 * DAY_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/feed".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    assert!(restored.receive(None).await.is_err());
    assert_eq!(restored.app().record(), &before);
    assert_eq!(restored.app().restore_held_cursor(), None);
    restored.receive(None).await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    assert!(
        held(&restored, w.bob_id()).updated_at()
            > AppRecord::verify(&before, &w.bob_id())
                .unwrap()
                .updated_at()
    );
    assert_eq!(
        on_homeserver(&w, &restored).unwrap().signed(),
        restored.app().record()
    );
}

#[tokio::test]
async fn test_own_record_shortened_age_limit_controls_renewal_and_expiry() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    w.bob
        .next_doc(|body| body.record_max_age = Some(10 * 86_400))
        .unwrap();
    shop.app_mut().refresh_identity().await.unwrap();
    w.clock.advance_ms(6 * DAY_MS);
    shop.receive(None).await.unwrap();
    let renewed = held(&shop, w.bob_id());
    assert!(renewed.updated_at() > first.updated_at());
    assert!(renewed.expires_at().unwrap() <= renewed.updated_at() + 10 * DAY_MS);
    assert_eq!(
        PaykitMember::read(&renewed).unwrap(),
        PaykitMember::read(&first).unwrap()
    );
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), renewed.signed());
}

#[tokio::test]
async fn test_own_record_losing_request_scope_withdraws_public_endpoints() {
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().request().pay().build().unwrap(),
            publishing(),
            store(),
        )
        .await;
    wallet
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let old = wallet.app().cert().clone();
    let narrower = crate::certificate::renew(
        &w,
        wallet.app_id(),
        &AppScopes::new().pay().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, wallet.app_id(), &old, &narrower);
    w.clock.advance_ms(1_000);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), narrower.hash());
    assert!(wallet.own_public_endpoints().unwrap().is_empty());
    let bytes = w
        .homeserver
        .file(w.alice_id(), &app_record_path(&wallet.app_id()))
        .unwrap();
    let signed = serde_json::from_slice(&bytes).unwrap();
    let published = AppRecord::verify(&signed, &w.alice_id()).unwrap();
    let member = PaykitMember::read(&published).unwrap();
    assert!(member.public_endpoints.is_empty());
    assert!(!member.endpoint_identifiers.is_empty());
    assert_eq!(published.expires_at(), None);
    assert_eq!(published.signed(), wallet.app().record());
}

#[tokio::test]
async fn test_own_record_failed_renewal_keeps_exact_bytes_until_delivery() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let published = on_homeserver(&w, &shop).unwrap();
    w.clock.advance_ms(16 * DAY_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    let error = shop.update_own_record().await.unwrap_err();
    assert!(
        matches!(&error, PaykitError::Client(client) if client.code() == Some(pubky_switch_core::http::ErrorCode::Unavailable)),
        "{error:?}"
    );
    let pending = shop.app().record().clone();
    assert_ne!(&pending, published.signed());
    assert_eq!(
        PaykitMember::read(&held(&shop, w.bob_id())).unwrap(),
        PaykitMember::read(&published).unwrap()
    );
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        published.signed()
    );
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), &pending);
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &pending);
}

#[tokio::test]
async fn test_own_record_labelled_scope_cannot_publish_public_endpoints() {
    let w = World::new();
    let mut shop = w
        .join_with(
            &w.bob,
            "hosted",
            AppScopes::new()
                .request_label(&paykit_lib::Label::parse("shop1").unwrap())
                .build()
                .unwrap(),
            publishing(),
            store(),
        )
        .await;
    let before = shop.app().record().clone();
    let error = shop
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::ScopeMissing(scope)) if scope == "paykit_request")
    );
    assert_eq!(shop.app().record(), &before);
    assert!(PaykitMember::read(&on_homeserver(&w, &shop).unwrap())
        .unwrap()
        .public_endpoints
        .is_empty());
}

/// The publisher's wall clock, independent of the payer and service clocks.
struct PublisherClock {
    times: Mutex<(u64, Option<u64>)>,
}

impl PublisherClock {
    fn new(now: u64) -> Self {
        Self {
            times: Mutex::new((now, None)),
        }
    }

    /// Change the wall time immediately after the next read of it.
    fn roll_back_after_read(&self, now: u64, later: u64) {
        *self.times.lock().unwrap() = (now, Some(later));
    }
}

impl Clock for PublisherClock {
    fn now_ms(&self) -> u64 {
        let mut times = self.times.lock().unwrap();
        let now = times.0;
        if let Some(later) = times.1.take() {
            times.0 = later;
        }
        now
    }
}

async fn shop_on_clock(w: &World, clock: Arc<PublisherClock>) -> Paykit {
    let base = store();
    let shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            Arc::clone(&base),
        )
        .await;
    let mut env = w.env(&w.bob, base);
    env.clock = clock as Arc<dyn Clock>;
    Box::pin(Paykit::load(
        env,
        publishing(),
        shop.app_id(),
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap()
}

#[tokio::test]
async fn test_own_record_mutation_after_clock_rollback_stays_payable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let clock = Arc::new(PublisherClock::new(w.clock.now_ms()));
    let mut shop = Box::pin(shop_on_clock(&w, Arc::clone(&clock))).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1first"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    *clock.times.lock().unwrap() = (w.clock.now_ms() - 40 * DAY_MS, None);
    shop.set_public_endpoint(id.clone(), payload("lno1changed"))
        .await
        .unwrap();
    let changed = on_homeserver(&w, &shop).unwrap();
    assert!(changed.updated_at() > first.updated_at());
    assert_eq!(changed.signed(), shop.app().record());
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1, "{read:?}");
    assert!(read.failures.is_empty(), "{read:?}");
    assert_eq!(read.endpoints[0].endpoint_id, id);
    assert_eq!(read.endpoints[0].payload, payload("lno1changed"));
    assert_eq!(
        changed.expires_at(),
        Some(changed.updated_at() + RECORD_LIFETIME_MS)
    );
    let signed = shop.app().record().clone();
    shop.update_own_record().await.unwrap();
    assert_eq!(shop.app().record(), &signed);
}

#[tokio::test]
async fn test_own_record_renewal_during_clock_rollback_stays_payable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let clock = Arc::new(PublisherClock::new(w.clock.now_ms()));
    let mut shop = Box::pin(shop_on_clock(&w, Arc::clone(&clock))).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    w.clock.advance_ms(16 * DAY_MS);
    // The half-life check observes the current time; before the writer signs, the host
    // clock rolls back. The payer and the service keep their current wall time.
    clock.roll_back_after_read(w.clock.now_ms(), first.updated_at() - 40 * DAY_MS);
    shop.update_own_record().await.unwrap();
    let renewed = on_homeserver(&w, &shop).unwrap();
    assert!(renewed.updated_at() > first.updated_at());
    assert_eq!(renewed.signed(), shop.app().record());
    assert_eq!(
        PaykitMember::read(&renewed).unwrap(),
        PaykitMember::read(&first).unwrap()
    );
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1, "{read:?}");
    assert!(read.failures.is_empty(), "{read:?}");
    assert_eq!(read.endpoints[0].endpoint_id, id);
    assert_eq!(read.endpoints[0].payload, payload("lno1tips"));
    assert_eq!(
        renewed.expires_at(),
        Some(renewed.updated_at() + RECORD_LIFETIME_MS)
    );
    let signed = shop.app().record().clone();
    shop.update_own_record().await.unwrap();
    assert_eq!(shop.app().record(), &signed);
}

#[tokio::test]
async fn test_own_record_completed_read_renews_after_clock_rollback() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let clock = Arc::new(PublisherClock::new(w.clock.now_ms()));
    let mut shop = Box::pin(shop_on_clock(&w, Arc::clone(&clock))).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    w.clock.advance_ms(31 * DAY_MS);
    *clock.times.lock().unwrap() = (first.updated_at() - 40 * DAY_MS, None);
    shop.receive(None).await.unwrap();
    assert!(shop.app().highest_service_time().unwrap() >= w.clock.now_ms());
    let before_payer_read = w.clock.now_ms();
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1, "{read:?}");
    assert!(read.failures.is_empty(), "{read:?}");
    assert_eq!(read.endpoints[0].payload, payload("lno1tips"));
    let renewed = on_homeserver(&w, &shop).unwrap();
    assert!(renewed.updated_at() >= before_payer_read);
    assert_eq!(renewed.signed(), shop.app().record());
    assert_eq!(
        renewed.expires_at(),
        Some(renewed.updated_at() + RECORD_LIFETIME_MS)
    );
    let signed = shop.app().record().clone();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), &signed);
}

#[tokio::test]
async fn test_own_record_restored_endpoint_after_clock_rollback_stays_payable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let clock = Arc::new(PublisherClock::new(w.clock.now_ms()));
    let mut shop = Box::pin(shop_on_clock(&w, Arc::clone(&clock))).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1first"))
        .await
        .unwrap();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    shop.set_public_endpoint(id.clone(), payload("lno1restored"))
        .await
        .unwrap_err();
    let backup = shop.backup().await.unwrap();
    let before = held(&shop, w.bob_id());
    drop(shop);
    w.clock.advance_ms(31 * DAY_MS);
    *clock.times.lock().unwrap() = (before.updated_at() - 40 * DAY_MS, None);
    let mut env = w.env(&w.bob, store());
    env.clock = clock as Arc<dyn Clock>;
    let mut restored = Box::pin(Paykit::restore(
        env,
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    restored
        .set_public_endpoint(id.clone(), payload("lno1restored"))
        .await
        .unwrap();
    let before_payer_read = w.clock.now_ms();
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert_eq!(read.endpoints.len(), 1, "{read:?}");
    assert!(read.failures.is_empty(), "{read:?}");
    assert_eq!(read.endpoints[0].endpoint_id, id);
    assert_eq!(read.endpoints[0].payload, payload("lno1restored"));
    let published = on_homeserver(&w, &restored).unwrap();
    assert!(published.updated_at() >= before_payer_read);
    assert_eq!(published.signed(), restored.app().record());
    assert_eq!(
        published.expires_at(),
        Some(published.updated_at() + RECORD_LIFETIME_MS)
    );
    restored
        .set_public_endpoint(id, payload("lno1restored"))
        .await
        .unwrap();
    assert_eq!(restored.app().record(), published.signed());
}
