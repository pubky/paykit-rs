//! Publishing Private Payment Lists, the payee's side (Paykit spec 8.4 "Publishing", Paykit
//! wire 4.13, 4.14): versions, the list and its changes, the chosen counterparties,
//! renewals, the answers to proofs and list requests, single-use payloads the rail issues and
//! takes back, the payee's check of what it offered, and backups.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    AppScopes, OfferedEndpoint, PayOptions, PayOutcome, Paykit, PaykitConfig, PaykitError, Rail,
    Refusal, SentList, UnrequestedPayment, LISTS_REFRESH_MS, LIST_LIFETIME_MS,
};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{MemoryStore, StateStore, Write, WriteBatch};

use crate::common::{btc, endpoint, is_corrupt, put_state, rewrite_state, store, World, HOUR_MS};

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
    // The shop holds `paykit_request` and takes the proof; only the till owes a newer list.
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
async fn test_publishing_list_request_from_a_stranger_or_about_another_app_not_answered() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Asked before alice got a list: the shop doesn't publish to her.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    ask_for_the_list(&w, &shop, None, [7; 16]);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_list_request_naming_the_current_version_answered_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let current = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Used by her word: a newer version, with a fresh single-use payload.
    ask_for_the_list(&w, &shop, Some(current.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(w.rail.issued().len(), 2);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let newer = got.lists.sent[0].clone();
    // Named again, it gets no second one: the newest is sent again, nothing issued, so an
    // answer lost on the way is answered.
    ask_for_the_list(&w, &shop, Some(current.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(got.lists.resent, vec![newer]);
    assert_eq!(w.rail.issued().len(), 2);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_newest_sent_again_at_most_once_an_hour() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let older = w.publish_to_alice(&mut shop).await;
    w.clock.advance_ms(1000);
    let current = shop.publish_list_to(&[w.alice_id()]).await.unwrap().sent[0].clone();
    wallet.receive(None).await.unwrap();
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    assert_eq!(
        shop.receive(None).await.unwrap().lists.resent,
        vec![current.clone()]
    );
    // Within the hour, a request naming it again gets nothing, and isn't answered later.
    w.clock.advance_ms(HOUR_MS - 1);
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(
        got.lists.sent.is_empty() && got.lists.resent.is_empty(),
        "{:?}",
        got.lists
    );
    w.clock.advance_ms(1);
    assert!(shop.receive(None).await.unwrap().lists.resent.is_empty());
    // An hour on, it is sent again, once: an hour later, unasked, it isn't.
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    assert_eq!(
        shop.receive(None).await.unwrap().lists.resent,
        vec![current]
    );
    w.clock.advance_ms(HOUR_MS);
    assert!(shop.receive(None).await.unwrap().lists.resent.is_empty());
    assert_eq!(w.rail.issued().len(), 2);
}

#[tokio::test]
async fn test_publishing_list_requests_answered_within_the_daily_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(2),
            store(),
        )
        .await;
    assert_eq!(PaykitConfig::default().list_answers_per_day, 4);
    let mut current = w.publish_to_alice(&mut shop).await.version;
    wallet.receive(None).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(1000);
        ask_for_the_list(&w, &shop, Some(current), shop.app_id());
        let got = shop.receive(None).await.unwrap();
        assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
        current = got.lists.sent[0].version;
    }
    // Past the bound: no answer and nothing issued, until 24 hours after the first answer.
    ask_for_the_list(&w, &shop, Some(current), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(
        got.lists.sent.is_empty() && got.lists.resent.is_empty(),
        "{:?}",
        got.lists
    );
    assert_eq!(w.rail.issued().len(), 3);
    w.clock.advance_ms(24 * HOUR_MS - 1000 - 1);
    ask_for_the_list(&w, &shop, Some(current), shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    // The version named past the bound wasn't answered: named again then, it is.
    w.clock.advance_ms(1);
    ask_for_the_list(&w, &shop, Some(current), shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    assert_eq!(w.rail.issued().len(), 4);
}

#[tokio::test]
async fn test_publishing_list_answers_bounded_in_any_24_hours_not_by_the_calendar_day() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(1),
            store(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Answered a minute before a calendar day ends.
    let midnight = (w.clock.now_ms() / DAY_MS + 1) * DAY_MS;
    w.clock.advance_ms(midnight - 60_000 - w.clock.now_ms());
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    // A minute into the next day, the bound still holds.
    w.clock.advance_ms(2 * 60_000);
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    // 24 hours after that answer, it is answered again.
    w.clock.advance_ms(DAY_MS - 2 * 60_000);
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
}

#[tokio::test]
async fn test_publishing_list_requests_naming_none_answered_within_the_daily_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(2),
            store(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    for _ in 0..2 {
        ask_for_the_list(&w, &shop, None, shop.app_id());
        assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    }
    // Past the bound: no answer, nothing issued. Renewals don't count against it.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    assert_eq!(w.rail.issued().len(), 3);
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    // A day on, a request is answered again.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
}

#[tokio::test]
async fn test_publishing_list_request_naming_an_older_version_resends_the_current() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let older = w.publish_to_alice(&mut shop).await;
    w.clock.advance_ms(1000);
    let current = shop.publish_list_to(&[w.alice_id()]).await.unwrap().sent[0].clone();
    wallet.receive(None).await.unwrap();
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(got.lists.resent, vec![current]);
    assert_eq!(w.rail.issued().len(), 2);
    // Named again, it is not sent again.
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.resent.is_empty(), "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_newest_sent_again_held_while_the_deny_copy_is_unknown() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    let older = shop.published_lists().await.unwrap()[0].version;
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // She names the older version; both of the receive's reads of the lists fail.
    ask_for_the_list(&w, &shop, Some(older), shop.app_id());
    w.clock.advance_ms(LISTS_REFRESH_MS);
    fail_list_reads(&w, 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.resent.is_empty(), "{:?}", got.lists);
    // Read again, the newest goes again.
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.resent.len(), 1, "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_request_held_past_the_bound_answered_once_by_a_change() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(1),
            store(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    // Past the bound, a second request is held.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    // A list change sends her a newer version: it answers the held request.
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert_eq!(changed.sent.len(), 1, "{changed:?}");
    // Once the bound frees, nothing more goes for it.
    w.clock.advance_ms(DAY_MS);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_proofs_answered_within_the_daily_bound() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(1),
            store(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // The first proof of a single-use endpoint gets a newer version.
    let after = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert_eq!(after.sent.len(), 1, "{after:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // The second, in the same day, is past the bound: no fresh payload is issued.
    let issued = w.rail.issued().len();
    let held = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert!(held.sent.is_empty(), "{held:?}");
    assert_eq!(w.rail.issued().len(), issued);
    // Once the bound frees, it is answered.
    w.clock.advance_ms(DAY_MS);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_list_request_naming_a_used_older_version_resends_the_newest() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let used = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    let newer = paid_single_use(&w, &mut wallet, &mut shop).await;
    assert_eq!(newer.sent.len(), 1);
    // Asked while holding the version used, before the newer one came: the newer one is
    // sent again, nothing issued.
    ask_for_the_list(&w, &shop, Some(used.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(got.lists.resent, newer.sent);
    assert_eq!(w.rail.issued().len(), 2);
}

#[tokio::test]
async fn test_publishing_newer_version_supersedes_a_resend() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let older = w.publish_to_alice(&mut shop).await;
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    wallet.receive(None).await.unwrap();
    // Asked about the older one as the newer one's renewal is due: the renewal answers it.
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    ask_for_the_list(&w, &shop, Some(older.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert!(got.lists.resent.is_empty(), "{:?}", got.lists);
    let got = shop.receive(None).await.unwrap();
    assert!(
        got.lists.sent.is_empty() && got.lists.resent.is_empty(),
        "{:?}",
        got.lists
    );
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
    // Named again, it is answered: the newest is sent again, nothing issued.
    let issued = w.rail.issued().len();
    ask_for_the_list(&w, &shop, Some(first.version), shop.app_id());
    let again = shop.receive(None).await.unwrap();
    assert!(again.lists.sent.is_empty(), "{:?}", again.lists);
    assert_eq!(again.lists.resent, got.lists.sent);
    assert_eq!(w.rail.issued().len(), issued);
}

#[tokio::test]
async fn test_publishing_expired_version_named_past_the_bound_held_until_it_frees() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
            PaykitConfig::default().with_list_answers_per_day(1),
            store(),
        )
        .await;
    let first = w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Renewed at 16 days; at 30 days the first has expired.
    w.clock.advance_ms(16 * DAY_MS);
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    w.clock.advance_ms(14 * DAY_MS + 1);
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert_eq!(shop.receive(None).await.unwrap().lists.sent.len(), 1);
    // Past the bound, the expired version named gets nothing yet: the request is held.
    ask_for_the_list(&w, &shop, Some(first.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(
        got.lists.sent.is_empty() && got.lists.resent.is_empty(),
        "{:?}",
        got.lists
    );
    // Once the bound frees, the held request is answered with a newer version.
    w.clock.advance_ms(DAY_MS);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    // Named again, it was answered: the newest goes again, nothing issued.
    ask_for_the_list(&w, &shop, Some(first.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(got.lists.resent.len(), 1, "{:?}", got.lists);
}

/// The lengths of what bob's shop remembers of alice's list requests: the versions owed
/// an answer and those answered.
async fn remembered(shop: &Paykit, w: &World) -> (usize, usize) {
    let key = format!("lp/{}", pubky_switch_core::enc::b64(&w.alice_id()));
    let bytes = shop
        .app()
        .protocol_get(key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let len = |name: &str| value["d"][name].as_array().map_or(0, Vec::len);
    (len("asking"), len("named"))
}

#[tokio::test]
async fn test_publishing_versions_a_counterparty_names_remembered_up_to_sixteen() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    wallet.receive(None).await.unwrap();
    // Twenty requests naming versions the shop never sent: all owed, sixteen remembered.
    for version in 1..=20 {
        ask_for_the_list(&w, &shop, Some(version), shop.app_id());
    }
    let got = shop.receive(None).await.unwrap();
    // One newer version answers them all, and at most sixteen are remembered as answered.
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(remembered(&shop, &w).await, (0, 16));
    // Twenty more: still bounded, and the state reads back.
    for version in 21..=40 {
        ask_for_the_list(&w, &shop, Some(version), shop.app_id());
    }
    shop.receive(None).await.unwrap();
    let (asking, named) = remembered(&shop, &w).await;
    assert!(asking <= 16 && named <= 16, "{asking} {named}");
    shop.published_lists().await.unwrap();
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
    /// Commits under the prefix that still go before the next fails.
    skip: std::sync::atomic::AtomicUsize,
}

impl FailingCommits {
    fn fail(&self, prefix: Option<&'static [u8]>) {
        *self.prefix.lock().unwrap() = prefix;
    }

    /// Fail commits under `prefix` after `skip` of them have gone.
    fn fail_after(&self, prefix: &'static [u8], skip: usize) {
        self.skip.store(skip, std::sync::atomic::Ordering::SeqCst);
        self.fail(Some(prefix));
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
                let skip = self.skip.load(std::sync::atomic::Ordering::SeqCst);
                if skip > 0 {
                    self.skip
                        .store(skip - 1, std::sync::atomic::Ordering::SeqCst);
                } else {
                    return Err(pubky_switch_client::error::StoreError::Unavailable);
                }
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
async fn test_publishing_payloads_of_a_version_never_queued_released_and_the_list_sent_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    base.fail(Some(b"out/"));
    // The store's failure is the call's: no list is reported as merely not sent.
    let failed = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(
            failed,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(_))
        ),
        "{failed:?}"
    );
    base.fail(None);
    assert!(released(&w).is_empty());
    // The version is recorded, and never left: the next receive gives its payload back,
    // once, and sends her a newer version, still owed.
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w).len(), 1);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // Given back already, it isn't given back again when the app is removed; the version
    // she holds is.
    revoke_shop(&w, &shop);
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w).len(), 2);
}

#[tokio::test]
async fn test_publishing_payloads_of_a_version_not_recorded_released() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    base.fail(Some(b"p/lp/"));
    let failed = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(
            failed,
            PaykitError::Client(pubky_switch_client::app::AppError::Store(_))
        ),
        "{failed:?}"
    );
    base.fail(None);
    assert!(shop.published_lists().await.unwrap().is_empty());
    // Kept to give back, the payload goes back at the next receive.
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qissued1".to_owned()]);
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

/// Bob's shop holding `paykit_request` and `paykit_manage_contacts`, so it reads the deny
/// list, with its list published to alice and taken by `wallet`.
async fn shop_reading_lists(w: &World, wallet: &mut Paykit) -> Paykit {
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(wallet).await, 1);
    shop
}

/// Whether the only list not sent is alice's, held while the copy of the deny list isn't
/// known.
fn held_for_alice(w: &World, published: &paykit_sdk::ListsPublished) -> bool {
    published.sent.is_empty()
        && published.not_sent.len() == 1
        && published.not_sent[0].counterparty == w.alice_id()
        && published.not_sent[0].reason.contains("isn't known")
}

/// Whether `published` names alice as not sent for being blocked.
fn blocked_for_alice(w: &World, published: &paykit_sdk::ListsPublished) -> bool {
    published.sent.is_empty()
        && published.not_sent.len() == 1
        && published.not_sent[0].counterparty == w.alice_id()
        && published.not_sent[0].reason == Refusal::CounterpartyBlocked.to_string()
}

#[tokio::test]
async fn test_publishing_blocked_counterparty_gets_no_renewal_answer_or_resend() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    let older = shop.published_lists().await.unwrap()[0].version;
    w.clock.advance_ms(1000);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    shop.block(&w.alice_id()).await.unwrap();
    // Asked about an older version, the newest isn't sent her again.
    ask_for_the_list(&w, &shop, Some(older), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.resent.is_empty(), "{:?}", got.lists);
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert!(blocked_for_alice(&w, &got.lists), "{:?}", got.lists);
    ask_for_the_list(&w, &shop, None, shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert!(blocked_for_alice(&w, &got.lists), "{:?}", got.lists);
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(blocked_for_alice(&w, &changed), "{changed:?}");
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert_eq!(w.rail.issued().len(), 2);
}

#[tokio::test]
async fn test_publishing_to_a_blocked_counterparty_refused() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    shop.block(&w.alice_id()).await.unwrap();
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::CounterpartyBlocked)),
        "{refused:?}"
    );
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert_eq!(w.rail.issued().len(), 1);
}

