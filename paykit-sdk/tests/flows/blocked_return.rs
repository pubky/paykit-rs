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
    contacts_wallet_with(w, store, PaykitConfig::default()).await
}

/// [`contacts_wallet`], with `config`.
async fn contacts_wallet_with(
    w: &World,
    store: Arc<dyn StateStore>,
    config: PaykitConfig,
) -> Paykit {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            config,
            store,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    wallet
}

/// How many lost-messages notices `app` shows.
async fn lost_count(app: &Paykit) -> usize {
    app.notices()
        .await
        .unwrap()
        .iter()
        .filter(|n| n.kind == NoticeKind::BlockedMessagesLost)
        .count()
}

/// Bob's request dropped for the block, bob unblocked, asked, and the request back.
async fn came_back(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    dropped_for_the_block(w, wallet, shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(wallet, &w.bob_id()).await.len(), 1);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
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
    // The host sees that it is paid only by hand.
    assert!(wallet.requests().await.unwrap()[0].manual_only);
    // A request bob makes later, after the block's messages, isn't held.
    w.clock.advance_ms(60 * MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 2);
    assert_eq!(held_for_the_user_count(&wallet).await, 1);
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.iter().filter(|v| v.manual_only).count(), 1);
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
async fn test_blocked_return_window_widened_by_the_pass_read_is_not_expired_with_the_old_mark() {
    // Bob stays blocked, and his mark is about to expire. In one receive the inbox tells of a
    // window of entries it lost, and carol's mark, ready to ask, makes the pass read the
    // lists: that read applies the window to bob's mark, which now runs to the window's end.
    // The pass then judges the mark as it is kept after the read: it isn't expired by the
    // times it held before, so the window can still be asked for once bob is unblocked.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    assert_eq!(block_marks(&wallet).await, 1);
    // Half an hour before bob's mark expires, carol's request is dropped for a block too,
    // and she is unblocked.
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 30 * MINUTE_MS);
    request_from(&mut carol_shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.carol_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 2);
    wallet.unblock(&w.carol_id()).await.unwrap();
    // Past the expiry of bob's mark as it stands, the inbox tells of the lost window.
    w.clock.advance_ms(31 * MINUTE_MS);
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.carol_id()).await.len(), 1);
    // Bob's mark is kept with the window, and nothing is told as lost.
    assert_eq!(block_marks(&wallet).await, 2);
    assert_eq!(lost_count(&wallet).await, 0);
    // Unblocked, he is asked over the window.
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let asks = asked(&wallet, &w.bob_id()).await;
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(asks[0].1.is_some_and(|until| until >= now), "{asks:?}");
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

