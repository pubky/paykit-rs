//! Publishing Private Payment Lists, the payee's side (Paykit spec 8.4 "Publishing", Paykit
//! wire 4.13, 4.14): versions, the list and its changes, the chosen counterparties,
//! renewals, the answers to proofs and list requests, single-use payloads the rail issues and
//! takes back, the payee's check of what it offered, and backups.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    AppScopes, OfferedEndpoint, PayOptions, PayOutcome, Paykit, PaykitConfig, PaykitError, Rail,
    Refusal, SentList, UnrequestedPayment, LIST_LIFETIME_MS,
};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{MemoryStore, StateStore, Write, WriteBatch};

use crate::common::{btc, endpoint, store, World, HOUR_MS};

const DAY_MS: u64 = 24 * HOUR_MS;

fn to_bob(w: &World, id: &str) -> UnrequestedPayment {
    UnrequestedPayment::new(w.bob_id(), btc("0.001"))
        .with_options(PayOptions::default().with_endpoint(endpoint(id)))
}

/// How many Private Payment Lists `wallet` takes at its next receive.
async fn lists_taken(wallet: &mut Paykit) -> usize {
    let got = wallet.receive(None).await.unwrap();
    got.report
        .events
        .iter()
        .filter(|e| {
            matches!(e, AppEvent::MessageReceived { kind, .. }
                if kind == "paykit.private_payment_list")
        })
        .count()
}

/// The payload the rail was handed last for the identifier `id`.
fn last_offered(w: &World, id: &str) -> String {
    let (candidates, _) = w.rail.selections().pop().unwrap();
    candidates
        .iter()
        .find(|c| c.endpoint_id.as_str() == id)
        .map(|c| c.payload.as_str().to_owned())
        .unwrap()
}

/// The payloads the rail took back.
fn released(w: &World) -> Vec<String> {
    w.rail
        .released()
        .iter()
        .map(|r| r.payload.as_str().to_owned())
        .collect()
}

/// Bob's list: a single-use on-chain endpoint and a reusable Lightning offer of `offer`.
fn list(offer: &str) -> BTreeMap<paykit_lib::PaymentEndpointIdentifier, OfferedEndpoint> {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(endpoint("btc-bitcoin-p2wpkh"), OfferedEndpoint::SingleUse);
    endpoints.insert(
        endpoint("btc-lightning-bolt12"),
        OfferedEndpoint::Reusable(PaymentEndpointPayload::new(offer).unwrap()),
    );
    endpoints
}

/// Alice's `wallet` asks bob's `shop` for its list, naming `held_version`.
fn ask_for_the_list(w: &World, shop: &Paykit, held_version: Option<u64>, app_id: [u8; 16]) {
    let body = paykit_lib::bodies::PrivatePaymentListRequest {
        held_version,
        app_id: Some(app_id),
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.alice,
        "wallet",
        &w.bob,
        &[shop.app_id()],
        "paykit.private_payment_list_request",
        Vec::new(),
        body,
    );
}

/// Pay the single-use endpoint of the list `wallet` holds, and let `shop` take the proof.
/// Returns what the shop's receive sent.
async fn paid_single_use(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
) -> paykit_sdk::ListsPublished {
    let paid = wallet
        .pay_without_request(to_bob(w, "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap().lists
}

#[tokio::test]
async fn test_publishing_list_sent_with_a_fresh_single_use_payload_each_version() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let now = w.clock.now_ms();
    let sent = w.publish_to_alice(&mut shop).await;
    assert_eq!(sent.version, now);
    let issued = w.rail.issued();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].counterparty, w.alice_id());
    assert_eq!(issued[0].endpoint_id, endpoint("btc-bitcoin-p2wpkh"));
    assert_eq!(issued[0].list_version, now);
    assert_eq!(issued[0].expires_at_ms, now + LIST_LIFETIME_MS);
    assert_eq!(lists_taken(&mut wallet).await, 1);

    // The proof of the single-use endpoint consumes the version: a newer one goes, with a
    // payload issued for it.
    let after = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert_eq!(last_offered(&w, "btc-bitcoin-p2wpkh"), "bc1qissued1");
    assert_eq!(after.sent.len(), 1, "{after:?}");
    assert!(after.sent[0].version > sent.version);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let again = wallet
        .pay_without_request(to_bob(&w, "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(again, PayOutcome::Paid { .. }), "{again:?}");
    assert_eq!(last_offered(&w, "btc-bitcoin-p2wpkh"), "bc1qissued2");
    let published = shop.published_lists().await.unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].version, after.sent[0].version);
    assert!(published[0].active);
}