/// Whether alice's wallet finds no endpoint to pay bob: the empty version replaced the list.
async fn no_usable_list(w: &World, wallet: &mut Paykit) -> bool {
    let refused = wallet
        .pay_without_request(to_bob(w, "btc-lightning-bolt12"))
        .await
        .unwrap_err();
    matches!(refused.refusal(), Some(Refusal::NoDestination(_)))
}

#[tokio::test]
async fn test_publishing_withdrawal_goes_to_a_blocked_counterparty() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    shop.block(&w.alice_id()).await.unwrap();
    // Blocked, she is sent no list, but the withdrawal goes: she could still pay bob.
    let withdrawn = shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(withdrawn.sent.len(), 1, "{withdrawn:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
    // Every other list stays held.
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(changed.sent.is_empty(), "{changed:?}");
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_withdrawal_goes_while_the_deny_copy_is_unknown() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    // Both of a receive's reads of the lists fail: the copy isn't known after it.
    w.clock.advance_ms(LISTS_REFRESH_MS);
    fail_list_reads(&w, 2);
    shop.receive(None).await.unwrap();
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(held_for_alice(&w, &changed), "{changed:?}");
    fail_list_reads(&w, 1);
    let withdrawn = shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(withdrawn.sent.len(), 1, "{withdrawn:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}

#[tokio::test]
async fn test_publishing_withdrawal_refused_for_good_not_sent_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    shop.block(&w.alice_id()).await.unwrap();
    // Her switch refuses the empty version for good: no re-send changes that.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "inbox".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    for _ in 0..3 {
        w.clock.advance_ms(2 * HOUR_MS);
        let got = shop.receive(None).await.unwrap();
        assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    }
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_restored_withdrawal_refused_for_good_not_sent_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // The withdrawal's delivery fails for now: the backup holds it unseen, in its outbox.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    // The host flushes the restored outbox, and her switch refuses it for good: no re-send
    // changes that, and nothing of it is sent again.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "inbox".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    let mut shop = restored_shop(&w, &backup).await;
    shop.flush().await.unwrap();
    for _ in 0..4 {
        w.clock.advance_ms(2 * HOUR_MS);
        let got = shop.receive(None).await.unwrap();
        shop.flush().await.unwrap();
        assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    }
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

#[tokio::test]
async fn test_publishing_withdrawal_that_never_left_sent_again_an_hour_after_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // The empty version is recorded, and its queueing fails: it never left.
    base.fail(Some(b"out/"));
    assert!(shop.withdraw_list_from(&w.alice_id()).await.is_err());
    base.fail(None);
    // Not within the hour of the withdrawal: nothing goes yet.
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    w.clock.advance_ms(HOUR_MS - 1);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    // An hour after it, it goes again; reached, never again.
    w.clock.advance_ms(1);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
    w.clock.advance_ms(2 * HOUR_MS);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
}

