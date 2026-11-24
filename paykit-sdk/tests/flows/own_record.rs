//! The app's own record (Paykit spec 4, Paykit wire 3.2): its public endpoints, published by
//! a payee app that publishes its record, with `expires_at` and renewal past half its
//! lifetime, and the capabilities of a paying app.

mod receive_upkeep;
mod restored_publication;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use paykit_lib::app_record::PaykitMember;
use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    AppScopes, PayOptions, PayOutcome, PayeeStatus, Paykit, PaykitConfig, PaykitError, PaymentMode,
    Refusal, UnrequestedPayment,
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

    assert_eq!(published.expires_at(), None);
    w.clock.set_ms(published.updated_at() + 31 * DAY_MS);
    let outcome = wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    let PayOutcome::Paid { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.executions(&attempt_id), 1);
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        published.signed()
    );

    w.clock.set_ms(published.updated_at() + 90 * DAY_MS);
    let read = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
    assert!(read.endpoints.is_empty());
    assert!(read.failures.iter().any(|failure| {
        failure.app_id == shop.app_id()
            && matches!(
                failure.reason,
                paykit_sdk::app_records::PublicAppFailureReason::Expired(_)
            )
    }));
    let refused = wallet.pay_without_request(public_to_bob(&w)).await;
    assert!(matches!(
        refused.outcome.unwrap_err().refusal(),
        Some(Refusal::NoDestination(_))
    ));
    assert_eq!(w.rail.all_executions().len(), 1);
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
        .outcome
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
    let refused = shop.set_public_endpoints(over, None).await.unwrap_err();
    assert!(
        matches!(refused, PaykitError::Refused(Refusal::Usage(_))),
        "{refused:?}"
    );
    assert_eq!(shop.app().record(), &before);
}

#[tokio::test]
async fn test_own_record_public_deadline_is_optional_and_can_be_set_explicitly() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let record = held(&shop, w.bob_id());
    assert_eq!(record.expires_at(), None);
    let cutoff = w.clock.now_ms() + 3 * DAY_MS;
    shop.set_public_endpoints(shop.own_public_endpoints().unwrap(), Some(cutoff))
        .await
        .unwrap();
    assert_eq!(on_homeserver(&w, &shop).unwrap().expires_at(), Some(cutoff));
    shop.set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap();
    assert_eq!(held(&shop, w.bob_id()).expires_at(), Some(cutoff));
    w.clock.advance_ms(2 * DAY_MS);
    shop.update_own_record().await.unwrap();
    let renewed = held(&shop, w.bob_id());
    assert!(renewed.updated_at() > record.updated_at());
    assert_eq!(renewed.expires_at(), Some(cutoff));
    w.clock.set_ms(cutoff);
    shop.update_own_record().await.unwrap();
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert_eq!(on_homeserver(&w, &shop).unwrap().expires_at(), None);
}

#[tokio::test]
async fn test_own_record_renewed_past_half_its_lifetime_only() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    // With more than half the identity's 90 days left, a receive signs nothing.
    w.clock.advance_ms(44 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(held(&shop, w.bob_id()).signed(), first.signed());
    // Past half the identity trust window, the same endpoints receive a fresh signature.
    w.clock.advance_ms(2 * DAY_MS);
    shop.receive(None).await.unwrap();
    let renewed = held(&shop, w.bob_id());
    assert!(renewed.updated_at() > first.updated_at());
    assert_eq!(renewed.expires_at(), None);
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
    assert_eq!(published.expires_at(), None);
}

/// Bob's publishing shop with two public endpoints, loaded again without publishing.
async fn shop_no_longer_publishing(w: &World) -> Paykit {
    let base = store();
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            Arc::clone(&base),
        )
        .await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    shop.set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap();
    let app_id = shop.app_id();
    drop(shop);
    w.load_with(
        &w.bob,
        base,
        app_id,
        Arc::clone(&w.clock),
        PaykitConfig::default(),
    )
    .await
}

/// The shop's held record carries no public endpoints and no expiry, and its Homeserver copy
/// still holds the two it published.
fn holds_none_published_two(w: &World, shop: &Paykit) {
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    assert_eq!(held(shop, w.bob_id()).expires_at(), None);
    let published = on_homeserver(w, shop).unwrap();
    assert_eq!(
        PaykitMember::read(&published)
            .unwrap()
            .public_endpoints
            .len(),
        2
    );
}

#[tokio::test]
async fn test_own_record_removal_without_publishing_withdraws_every_endpoint() {
    let w = World::new();
    let mut shop = shop_no_longer_publishing(&w).await;
    // Removing one of the two signs the record the upkeep would: none held.
    shop.remove_public_endpoint(&endpoint("btc-lightning-bolt12"))
        .await
        .unwrap();
    holds_none_published_two(&w, &shop);
    // The upkeep then finds nothing to change.
    let record = shop.app().record().clone();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.app().record(), &record);
}