#[tokio::test]
async fn test_publishing_reusable_endpoint_paid_sends_no_newer_list() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let got = shop.receive(None).await.unwrap();
    assert_eq!(shop.payee_activity().await.unwrap().len(), 1);
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(w.rail.issued().len(), 1);
}

#[tokio::test]
async fn test_publishing_proof_of_an_older_version_sends_no_newer_list() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // A newer version is in her inbox, not read yet, when she pays the one she holds.
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    let after = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert_eq!(last_offered(&w, "btc-bitcoin-p2wpkh"), "bc1qissued1");
    assert!(after.sent.is_empty(), "{after:?}");
    assert_eq!(w.rail.issued().len(), 2);
}

#[tokio::test]
async fn test_publishing_proof_of_another_apps_list_leaves_this_apps_alone() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let mut till = w
        .join(&w.bob, "till", AppScopes::new().request().build().unwrap())
        .await;
    // Both apps publish in one millisecond: their versions are the same number.
    w.publish_to_alice(&mut shop).await;
    till.set_list_endpoints(list("lno1till")).await.unwrap();
    till.publish_list_to(&[w.alice_id()]).await.unwrap();
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(
            UnrequestedPayment::new(w.bob_id(), btc("0.001")).with_options(
                PayOptions::default()
                    .with_endpoint(endpoint("btc-bitcoin-p2wpkh"))
                    .with_list_app(till.app_id()),
            ),
        )
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    assert_eq!(last_offered(&w, "btc-bitcoin-p2wpkh"), "bc1qissued2");
    // The shop holds `request` and takes the proof; only the till owes a newer list.
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    assert_eq!(till.receive(None).await.unwrap().lists.sent.len(), 1);
}

#[tokio::test]
async fn test_publishing_refused_without_a_list_to_itself_or_without_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
    shop.set_list_endpoints(list("lno1reusable")).await.unwrap();
    let refused = shop.publish_list_to(&[w.bob_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
    let refused = wallet
        .set_list_endpoints(list("lno1reusable"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::ScopeMissing(_))),
        "{refused:?}"
    );
    assert!(w.rail.issued().is_empty());
}

#[tokio::test]
async fn test_publishing_changed_list_sent_to_every_counterparty_that_gets_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert_eq!(changed.sent.len(), 1, "{changed:?}");
    assert_eq!(changed.sent[0].counterparty, w.alice_id());
    assert_eq!(lists_taken(&mut wallet).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert_eq!(last_offered(&w, "btc-lightning-bolt12"), "lno1changed");
}

#[tokio::test]
async fn test_publishing_same_list_again_sends_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let same = shop.set_list_endpoints(list("lno1reusable")).await.unwrap();
    assert!(same.sent.is_empty() && same.not_sent.is_empty(), "{same:?}");
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert_eq!(w.rail.issued().len(), 1);
}

#[tokio::test]
async fn test_publishing_versions_rise_within_one_millisecond() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    let second = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(second.sent[0].version, first.version + 1);
}

#[tokio::test]
async fn test_publishing_list_not_sent_spends_no_version() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.set_list_endpoints(list("lno1reusable")).await.unwrap();
    w.rail.set_issue_fails(true);
    let published = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert!(published.sent.is_empty());
    assert_eq!(published.not_sent.len(), 1, "{published:?}");
    assert_eq!(published.not_sent[0].counterparty, w.alice_id());
    assert!(shop.published_lists().await.unwrap().is_empty());
    assert_eq!(lists_taken(&mut wallet).await, 0);
    w.rail.set_issue_fails(false);
    let published = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(published.sent[0].version, w.clock.now_ms());
}

#[tokio::test]
async fn test_publishing_list_too_large_for_a_message_refused() {
    let w = World::new();
    let mut shop = w.shop().await;
    let huge = "x".repeat(70_000);
    let refused = shop.set_list_endpoints(list(&huge)).await.unwrap_err();
    assert!(
        matches!(
            refused,
            PaykitError::Lib(paykit_lib::Error::Validation(ref why)) if why.contains("one message")
        ),
        "{refused:?}"
    );
    // Nothing was kept: no list is set.
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::Usage(_))),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_publishing_issued_payloads_past_the_limit_not_sent_and_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // The longest reusable offer the list takes: its single-use endpoint isn't counted
    // before a payload is issued for it.
    let (mut fits, mut over) = (1usize, 70_000usize);
    while over - fits > 1 {
        let mid = (fits + over) / 2;
        match shop.set_list_endpoints(list(&"x".repeat(mid))).await {
            Ok(_) => fits = mid,
            Err(_) => over = mid,
        }
    }
    shop.set_list_endpoints(list(&"x".repeat(fits)))
        .await
        .unwrap();
    let published = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert!(published.sent.is_empty());
    assert_eq!(published.not_sent.len(), 1, "{published:?}");
    assert!(
        published.not_sent[0].reason.contains("one message"),
        "{published:?}"
    );
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    assert!(shop.published_lists().await.unwrap().is_empty());
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_renewed_once_half_its_lifetime_is_left() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert!(got.lists.sent[0].version > first.version);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // The renewal carries a fresh single-use payload.
    assert_eq!(w.rail.issued().len(), 2);
}