#[tokio::test]
async fn test_publishing_unblocked_counterparty_renewed_at_the_next_receive() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    shop.block(&w.alice_id()).await.unwrap();
    shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(shop.receive(None).await.unwrap().lists.sent.is_empty());
    shop.unblock(&w.alice_id()).await.unwrap();
    // The changed list was owed her: it goes once she is unblocked.
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert_eq!(last_offered(&w, "btc-lightning-bolt12"), "lno1changed");
}

/// The next `n` reads of bob's lists fail.
fn fail_list_reads(w: &World, n: usize) {
    for _ in 0..n {
        w.bob
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
    }
}

#[tokio::test]
async fn test_publishing_with_the_deny_list_unread_sends_nothing() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    // The copy of the lists is too old to judge by, and reading them again fails.
    w.clock.advance_ms(LISTS_REFRESH_MS);
    fail_list_reads(&w, 1);
    // The copy is unknown after the failed read: the send is held, nothing sent.
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    // Its renewal due, the receive's own read of the lists and the one before sending fail.
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    fail_list_reads(&w, 2);
    let got = shop.receive(None).await.unwrap();
    assert!(held_for_alice(&w, &got.lists), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
    // Read again, the renewal goes.
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
}

#[tokio::test]
async fn test_publishing_with_the_deny_list_unknown_holds_a_changed_list_and_an_answer() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    // The copy is due again and its read fails: it isn't known, so the changed list is
    // held for alice, nothing issued for her.
    w.clock.advance_ms(LISTS_REFRESH_MS);
    fail_list_reads(&w, 1);
    let changed = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(held_for_alice(&w, &changed), "{changed:?}");
    assert_eq!(w.rail.issued().len(), 1);
    // Her list request is owed but not answered while both of the receive's reads fail.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    fail_list_reads(&w, 2);
    let got = shop.receive(None).await.unwrap();
    assert!(held_for_alice(&w, &got.lists), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
    // Read again, what she is owed goes: one newer version for both the change and the
    // request.
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
}