#[tokio::test]
async fn test_own_record_set_without_publishing_withdraws_every_endpoint() {
    let w = World::new();
    let mut shop = shop_no_longer_publishing(&w).await;
    let mut kept = BTreeMap::new();
    kept.insert(endpoint("btc-onchain-address"), payload("bc1q"));
    shop.set_public_endpoints(kept, None).await.unwrap();
    holds_none_published_two(&w, &shop);
}

#[tokio::test]
async fn test_own_record_same_endpoint_without_publishing_withdraws_every_endpoint() {
    let w = World::new();
    let mut shop = shop_no_longer_publishing(&w).await;
    shop.set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap();
    holds_none_published_two(&w, &shop);
}

#[tokio::test]
async fn test_own_record_upkeep_without_publishing_withdraws_every_endpoint() {
    let w = World::new();
    let mut shop = shop_no_longer_publishing(&w).await;
    shop.update_own_record().await.unwrap();
    holds_none_published_two(&w, &shop);
}

/// A store whose next commit of the client's own state, once armed, is applied and then
/// answers that its result is unknown, as a store that can't tell may. Paykit's own state
/// (`p/paykit/`) commits as it is.
struct UnknownOnce {
    inner: Arc<dyn pubky_switch_client::store::StateStore>,
    armed: std::sync::atomic::AtomicBool,
}

impl UnknownOnce {
    fn clients(batch: &pubky_switch_client::store::WriteBatch) -> bool {
        use pubky_switch_client::store::Write;
        batch.writes().iter().any(|write| {
            !matches!(write, Write::Put { key, .. } | Write::Delete { key, .. } if key.starts_with(b"p/paykit/"))
        })
    }

    fn after(
        &self,
        clients: bool,
        result: Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        let applied = result?;
        if applied == pubky_switch_client::store::CommitOutcome::Applied
            && clients
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pubky_switch_client::error::StoreError::Unknown);
        }
        Ok(applied)
    }
}

impl pubky_switch_client::store::StateStore for UnknownOnce {
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
        let clients = Self::clients(&batch);
        let result = self
            .inner
            .commit(batch)
            .map(|()| pubky_switch_client::store::CommitOutcome::Applied);
        self.after(clients, result).map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        let clients = Self::clients(&batch);
        self.after(clients, self.inner.commit_if(ns, owner, batch))
    }
}

#[tokio::test]
async fn test_own_record_edit_after_an_unknown_store_result_waits_for_a_load() {
    let w = World::new();
    let base = store();
    let faulty = Arc::new(UnknownOnce {
        inner: Arc::clone(&base),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            Arc::clone(&faulty) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let a = endpoint("btc-lightning-bolt12");
    let b = endpoint("btc-onchain-address");
    shop.set_public_endpoint(a.clone(), payload("lno1tips"))
        .await
        .unwrap();
    // Removing A is applied, but the store can't tell.
    faulty
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = shop.remove_public_endpoint(&a).await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(
                pubky_switch_client::error::StoreError::Unknown
            ))
        ),
        "{error:?}"
    );
    // The record this handle holds may not be the one stored: reading its endpoints is
    // refused, naming the way out.
    let refused = shop.own_public_endpoints().unwrap_err();
    let PaykitError::Refused(Refusal::NotNow(text)) = &refused else {
        panic!("{refused:?}");
    };
    assert!(text.contains("load the app again"), "{text}");
    // Adding B on this handle would build on the record it still holds, with A: refused,
    // naming the way out, and nothing is signed.
    let record = shop.app().record().clone();
    let refused = shop
        .set_public_endpoint(b.clone(), payload("bc1q"))
        .await
        .unwrap_err();
    let PaykitError::Refused(Refusal::NotNow(text)) = &refused else {
        panic!("{refused:?}");
    };
    assert!(text.contains("load the app again"), "{text}");
    assert_eq!(shop.app().record(), &record);
    let app_id = shop.app_id();
    drop(shop);
    // Loaded again, the app holds the withdrawal, and adding B never brings A back.
    let mut shop = w
        .load_with(&w.bob, base, app_id, Arc::clone(&w.clock), publishing())
        .await;
    assert!(shop.own_public_endpoints().unwrap().is_empty());
    shop.set_public_endpoint(b.clone(), payload("bc1q"))
        .await
        .unwrap();
    let held = shop.own_public_endpoints().unwrap();
    assert_eq!(held.keys().collect::<Vec<_>>(), [&b]);
    let published = PaykitMember::read(&on_homeserver(&w, &shop).unwrap()).unwrap();
    assert!(!published.public_endpoints.contains_key(&a));
}

#[tokio::test]
async fn test_own_record_edit_after_an_unknown_read_result_waits_for_a_load() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let faulty = Arc::new(UnknownOnce {
        inner: store(),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            publishing(),
            Arc::clone(&faulty) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    wallet
        .pay_without_request(public_to_bob(&w))
        .await
        .outcome
        .unwrap();
    // The read that takes the proof is applied, but the store can't tell: a read may take
    // another device's record into the own record, so the record held may be stale.
    faulty
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = shop.receive(None).await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(
                pubky_switch_client::error::StoreError::Unknown
            ))
        ),
        "{error:?}"
    );
    let record = shop.app().record().clone();
    let refused = shop
        .set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap_err();
    let PaykitError::Refused(Refusal::NotNow(text)) = &refused else {
        panic!("{refused:?}");
    };
    assert!(text.contains("load the app again"), "{text}");
    assert_eq!(shop.app().record(), &record);
}