#[tokio::test]
async fn test_blocked_return_no_ask_while_the_deny_list_holds_the_sender() {
    // The client's view shows bob blocked, as a block another own app made, which the client
    // learned from the feed, while this app's copy of the deny list, read after the
    // message's acceptance but before the block, doesn't: no ask may go to him.
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The client's copy isn't known: the entry waits; Paykit reads the lists, bob not
    // blocked.
    lists.lists_unknown(true);
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    assert!(wallet.app().protocol_get(b"lr").await.unwrap().is_some());
    // Bob is blocked at the client, with no own block mark of Paykit's.
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    lists.lists_unknown(false);
    w.clock.advance_ms(MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // Unblocked: once the copy that showed him denied is due for a refresh, the ask reads
    // the lists and he is asked.
    wallet.app_mut().undeny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_late_start_told_once() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let now = w.clock.now_ms();
    let earliest = now - paykit_sdk::KEPT_MESSAGE_MS - 24 * 60 * MINUTE_MS;
    seed_block_mark(&mut wallet, &w.bob_id(), earliest, now - 1_000).await;
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(lost_count(&wallet).await, 1);
    for _ in 0..3 {
        w.clock.advance_ms(MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(lost_count(&wallet).await, 1);
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_asked_window_not_told_lost_at_expiry() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    came_back(&w, &mut wallet, &mut shop).await;
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    assert_eq!(lost_count(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_asked_window_not_told_lost_near_its_end() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    let t0 = w.clock.now_ms();
    came_back(&w, &mut wallet, &mut shop).await;
    // Near the end of the 30 days, then past them.
    let target = t0 + paykit_sdk::KEPT_MESSAGE_MS - 2 * MINUTE_MS;
    w.clock.advance_ms(target - w.clock.now_ms());
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(lost_count(&wallet).await, 0);
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(lost_count(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_new_window_after_an_ask_told_lost_alone() {
    // Asked and back, bob is blocked again and sends another request: a new window, which
    // expires unasked and alone is told lost.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    came_back(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60 * MINUTE_MS);
    let second = w.clock.now_ms();
    request_from(&mut shop, w.alice_id(), draft("2", second)).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS + 11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let lost: Vec<_> = wallet
        .notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|n| n.kind == NoticeKind::BlockedMessagesLost)
        .collect();
    assert_eq!(lost.len(), 1, "{lost:?}");
    assert!(lost[0].lost_from_ms.is_some_and(|from| from >= second));
}

#[tokio::test]
async fn test_blocked_return_queued_ask_not_flushed_by_a_receive_once_blocked_again() {
    // Unblocked, the ask is queued and bob's inbox doesn't take it yet; the user blocks him
    // again: no receive delivers it while he is blocked. (A host's own `Paykit::flush`
    // delivers what the client's outbox holds, as for every queued message.)
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: serde_json::Value::Null,
        });
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.block(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_return_sixty_five_in_one_read_all_come_back() {
    // 65 messages from one blocked sender in one read: 64 marked, the 65th held, none lost.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    for i in 0..65 {
        request_from(
            &mut shop,
            w.alice_id(),
            draft(&format!("{}", i + 1), w.clock.now_ms()),
        )
        .await;
    }
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 64);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 65);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 65);
}

#[tokio::test]
async fn test_blocked_return_repeat_of_a_request_paid_only_by_hand_not_made() {
    // The request that came back is paid by hand; its dispatch is left in flight, and a
    // resume makes no repeat: a repeat is an automatic payment.
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut wallet = contacts_wallet_with(&w, store(), crate::common::day_window()).await;
    let mut shop = w.shop().await;
    came_back(&w, &mut wallet, &mut shop).await;
    let request = wallet.requests().await.unwrap()[0].request;
    assert!(wallet.requests().await.unwrap()[0].manual_only);
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    w.clock.advance_ms(60_000);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_entry_held_while_no_mark_may_go() {
    // Past the bound on marks, with every mark naming pairs still to forget, bob's message
    // waits: no mark goes with its pairs unforgotten. Once they are forgotten, it is marked.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    let mut writes = Vec::new();
    let now = w.clock.now_ms();
    for i in 0..paykit_sdk::MAX_DROPPED_MARKS {
        let sender = [u8::try_from(i + 100).unwrap(); 32];
        let value = serde_json::json!({
            "v": 1,
            "d": {
                "sender": pubky_switch_core::enc::b64(&sender),
                "earliest_ms": now - 2_000 - i as u64,
                "latest_ms": now - 1_000 - i as u64,
                "due": true,
                "asked_at_ms": now,
                "queued": null,
                "pending_forget": ["123e4567-e89b-42d3-a456-426614174000"],
            }
        });
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key: format!("bd/{}", pubky_switch_core::enc::b64(&sender)).into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        });
    }
    wallet.app_mut().protocol_commit(&writes).await.unwrap();
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(!senders.contains(&w.bob_id()));
    assert_eq!(wallet.spam_count(&w.bob_id()).await.unwrap(), 0);
    // The pass after that read forgot the seeded pairs: the next read marks bob.
    wallet.receive(None).await.unwrap();
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(senders.contains(&w.bob_id()));
    assert_eq!(senders.len(), paykit_sdk::MAX_DROPPED_MARKS);
}

/// A block mark of `sender`, unasked, from `earliest` to `latest`.
async fn seed_block_mark(app: &mut Paykit, sender: &[u8; 32], earliest: u64, latest: u64) {
    let mark = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(sender),
            "earliest_ms": earliest,
            "latest_ms": latest,
            "due": false,
            "asked_at_ms": null,
            "queued": null,
        }
    });
    app.app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("bd/{}", pubky_switch_core::enc::b64(sender)).into_bytes(),
            value: serde_json::to_vec(&mark).unwrap(),
        }])
        .await
        .unwrap();
}