#[tokio::test]
async fn test_publishing_block_through_the_client_seen_at_the_next_lists_read() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    shop.app_mut().deny(&w.alice_id()).await.unwrap();
    // The copy read when the list was sent is judged by for 10 minutes.
    w.clock.advance_ms(LISTS_REFRESH_MS - 1);
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    w.clock.advance_ms(1);
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::CounterpartyBlocked)),
        "{refused:?}"
    );
    assert_eq!(lists_taken(&mut wallet).await, 0);
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
async fn test_publishing_late_proof_of_an_untracked_version_checked_by_the_rail_alone() {
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
    // Tracked until 30 days after it expired, renewals or not, and checked against the
    // payload offered; past that, a late proof of the payment is still credited, by the
    // payment method alone.
    let forgotten_at = start + LIST_LIFETIME_MS + 30 * DAY_MS;
    w.clock.advance_ms(forgotten_at - 1 - w.clock.now_ms());
    shop.receive(None).await.unwrap();
    assert!(shop.verify_payment(&record).await.unwrap().is_some());
    w.clock.advance_ms(1);
    shop.receive(None).await.unwrap();
    assert!(shop.verify_payment(&record).await.unwrap().is_some());
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

/// Alice's wallet holds a list signed with the shop's key at the version the shop then
/// publishes, with `endpoints`: she keeps it, ignoring the shop's own of the same version,
/// and pays its Lightning offer. Returns the shop and the payee's record of the payment.
async fn paid_to_a_list_at_the_shops_version(
    w: &World,
    endpoints: std::collections::BTreeMap<
        paykit_lib::PaymentEndpointIdentifier,
        paykit_lib::bodies::ListEndpoint,
    >,
    id: &str,
) -> (Paykit, [u8; 32]) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let version = w.clock.now_ms();
    w.list_to_alice_with(&mut shop, &[wallet.app_id()], version, endpoints)
        .await;
    wallet.receive(None).await.unwrap();
    assert_eq!(w.publish_to_alice(&mut shop).await.version, version);
    wallet.receive(None).await.unwrap();
    let paid = wallet.pay_without_request(to_bob(w, id)).await.unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    (shop, record)
}

fn offer(
    id: &str,
    payload: &str,
) -> std::collections::BTreeMap<
    paykit_lib::PaymentEndpointIdentifier,
    paykit_lib::bodies::ListEndpoint,
> {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint(id),
        paykit_lib::bodies::ListEndpoint {
            payload: PaymentEndpointPayload::new(payload).unwrap(),
            single_use: false,
        },
    );
    endpoints
}