#[tokio::test]
async fn test_own_record_upkeep_keeps_a_member_it_does_not_read() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    // A Paykit member carrying a field this version doesn't read, and no capabilities.
    let mut member = serde_json::Map::new();
    member.insert("future_field".into(), serde_json::json!("kept"));
    shop.app_mut()
        .set_protocol_member_with_lifetime("paykit", Some(member), None)
        .await
        .unwrap();
    let signed = shop.app().record().clone();
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &signed);
    // The upkeep finds nothing to change: the accepted record keeps its exact bytes.
    shop.receive(None).await.unwrap();
    shop.update_own_record().await.unwrap();
    assert_eq!(shop.app().record(), &signed);
    assert_eq!(on_homeserver(&w, &shop).unwrap().signed(), &signed);
}

#[tokio::test]
async fn test_own_record_unknown_identity_refresh_stops_edits_and_upkeep() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let faulty = Arc::new(UnknownOnce {
        inner: store(),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut restored = Paykit::restore(
        w.env(
            &w.bob,
            Arc::clone(&faulty) as Arc<dyn pubky_switch_client::store::StateStore>,
        ),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // A renewal is due, and bob's identity.json is newer than the one restored. The first
    // read's refresh of the own identity, which can take a renewal into the own record,
    // takes the newer copy, applied but the store can't tell.
    w.clock.advance_ms(46 * DAY_MS);
    let _seller_wallet = w
        .join(
            &w.bob,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    faulty
        .armed
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = restored.receive(None).await.unwrap_err();
    assert!(
        matches!(
            error,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(
                pubky_switch_client::error::StoreError::Unknown
            ))
        ),
        "{error:?}"
    );
    // An edit is refused, naming the way out, and the upkeep signs nothing.
    let refused = restored
        .set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap_err();
    let PaykitError::Refused(Refusal::NotNow(text)) = &refused else {
        panic!("{refused:?}");
    };
    assert!(text.contains("load the app again"), "{text}");
    let record = restored.app().record().clone();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().record(), &record);
}

#[tokio::test]
async fn test_own_record_withdrawal_reports_its_unanswered_delivery() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoints(
        BTreeMap::from([(id.clone(), payload("lno1tips"))]),
        Some(w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    // The withdrawal signs with its cleared policy. A delivery failure leaves the
    // previous offer on the Homeserver until the next read.
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
    assert_eq!(held(&shop, w.bob_id()).expires_at(), None);
    let published = on_homeserver(&w, &shop).unwrap();
    assert!(PaykitMember::read(&published)
        .unwrap()
        .public_endpoints
        .contains_key(&id));
    assert_ne!(published.signed(), shop.app().record());
    // The next receive delivers it.
    shop.receive(None).await.unwrap();
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        shop.app().record()
    );
}

#[tokio::test]
async fn test_own_record_restored_before_a_withdrawal_has_nothing_owed() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // The backup holds the record with the endpoint and no withdrawal owed: the first read
    // and the upkeep keep the record as it is.
    let record = restored.app().record().clone();
    restored.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(restored.app().record(), &record);
    assert_eq!(
        restored
            .own_public_endpoints()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [&id]
    );
}

#[tokio::test]
async fn test_own_record_restored_withdrawal_finishes_delivery_after_the_first_read() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoints(
        BTreeMap::from([(id.clone(), payload("lno1tips"))]),
        Some(w.clock.now_ms() + DAY_MS),
    )
    .await
    .unwrap();
    // The backup retains the signed withdrawal whose delivery failed.
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    shop.remove_public_endpoint(&id).await.unwrap_err();
    assert_eq!(held(&shop, w.bob_id()).expires_at(), None);
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // Nothing is signed before the confirmed first read.
    let restored_record = restored.app().record().clone();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/feed".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    assert!(restored.receive(None).await.is_err());
    assert_eq!(restored.app().record(), &restored_record);
    // The restored app can now publish the withdrawal.
    restored.receive(None).await.unwrap();
    assert_ne!(restored.app().record(), &restored_record);
    assert!(restored.own_public_endpoints().unwrap().is_empty());
    assert_eq!(held(&restored, w.bob_id()).expires_at(), None);
    assert_eq!(
        on_homeserver(&w, &restored).unwrap().signed(),
        restored.app().record()
    );
}

/// Bob's publishing shop whose withdrawal of its last endpoint reached no switch, backed up
/// then, and restored on a new store.
async fn restored_after_an_undelivered_withdrawal(w: &World) -> Paykit {
    let mut shop = publishing_shop(w).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 0,
        });
    shop.remove_public_endpoint(&id).await.unwrap_err();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