#[tokio::test]
async fn test_blocked_return_request_written_later_still_paid_only_by_hand() {
    // The request comes back, and its record's write is refused for good: it is shown
    // unrecorded, paid only by hand, and the record written later holds the hold.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    shop.receive(None).await.unwrap();
    crate::common::refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 1);
    assert_eq!(views[0].status, paykit_sdk::RequestStatus::Unrecorded);
    assert!(views[0].manual_only);
    assert!(!held_for_the_user(&wallet).await);
    wallet.receive(None).await.unwrap();
    assert!(held_for_the_user(&wallet).await);
    let views = wallet.requests().await.unwrap();
    assert_eq!(views[0].status, paykit_sdk::RequestStatus::Open);
    assert!(views[0].manual_only);
}

#[tokio::test]
async fn test_blocked_return_marks_and_widenings_dated_past_the_backup_bounded_at_a_restore() {
    // A backup carries a block mark and a widening dated two days past any service time it
    // verified: a restore bounds them by that time plus the clock tolerance.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let ahead = w.clock.now_ms() + 2 * 24 * 60 * MINUTE_MS;
    crate::common::seed_marks(&mut wallet, "bd/", 1, ahead).await;
    let widening = serde_json::json!({ "v": 1, "d": { "from_ms": ahead, "until_ms": u64::MAX } });
    wallet
        .app_mut()
        .protocol_commit(&[pubky_switch_client::app::LocalWrite::Put {
            key: format!("bw/{ahead:020}").into_bytes(),
            value: serde_json::to_vec(&widening).unwrap(),
        }])
        .await
        .unwrap();
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
    let bound = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    let marks = restored.app().protocol_scan(b"bd/").await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["latest_ms"].as_u64().unwrap() <= bound, "{mark}");
    for (key, value) in restored.app().protocol_scan(b"bw/").await.unwrap() {
        let window: serde_json::Value = serde_json::from_slice(&value).unwrap();
        assert!(
            window["d"]["from_ms"].as_u64().unwrap() <= bound,
            "{window}"
        );
        assert!(String::from_utf8(key).unwrap() <= format!("bw/{bound:020}"));
    }
}

#[tokio::test]
async fn test_blocked_return_mark_of_a_sender_still_blocked_spares_the_lists_read() {
    // While bob stays blocked and the copy of the lists is fresh, his mark makes no receive
    // read the lists beyond its own 10-minute refresh: a read armed to fail isn't made.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    assert!(wallet.app().protocol_get(b"lr").await.unwrap().is_some());
    for _ in 0..3 {
        w.clock.advance_ms(MINUTE_MS);
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
        wallet.receive(None).await.unwrap();
        assert!(wallet.app().protocol_get(b"lr").await.unwrap().is_some());
        // The armed fault stays for the next read; this one takes it.
        wallet.receive(None).await.unwrap();
    }
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
}

/// Bob's request dropped for the block, bob unblocked and asked: the ask is delivered, and
/// his shop hasn't answered yet.
async fn asked_not_answered(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    dropped_for_the_block(w, wallet, shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_request_after_its_mark_went_for_the_bound_paid_only_by_hand() {
    // Bob was asked; before his request comes back, the bound on marks drops his mark for
    // another sender's. The request that then comes back still opens paid only by hand.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    asked_not_answered(&w, &mut wallet, &mut shop).await;
    // 128 newer marks, asked already, and carol's message dropped for her block.
    let now = w.clock.now_ms();
    let mut writes = Vec::new();
    for i in 0..paykit_sdk::MAX_DROPPED_MARKS {
        let sender = [u8::try_from(i + 100).unwrap(); 32];
        let value = serde_json::json!({
            "v": 1,
            "d": {
                "sender": pubky_switch_core::enc::b64(&sender),
                "earliest_ms": now - 1_000 + i as u64,
                "latest_ms": now - 1_000 + i as u64,
                "due": true,
                "asked_at_ms": now,
                "queued": null,
            }
        });
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key: format!("bd/{}", pubky_switch_core::enc::b64(&sender)).into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        });
    }
    wallet.app_mut().protocol_commit(&writes).await.unwrap();
    let mut carol = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    request_from(&mut carol, w.alice_id(), draft("7", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.carol_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(senders.contains(&w.carol_id()));
    assert!(
        !senders.contains(&w.bob_id()),
        "bob's mark went for the bound"
    );
    // Bob's shop answers; his request comes back with no mark of his.
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].manual_only);
    assert!(held_for_the_user(&wallet).await);
}