#[tokio::test]
async fn test_publishing_proof_of_an_endpoint_the_tracked_version_lacks_not_offered() {
    let w = World::new();
    let (mut shop, record) = paid_to_a_list_at_the_shops_version(
        &w,
        offer("btc-lightning-bolt11", "lnbc1other"),
        "btc-lightning-bolt11",
    )
    .await;
    let refused = shop.verify_payment(&record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotOffered)),
        "{refused:?}"
    );
}

#[tokio::test]
async fn test_publishing_proof_past_the_kept_offer_credited_by_the_rail_alone() {
    let w = World::new();
    let (mut shop, record) = paid_to_a_list_at_the_shops_version(
        &w,
        offer("btc-lightning-bolt11", "lnbc1other"),
        "btc-lightning-bolt11",
    )
    .await;
    // The newest version she holds stays tracked, but its offer is kept only until 30 days
    // after its expiry: after that, the proof is checked by the payment method alone.
    let expires_at = shop.published_lists().await.unwrap()[0].expires_at_ms;
    w.clock
        .advance_ms(expires_at + 30 * DAY_MS - 1 - w.clock.now_ms());
    let refused = shop.verify_payment(&record).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotOffered)),
        "{refused:?}"
    );
    w.clock.advance_ms(1);
    assert!(shop.verify_payment(&record).await.unwrap().is_some());
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
}

#[tokio::test]
async fn test_publishing_proof_of_another_payload_at_a_tracked_version_not_confirmed() {
    let w = World::new();
    let (mut shop, record) = paid_to_a_list_at_the_shops_version(
        &w,
        offer("btc-lightning-bolt12", "lno1other"),
        "btc-lightning-bolt12",
    )
    .await;
    // Checked against the payload the shop offered, the payment to another isn't confirmed.
    assert_eq!(shop.verify_payment(&record).await.unwrap(), None);
    assert_eq!(shop.confirm_payment(&record).await.unwrap(), None);
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
    assert!(!shop
        .app()
        .cert()
        .scopes()
        .iter()
        .any(|s| s == "paykit_request"));
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
    // The host sets its list again: she is sent a version above the lost one.
    assert!(shop.lists_await_host().await.unwrap());
    let set = shop.set_list_endpoints(alice_list()).await.unwrap();
    assert_eq!(set.sent.len(), 1, "{set:?}");
    assert!(set.sent[0].version > lost.version);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // She asks naming the version the restore lost: it is answered above it.
    ask_for_the_list(&w, &shop, Some(lost.version), shop.app_id());
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 1, "{:?}", got.lists);
    assert!(got.lists.sent[0].version > lost.version);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    // Named again, the version it doesn't track gets no second one: the newest goes again.
    ask_for_the_list(&w, &shop, Some(lost.version), shop.app_id());
    let again = shop.receive(None).await.unwrap();
    assert!(again.lists.sent.is_empty(), "{:?}", again.lists);
    assert_eq!(again.lists.resent, got.lists.sent);
}

/// The list [`World::publish_to_alice`] sets.
fn alice_list() -> BTreeMap<paykit_lib::PaymentEndpointIdentifier, OfferedEndpoint> {
    let mut endpoints = BTreeMap::new();
    endpoints.insert(endpoint("btc-bitcoin-p2wpkh"), OfferedEndpoint::SingleUse);
    endpoints.insert(
        endpoint("btc-lightning-bolt12"),
        OfferedEndpoint::Reusable(PaymentEndpointPayload::new("lno1reusable").unwrap()),
    );
    endpoints
}

/// Bob's local key of what he sent `counterparty`.
fn sent_key(counterparty: &[u8; 32]) -> String {
    format!("lp/{}", pubky_switch_core::enc::b64(counterparty))
}

#[tokio::test]
async fn test_publishing_lists_sent_kept_under_another_counterparty_refused_as_corrupt() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(shop.published_lists().await.unwrap().len(), 1);
    // Alice's key holds what names carol: no read takes it for carol's, or as nothing sent.
    let carol = [7; 32];
    rewrite_state(&mut shop, &sent_key(&w.alice_id()), |d| {
        d["counterparty"] = pubky_switch_core::enc::b64(&carol).into();
    })
    .await;
    assert!(is_corrupt(&shop.published_lists().await.unwrap_err()));
    assert!(is_corrupt(
        &shop.withdraw_list_from(&w.alice_id()).await.unwrap_err()
    ));
    assert!(is_corrupt(&shop.receive(None).await.unwrap_err()));
    assert!(shop
        .app()
        .protocol_get(sent_key(&carol).as_bytes())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_publishing_lists_sent_with_no_version_refused_as_corrupt() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    // A run forgets a counterparty with its last version: kept without one, it isn't absent.
    rewrite_state(&mut shop, &sent_key(&w.alice_id()), |d| {
        d["versions"] = serde_json::json!([]);
    })
    .await;
    assert!(is_corrupt(&shop.published_lists().await.unwrap_err()));
    assert!(is_corrupt(
        &shop.publish_list_to(&[w.alice_id()]).await.unwrap_err()
    ));
}

#[tokio::test]
async fn test_publishing_list_request_against_lists_sent_under_another_key_parked() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    shop.receive(None).await.unwrap();
    rewrite_state(&mut shop, &sent_key(&w.alice_id()), |d| {
        d["counterparty"] = pubky_switch_core::enc::b64(&[7; 32]).into();
    })
    .await;
    // The receive step reads alice's lists sent as corrupt: her request is held, not taken.
    ask_for_the_list(&w, &shop, None, shop.app_id());
    assert!(is_corrupt(&shop.receive(None).await.unwrap_err()));
    let parked = shop.app_mut().parked_entries().await.unwrap();
    assert_eq!(parked.len(), 1, "{parked:?}");
}