#[tokio::test]
async fn test_publishing_renewed_after_the_app_did_not_run_past_its_expiry() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(LIST_LIFETIME_MS + 31 * DAY_MS);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(shop.published_lists().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_publishing_not_renewed_with_more_than_half_its_lifetime_left() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(LIST_LIFETIME_MS / 2 - 1);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_list_request_naming_none_answered() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    ask_for_the_list(&w, &shop, None, shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // Answered once: nothing is owed after.
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
}

#[tokio::test]
async fn test_publishing_list_request_naming_the_usable_version_not_answered() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Asked before alice got a list: the shop doesn't publish to her.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    let sent = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // The version she holds, unused and unexpired, or another app's.
    ask_for_the_list(&w, &shop, Some(sent.version), shop.app_id());
    ask_for_the_list(&w, &shop, None, [7; 16]);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_list_request_naming_a_used_version_answered() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let used = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let newer = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert_eq!(newer.sent.len(), 1);
    // Asked while holding the version used, before the newer one came, she gets one more.
    ask_for_the_list(&w, &shop, Some(used.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    // Naming the newer one, unused, she gets none.
    ask_for_the_list(&w, &shop, Some(got.lists.sent[0].version), shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
}

#[tokio::test]
async fn test_publishing_list_request_naming_an_expired_version_answered() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Renewed at 16 days; at 30 days the first has expired and the renewal has more than
    // half its lifetime left.
    w.clock.advance_ms(16 * DAY_MS);
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(14 * DAY_MS + 1);
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    ask_for_the_list(&w, &shop, Some(first.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_withdrawn_counterparty_sent_an_empty_list_and_nothing_after() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let refused = shop.withdraw_list_from(&w.alice_id()).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotFound)),
        "{refused:?}"
    );
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let withdrawn = shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(withdrawn.sent.len(), 1);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let refused = wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NoDestination(_))),
        "{refused:?}"
    );
    assert!(!shop.published_lists().await.unwrap()[0].active);
    // No change, renewal or answer reaches her after.
    assert!(shop
        .set_list_endpoints(list("lno1changed"))
        .await
        .unwrap()
        .sent
        .is_empty());
    ask_for_the_list(&w, &shop, None, shop.app_id());
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert_eq!(w.rail.issued().len(), 1);
    // Once a late proof of its last version can't come any more, she is forgotten.
    w.clock.advance_ms(LIST_LIFETIME_MS / 2 + 30 * DAY_MS);
    shop.receive(None).await.unwrap();
    assert!(shop.published_lists().await.unwrap().is_empty());
    let refused = shop.withdraw_list_from(&w.alice_id()).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotFound)),
        "{refused:?}"
    );
}

/// A store whose commits that put a key starting with `prefix` fail while it is set: with
/// `out/`, the list is recorded and then its queueing fails, as a crash between the two
/// leaves it; with `p/lp/`, its recording fails.
#[derive(Default)]
struct FailingCommits {
    inner: MemoryStore,
    prefix: std::sync::Mutex<Option<&'static [u8]>>,
}

impl FailingCommits {
    fn fail(&self, prefix: Option<&'static [u8]>) {
        *self.prefix.lock().unwrap() = prefix;
    }
}

impl StateStore for FailingCommits {
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

    fn commit(&self, batch: WriteBatch) -> Result<(), pubky_switch_client::error::StoreError> {
        if let Some(prefix) = *self.prefix.lock().unwrap() {
            let hit = batch
                .writes
                .iter()
                .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(prefix)));
            if hit {
                return Err(pubky_switch_client::error::StoreError::Unavailable);
            }
        }
        self.inner.commit(batch)
    }
}