#[tokio::test]
async fn test_blocked_return_request_after_its_mark_expired_paid_only_by_hand() {
    // Bob is asked near the end of his mark's 30 days; his request comes back, and waits
    // while the copy of the lists isn't known, past the mark's expiry. Taken then, it still
    // opens paid only by hand.
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(29 * 24 * 60 * MINUTE_MS);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    shop.receive(None).await.unwrap();
    lists.lists_unknown(true);
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    // Past the mark's 30 days, the request still waits; the mark goes.
    w.clock.advance_ms(2 * 24 * 60 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    lists.lists_unknown(false);
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].manual_only);
}

#[tokio::test]
async fn test_blocked_return_older_request_judged_by_a_newer_window() {
    // Bob is asked for his first request; before it comes back he is blocked again, and a
    // second request is dropped: a new window. Both come back, the first judged by the
    // newer window's latest time, and both open paid only by hand.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    asked_not_answered(&w, &mut wallet, &mut shop).await;
    w.clock.advance_ms(60 * MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 2);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 2, "{views:?}");
    assert!(views.iter().all(|view| view.manual_only), "{views:?}");
}

#[tokio::test]
async fn test_blocked_return_recurring_request_dated_in_a_window_opens_with_no_mark() {
    // The control: a recurring request of bob's dated inside his block's window opens with
    // no mark, where a one-time one would be paid only by hand. This base pays no recurring
    // request automatically; the executor part takes the mark for it (S-BS-5).
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    let body = serde_json::json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "plan-1",
        "proposal_expires_at": pubky_switch_core::time::rfc3339(w.clock.now_ms() + 24 * 60 * MINUTE_MS),
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" },
        "recurrence": {
            "anchor": "2026-01-31T00:00:00.000Z",
            "ends_at": null,
            "every": 1,
            "starts_at": "2026-01-31T00:00:00.000Z",
            "unit": "month"
        }
    });
    let recurring = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
    asked_not_answered(&w, &mut wallet, &mut shop).await;
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &recurring);
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].recurring);
    assert!(!views[0].manual_only);
    assert!(!held_for_the_user(&wallet).await);
}