#[tokio::test]
async fn test_publishing_release_kept_under_another_counterparty_refused_as_corrupt() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    shop.receive(None).await.unwrap();
    let release = |counterparty: &[u8; 32]| {
        serde_json::json!({
            "v": 1,
            "d": {
                "counterparty": pubky_switch_core::enc::b64(counterparty),
                "version": 7,
                "payloads": { "btc-bitcoin-p2wpkh": "bc1qkept" },
            }
        })
    };
    let at_alice = format!(
        "rl/{}/{:020}",
        pubky_switch_core::enc::b64(&w.alice_id()),
        7
    );
    // Bound to its key, a release is asked of the method.
    put_state(&mut shop, &at_alice, &release(&w.alice_id())).await;
    shop.receive(None).await.unwrap();
    assert_eq!(released(&w), vec!["bc1qkept".to_owned()]);
    // One naming carol at alice's key goes to no one.
    put_state(&mut shop, &at_alice, &release(&[7; 32])).await;
    assert!(is_corrupt(&shop.receive(None).await.unwrap_err()));
    assert_eq!(w.rail.released().len(), 1);
}

#[tokio::test]
async fn test_publishing_list_misspelled_refused_as_corrupt() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    rewrite_state(&mut shop, "ll", |d| {
        d["endpoints"]["not an identifier"] = serde_json::Value::Null;
    })
    .await;
    assert!(is_corrupt(
        &shop.publish_list_to(&[w.alice_id()]).await.unwrap_err()
    ));
    assert!(is_corrupt(
        &shop
            .set_list_endpoints(list("lno1other"))
            .await
            .unwrap_err()
    ));
}

#[tokio::test]
async fn test_publishing_last_version_below_a_version_sent_refused_as_corrupt() {
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = w.publish_to_alice(&mut shop).await;
    w.clock.advance_ms(1_000);
    put_state(
        &mut shop,
        "lv",
        &serde_json::json!({ "v": 1, "d": first.version - 1 }),
    )
    .await;
    // The next version would be built on a last version below one sent: none goes.
    assert!(is_corrupt(
        &shop.publish_list_to(&[w.alice_id()]).await.unwrap_err()
    ));
    assert_eq!(
        shop.published_lists().await.unwrap()[0].version,
        first.version
    );
}

/// A changed list whose queueing fails (as a crash between its record and its queueing
/// leaves it) still reaches the counterparty: the change is owed until it is sent.
#[tokio::test]
async fn test_publishing_changed_list_owed_after_its_queueing_failed() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    base.fail(Some(b"out/"));
    assert!(shop.set_list_endpoints(list("lno1changed")).await.is_err());
    base.fail(None);
    // The host sets the list again, and the app receives: the change should reach alice.
    let again = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    let got = shop.receive(None).await.unwrap();
    w.clock.advance_ms(2 * HOUR_MS);
    let later = shop.receive(None).await.unwrap();
    let taken = lists_taken(&mut wallet).await;
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert_eq!(
        last_offered(&w, "btc-lightning-bolt12"),
        "lno1changed",
        "again={again:?} got={:?} later={:?} taken={taken}",
        got.lists,
        later.lists
    );
}

/// A restore gives back no single-use payload of a version the payer received before the
/// backup: she can still pay it.
#[tokio::test]
async fn test_publishing_restore_keeps_the_payload_of_a_delivered_version() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    shop.receive(None).await.unwrap();
    let given_back = released(&w);
    // She pays the single-use payload of the version she holds.
    let paid = wallet
        .pay_without_request(to_bob(&w, "btc-bitcoin-p2wpkh"))
        .await
        .unwrap();
    assert!(matches!(paid, PayOutcome::Paid { .. }), "{paid:?}");
    let payload = last_offered(&w, "btc-bitcoin-p2wpkh");
    assert!(
        !given_back.contains(&payload),
        "paid {payload}, which the restored shop gave back: {given_back:?}"
    );
}

/// The same with the version settled before the backup: seen reached, nothing goes back.
#[tokio::test]
async fn test_publishing_restore_keeps_the_payload_of_a_version_settled_before_the_backup() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    shop.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    shop.receive(None).await.unwrap();
    assert!(released(&w).is_empty(), "released {:?}", released(&w));
}

/// After a restore no list but a withdrawal goes until the host sets its list again: a
/// counterparty withdrawn from after the backup isn't renewed, and the backup's list isn't
/// sent on its own.
#[tokio::test]
async fn test_publishing_restored_lists_wait_for_the_host() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let backup = shop.backup().await.unwrap();
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    assert!(shop.lists_await_host().await.unwrap());
    // The host is told once.
    let waits = |notices: Vec<paykit_sdk::Notice>| {
        notices
            .iter()
            .filter(|n| n.kind == paykit_sdk::NoticeKind::ListsAwaitHost)
            .count()
    };
    assert_eq!(waits(shop.notices().await.unwrap()), 1);
    // The renewal is due: it waits for the host, nothing goes.
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(got.lists.not_sent.len(), 1, "{:?}", got.lists);
    let refused = shop.publish_list_to(&[w.alice_id()]).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert!(no_usable_list(&w, &mut wallet).await);
    // The host re-applies the withdrawal, which goes, then sets its list: she gets no list.
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(waits(shop.notices().await.unwrap()), 1);
    let set = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(set.sent.is_empty(), "{set:?}");
    assert!(!shop.lists_await_host().await.unwrap());
    // Set, the list waits no more, and the notice goes.
    assert_eq!(waits(shop.notices().await.unwrap()), 0);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// An app that never published a list waits for nothing after a restore, and isn't told it