/// The shop's Homeserver copy offers no public endpoint, and is the record it holds.
fn withdrawn_on_homeserver(w: &World, shop: &Paykit) {
    let published = on_homeserver(w, shop).unwrap();
    assert!(PaykitMember::read(&published)
        .unwrap()
        .public_endpoints
        .is_empty());
    assert_eq!(published.signed(), shop.app().record());
}

#[tokio::test]
async fn test_own_record_restored_withdrawal_signed_anew_at_the_receive() {
    let w = World::new();
    let mut restored = restored_after_an_undelivered_withdrawal(&w).await;
    restored.receive(None).await.unwrap();
    withdrawn_on_homeserver(&w, &restored);
}

#[tokio::test]
async fn test_own_record_restored_withdrawal_signed_anew_at_the_update() {
    let w = World::new();
    let mut restored = restored_after_an_undelivered_withdrawal(&w).await;
    restored.update_own_record().await.unwrap();
    withdrawn_on_homeserver(&w, &restored);
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
async fn test_own_record_update_returns_where_the_record_stands() {
    use pubky_switch_client::app::Publication;
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
    // The record listing what the payment method pays now fails its Homeserver write: the
    // update says it is owed.
    let only = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![only.clone()]);
    w.homeserver.fail_writes(1, false);
    let publication = shop.update_own_record().await.unwrap();
    assert!(
        matches!(publication, Publication::Owed { .. }),
        "{publication:?}"
    );
    assert!(on_homeserver(&w, &shop).is_none_or(|record| {
        PaykitMember::read(&record).unwrap().endpoint_identifiers != [only.clone()]
    }));
    // The Homeserver takes writes again: the next update publishes it.
    let publication = shop.update_own_record().await.unwrap();
    assert_eq!(publication, Publication::Published);
    let published = PaykitMember::read(&on_homeserver(&w, &shop).unwrap()).unwrap();
    assert_eq!(published.endpoint_identifiers, [only]);
}

#[tokio::test]
async fn test_own_record_update_returns_the_renewal_and_publication_status() {
    use pubky_switch_client::app::Publication;
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    // A renewal due whose Homeserver write fails: owed.
    w.clock.advance_ms(46 * DAY_MS);
    let before = shop.app().record().clone();
    w.homeserver.fail_writes(1, false);
    let publication = shop.update_own_record().await.unwrap();
    assert!(
        matches!(publication, Publication::Owed { .. }),
        "{publication:?}"
    );
    assert_ne!(shop.app().record(), &before);
    // Nothing to change, and the write owed fails at each try of the call: still owed.
    w.homeserver.fail_writes(2, false);
    let publication = shop.update_own_record().await.unwrap();
    assert!(
        matches!(publication, Publication::Owed { .. }),
        "{publication:?}"
    );
    // The write goes through: published.
    assert_eq!(
        shop.update_own_record().await.unwrap(),
        Publication::Published
    );
    // Nothing owed, and the read that checks the Homeserver copy fails: unchecked.
    w.homeserver.set_down(true);
    let publication = shop.update_own_record().await.unwrap();
    w.homeserver.set_down(false);
    assert!(
        matches!(publication, Publication::Unchecked { .. }),
        "{publication:?}"
    );
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        shop.app().record()
    );
}

#[tokio::test]
async fn test_own_record_oversized_capabilities_refuse_the_update() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let before = wallet.app().record().clone();
    w.rail.set_endpoint_identifiers(oversized_capabilities());
    let error = wallet.update_own_record().await.unwrap_err();
    assert!(is_usage(&error), "{error:?}");
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
async fn test_own_record_oversized_capabilities_left_owed_at_the_receive() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let before = wallet.app().record().clone();
    w.rail.set_endpoint_identifiers(oversized_capabilities());
    // Refused before anything is signed; the receive goes on and tells the host.
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.app_record_owed.is_some(), "{summary:?}");
    assert_eq!(wallet.app().record(), &before);
    let supported = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.app_record_owed, None, "{summary:?}");
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported]
    );
}

/// A list of Payment Endpoint Identifiers no app record can carry: its `app.put` is over
/// the Switch's cap on a signed operation (Switch wire 6.1).
/// A payment method whose list of what it pays is a different one at each reading, the last
/// repeated.
struct ShiftingRail {
    fake: Arc<paykit_sdk::rail::FakeRail>,
    lists: Mutex<Vec<Vec<paykit_lib::PaymentEndpointIdentifier>>>,
}

impl paykit_sdk::Rail for ShiftingRail {
    fn declaration(&self) -> paykit_sdk::rail::RailDeclaration {
        self.fake.declaration()
    }

    fn select_endpoints(
        &self,
        candidates: &[paykit_sdk::rail::PaymentEndpointCandidate],
        amount: Option<&paykit_lib::PaymentAmount>,
    ) -> Result<paykit_sdk::rail::EndpointSelection, paykit_sdk::rail::RailError> {
        self.fake.select_endpoints(candidates, amount)
    }

    fn endpoint_identifiers(&self) -> Vec<paykit_lib::PaymentEndpointIdentifier> {
        let mut lists = self.lists.lock().unwrap();
        if lists.len() > 1 {
            lists.remove(0)
        } else {
            lists[0].clone()
        }
    }