#[tokio::test]
async fn test_blocked_return_request_dropped_between_backup_and_restore_paid_only_by_hand() {
    // After the backup, bob's request is dropped for a block and bob is unblocked again; the
    // device is then lost. The backup holds no mark of his, and the lists don't show him at
    // the restore. The restore's re-send to the counterparty of an open request asks him,
    // and the request that comes back opens paid only by hand: the restore raised the block
    // floor to its first read.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    let backup = wallet.backup().await.unwrap();
    w.clock.advance_ms(60 * MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    drop(wallet);
    let mut wallet = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    w.clock.advance_ms(MINUTE_MS);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    // No mark of bob's: the lists don't hold him.
    assert_eq!(block_marks(&wallet).await, 0);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    let views = wallet.requests().await.unwrap();
    assert_eq!(views.len(), 2, "{views:?}");
    // The request that came back is the one opened with the mark.
    assert_eq!(held_for_the_user_count(&wallet).await, 1);
    // The request the backup held is still open, under the restore's hold, and dated before
    // the floor the restore raised: its view shows it paid only by hand too, whatever its
    // status shows.
    assert!(
        views
            .iter()
            .all(|view| view.status == paykit_sdk::RequestStatus::Held && view.manual_only),
        "{views:?}"
    );
}

#[tokio::test]
async fn test_blocked_return_open_request_the_floor_holds_shows_manual_only_while_blocked() {
    // Carol's request opens as any other. Bob's request is then dropped for a block and asked
    // for again, which raises the floor past carol's request: it is paid only by hand, and
    // its view says so. Blocking carol changes the status shown, not that: the request is
    // still open, and still one no app pays automatically.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let carols = request_from(&mut carol_shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let view = wallet.request(&carols).await.unwrap();
    assert_eq!(view.status, paykit_sdk::RequestStatus::Open);
    assert!(!view.manual_only);
    w.clock.advance_ms(MINUTE_MS);
    wallet_raises_the_floor(&w, &mut wallet, &mut shop).await;
    let view = wallet.request(&carols).await.unwrap();
    assert_eq!(view.status, paykit_sdk::RequestStatus::Open);
    assert!(view.manual_only);
    assert!(!held_for_the_user(&wallet).await);
    wallet.block(&w.carol_id()).await.unwrap();
    let view = wallet.request(&carols).await.unwrap();
    assert_eq!(view.status, paykit_sdk::RequestStatus::Blocked);
    assert!(view.manual_only, "{view:?}");
}

/// The block floor `app` holds in the identity's `block_floor` record (Paykit wire 5.6).
async fn shared_floor(app: &Paykit) -> Option<u64> {
    app.app()
        .records_of("block_floor")
        .await
        .unwrap()
        .iter()
        .find_map(|record| {
            let value = record.content.value()?;
            paykit_lib::records::BlockFloorRecord::read(value)
                .ok()
                .map(|record| record.floor_ms)
        })
}

/// Alice's second payer app, `name`, joined now.
async fn payer_app(w: &World, name: &str) -> Paykit {
    w.join_with(
        &w.alice,
        name,
        AppScopes::new().pay().build().unwrap(),
        PaykitConfig::default(),
        store(),
    )
    .await
}

#[tokio::test]
async fn test_blocked_return_request_reaching_a_new_payer_app_first_paid_only_by_hand() {
    // The wallet drops bob's request for the block and keeps the mark; alice then adds a
    // phone, a payer app with no mark of bob's. Once bob is unblocked, the wallet raises the
    // identity's block floor in the batch of its ask; the request comes back and the phone
    // reads it first: it opens the record with `manual_only`, which the wallet then sees.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    let mut phone = payer_app(&w, "phone").await;
    assert_eq!(shared_floor(&phone).await, None);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(&wallet).await.is_some());
    shop.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let views = phone.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].manual_only, "{views:?}");
    assert!(held_for_the_user(&phone).await);
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap()[0].manual_only);
    // A request bob makes later is dated above the floor: no app holds it.
    w.clock.advance_ms(60 * MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    let views = phone.requests().await.unwrap();
    assert_eq!(views.iter().filter(|view| view.manual_only).count(), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_raised_again_before_the_new_app_reads_still_pays_only_by_hand() {
    // The wallet asks bob, raising the floor; bob's request comes back; then the wallet asks
    // carol, raising it again before the phone reads. The phone's feed shows the first raise
    // only as its stub, so at bob's entry it holds no floor yet and opens the record without
    // `manual_only`; by the end of the read it holds the second raise, which covers bob's
    // request: the request shows as paid only by hand, and a resume makes no repeat of it.
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    w.clock.advance_ms(MINUTE_MS);
    request_from(&mut carol_shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.app_mut().deny(&w.carol_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 2);
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            AppScopes::new().pay().build().unwrap(),
            crate::common::day_window(),
            store(),
        )
        .await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(asked(&wallet, &w.carol_id()).await.is_empty());
    let first = shared_floor(&wallet).await.unwrap();
    shop.receive(None).await.unwrap();
    // Carol asked without a read of the feed, so the wallet doesn't take bob's request first.
    wallet.unblock(&w.carol_id()).await.unwrap();
    wallet
        .request_resend(
            w.carol_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    assert!(shared_floor(&wallet).await.unwrap() > first);
    phone.receive(None).await.unwrap();
    let views = phone.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].manual_only, "{views:?}");
    let request = views[0].request;
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    w.clock.advance_ms(MINUTE_MS);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_that_does_not_read_holds_every_one_time_request() {
    // The identity's floor record holds a value no app can read, with none seen before: a
    // payer app that joins then can't tell which requests came back, so every one-time
    // request it opens is paid only by hand.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "block_floor",
            pubky_switch_core::ident::Policy::new("pay", &["pay"]),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    );
    wallet
        .app_mut()
        .put_record(&target, br#"{"v":1}"#, Vec::new())
        .await
        .unwrap();
    let mut phone = payer_app(&w, "phone").await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    let views = phone.requests().await.unwrap();
    assert_eq!(views.len(), 1, "{views:?}");
    assert!(views[0].manual_only, "{views:?}");
    assert!(held_for_the_user(&phone).await);
}

/// The identity's `block_floor` record.
fn floor_record(w: &World) -> pubky_switch_client::app::RecordTarget {
    pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            "block_floor",
            pubky_switch_core::ident::Policy::new("pay", &["pay"]),
        ),
        &paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
    )
}