/// does.
#[tokio::test]
async fn test_publishing_restore_without_a_list_waits_for_nothing() {
    let w = World::new();
    let shop = w.shop().await;
    let mut shop = shop;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let shop = restored_shop(&w, &backup).await;
    assert!(!shop.lists_await_host().await.unwrap());
    assert!(shop
        .notices()
        .await
        .unwrap()
        .iter()
        .all(|n| n.kind != paykit_sdk::NoticeKind::ListsAwaitHost));
}

/// After a restore the backup's list isn't sent until the host sets its list, and then the
/// host's list goes, not the backup's.
#[tokio::test]
async fn test_publishing_restored_list_not_republished_until_the_host_sets_it() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    assert_eq!(lists_taken(&mut wallet).await, 1);
    let backup = shop.backup().await.unwrap();
    // The user removes the reusable endpoint after the backup.
    shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    // The host sets its list: that one goes.
    let set = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert_eq!(set.sent.len(), 1, "{set:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert_eq!(last_offered(&w, "btc-lightning-bolt12"), "lno1changed");
}

/// The same for a counterparty also blocked: the shared deny list still holds her after
/// the restore, so no renewal goes.
#[tokio::test]
async fn test_publishing_withdrawal_after_the_backup_lost_but_the_block_holds() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = shop_reading_lists(&w, &mut wallet).await;
    let backup = shop.backup().await.unwrap();
    shop.block(&w.alice_id()).await.unwrap();
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    w.clock.advance_ms(LIST_LIFETIME_MS / 2);
    let got = shop.receive(None).await.unwrap();
    assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 0);
}

/// A change whose fan-out stops at the first counterparty, its queueing failing, still
/// reaches every counterparty that gets the lists, the one never tried included.
#[tokio::test]
async fn test_publishing_change_reaches_a_counterparty_the_failed_fan_out_never_tried() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut other = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    other.app_mut().invite(&w.bob_id()).await.unwrap();
    let base = Arc::new(FailingCommits::default());
    let mut shop = shop_on(&w, &base).await;
    shop.publish_list_to(&[w.alice_id(), w.carol_id()])
        .await
        .unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert_eq!(lists_taken(&mut other).await, 1);
    base.fail(Some(b"out/"));
    assert!(shop.set_list_endpoints(list("lno1changed")).await.is_err());
    base.fail(None);
    let got = shop.receive(None).await.unwrap();
    assert_eq!(got.lists.sent.len(), 2, "{:?}", got.lists);
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert_eq!(lists_taken(&mut other).await, 1);
}

/// The backup's list still queued at the backup is taken out of the restored outbox before
/// the host flushes, and an empty newer version goes instead: the payer takes that one
/// alone, holds no list, and can't pay the endpoints the user removed since. Once the host
/// sets its list, she gets it.
#[tokio::test]
async fn test_publishing_restored_outbox_list_taken_out_and_withdrawn() {
    let w = World::new();
    // Alice's wallet doesn't admit bob yet: what he sends her stays queued.
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    // The user removes the reusable endpoint after the backup; the device is lost.
    shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    assert!(shop.lists_await_host().await.unwrap());
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    // She takes the empty version alone: the carried one never left.
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
    assert!(wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .is_err());
    // The host sets its list: she is sent it, though the restore withdrew it.
    let set = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert_eq!(set.sent.len(), 1, "{set:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .unwrap();
    assert_eq!(last_offered(&w, "btc-lightning-bolt12"), "lno1changed");
}

/// The restore's empty version fails to reach the payer's inbox at the host's first flush:
/// the backup's list, taken out of the restored outbox, doesn't go in its place, so she
/// holds nothing she could pay, and takes the empty version alone once it goes.
#[tokio::test]
async fn test_publishing_restored_outbox_list_never_goes_while_the_withdrawal_fails() {
    let w = World::new();
    // Alice's wallet doesn't admit bob yet: what he sends her stays queued.
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    // The first delivery to her inbox fails to connect.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "inbox".into(),
            after: 0,
        });
    shop.flush().await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 0);
    assert!(wallet
        .pay_without_request(to_bob(&w, "btc-lightning-bolt12"))
        .await
        .is_err());
    shop.flush().await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// Taking the carried list out of the restored outbox gives up every message to that