/// Bob's shop on `base`, with his list set.
async fn shop_on(w: &World, base: &Arc<FailingCommits>) -> Paykit {
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(base) as Arc<dyn StateStore>,
        )
        .await;
    shop.set_list_endpoints(list("lno1reusable")).await.unwrap();
    shop
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_never_queued_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    base.fail(Some(b"out/"));
    let published = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(published.not_sent.len(), 1, "{published:?}");
    base.fail(None);
    assert!(released(&w).is_empty());
    // The version is recorded, and never left: the next receive gives its payload back,
    // once.
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w).len(), 1);
    assert_eq!(lists_taken(&mut wallet).await, 0);
    // Given back already, it isn't given back again when the app is removed.
    revoke_shop(&w, &shop);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w).len(), 1);
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_not_recorded_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    base.fail(Some(b"p/lp/"));
    let published = shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(published.not_sent.len(), 1, "{published:?}");
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    base.fail(None);
    assert!(shop.published_lists().await.unwrap().is_empty());
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_overtaken_by_a_newer_one_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.set_list_endpoints(list("lno1reusable")).await.unwrap();
    // The first version's try doesn't reach her switch; the newer one's does, first.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "inbox".into(),
            after: 0,
        });
    let first = shop.publish_list_to(&[w.alice_id()]).await.unwrap().sent[0].clone();
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    w.clock.advance_ms(1000);
    shop.flush().await.unwrap();
    assert!(released(&w).is_empty());
    // She ignores the first, accepted after the newer one: its payload goes back.
    shop.receive(None).await.unwrap();
    let given_back = w.rail.released();
    assert_eq!(given_back.len(), 1, "{given_back:?}");
    assert_eq!(given_back[0].list_version, first.version);
    assert_eq!(lists_taken(&mut wallet).await, 2);
    wallet
        .pay_without_request(to_bob(&w, "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert_eq!(last_offered(&w, "btc-bitcoin-p2wpkh"), "bc1qissued2");
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_the_client_forgot_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Accepted at once, and forgotten by the client before the shop read it: it may have
    // reached her.
    w.clock.advance_ms(31 * DAY_MS);
    shop.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(released(&w).is_empty(), "{:?}", released(&w));
}

#[tokio::test]
async fn test_publishing_payloads_of_an_undelivered_version_released() {
    let w = World::new();
    // Alice's wallet never admits bob.
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    // Queued, waiting for her to admit bob: kept.
    shop.receive(None).await.unwrap();
    assert!(released(&w).is_empty());
    // Given up after 30 days of tries, it never reached her.
    for _ in 0..6 {
        w.clock.advance_ms(8 * DAY_MS);
        shop.flush().await.unwrap();
    }
    shop.receive(None).await.unwrap();
    let given_back = w.rail.released();
    assert_eq!(given_back.len(), 1, "{given_back:?}");
    assert_eq!(given_back[0].list_version, first.version);
    assert_eq!(given_back[0].payload.as_str(), "bc1qissued1");
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_undelivered_version_decided_at_the_receive_that_reports_it() {
    let w = World::new();
    w.join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    for _ in 0..6 {
        w.clock.advance_ms(8 * DAY_MS);
        shop.flush().await.unwrap();
    }
    // Given up undelivered: a call outside a receive lists no duty events, so it leaves the
    // version as it is, and its event for the next receive's report.
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert!(released(&w).is_empty());
    let got = shop.receive(None).await.unwrap();
    assert!(
        got.report.events.iter().any(|e| matches!(e,
            AppEvent::MessageUndelivered { msg_id, .. } if *msg_id == first.msg_id)),
        "{:?}",
        got.report.events
    );
    let given_back = w.rail.released();
    assert_eq!(given_back.len(), 1, "{given_back:?}");
    assert_eq!(given_back[0].list_version, first.version);
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_refused_for_good_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "inbox".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let first = w.publish_to_alice(&mut shop).await;
    assert!(matches!(
        shop.message_delivery(&first.msg_id).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::Failed { .. })
    ));
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_that_reached_the_payer_kept() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // A newer version supersedes it once accepted after it: she may have paid the first.
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(released(&w).is_empty(), "{:?}", released(&w));
    assert_eq!(lists_taken(&mut wallet).await, 2);
}

/// Bob revokes the shop.
fn revoke_shop(w: &World, shop: &Paykit) {
    w.bob.switch().revoke(shop.app_id()).unwrap();
}

#[tokio::test]
async fn test_publishing_payloads_released_when_the_app_is_removed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    revoke_shop(&w, &shop);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    assert!(shop.published_lists().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_publishing_payload_a_proof_used_not_released_when_the_app_is_removed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        paid_single_use(&w, &mut wallet, &mut shop).await.sent.len(),
        1
    );
    revoke_shop(&w, &shop);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued2".to_owned()]);
}