/// The value of the `block_floor` record as `app` holds it.
async fn floor_value(app: &Paykit) -> serde_json::Value {
    let records = app.app().records_of("block_floor").await.unwrap();
    let value = records[0].content.value().expect("a value");
    serde_json::from_slice(value).unwrap()
}

/// The wallet takes a floor with a member it doesn't know, written by another payer app;
/// `spoil` then leaves the record without a value that reads, which the wallet takes too.
/// Its next raise is built on the value it saw: the unknown member is written back as read.
async fn raise_after_the_floor_stopped_reading(spoil: Spoil) {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut phone = payer_app(&w, "phone").await;
    let mut shop = w.shop().await;
    let target = floor_record(&w);
    let mut first: serde_json::Value = serde_json::from_slice(
        &paykit_lib::records::BlockFloorRecord::new(1)
            .to_json()
            .unwrap(),
    )
    .unwrap();
    let unknown = serde_json::json!({"since": "2", "kinds": ["a", {"b": null}]});
    first["later_member"] = unknown.clone();
    phone
        .app_mut()
        .put_record(&target, &serde_json::to_vec(&first).unwrap(), Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, Some(1));
    match &spoil {
        Spoil::Malformed => phone
            .app_mut()
            .put_record(&target, br#"{"v":1}"#, Vec::new())
            .await
            .unwrap(),
        Spoil::Deleted => phone
            .app_mut()
            .delete_record(&target, Vec::new())
            .await
            .unwrap(),
    };
    wallet.receive(None).await.unwrap();
    // A later write that doesn't read is ignored while the value seen reads, so the copy
    // keeps the value seen; a delete is taken, and leaves only the mark of the value seen.
    let held = match spoil {
        Spoil::Malformed => Some(1),
        Spoil::Deleted => None,
    };
    assert_eq!(shared_floor(&wallet).await, held);
    wallet_raises_the_floor(&w, &mut wallet, &mut shop).await;
    let raised = floor_value(&wallet).await;
    assert_eq!(raised["later_member"], unknown, "{raised}");
    assert!(shared_floor(&wallet).await.is_some_and(|floor| floor > 1));
    // Nothing but the floor changed: the value is the one seen, raised.
    let mut expected = first;
    expected["floor"] = raised["floor"].clone();
    assert_eq!(raised, expected);
}

/// How the floor record stops holding a value that reads.
enum Spoil {
    Malformed,
    Deleted,
}

#[tokio::test]
async fn test_blocked_return_floor_raised_over_a_value_that_does_not_read_keeps_unknown_members() {
    raise_after_the_floor_stopped_reading(Spoil::Malformed).await;
}

#[tokio::test]
async fn test_blocked_return_floor_raised_over_a_deleted_record_keeps_unknown_members() {
    raise_after_the_floor_stopped_reading(Spoil::Deleted).await;
}

#[tokio::test]
async fn test_blocked_return_dispatch_of_a_request_dated_before_the_floor_not_repeated() {
    // Carol's request opens as any other, and its payment is left in flight. Bob's request
    // is then dropped for a block and asked for again, which raises the floor past carol's
    // request. Whatever its record holds, no app pays automatically a one-time request
    // dated before the floor: another app may have opened it before it held the floor. So
    // a resume makes no repeat of its dispatch.
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut wallet = contacts_wallet_with(&w, store(), crate::common::day_window()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let carols = request_from(&mut carol_shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(!wallet.request(&carols).await.unwrap().manual_only);
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(&carols, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    w.clock.advance_ms(MINUTE_MS);
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(&wallet).await.is_some());
    w.clock.advance_ms(MINUTE_MS);
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

/// Alice's wallet and phone, carol's request opened on the phone, and bob's request on its
/// way to being dropped for the block: the setup of the floor's two windows at a dispatch.
async fn phone_paying_carol(w: &World) -> (Paykit, Paykit, Paykit, paykit_sdk::RequestRef) {
    // On the heap: the setup nests deep.
    Box::pin(phone_paying_carol_on(w, store())).await
}

/// [`phone_paying_carol`], the phone on `phone_store`.
async fn phone_paying_carol_on(
    w: &World,
    phone_store: Arc<dyn StateStore>,
) -> (Paykit, Paykit, Paykit, paykit_sdk::RequestRef) {
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS)
            .with_idempotent(true),
    );
    let mut wallet = contacts_wallet(w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            AppScopes::new().pay().build().unwrap(),
            crate::common::day_window(),
            phone_store,
        )
        .await;
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let carols = request_from(&mut carol_shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    phone.receive(None).await.unwrap();
    assert!(!phone.request(&carols).await.unwrap().manual_only);
    w.clock.advance_ms(MINUTE_MS);
    (wallet, phone, w.shop().await, carols)
}

/// A store that runs a hook once, during the commit that journals `Paying`: after the final
/// payment check and before the payment method's call.
#[derive(Default)]
struct PayingStore {
    inner: pubky_switch_client::store::MemoryStore,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PayingStore {
    fn on_paying(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }
}

impl StateStore for PayingStore {
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
        const PAYING: &[u8] = b"\"stage\":\"paying\"";
        let paying = batch.writes.iter().any(|write| {
            matches!(write, pubky_switch_client::store::Write::Put { key, value, .. }
                if key.starts_with(b"p/j/") && value.windows(PAYING.len()).any(|w| w == PAYING))
        });
        self.inner.commit(batch)?;
        if paying {
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
            }
        }
        Ok(())
    }
}

/// The wallet drops bob's request for the block and unblocks him: its next receive, once
/// the lists are read again, asks for the request and raises the floor past carol's.
async fn wallet_about_to_raise_the_floor(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(wallet).await, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
}

/// The phone starts carol's payment automatically, a first dispatch, and `during` runs
/// while its journal write of `Paying` commits. Returns the attempt, left open.
async fn automatic_first_dispatch(
    w: &Arc<World>,
    during: impl FnOnce(&World, Paykit, Paykit) + Send + 'static,
) -> (Paykit, paykit_lib::AttemptId) {
    let phone_store = Arc::new(PayingStore::default());
    // On the heap: the setup nests deep.
    let (mut wallet, mut phone, mut shop, carols) = Box::pin(phone_paying_carol_on(
        w,
        Arc::clone(&phone_store) as Arc<dyn StateStore>,
    ))
    .await;
    Box::pin(wallet_about_to_raise_the_floor(w, &mut wallet, &mut shop)).await;
    assert_eq!(shared_floor(&wallet).await, None);
    let world = Arc::clone(w);
    phone_store.on_paying(move || during(&world, wallet, shop));
    let outcome = phone
        .pay_request_automatically(&carols, paykit_sdk::PayOptions::default())
        .await
        .unwrap();
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    (phone, attempt_id)
}

#[tokio::test]
async fn test_blocked_return_floor_raised_during_the_paying_commit_holds_a_first_dispatch() {
    // An automatic first dispatch reads the floor, which no app has raised, and journals
    // `Paying`. While that write commits, the wallet asks for bob's request again, raising
    // the floor past carol's request. The floor read again after the commit holds the
    // payment: the method isn't called, and the attempt is known unsubmitted.
    let w = Arc::new(World::new());
    let (phone, attempt_id) = automatic_first_dispatch(&w, |w, mut wallet, _shop| {
        let bob = w.bob_id();
        // The wallet runs on its own thread while the phone's commit waits for it.
        let raised = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    wallet.receive(None).await.unwrap();
                    assert_eq!(asked(&wallet, &bob).await.len(), 1);
                    shared_floor(&wallet).await
                })
        })
        .join()
        .unwrap();
        assert!(raised.is_some());
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_blocked_return_floor_read_that_fails_after_the_paying_commit_holds_a_first_dispatch()
{
    // No raise at all, but the read of the floor after the journal write of `Paying` fails:
    // it never counts as a floor that doesn't hold the request, so the method isn't called.
    let w = Arc::new(World::new());
    let (phone, attempt_id) = automatic_first_dispatch(&w, |w, _wallet, _shop| {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "/records/".into(),
                after: 0,
            });
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

/// The wallet drops bob's request for the block and asks for it again, raising the floor
/// past carol's request; the phone reads nothing meanwhile.
async fn wallet_raises_the_floor(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(wallet).await, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(wallet).await.is_some());
}

/// The phone pays carol's request by hand, left in flight.
async fn paid_in_flight(
    w: &World,
    phone: &mut Paykit,
    carols: &paykit_sdk::RequestRef,
) -> paykit_lib::AttemptId {
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = phone
        .pay_request(carols, paykit_sdk::PayOptions::default())
        .await
        .unwrap()
    else {
        panic!("in flight");
    };
    attempt_id
}

#[tokio::test]
async fn test_blocked_return_floor_raised_before_the_attempt_read_again_at_the_repeat() {
    // The wallet raises the floor past carol's request before the phone's attempt on it is
    // committed; the phone's copy doesn't hold the raise. At the resume, the floor read
    // again immediately before the repeat holds it: no repeat.
    let w = World::new();
    let (mut wallet, mut phone, mut shop, carols) = phone_paying_carol(&w).await;
    wallet_raises_the_floor(&w, &mut wallet, &mut shop).await;
    assert_eq!(shared_floor(&phone).await, None);
    let attempt_id = paid_in_flight(&w, &mut phone, &carols).await;
    w.clock.advance_ms(MINUTE_MS);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_raised_after_the_attempt_read_again_at_the_repeat() {
    // The phone's attempt on carol's request is committed and dispatched first; the wallet
    // then raises the floor past it, and the phone's copy doesn't hold the raise. At the
    // resume, the floor read again immediately before the repeat holds it: no repeat.
    let w = World::new();
    let (mut wallet, mut phone, mut shop, carols) = phone_paying_carol(&w).await;
    let attempt_id = paid_in_flight(&w, &mut phone, &carols).await;
    wallet_raises_the_floor(&w, &mut wallet, &mut shop).await;
    assert_eq!(shared_floor(&phone).await, None);
    w.clock.advance_ms(MINUTE_MS);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_read_that_fails_at_the_repeat_holds_it() {
    // No raise at all, but the read of the floor before the repeat fails: it never counts as
    // a floor that doesn't hold the request, so no repeat on that read. A read that works
    // lets the next resume repeat.
    let w = World::new();
    let (_wallet, mut phone, _shop, carols) = phone_paying_carol(&w).await;
    let attempt_id = paid_in_flight(&w, &mut phone, &carols).await;
    w.clock.advance_ms(MINUTE_MS);
    // The resume reads the request record again first, then the floor, which fails.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/records/".into(),
            after: 1,
        });
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
}

#[tokio::test]
async fn test_blocked_return_floor_write_kept_by_the_client_names_its_batch() {
    // Bob is unblocked, and the host asks him for his messages again. The answer to the batch
    // that raises the floor and queues the ask is lost, so the client keeps the batch: the
    // call's error names it, and never reads as a floor to wait for with nothing queued. The
    // batch settles at the next read: the floor is raised and the ask is sent, with no
    // second call.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let error = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap_err();
    assert!(error.kept_batch().is_some(), "{error}");
    assert!(error.refusal().is_none(), "{error}");
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(&wallet).await.is_some());
}

#[tokio::test]
async fn test_blocked_return_floor_write_kept_at_a_receive_is_asked_once() {
    // Bob is unblocked and his mark asks, and the answer to the batch that raises the floor
    // and queues the ask is lost: the client keeps the batch, the read doesn't fail, and the
    // mark names the batch. The batch settles at the next read, and no later pass asks anew:
    // one request, with its raise.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    wallet.receive(None).await.unwrap();
    let marks = wallet.app().protocol_scan(b"bd/").await.unwrap();
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["queued"]["kept_batch"].is_array(), "{mark}");
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(&wallet).await.is_some());
    assert_eq!(block_marks(&wallet).await, 1);
}

#[tokio::test]
async fn test_blocked_return_floor_write_that_fails_keeps_the_ask_waiting() {
    // Bob is unblocked and his mark asks, but the batch that raises the floor and queues the
    // ask is refused: nothing is queued, the read doesn't fail, and the ask waits. The next
    // receive raises the floor and asks.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(shared_floor(&wallet).await, None);
    assert_eq!(block_marks(&wallet).await, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert!(shared_floor(&wallet).await.is_some());
}