/// payer the outbox carries: a request queued with it, which this store never saw accepted,
/// isn't sent again, and the host is told of it as undelivered, naming her and the request.
#[tokio::test]
async fn test_publishing_restored_outbox_take_out_tells_of_a_request_carried_with_the_list() {
    let w = World::new();
    // Alice's wallet doesn't admit bob yet: what he sends her stays queued.
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let now = w.clock.now_ms();
    let sent = shop
        .send_request(w.alice_id(), crate::common::draft("1", now))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    shop.receive(None).await.unwrap();
    let notices = shop.notices().await.unwrap();
    let told = notices
        .iter()
        .find(|notice| notice.kind == paykit_sdk::NoticeKind::MessageUndelivered);
    let told = told.unwrap_or_else(|| panic!("{notices:?}"));
    assert_eq!(told.counterparty, w.alice_id());
    assert_eq!(told.msg_id.as_deref(), Some(sent.msg_id.as_str()));
    assert_eq!(told.request_hash, Some(request.request_hash));
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    let got = wallet.receive(None).await.unwrap();
    let kinds: Vec<&str> = got
        .report
        .events
        .iter()
        .filter_map(|event| match event {
            AppEvent::MessageReceived { kind, .. } => Some(kind.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["paykit.private_payment_list"], "{kinds:?}");
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// After a restore the host re-applies a withdrawal from a counterparty first sent a list
/// after the backup, which the restored state doesn't know: it goes, and supersedes the
/// list she holds.
#[tokio::test]
async fn test_publishing_restored_withdrawal_reaches_a_counterparty_sent_a_list_since() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.set_list_endpoints(alice_list()).await.unwrap();
    let backup = shop.backup().await.unwrap();
    // Sent after the backup, then the device is lost.
    shop.publish_list_to(&[w.alice_id()]).await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    // A backup holding a list it never sent waits for the host as well.
    assert!(shop.lists_await_host().await.unwrap());
    let withdrawn = shop.withdraw_list_from(&w.alice_id()).await;
    assert!(withdrawn.is_ok(), "{withdrawn:?}");
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
    // Withdrawn by the host, she isn't sent the list when it is set.
    let set = shop.set_list_endpoints(alice_list()).await.unwrap();
    assert!(set.sent.is_empty(), "{set:?}");
}

/// Outside the wait after a restore, a withdrawal from a counterparty that never got a
/// list is refused.
#[tokio::test]
async fn test_publishing_withdrawal_from_a_counterparty_never_sent_a_list_refused() {
    let w = World::new();
    let mut shop = w.shop().await;
    shop.set_list_endpoints(alice_list()).await.unwrap();
    let refused = shop.withdraw_list_from(&w.alice_id()).await.unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotFound)),
        "{refused:?}"
    );
}

/// The host's own withdrawal ends the restore's: withdrawn by the host after the restore
/// withdrew it, she isn't sent the list when it is set.
#[tokio::test]
async fn test_publishing_host_withdrawal_ends_the_restores() {
    let w = World::new();
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    shop.withdraw_list_from(&w.alice_id()).await.unwrap();
    let set = shop.set_list_endpoints(list("lno1changed")).await.unwrap();
    assert!(set.sent.is_empty(), "{set:?}");
    lists_taken(&mut wallet).await;
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// Bob's shop restored from `backup` on `base`, armed to fail under `prefix` after `skip`
/// commits there: the restore fails after its commit, and the shop is loaded from `base`.
async fn restored_then_loaded(
    w: &World,
    backup: &paykit_sdk::PaykitBackup,
    prefix: &'static [u8],
    skip: usize,
) -> Paykit {
    let base = Arc::new(FailingCommits::default());
    base.fail_after(prefix, skip);
    let restored = Paykit::restore(
        w.env(&w.bob, Arc::clone(&base) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(
        restored.is_err(),
        "the restore's withdrawal fails after its commit"
    );
    drop(restored);
    base.fail(None);
    let app_id = w.bob.identity().app_keys("shop").app_id_bytes();
    w.load_with(
        &w.bob,
        base as Arc<dyn StateStore>,
        app_id,
        Arc::clone(&w.clock),
        PaykitConfig::default(),
    )
    .await
}

/// A restore whose withdrawal of the carried list fails after its commit: the load takes
/// the list out and withdraws it before the host can flush, so the payer takes the empty
/// version alone and holds no list.
#[tokio::test]
async fn test_publishing_restored_outbox_list_withdrawn_by_the_load_after_a_failed_restore() {
    let w = World::new();
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    // The restore's own commit goes; the withdrawal's first write fails.
    let mut shop = restored_then_loaded(&w, &backup, b"p/lp/", 1).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// A restore withdrawal recorded but not queued goes again at once, by the load here, not
/// an hour later.
#[tokio::test]
async fn test_publishing_restored_withdrawal_not_queued_sent_again_at_once() {
    let w = World::new();
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w.shop().await;
    w.publish_to_alice(&mut shop).await;
    let backup = shop.backup().await.unwrap();
    drop(shop);
    // The restore's commit, carrying the outbox, goes, and so does taking the carried list
    // out of it, which writes nothing there; the withdrawal's queueing fails.
    let mut shop = restored_then_loaded(&w, &backup, b"out/", 1).await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}

/// Two versions the restored outbox carries for a counterparty blocked since the backup:
/// the restore takes both out and withdraws once, above both, though she is blocked, and no
/// receive withdraws again.
#[tokio::test]
async fn test_publishing_restored_outbox_versions_withdrawn_once_above_each() {
    let w = World::new();
    // Alice's wallet doesn't admit bob yet: what he sends her stays queued.
    let mut wallet = w
        .join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    w.publish_to_alice(&mut shop).await;
    w.clock.advance_ms(1);
    let second = shop.set_list_endpoints(list("lno1second")).await.unwrap();
    assert_eq!(second.sent.len(), 1, "{second:?}");
    let backup = shop.backup().await.unwrap();
    shop.block(&w.alice_id()).await.unwrap();
    drop(shop);
    let mut shop = restored_shop(&w, &backup).await;
    let published = shop.published_lists().await.unwrap();
    assert_eq!(published.len(), 1, "{published:?}");
    assert!(!published[0].active);
    assert!(published[0].version > second.sent[0].version);
    for _ in 0..2 {
        let got = shop.receive(None).await.unwrap();
        assert!(got.lists.sent.is_empty(), "{:?}", got.lists);
    }
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.flush().await.unwrap();
    // The withdrawal alone: neither carried version left.
    assert_eq!(lists_taken(&mut wallet).await, 1);
    assert!(no_usable_list(&w, &mut wallet).await);
}