#[tokio::test]
async fn test_publishing_release_refused_asked_again_at_the_next_receive() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    w.rail.set_release_fails(true);
    revoke_shop(&w, &shop);
    shop.receive(None).await.unwrap();
    assert!(released(&w).is_empty());
    w.rail.set_release_fails(false);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    // Taken once, it isn't asked again.
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w).len(), 1);
}

#[tokio::test]
async fn test_publishing_proof_of_a_version_not_offered_not_confirmed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // A list signed with the shop's key that the shop didn't send her: it offered her none.
    w.list_to_alice(&mut shop, &[wallet.app_id()], w.clock.now_ms())
        .await;
    wallet.receive(None).await.unwrap();
    let paid = wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    let refused = shop.verify_payment(&sales[0].record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotOffered)),
        "{refused:?}"
    );
    let refused = shop.confirm_payment(&sales[0].record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotOffered)),
        "{refused:?}"
    );
    assert!(wallet.receive(None).await.unwrap().applied.is_empty());
}

#[tokio::test]
async fn test_publishing_proof_of_an_offered_version_confirmed_against_its_payload() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    paid_single_use(&w, &mut wallet, &mut shop).await;
    let sales = shop.payee_activity().await.unwrap();
    assert!(shop
        .confirm_payment(&sales[0].record)
        .await
        .unwrap()
        .is_some());
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 2);
}

#[tokio::test]
async fn test_publishing_proof_checked_until_its_version_is_forgotten() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let start = w.clock.now_ms();
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    // Tracked until 30 days after it expired, renewals or not.
    let forgotten_at = start + LIST_LIFETIME_MS + 30 * DAY_MS;
    w.clock.advance_ms(forgotten_at - 1 - w.clock.now_ms());
    shop.receive(None).await.unwrap();
    assert!(shop.verify_payment(&record).await.unwrap().is_some());
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    let refused = shop.verify_payment(&record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotOffered)),
        "{refused:?}"
    );
}

/// Bob's signer renews his shop's certificate for `scopes` and lists it in his next
/// `identity.json` (Switch wire 3.8).
fn renew_shop(w: &World, shop: &Paykit, scopes: &paykit_sdk::RequestedScopes) {
    use pubky_switch_core::records::{ListedApp, RenewedCert};
    let keys = w.bob.identity().app_keys("shop");
    let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
    let renewed = pubky_switch_client::testing::issue_cert(
        w.bob.identity().key(),
        &keys,
        &pubky_switch_client::testing::AppSpec::new(&names).with_permissions(&scopes.permissions),
        w.clock.now_ms() + 1_000,
    )
    .unwrap();
    let app_id = shop.app_id();
    w.bob
        .switch()
        .signer_op(pubky_switch_core::ops::OpKind::CertPut {
            target_app_id: app_id,
            cert: renewed.signed().clone(),
        })
        .unwrap();
    let old = shop.app().cert().hash();
    w.bob
        .next_doc(|body| {
            body.apps.retain(|a| a.app_id != app_id);
            body.apps.push(ListedApp {
                app_id,
                cert: renewed.hash(),
            });
            body.renewed.push(RenewedCert {
                app_id,
                cert: old,
                until: None,
            });
        })
        .unwrap();
}

#[tokio::test]
async fn test_publishing_no_renewal_under_a_certificate_without_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    renew_shop(&w, &shop, &AppScopes::new().contacts().build().unwrap());
    w.clock.advance_ms(2_000);
    let _ = shop.receive(None).await;
    shop.receive(None).await.unwrap();
    assert!(!shop.app().cert().scopes().iter().any(|s| s == "request"));
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(w.rail.issued().len(), 1);
}

/// Bob's shop restored from `backup`, on a store of its own.
async fn restored_shop(w: &World, backup: &paykit_sdk::PaykitBackup) -> Paykit {
    let mut shop = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    shop.receive(None).await.unwrap();
    shop
}

#[tokio::test]
async fn test_publishing_state_restored_and_versions_stay_above_the_lost_ones() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    // Sent after the backup, then the device is lost.
    let lost: SentList = shop.publish_list_to(&[w.alice_id()]).await.unwrap().sent[0].clone();
    drop(shop);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(10);

    let mut shop = restored_shop(&w, &backup).await;
    let published = shop.published_lists().await.unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].version, first.version);
    // She asks naming the version the restore lost: it is answered above it.
    ask_for_the_list(&w, &shop, Some(lost.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert!(got.lists.sent[0].version > lost.version);
    assert_eq!(lists_taken(&mut wallet).await, 1);
}