    fn transfer(
        &self,
        endpoint_id: &paykit_lib::PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &paykit_lib::PaymentAmount,
    ) -> paykit_sdk::rail::Transfer {
        self.fake.transfer(endpoint_id, payload, amount)
    }

    fn fraction_digits(&self, endpoint_id: &paykit_lib::PaymentEndpointIdentifier) -> Option<u32> {
        self.fake.fraction_digits(endpoint_id)
    }

    fn pay(
        &self,
        payment: paykit_sdk::rail::RailPayment,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<paykit_sdk::rail::RailOutcome, paykit_sdk::rail::RailError>,
    > {
        self.fake.pay(payment)
    }

    fn payment(
        &self,
        idempotency_key: paykit_lib::AttemptId,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<Option<paykit_sdk::rail::RailOutcome>, paykit_sdk::rail::RailError>,
    > {
        self.fake.payment(idempotency_key)
    }

    fn verify(
        &self,
        check: paykit_sdk::rail::ProofCheck,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<Option<u64>, paykit_sdk::rail::RailError>,
    > {
        self.fake.verify(check)
    }

    fn settlement_id(
        &self,
        endpoint_id: &paykit_lib::PaymentEndpointIdentifier,
        proof: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<Vec<u8>> {
        self.fake.settlement_id(endpoint_id, proof)
    }
}

#[tokio::test]
async fn test_own_record_join_signs_the_capabilities_it_checked() {
    let w = World::new();
    // The method pays one identifier at the join's check, and a list the record can't carry
    // at every reading after.
    let only = endpoint("btc-onchain-address");
    let rail = Arc::new(ShiftingRail {
        fake: Arc::clone(&w.rail),
        lists: Mutex::new(vec![vec![only.clone()], oversized_capabilities()]),
    });
    let wallet = Box::pin(Paykit::join(
        w.env(&w.alice, store()),
        publishing(),
        w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap()),
        rail as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    // The join's first record carries what was checked.
    let member = PaykitMember::read(&held(&wallet, w.alice_id())).unwrap();
    assert_eq!(member.endpoint_identifiers, [only]);
}

pub(crate) fn oversized_capabilities() -> Vec<paykit_lib::PaymentEndpointIdentifier> {
    (0..10_000)
        .map(|i| endpoint(&format!("btc-{i:057}")))
        .collect()
}

fn is_usage(error: &PaykitError) -> bool {
    matches!(
        error,
        PaykitError::Client(pubky_switch_client::app::AppError::Refused(
            pubky_switch_client::app::Refusal::Usage(_)
        ))
    )
}

#[tokio::test]
async fn test_own_record_oversized_capabilities_refuse_the_join() {
    use pubky_switch_client::store::{Fault, FaultStore, StateStore};

    let w = World::new();
    w.rail.set_endpoint_identifiers(oversized_capabilities());
    let request = w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap());
    let app_id = request.keys.app_id_bytes();
    let base = store();
    let fault = Arc::new(FaultStore::new(Arc::clone(&base), Fault::None));
    let tip = w.alice.switch().tip_seq();
    let joined = Box::pin(Paykit::join(
        w.env(&w.alice, Arc::clone(&fault) as Arc<dyn StateStore>),
        publishing(),
        request.clone(),
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await;
    let error = joined.expect_err("a record over the cap was joined with");
    assert!(is_usage(&error), "{error:?}");
    // Refused before the client's join: no commit, nothing at the switch or the Homeserver.
    assert_eq!(fault.commits(), 0);
    assert_eq!(w.alice.switch().tip_seq(), tip);
    assert!(w.alice.switch().app(&app_id).record.is_none());
    assert!(w
        .homeserver
        .file(w.alice_id(), &app_record_path(&app_id))
        .is_none());
    let loaded = Box::pin(Paykit::load(
        w.env(&w.alice, Arc::clone(&base)),
        publishing(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await;
    assert!(
        matches!(
            loaded,
            Err(PaykitError::Client(
                pubky_switch_client::app::AppError::Refused(
                    pubky_switch_client::app::Refusal::NotJoined
                )
            ))
        ),
        "the join wrote the app's state"
    );
    // The same approval joins once the list fits, and its record lists it.
    let supported = endpoint("btc-onchain-address");
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    let wallet = Box::pin(Paykit::join(
        w.env(&w.alice, base),
        publishing(),
        request,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported]
    );
}

/// Alice's wallet, publishing its record, joined while the delivery of the record carrying
/// its capabilities fails: the switch takes the join's registration, then is unreachable
/// for the `app.put` that follows. Returns the wallet and the record it kept.
async fn joined_with_failed_delivery(
    w: &World,
    supported: &paykit_lib::PaymentEndpointIdentifier,
) -> (Paykit, pubky_switch_core::sig::SignedDoc) {
    w.rail.set_endpoint_identifiers(vec![supported.clone()]);
    let request = w.approve(&w.alice, "wallet", &AppScopes::new().pay().build().unwrap());
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/ops".into(),
            after: 1,
        });
    let wallet = Box::pin(Paykit::join(
        w.env(&w.alice, store()),
        publishing(),
        request,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .expect("the joined app is returned");
    // The record with the capabilities is kept and owed: the switch and the Homeserver
    // hold the join's record, without them.
    let kept = wallet.app().record().clone();
    assert_eq!(
        PaykitMember::read(&held(&wallet, w.alice_id()))
            .unwrap()
            .endpoint_identifiers,
        vec![supported.clone()]
    );
    let at_switch = w.alice.switch().app(&wallet.app_id()).record.unwrap();
    assert_ne!(at_switch, kept);
    assert_eq!(
        PaykitMember::read(&AppRecord::verify(&at_switch, &w.alice_id()).unwrap()).unwrap(),
        PaykitMember::default()
    );
    let published = w
        .homeserver
        .file(w.alice_id(), &app_record_path(&wallet.app_id()))
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<pubky_switch_core::sig::SignedDoc>(&published).unwrap(),
        at_switch
    );
    (wallet, kept)
}

/// The record `wallet` kept is the one at alice's switch and on her Homeserver.
fn delivered(w: &World, wallet: &Paykit, kept: &pubky_switch_core::sig::SignedDoc) {
    assert_eq!(wallet.app().record(), kept);
    assert_eq!(
        w.alice.switch().app(&wallet.app_id()).record.as_ref(),
        Some(kept)
    );
    let bytes = w
        .homeserver
        .file(w.alice_id(), &app_record_path(&wallet.app_id()))
        .unwrap();
    assert_eq!(
        &serde_json::from_slice::<pubky_switch_core::sig::SignedDoc>(&bytes).unwrap(),
        kept
    );
}

#[tokio::test]
async fn test_own_record_join_delivery_failure_completed_by_the_update() {
    let w = World::new();
    let supported = endpoint("btc-onchain-address");
    let (mut wallet, kept) = joined_with_failed_delivery(&w, &supported).await;
    Box::pin(wallet.update_own_record()).await.unwrap();
    delivered(&w, &wallet, &kept);
}

#[tokio::test]
async fn test_own_record_join_delivery_failure_completed_by_the_receive() {
    let w = World::new();
    let supported = endpoint("btc-onchain-address");
    let (mut wallet, kept) = joined_with_failed_delivery(&w, &supported).await;
    Box::pin(wallet.receive(None)).await.unwrap();
    delivered(&w, &wallet, &kept);
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
    shop.receive(None).await.unwrap();
    assert_eq!(
        on_homeserver(&w, &shop).unwrap().signed(),
        shop.app().record()
    );
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
    let halfway = first.updated_at() + 45 * DAY_MS;
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
            .set_public_endpoints(BTreeMap::from([(id, payload("lno1changed"))]), None)
            .await
            .map(|_| ()),
        RecordChange::Remove => shop.remove_public_endpoint(&id).await.map(|_| ()),
        RecordChange::Capabilities => shop.update_own_record().await.map(|_| ()),
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
        let earlier = w.bob.switch().tip_seq();
        shop.app_mut().invite(&w.carol_id()).await.unwrap();
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
        // Collection pages from before the invitation cannot confirm the first read.
        crate::common::delay_listing_pages(&w.bob, earlier);
        let pending = change_record(&mut restored, change).await.unwrap_err();
        assert!(
            matches!(
                pending,
                PaykitError::Client(pubky_switch_client::app::AppError::Refused(
                    pubky_switch_client::app::Refusal::NotNow(_)
                ))
            ),
            "{pending:?}"
        );
        assert_eq!(restored.app().restore_held_cursor(), None);
        assert!(restored.app().listing_owed().await.unwrap());
        assert_eq!(restored.app().record(), &before);
        assert_eq!(
            on_homeserver(&w, &restored).unwrap().signed(),
            published.signed()
        );
        crate::common::finish_listing(&mut restored).await;
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
    w.clock.advance_ms(46 * DAY_MS);
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
    assert_eq!(renewed.expires_at(), None);
    assert_eq!(
        renewed.trusted_until(10 * 86_400),
        renewed.updated_at() + 10 * DAY_MS
    );
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

/// Alice's publishing wallet holding `paykit_request` and `paykit_pay`, with one public
/// endpoint.
async fn publishing_wallet_with_an_endpoint(w: &World) -> Paykit {
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
    wallet
}

#[tokio::test]
async fn test_own_record_renewal_without_request_scope_signs_no_endpoint() {
    let w = World::new();
    let mut wallet = publishing_wallet_with_an_endpoint(&w).await;
    let old = wallet.app().cert().clone();
    let narrower = crate::certificate::renew(
        &w,
        wallet.app_id(),
        &AppScopes::new().pay().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, wallet.app_id(), &old, &narrower);
    w.clock.advance_ms(1_000);
    // The clean-up after the read fails: the payment method's list no longer fits.
    w.rail.set_endpoint_identifiers(oversized_capabilities());
    let path = app_record_path(&wallet.app_id());
    let writes = w.homeserver.writes(&w.alice_id(), &path);
    // The receive goes on past the failed clean-up: the records the renewal adds are listed,
    // and the host is told the record is owed.
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.app_record_owed.is_some(), "{summary:?}");
    assert_eq!(summary.records_unlisted, None, "{summary:?}");
    // The record signed with the renewed certificate, the one the Homeserver got, carries
    // no public endpoint and no expiry: the earlier member was never signed under it.
    assert_eq!(wallet.app().cert().hash(), narrower.hash());
    let held = held(&wallet, w.alice_id());
    assert_eq!(held.cert().hash(), narrower.hash());
    assert!(PaykitMember::read(&held)
        .unwrap()
        .public_endpoints
        .is_empty());
    assert_eq!(held.expires_at(), None);
    assert_eq!(w.homeserver.writes(&w.alice_id(), &path), writes + 1);
    let bytes = w.homeserver.file(w.alice_id(), &path).unwrap();
    let published =
        AppRecord::verify(&serde_json::from_slice(&bytes).unwrap(), &w.alice_id()).unwrap();
    assert_eq!(published.cert().hash(), narrower.hash());
    assert!(PaykitMember::read(&published)
        .unwrap()
        .public_endpoints
        .is_empty());
}

#[tokio::test]
async fn test_own_record_renewal_keeping_request_scope_keeps_the_member() {
    let w = World::new();
    let mut wallet = publishing_wallet_with_an_endpoint(&w).await;
    // The member also carries a field this version doesn't read.
    let mut raw = held(&wallet, w.alice_id())
        .body()
        .protocols
        .get("paykit")
        .cloned()
        .unwrap();
    raw.insert("future_field".into(), serde_json::json!("kept"));
    wallet
        .app_mut()
        .set_protocol_member_with_lifetime("paykit", Some(raw), Some(30 * DAY_MS))
        .await
        .unwrap();
    let member = |wallet: &Paykit| {
        held(wallet, w.alice_id())
            .body()
            .protocols
            .get("paykit")
            .cloned()
    };
    let before = member(&wallet);
    let before_bytes = serde_json::to_vec(&before).unwrap();
    let path = app_record_path(&wallet.app_id());
    let writes = w.homeserver.writes(&w.alice_id(), &path);
    let expiry_set = held(&wallet, w.alice_id()).expires_at().is_some();
    let old = wallet.app().cert().clone();
    let renewed = crate::certificate::renew(
        &w,
        wallet.app_id(),
        &AppScopes::new().request().pay().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, wallet.app_id(), &old, &renewed);
    w.clock.advance_ms(1_000);
    wallet.receive(None).await.unwrap();
    // The same member, byte for byte, under the renewed certificate, still with its expiry,
    // in the one record the Homeserver got.
    assert_eq!(wallet.app().cert().hash(), renewed.hash());
    assert_eq!(member(&wallet), before);
    assert!(expiry_set && held(&wallet, w.alice_id()).expires_at().is_some());
    assert_eq!(w.homeserver.writes(&w.alice_id(), &path), writes + 1);
    let bytes = w.homeserver.file(w.alice_id(), &path).unwrap();
    let published =
        AppRecord::verify(&serde_json::from_slice(&bytes).unwrap(), &w.alice_id()).unwrap();
    assert_eq!(published.cert().hash(), renewed.hash());
    let published_member = published.body().protocols.get("paykit").cloned();
    assert_eq!(serde_json::to_vec(&published_member).unwrap(), before_bytes);
}

/// Alice's publishing wallet with no endpoint, and a second device of it restored from its
/// backup, each having read once.
async fn wallet_on_two_devices(w: &World) -> (Paykit, Paykit) {
    let mut first = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().request().pay().build().unwrap(),
            publishing(),
            store(),
        )
        .await;
    first.receive(None).await.unwrap();
    let backup = first.backup().await.unwrap();
    let mut second = Paykit::restore(
        w.env(&w.alice, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    second.receive(None).await.unwrap();
    (first, second)
}

#[tokio::test]
async fn test_own_record_endpoint_from_another_device_keeps_its_null_deadline() {
    let w = World::new();
    let (mut first, mut second) = wallet_on_two_devices(&w).await;
    // Neither device selected a public deadline. The second takes the signed record.
    first
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    second.receive(None).await.unwrap();
    assert_eq!(second.own_public_endpoints().unwrap().len(), 1);
    assert_eq!(held(&second, w.alice_id()).expires_at(), None);
    // A certificate renewal keeps the endpoint without adding a deadline.
    let old = second.app().cert().clone();
    let renewed = crate::certificate::renew(
        &w,
        second.app_id(),
        &AppScopes::new().request().pay().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, second.app_id(), &old, &renewed);
    w.clock.advance_ms(1_000);
    second.receive(None).await.unwrap();
    let held = held(&second, w.alice_id());
    assert_eq!(held.cert().hash(), renewed.hash());
    assert_eq!(PaykitMember::read(&held).unwrap().public_endpoints.len(), 1);
    assert_eq!(held.expires_at(), None);
}

#[tokio::test]
async fn test_own_record_renewal_over_another_devices_endpoint_preserves_its_null_deadline() {
    let w = World::new();
    let (mut first, mut second) = wallet_on_two_devices(&w).await;
    first
        .set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    // The second device takes the first's record, and its upkeep fails each time after:
    // the payment method's list no longer fits. Its own ask was never set.
    w.rail.set_endpoint_identifiers(oversized_capabilities());
    let summary = second.receive(None).await.unwrap();
    assert!(summary.app_record_owed.is_some(), "{summary:?}");
    assert_eq!(second.own_public_endpoints().unwrap().len(), 1);
    // Renewal preserves the endpoint without assigning a deadline.
    let old = second.app().cert().clone();
    let renewed = crate::certificate::renew(
        &w,
        second.app_id(),
        &AppScopes::new().request().pay().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, second.app_id(), &old, &renewed);
    w.clock.advance_ms(1_000);
    let summary = second.receive(None).await.unwrap();
    assert!(summary.app_record_owed.is_some(), "{summary:?}");
    let held = held(&second, w.alice_id());
    assert_eq!(held.cert().hash(), renewed.hash());
    assert_eq!(PaykitMember::read(&held).unwrap().public_endpoints.len(), 1);
    assert_eq!(held.expires_at(), None);
}

#[tokio::test]
async fn test_own_record_renewal_without_pay_scope_signs_no_capabilities() {
    let w = World::new();
    let mut wallet = publishing_wallet_with_an_endpoint(&w).await;
    let old = wallet.app().cert().clone();
    let narrower = crate::certificate::renew(
        &w,
        wallet.app_id(),
        &AppScopes::new().request().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, wallet.app_id(), &old, &narrower);
    w.clock.advance_ms(1_000);
    wallet.receive(None).await.unwrap();
    // Signed with the renewed certificate, Paykit's member has no capabilities, and keeps
    // its public endpoint under its expiry.
    let held = held(&wallet, w.alice_id());
    assert_eq!(held.cert().hash(), narrower.hash());
    let member = held.body().protocols.get("paykit").unwrap();
    assert!(!member.contains_key("capabilities"), "{member:?}");
    assert_eq!(PaykitMember::read(&held).unwrap().public_endpoints.len(), 1);
    assert_eq!(held.expires_at(), None);
}

#[tokio::test]
async fn test_own_record_member_another_device_changed_is_taken() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let backup = shop.backup().await.unwrap();
    let mut other = Paykit::restore(
        w.env(&w.bob, store()),
        publishing(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    other.receive(None).await.unwrap();
    // The first device adds an endpoint; the other takes its record at its next read, and
    // with it the news that Paykit's member changed, which leaves nothing for the host.
    shop.set_public_endpoint(endpoint("btc-onchain-address"), payload("bc1q"))
        .await
        .unwrap();
    other.receive(None).await.unwrap();
    assert_eq!(other.own_public_endpoints().unwrap().len(), 2);
    assert_eq!(held(&other, w.bob_id()).expires_at(), None);
    let events = other.app_mut().unacknowledged_events().await.unwrap();
    assert!(
        !events.iter().any(|event| matches!(
            event,
            pubky_switch_client::app::AppEvent::MemberChanged { .. }
        )),
        "{events:?}"
    );
}

#[tokio::test]
async fn test_own_record_failed_renewal_keeps_exact_bytes_until_delivery() {
    let w = World::new();
    let mut shop = publishing_shop(&w).await;
    shop.set_public_endpoint(endpoint("btc-lightning-bolt12"), payload("lno1tips"))
        .await
        .unwrap();
    let published = on_homeserver(&w, &shop).unwrap();
    w.clock.advance_ms(46 * DAY_MS);
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
    assert_eq!(changed.expires_at(), None);
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
    w.clock.advance_ms(46 * DAY_MS);
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
    assert_eq!(renewed.expires_at(), None);
    let signed = shop.app().record().clone();
    shop.update_own_record().await.unwrap();
    assert_eq!(shop.app().record(), &signed);
}

#[tokio::test]
async fn test_own_record_fixed_deadline_renewal_during_clock_rollback_stays_payable() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let clock = Arc::new(PublisherClock::new(w.clock.now_ms()));
    let mut shop = Box::pin(shop_on_clock(&w, Arc::clone(&clock))).await;
    let id = endpoint("btc-lightning-bolt12");
    shop.set_public_endpoint(id.clone(), payload("lno1tips"))
        .await
        .unwrap();
    let cutoff = w.clock.now_ms() + 60 * DAY_MS;
    shop.set_public_endpoints(shop.own_public_endpoints().unwrap(), Some(cutoff))
        .await
        .unwrap();
    let first = held(&shop, w.bob_id());
    w.clock.advance_ms(46 * DAY_MS);
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
    assert_eq!(renewed.expires_at(), Some(cutoff));
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
    w.clock.advance_ms(46 * DAY_MS);
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
    assert_eq!(renewed.expires_at(), None);
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
    assert_eq!(published.expires_at(), None);
    restored
        .set_public_endpoint(id, payload("lno1restored"))
        .await
        .unwrap();
    assert_eq!(restored.app().record(), published.signed());
}
