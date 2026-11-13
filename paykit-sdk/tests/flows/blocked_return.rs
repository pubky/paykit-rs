//! Messages dropped for the user's block come back once the sender is unblocked (Paykit
//! spec 8.1, "Blocked counterparties"; Paykit wire 4.1): dropped as never processed, marked,
//! asked for again, and a request that comes back is held for the user.

use std::sync::Arc;

use paykit_sdk::{AppScopes, NoticeKind, Paykit, PaykitConfig};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;

use crate::common::{asked, draft, request_from, store, ListsStore, World};

const MINUTE_MS: u64 = 60_000;

/// Alice's wallet holding `pay` and `manage_contacts`, on `store`, inviting bob.
async fn contacts_wallet(w: &World, store: Arc<dyn StateStore>) -> Paykit {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    wallet
}

/// The marks of messages dropped for a block that `app` keeps.
async fn block_marks(app: &Paykit) -> usize {
    app.app().protocol_scan(b"bd/").await.unwrap().len()
}

/// How many of the payer records `app` holds carry `manual_only`.
async fn held_for_the_user_count(app: &Paykit) -> usize {
    let records = app.app().records_of("payer_activity").await.unwrap();
    records
        .iter()
        .filter(|record| {
            record.content.value().is_some_and(|value| {
                serde_json::from_slice::<serde_json::Value>(value)
                    .map(|v| v["manual_only"] == serde_json::Value::Bool(true))
                    .unwrap_or(false)
            })
        })
        .count()
}

/// Whether a payer record `app` holds carries `manual_only`.
async fn held_for_the_user(app: &Paykit) -> bool {
    held_for_the_user_count(app).await > 0
}

/// Bob's request reaches alice's inbox, then she blocks him through the client: her wallet
/// drops it for the block, as never processed.
async fn dropped_for_the_block(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
    assert_eq!(block_marks(wallet).await, 1);
}

#[tokio::test]
async fn test_blocked_return_request_comes_back_held_for_the_user() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    // Blocked, bob is asked nothing.
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // Unblocked, and a read of the lists shows it: he is asked to send it again.
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    // His shop answers; the request comes back, taken as new, and held for the user.
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    assert!(held_for_the_user(&wallet).await);
    // A request bob makes later, after the block's messages, isn't held.
    w.clock.advance_ms(60 * MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 2);
    assert_eq!(held_for_the_user_count(&wallet).await, 1);
}

#[tokio::test]
async fn test_blocked_return_request_not_held_when_it_wasnt_dropped() {
    // A request no block dropped opens its record as any other: no `manual_only`.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    assert!(!held_for_the_user(&wallet).await);
}

#[tokio::test]
async fn test_blocked_return_flagged_sender_only_counted() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    lists.flag(&w.bob_id());
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
    // Counted only: no mark, nothing asked later.
    assert_eq!(block_marks(&wallet).await, 0);
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_return_flagged_and_blocked_asked_only_once_neither() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    lists.flag(&w.bob_id());
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    // Unblocked but still flagged: a copy sent again would be counted only, so no ask.
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(wallet.app().protocol_get(b"lr").await.unwrap().is_some());
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // No longer flagged either, at a later read of the lists: he is asked.
    lists.unflag(&w.bob_id());
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_flagged_state_unknown_holds_the_ask() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    // Unblocked, but whether bob is flagged isn't known: no ask.
    lists.flagged_unknown(true);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // Known again: he is asked.
    lists.flagged_unknown(false);
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_own_unblock_unanswered_asks_nothing() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    // The wallet unblocks bob while its switch doesn't answer: the lists still show him
    // blocked. The own unblock isn't enough to ask for his messages, the permissive
    // direction: no ask.
    // One for the unblock's own call, one for the receive's call again.
    for _ in 0..2 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
    }
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    // No ask queued, delivered or not.
    let mark = format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    let mark: serde_json::Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get(mark.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(mark["d"]["queued"].is_null(), "{mark}");
    assert!(mark["d"]["asked_at_ms"].is_null(), "{mark}");
    // Read after the message, the lists still show bob: the unblock is still owed.
    let own_unblock = format!("bk/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    assert!(wallet
        .app()
        .protocol_get(own_unblock.as_bytes())
        .await
        .unwrap()
        .is_some());
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_return_mark_past_its_30_days_told_lost() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    // Still blocked past the 30 days bob keeps it: it can no longer come back, and the
    // host is told, once, with no content.
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    let notices = wallet.notices().await.unwrap();
    let lost: Vec<_> = notices
        .iter()
        .filter(|n| n.kind == NoticeKind::BlockedMessagesLost)
        .collect();
    assert_eq!(lost.len(), 1, "{notices:?}");
    assert_eq!(lost[0].counterparty, w.bob_id());
    assert!(lost[0].lost_from_ms.is_some() && lost[0].lost_until_ms.is_some());
    // Unblocked then, bob is asked nothing.
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // Acknowledged, it goes.
    wallet.acknowledge_notice(&lost[0].id).await.unwrap();
    assert!(wallet
        .notices()
        .await
        .unwrap()
        .iter()
        .all(|n| n.kind != NoticeKind::BlockedMessagesLost));
}

#[tokio::test]
async fn test_blocked_return_restore_marks_every_blocked_counterparty() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let _shop = w.shop().await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let mut restored = restored;
    // The first read of the lists after the restore marks bob, blocked, from the backup's
    // time on.
    restored.receive(None).await.unwrap();
    assert_eq!(block_marks(&restored).await, 1);
}

#[tokio::test]
async fn test_blocked_return_missed_window_marks_every_blocked_counterparty() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    // The inbox lost entries in a window while bob was blocked: any may have been his, so
    // he is marked over that window.
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    // The window is kept, and applied at the next read of the lists.
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    // Still blocked, nothing is asked; unblocked, he is asked over that window.
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let asks = asked(&wallet, &w.bob_id()).await;
    assert_eq!(asks.len(), 1, "{asks:?}");
    let (from, until) = asks[0];
    assert!(from.is_some_and(|from| from <= now - 1_000), "{asks:?}");
    assert!(until.is_some_and(|until| until >= now), "{asks:?}");
}

#[tokio::test]
async fn test_blocked_return_proof_comes_back_credited_without_a_hold() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    crate::public::publish(
        &w,
        serde_json::json!({
            "capabilities": { "endpoint_identifiers": ["btc-lightning-bolt12"] },
            "public_endpoints": { "btc-lightning-bolt12": "lno1tips" }
        }),
        Some(w.clock.now_ms() + 30 * 24 * 60 * MINUTE_MS),
    );
    let payment = paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.001"))
        .with_options(paykit_sdk::PayOptions::default().with_mode(
            paykit_sdk::PaymentMode::Public(pubky_switch_client::app::UserConfirmed::new()),
        ));
    let outcome = wallet.pay_without_request(payment).await.unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    // The proof reaches bob's inbox, then he blocks alice: it is dropped, marked.
    shop.app_mut().deny(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap().is_empty());
    assert_eq!(block_marks(&shop).await, 1);
    // Unblocked: alice is asked, her wallet sends it again, and it is credited, with no
    // hold: only a request that comes back is held.
    shop.unblock(&w.alice_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(asked(&shop, &w.alice_id()).await.len(), 1);
    wallet.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!(sales[0].status, paykit_sdk::PayeeStatus::ProofReceived);
    // The mark stays until it expires, asking no more than daily.
    w.clock.advance_ms(11 * MINUTE_MS);
    shop.receive(None).await.unwrap();
    assert_eq!(asked(&shop, &w.alice_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_mark_evicted_at_the_bound_told_lost() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    let before = w.clock.now_ms() - 1_000;
    let seeded =
        crate::common::seed_marks(&mut wallet, "bd/", paykit_sdk::MAX_DROPPED_MARKS, before).await;
    dropped_for_the_block_past_the_bound(&w, &mut wallet, &mut shop).await;
    // The bound holds: the oldest mark goes, and its sender's lost window is told.
    assert_eq!(block_marks(&wallet).await, paykit_sdk::MAX_DROPPED_MARKS);
    let lost: Vec<_> = wallet
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == NoticeKind::BlockedMessagesLost)
        .collect();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].counterparty, seeded[0]);
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(senders.contains(&w.bob_id()));
    assert!(!senders.contains(&seeded[0]));
}

/// As [`dropped_for_the_block`], with the bound on marks already full.
async fn dropped_for_the_block_past_the_bound(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_blocked_return_late_start_asks_what_is_kept_and_tells_the_rest_lost() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // A mark of bob's whose earliest message is past the 30 days he keeps, as after an app
    // that wasn't run for a month, and that hasn't asked yet; he is no longer blocked.
    let now = w.clock.now_ms();
    let earliest = now - paykit_sdk::KEPT_MESSAGE_MS - 24 * 60 * MINUTE_MS;
    let mark = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(&w.bob_id()),
            "earliest_ms": earliest,
            "latest_ms": now - 1_000,
            "due": false,
            "asked_at_ms": null,
            "queued": null,
        }
    });
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id())).into_bytes(),
            value: serde_json::to_vec(&mark).unwrap(),
        }])
        .await
        .unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    // The part he still keeps is asked for; the part before it is told lost, once.
    let asks = asked(&wallet, &w.bob_id()).await;
    assert_eq!(asks.len(), 1, "{asks:?}");
    let (from, _) = asks[0];
    assert!(from.is_some_and(|from| from > earliest), "{asks:?}");
    let lost: Vec<_> = wallet
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == NoticeKind::BlockedMessagesLost)
        .collect();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert_eq!(lost[0].lost_from_ms, Some(earliest));
    assert!(lost[0].lost_until_ms.is_some_and(|until| until > earliest));
}
