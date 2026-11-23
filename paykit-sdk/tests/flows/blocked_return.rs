//! Messages dropped for the user's block come back once the sender is unblocked (Paykit
//! spec 8.1, "Blocked counterparties"; Paykit wire 4.1): dropped as never processed, marked,
//! asked for again, and a request that comes back is held for the user.

use std::sync::Arc;

use paykit_sdk::{AppScopes, NoticeKind, Paykit, PaykitConfig};
use pubky_switch_client::app::Initiator;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use pubky_switch_client::transport::Transport;

use crate::common::{asked, draft, request_from, store, ListsStore, ReadHook, World};

const MINUTE_MS: u64 = 60_000;

/// Alice's wallet holding `paykit_pay` and `paykit_manage_contacts`, on `store`, inviting bob.
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
    app.app()
        .protocol_scan("paykit", b"bd/")
        .await
        .unwrap()
        .len()
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
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
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
    let error = wallet.unblock(&w.bob_id()).await.unwrap_err();
    assert!(error.kept_batch().is_some());
    w.clock.advance_ms(11 * MINUTE_MS);
    let _ = wallet.receive(None).await;
    // No ask queued, delivered or not: the unfinished raise didn't write the unblock.
    let mark = format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    let mark: serde_json::Value = serde_json::from_slice(
        &wallet
            .app()
            .protocol_get("paykit", mark.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(mark["d"]["queued"].is_null(), "{mark}");
    assert!(mark["d"]["asked_at_ms"].is_null(), "{mark}");
    // No unblock was submitted before the raise applied.
    let own_unblock = format!("bk/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    assert!(wallet
        .app()
        .protocol_get("paykit", own_unblock.as_bytes())
        .await
        .unwrap()
        .is_none());
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
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
    assert_eq!(lost[0].counterparty, Some(w.bob_id()));
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
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    restored.receive(None).await.unwrap();
    assert_eq!(block_marks(&restored).await, 1);
    assert_eq!(
        shared_floor(&restored).await,
        restored.app().highest_service_time()
    );
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
    w.alice
        .switch()
        .push_missed(1, now - 1_000, now + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    // The window is kept, and applied at the next read of the lists.
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    assert_eq!(shared_floor(&wallet).await, Some(now));
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
async fn test_blocked_return_missed_floor_uses_the_window_end_only_when_a_sender_is_marked() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.receive(None).await.unwrap();
    let start = w.clock.now_ms();
    w.clock.advance_ms(MINUTE_MS);
    w.alice.switch().push_missed(1, start, start + 1_000);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    assert_eq!(shared_floor(&wallet).await, None);

    wallet.block(&w.bob_id()).await.unwrap();
    let end = w.clock.now_ms();
    w.clock.advance_ms(MINUTE_MS);
    w.alice.switch().push_missed(1, start, end);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 1);
    assert_eq!(shared_floor(&wallet).await, Some(end));
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
    let outcome = wallet.pay_without_request(payment).await.outcome.unwrap();
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
    assert_eq!(lost[0].counterparty, Some(seeded[0]));
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
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id())).into_bytes(),
                value: serde_json::to_vec(&mark).unwrap(),
            }],
        )
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
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
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

/// Write bob's mark back as it stands when its request was delivered and the delivery isn't
/// written into it yet, as after a crash, or a flush by the host between two mark passes:
/// `queued` names the delivered request, and `asked_at_ms` is null. `speculative` as a
/// widening's mark.
async fn delivery_not_written_into_the_mark(w: &World, wallet: &mut Paykit, speculative: bool) {
    let sent = wallet
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.kind == "paykit.resend_request")
        .expect("the request");
    let key = format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    let marks = wallet
        .app()
        .protocol_scan("paykit", key.as_bytes())
        .await
        .unwrap();
    let mut mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["asked_at_ms"].is_u64(), "{mark}");
    mark["d"]["asked_at_ms"] = serde_json::Value::Null;
    mark["d"]["queued"] = serde_json::json!({
        "msg_id": sent.msg_id,
        "hash": pubky_switch_core::enc::b64(&sent.plaintext.hash()),
    });
    if speculative {
        mark["d"]["speculative"] = serde_json::Value::Bool(true);
    }
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: key.into_bytes(),
                value: serde_json::to_vec(&mark).unwrap(),
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_blocked_return_window_delivered_before_a_crash_not_told_lost_at_expiry() {
    // Bob's mark asks and the request is delivered, but the app stops before it writes the
    // delivery into the mark. Past the mark's expiry, the outbox shows the request delivered
    // in time: the mark goes, and its window isn't told lost.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    Box::pin(came_back(&w, &mut wallet, &mut shop)).await;
    delivery_not_written_into_the_mark(&w, &mut wallet, false).await;
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS - 11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(&wallet).await, 0);
    assert_eq!(lost_count(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_window_delivered_not_told_lost_when_a_widening_drops_it() {
    // Bob's mark asked and its request was delivered. The mark is then written back by hand
    // as it stands before the delivery is written into it, and made speculative (a
    // widening's) so the bound picks it. With the bound on marks full, a `missed` item's
    // widening marks carol, whom the deny list holds, and the bound drops bob's mark: the
    // outbox shows his request delivered, so his window isn't told lost.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    Box::pin(came_back(&w, &mut wallet, &mut shop)).await;
    delivery_not_written_into_the_mark(&w, &mut wallet, true).await;
    let now = w.clock.now_ms();
    crate::common::seed_marks(&mut wallet, "bd/", paykit_sdk::MAX_DROPPED_MARKS - 1, now).await;
    wallet.app_mut().deny(&w.carol_id()).await.unwrap();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(senders.contains(&w.carol_id()));
    assert!(!senders.contains(&w.bob_id()));
    assert_eq!(lost_count(&wallet).await, 0);
}

/// A store whose reads of one message's delivery fail while a `msg_id` is set: the outbox's
/// answer on that message can't be read.
#[derive(Default)]
struct FailingDeliveryReads {
    inner: pubky_switch_client::store::MemoryStore,
    msg_id: std::sync::Mutex<Option<String>>,
}

impl FailingDeliveryReads {
    /// Fail every read of `msg_id`'s delivery, or none.
    fn fail(&self, msg_id: Option<&str>) {
        *self.msg_id.lock().unwrap() = msg_id.map(str::to_owned);
    }
}

impl StateStore for FailingDeliveryReads {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        if let Some(id) = self.msg_id.lock().unwrap().as_deref() {
            if key.ends_with(format!("out/{id}").as_bytes())
                || key.ends_with(format!("sent/{id}").as_bytes())
            {
                return Err(pubky_switch_client::error::StoreError::Unavailable);
            }
        }
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
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
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
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl FailingDeliveryReads {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        apply(batch)
    }
}

/// A `msg_id` the outbox keeps nothing under: a request not delivered.
const UNDELIVERED: &str = "6f1c2b8e-3d4a-4f5b-9c6d-7e8f9a0b1c2d";

/// How many lost-messages notices `app` shows for `counterparty`.
async fn lost_for(app: &Paykit, counterparty: &[u8; 32]) -> usize {
    app.notices()
        .await
        .unwrap()
        .iter()
        .filter(|n| {
            n.kind == NoticeKind::BlockedMessagesLost && n.counterparty == Some(*counterparty)
        })
        .count()
}

/// The notices of lost messages `app` keeps owed.
async fn lost_owed(app: &Paykit) -> usize {
    app.app()
        .protocol_scan("paykit", b"bl/")
        .await
        .unwrap()
        .len()
}

/// Bob's mark asked and its request was delivered ([`came_back`]); the mark is written back
/// as it stands before the delivery is written into it, made speculative so the bound picks
/// it ([`delivery_not_written_into_the_mark`]), naming as queued `undelivered` in place of
/// the delivered request when given. The bound on marks is then filled, and carol, whom
/// alice blocks, sends a request: the next receive step counts it in a new mark, and the
/// bound drops bob's there.
async fn bound_drops_bob_at_the_next_receive_step(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    undelivered: Option<&str>,
) {
    came_back(w, wallet, shop).await;
    delivery_not_written_into_the_mark(w, wallet, true).await;
    if let Some(msg_id) = undelivered {
        let key = format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id()));
        let marks = wallet
            .app()
            .protocol_scan("paykit", key.as_bytes())
            .await
            .unwrap();
        let mut mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
        mark["d"]["queued"]["msg_id"] = serde_json::Value::String(msg_id.to_owned());
        wallet
            .app_mut()
            .protocol_commit(
                "paykit",
                &[pubky_switch_client::app::LocalWrite::Put {
                    key: key.into_bytes(),
                    value: serde_json::to_vec(&mark).unwrap(),
                }],
            )
            .await
            .unwrap();
    }
    Box::pin(bound_filled_and_carol_sends(w, wallet)).await;
}

/// The bound on marks filled, and carol, whom alice blocks, sends a request: the next
/// receive step counts it in a new mark, and the bound drops the speculative one.
async fn bound_filled_and_carol_sends(w: &World, wallet: &mut Paykit) {
    let now = w.clock.now_ms();
    crate::common::seed_marks(wallet, "bd/", paykit_sdk::MAX_DROPPED_MARKS - 1, now).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut carol = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    request_from(&mut carol, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.carol_id()).await.unwrap();
}

/// The bound held at the receive step that dropped bob's mark for carol's.
async fn bob_dropped_for_carol(w: &World, wallet: &Paykit) {
    let senders = crate::common::mark_senders(wallet, "bd/").await;
    assert!(senders.contains(&w.carol_id()));
    assert!(!senders.contains(&w.bob_id()));
    assert_eq!(senders.len(), paykit_sdk::MAX_DROPPED_MARKS);
}

#[tokio::test]
async fn test_blocked_return_window_delivered_before_eviction_not_told_lost_at_the_receive_step() {
    // Bob's mark names as queued a request the outbox shows delivered, the delivery not yet
    // written into it. The receive step that counts carol's request drops bob's mark for the
    // bound: the pass after the read finds the request delivered, so his window isn't told
    // lost, and nothing stays owed.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    Box::pin(bound_drops_bob_at_the_next_receive_step(
        &w,
        &mut wallet,
        &mut shop,
        None,
    ))
    .await;
    Box::pin(wallet.receive(None)).await.unwrap();
    bob_dropped_for_carol(&w, &wallet).await;
    assert_eq!(lost_count(&wallet).await, 0);
    assert_eq!(lost_owed(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_window_not_delivered_told_lost_after_the_receive_step() {
    // Bob's mark names as queued a request the outbox keeps nothing of: not delivered. The
    // receive step drops the mark for the bound, and the pass after the read tells his
    // window lost, once.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    Box::pin(bound_drops_bob_at_the_next_receive_step(
        &w,
        &mut wallet,
        &mut shop,
        Some(UNDELIVERED),
    ))
    .await;
    Box::pin(wallet.receive(None)).await.unwrap();
    bob_dropped_for_carol(&w, &wallet).await;
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 1);
    assert_eq!(lost_owed(&wallet).await, 0);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(wallet.receive(None)).await.unwrap();
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 1);
}

/// [`bound_drops_bob_at_the_next_receive_step`] on a store whose read of the delivery of
/// the request bob's mark names fails at the receive that drops it: the receive fails after
/// the step, the bound held and the notice still owed, told of nothing. Once the read
/// passes, the next receive settles it. Returns the wallet then.
async fn owed_through_a_failed_delivery_read(
    w: &World,
    undelivered: Option<&str>,
) -> (Paykit, Arc<FailingDeliveryReads>) {
    let failing = Arc::new(FailingDeliveryReads::default());
    let mut wallet = contacts_wallet(w, Arc::clone(&failing) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    Box::pin(bound_drops_bob_at_the_next_receive_step(
        w,
        &mut wallet,
        &mut shop,
        undelivered,
    ))
    .await;
    let named = match undelivered {
        Some(msg_id) => msg_id.to_owned(),
        None => {
            wallet
                .app()
                .sent_to(&w.bob_id(), 0, u64::MAX)
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.kind == "paykit.resend_request")
                .expect("the request")
                .msg_id
        }
    };
    failing.fail(Some(&named));
    assert!(Box::pin(wallet.receive(None)).await.is_err());
    bob_dropped_for_carol(w, &wallet).await;
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 0);
    assert_eq!(lost_owed(&wallet).await, 1);
    failing.fail(None);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(wallet.receive(None)).await.unwrap();
    assert_eq!(lost_owed(&wallet).await, 0);
    (wallet, failing)
}

#[tokio::test]
async fn test_blocked_return_delivery_read_failed_at_the_first_pass_delivered_not_told_lost() {
    let w = World::new();
    let (wallet, _store) = Box::pin(owed_through_a_failed_delivery_read(&w, None)).await;
    assert_eq!(lost_count(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_delivery_read_failed_at_the_first_pass_undelivered_told_lost_once() {
    let w = World::new();
    let (mut wallet, _store) =
        Box::pin(owed_through_a_failed_delivery_read(&w, Some(UNDELIVERED))).await;
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 1);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(wallet.receive(None)).await.unwrap();
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 1);
}

/// [`bound_drops_bob_at_the_next_receive_step`] naming `undelivered` as queued, on a store
/// whose read of its delivery fails at the receive that drops bob's mark: the notice stays
/// owed, and the backup taken then carries it. Returns the backup.
async fn backup_owing_a_lost_notice(w: &World, undelivered: &str) -> paykit_sdk::PaykitBackup {
    let failing = Arc::new(FailingDeliveryReads::default());
    let mut wallet = contacts_wallet(w, Arc::clone(&failing) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    Box::pin(bound_drops_bob_at_the_next_receive_step(
        w,
        &mut wallet,
        &mut shop,
        Some(undelivered),
    ))
    .await;
    failing.fail(Some(undelivered));
    assert!(Box::pin(wallet.receive(None)).await.is_err());
    failing.fail(None);
    bob_dropped_for_carol(w, &wallet).await;
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 0);
    assert_eq!(lost_owed(&wallet).await, 1);
    wallet.backup().await.unwrap()
}

#[tokio::test]
async fn test_blocked_return_restore_owed_lost_notice_settled_once_after_the_restore() {
    // A backup carries the notice owed on bob's dropped mark, whose request the outbox
    // holds nothing of. The restored outbox holds what the backup's did, nothing under that
    // `msg_id`: the first receive tells bob's window lost and keeps nothing owed, and the
    // next doesn't tell it again.
    let w = World::new();
    let backup = Box::pin(backup_owing_a_lost_notice(&w, UNDELIVERED)).await;
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(lost_owed(&restored).await, 1);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(restored.receive(None)).await.unwrap();
    assert_eq!(lost_for(&restored, &w.bob_id()).await, 1);
    assert_eq!(lost_owed(&restored).await, 0);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(restored.receive(None)).await.unwrap();
    assert_eq!(lost_for(&restored, &w.bob_id()).await, 1);
}

#[tokio::test]
async fn test_blocked_return_restore_owed_lost_notice_delivered_after_the_restore_not_told() {
    // The request bob's mark names is still queued when the bound drops the mark, its
    // delivery held, and the backup taken then carries the owed notice and the outbox. The
    // restore puts the request back under the same `msg_id`; once it is delivered, the
    // receive after finds it delivered within the window: nothing is told, nothing owed.
    let w = World::new();
    let failing = Arc::new(FailingDeliveryReads::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&failing) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.app_mut().hold_queued(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let key = format!("bd/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    let marks = wallet
        .app()
        .protocol_scan("paykit", key.as_bytes())
        .await
        .unwrap();
    let mut mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    let msg_id = mark["d"]["queued"]["msg_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{mark}"))
        .to_owned();
    mark["d"]["speculative"] = serde_json::Value::Bool(true);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: key.into_bytes(),
                value: serde_json::to_vec(&mark).unwrap(),
            }],
        )
        .await
        .unwrap();
    Box::pin(bound_filled_and_carol_sends(&w, &mut wallet)).await;
    failing.fail(Some(&msg_id));
    assert!(Box::pin(wallet.receive(None)).await.is_err());
    failing.fail(None);
    bob_dropped_for_carol(&w, &wallet).await;
    assert_eq!(lost_owed(&wallet).await, 1);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(lost_owed(&restored).await, 1);
    // This host delivers before the SDK's notice-settlement pass. It first confirms the
    // own identity and reads the restored client's page; a raw flush must not skip those.
    restored.app_mut().refresh_identity().await.unwrap();
    restored.app_mut().receive(None).await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    assert_eq!(lost_owed(&restored).await, 1);
    restored
        .app_mut()
        .release_queued(&w.bob_id())
        .await
        .unwrap();
    Box::pin(restored.flush()).await.unwrap();
    assert!(matches!(
        restored.app().delivery(&msg_id).await.unwrap(),
        Some(pubky_switch_client::app::Delivery::Accepted { .. })
    ));
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(restored.receive(None)).await.unwrap();
    assert_eq!(lost_for(&restored, &w.bob_id()).await, 0);
    assert_eq!(lost_owed(&restored).await, 0);
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
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
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
        .outcome
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
    wallet
        .app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
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
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("bd/{}", pubky_switch_core::enc::b64(sender)).into_bytes(),
                value: serde_json::to_vec(&mark).unwrap(),
            }],
        )
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
    let id = pubky_switch_core::enc::b64(&[19; 16]);
    let widening = serde_json::json!({ "v": 1, "d": {
        "id": id, "deny": null,
        "windows": [{ "from_ms": ahead, "until_ms": ahead, "floor_ms": ahead }]
    } });
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("bw/{id}").into_bytes(),
                value: serde_json::to_vec(&widening).unwrap(),
            }],
        )
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
    let marks = restored
        .app()
        .protocol_scan("paykit", b"bd/")
        .await
        .unwrap();
    assert_eq!(marks.len(), 1);
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["latest_ms"].as_u64().unwrap() <= bound, "{mark}");
    for (key, value) in restored
        .app()
        .protocol_scan("paykit", b"bw/")
        .await
        .unwrap()
    {
        let group: serde_json::Value = serde_json::from_slice(&value).unwrap();
        assert_eq!(String::from_utf8(key).unwrap(), format!("bw/{id}"));
        let window = &group["d"]["windows"][0];
        assert_eq!(window["from_ms"], bound);
        assert_eq!(window["until_ms"], bound);
        assert_eq!(window["floor_ms"], bound);
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
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
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
        assert!(wallet
            .app()
            .protocol_get("paykit", b"lr")
            .await
            .unwrap()
            .is_some());
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
    wallet
        .app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
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

#[tokio::test]
async fn test_blocked_return_payer_records_another_apps_unblock_with_the_floor() {
    let w = World::new();
    let mut payer = contacts_wallet(&w, store()).await;
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    payer.receive(None).await.unwrap();
    assert_eq!(shared_floor(&payer).await, None);

    contacts.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    payer.receive(None).await.unwrap();
    let expected = payer
        .app()
        .highest_service_time()
        .unwrap()
        .saturating_add(pubky_switch_core::records::CLOCK_TOLERANCE_MS);
    assert_eq!(shared_floor(&payer).await, Some(expected));

    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    payer.receive(None).await.unwrap();
    assert_eq!(shared_floor(&payer).await, Some(expected));
}

#[tokio::test]
async fn test_blocked_return_observed_unblock_retries_its_original_cutoff_after_reload() {
    let w = World::new();
    let storage = store();
    let mut payer = contacts_wallet(&w, Arc::clone(&storage)).await;
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    payer.receive(None).await.unwrap();
    contacts.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let expected = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::PermissionMissing,
            current: serde_json::Value::Null,
        });
    assert!(payer.receive(None).await.is_err());
    assert!(asked(&payer, &w.bob_id()).await.is_empty());
    assert_eq!(shared_floor(&payer).await, None);
    let app_id = payer.app_id();
    drop(payer);

    w.clock.advance_ms(MINUTE_MS);
    let mut payer = w
        .load_on_clock(&w.alice, storage, app_id, w.clock.clone())
        .await;
    payer.receive(None).await.unwrap();
    assert_eq!(shared_floor(&payer).await, Some(expected));
}

#[tokio::test]
async fn test_blocked_return_observed_unblock_and_a_later_own_block_survive_a_lost_answer() {
    let w = World::new();
    let mut payer = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut payer, &mut shop).await;
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    let expected = w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let result = payer.receive(None).await;
    assert!(result.unwrap_err().kept_batch().is_some());
    assert!(asked(&payer, &w.bob_id()).await.is_empty());

    payer.block(&w.carol_id()).await.unwrap();
    w.clock.advance_ms(MINUTE_MS);
    payer.receive(None).await.unwrap();
    assert_eq!(shared_floor(&payer).await, Some(expected));
    assert!(payer
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.carol_id()));
    assert_eq!(asked(&payer, &w.bob_id()).await.len(), 1);
    shop.receive(None).await.unwrap();
    payer.receive(None).await.unwrap();
    assert!(payer.requests().await.unwrap()[0].manual_only);
    assert_eq!(asked(&payer, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_own_unblock_raises_once_without_a_dropped_message() {
    let w = World::new();
    let base = store();
    let mut wallet = contacts_wallet(&w, Arc::clone(&base)).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let ahead = Arc::new(pubky_switch_client::clock::TestClock::new(
        w.clock.now_ms() + crate::common::HOUR_MS,
    ));
    let mut wallet = w.load_on_clock(&w.alice, base, app_id, ahead).await;
    assert_eq!(shared_floor(&wallet).await, None);
    assert_eq!(block_marks(&wallet).await, 0);
    let at = w.clock.now_ms();
    wallet.unblock(&w.bob_id()).await.unwrap();
    let floor = Some(at + pubky_switch_core::records::CLOCK_TOLERANCE_MS);
    assert_eq!(shared_floor(&wallet).await, floor);
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, floor);
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
}

#[tokio::test]
async fn test_blocked_return_own_unblock_waits_for_a_refused_raise() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    crate::common::refuse_writes(&w.alice, 1);
    let result = wallet.unblock(&w.bob_id()).await;
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert!(result.is_err());
    assert_eq!(shared_floor(&wallet).await, None);
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert_eq!(
        shared_floor(&wallet).await,
        Some(w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS)
    );
}

#[tokio::test]
async fn test_blocked_return_own_unblock_waits_for_applied_evidence_after_a_lost_answer() {
    let w = World::new();
    let base = store();
    let mut wallet = contacts_wallet(&w, Arc::clone(&base)).await;
    let app_id = wallet.app_id();
    wallet.block(&w.bob_id()).await.unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let result = wallet.unblock(&w.bob_id()).await;
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert!(result.unwrap_err().kept_batch().is_some());
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, base, app_id, Arc::clone(&w.clock))
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    let floor = shared_floor(&wallet).await;
    assert_eq!(
        floor,
        Some(w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS)
    );
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, floor);
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
}

#[tokio::test]
async fn test_blocked_return_contacts_only_unblock_needs_no_floor() {
    let w = World::new();
    let mut contacts = w
        .join(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    contacts.unblock(&w.bob_id()).await.unwrap();
    assert!(!contacts
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert_eq!(shared_floor(&contacts).await, None);
    contacts.receive(None).await.unwrap();
    assert_eq!(shared_floor(&contacts).await, None);
}

#[tokio::test]
async fn test_blocked_return_own_unblock_reads_a_concurrent_floor_first() {
    // The phone raises the floor; the wallet's copy doesn't hold it yet. The unblock reads the
    // floor from the switch before it writes, so it builds on the phone's floor, which already
    // covers its window: nothing is written over it, and the floor stays the phone's.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    let higher = w.clock.now_ms() + crate::common::HOUR_MS;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(higher)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(shared_floor(&wallet).await, None);
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, Some(higher));
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
}

#[tokio::test]
async fn test_blocked_return_own_unblock_waits_for_an_unreadable_floor() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet
        .app_mut()
        .put_record(&floor_record(&w), br#"{"v":1}"#, Vec::new())
        .await
        .unwrap();
    wallet.block(&w.bob_id()).await.unwrap();
    assert!(matches!(
        wallet.unblock(&w.bob_id()).await.unwrap_err().refusal(),
        Some(paykit_sdk::Refusal::NotNow(_))
    ));
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    wallet
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert_eq!(
        shared_floor(&wallet).await,
        Some(w.clock.now_ms() + pubky_switch_core::records::CLOCK_TOLERANCE_MS)
    );
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
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
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
        .outcome
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
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
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
            paykit_lib::collections::Collection::BlockFloor.fixed_policy(),
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

#[tokio::test]
async fn test_blocked_return_floor_raised_over_a_value_that_does_not_read_keeps_unknown_members() {
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
    phone
        .app_mut()
        .put_record(&target, br#"{"v":1}"#, Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, Some(1));
    wallet_raises_the_floor(&w, &mut wallet, &mut shop).await;
    let raised = floor_value(&wallet).await;
    assert_eq!(raised["later_member"], unknown, "{raised}");
    assert!(shared_floor(&wallet).await.is_some_and(|floor| floor > 1));
    // Nothing but the floor changed: the value is the one seen, raised.
    let mut expected = first;
    expected["floor"] = raised["floor"].clone();
    assert_eq!(raised, expected);
}

#[tokio::test]
async fn test_blocked_return_floor_delete_refused_and_the_floor_kept() {
    // Paykit wire 5.6: `block_floor` is permanent. No app deletes it, so the floor stays and
    // an unblock still goes.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut phone = payer_app(&w, "phone").await;
    let target = floor_record(&w);
    let value = paykit_lib::records::BlockFloorRecord::new(1)
        .to_json()
        .unwrap();
    phone
        .app_mut()
        .put_record(&target, &value, Vec::new())
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, Some(1));
    wallet.block(&w.carol_id()).await.unwrap();
    assert!(phone
        .app_mut()
        .delete_record(&target, Vec::new())
        .await
        .is_err());
    wallet.receive(None).await.unwrap();
    assert_eq!(shared_floor(&wallet).await, Some(1));
    wallet.unblock(&w.carol_id()).await.unwrap();
    assert!(!wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.carol_id()));
}

/// The key of the identity's `block_floor` record, as `app` derives it.
fn floor_key(w: &World, app: &Paykit) -> [u8; 32] {
    app.app().record_key(&floor_record(w)).unwrap()
}

/// Whether `app` keeps the floor's divergence hold (`fk`, Paykit wire 5.6).
async fn floor_held_divergent(app: &Paykit) -> bool {
    app.app()
        .protocol_get("paykit", b"fk")
        .await
        .unwrap()
        .is_some_and(|bytes| {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            value["d"]["divergent"] == serde_json::json!(true)
        })
}

/// Whether `app` refuses to unblock carol while the floor is held, keeping her denied.
async fn unblock_held(w: &World, app: &mut Paykit) -> bool {
    let refused = matches!(
        app.unblock(&w.carol_id()).await.unwrap_err().refusal(),
        Some(paykit_sdk::Refusal::FloorHeld)
    );
    refused
        && app
            .app_mut()
            .allow_list()
            .await
            .unwrap()
            .deny
            .contains(&w.carol_id())
}

#[tokio::test]
async fn test_blocked_return_divergent_floor_held_for_good() {
    // Paykit wire 5.6, Divergent floor: once the wallet sees the floor divergent it holds for
    // good. Every one-time request opens `manual_only`, the wallet writes no successor, so a
    // raise an unblock owes can't be written, and neither a later write that clears the view,
    // a reopen, nor a restore from a backup taken before the divergence, in place or fresh,
    // drops the hold. The user can't confirm it away: it isn't a divergent record shown.
    let w = World::new();
    let storage = store();
    let mut wallet = contacts_wallet(&w, Arc::clone(&storage)).await;
    let mut phone = payer_app(&w, "phone").await;
    let mut shop = w.shop().await;
    let target = floor_record(&w);
    phone
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let before = wallet.backup().await.unwrap();
    let key = floor_key(&w, &wallet);
    assert!(!floor_held_divergent(&wallet).await);

    w.alice
        .switch()
        .make_divergent([0x5a; 16], key, [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    assert!(unblock_held(&w, &mut wallet).await);
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&first).await.unwrap().manual_only);
    assert!(!wallet
        .divergent_records()
        .await
        .unwrap()
        .iter()
        .any(|shown| shown.record == key));

    // Another app's write clears the view. The hold stays, and the wallet writes nothing.
    phone
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(2)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let held = wallet.app().records_of("block_floor").await.unwrap();
    assert!(held.iter().all(|record| !record.divergent));
    assert_eq!(shared_floor(&wallet).await, Some(2));
    assert!(unblock_held(&w, &mut wallet).await);
    assert_eq!(shared_floor(&wallet).await, Some(2));
    w.clock.advance_ms(MINUTE_MS);
    let second = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.request(&second).await.unwrap().manual_only);
    let after = wallet.backup().await.unwrap();

    // Reopened.
    let app_id = wallet.app().app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&storage), app_id, Arc::clone(&w.clock))
        .await;
    assert!(floor_held_divergent(&wallet).await);
    assert!(unblock_held(&w, &mut wallet).await);

    // Restored in place from the backup taken before the divergence.
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        &before,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert!(floor_held_divergent(&restored).await);
    restored.receive(None).await.unwrap();
    assert!(unblock_held(&w, &mut restored).await);
    drop(restored);

    // Restored on a fresh device from a backup taken after it.
    let mut fresh = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &after,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert!(floor_held_divergent(&fresh).await);
    fresh.receive(None).await.unwrap();
    assert!(unblock_held(&w, &mut fresh).await);
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
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
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
        .outcome
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
    let router = Arc::clone(&w.router) as Arc<dyn Transport>;
    Box::pin(phone_paying_carol_through(w, phone_store, router)).await
}

/// [`phone_paying_carol_on`], the phone's requests going through `transport`.
async fn phone_paying_carol_through(
    w: &World,
    phone_store: Arc<dyn StateStore>,
    transport: Arc<dyn Transport>,
) -> (Paykit, Paykit, Paykit, paykit_sdk::RequestRef) {
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
    let mut wallet = contacts_wallet(w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut phone = w
        .join_through(
            &w.alice,
            "phone",
            AppScopes::new().pay().build().unwrap(),
            crate::common::day_window(),
            phone_store,
            transport,
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
/// payment check and before the payment method's call; and another once, during the next
/// commit that keeps the deny list as read from the switch.
#[derive(Default)]
pub(crate) struct PayingStore {
    inner: pubky_switch_client::store::MemoryStore,
    fail_payer_activity_read: std::sync::atomic::AtomicBool,
    hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    lists_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl PayingStore {
    pub(crate) fn fail_next_payer_activity_read(&self) {
        self.fail_payer_activity_read
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn on_paying(&self, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some(Box::new(hook));
    }

    fn on_deny_list_read(&self, hook: impl FnOnce() + Send + 'static) {
        *self.lists_hook.lock().unwrap() = Some(Box::new(hook));
    }
}

impl StateStore for PayingStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        let value = self.inner.get(ns, key)?;
        if self
            .fail_payer_activity_read
            .load(std::sync::atomic::Ordering::SeqCst)
            && key.starts_with(b"rec/")
            && value.as_deref().is_some_and(|bytes| {
                serde_json::from_slice::<serde_json::Value>(bytes).unwrap()["d"]["collection"]
                    == paykit_lib::collections::Collection::PayerActivity.name()
            })
        {
            self.fail_payer_activity_read
                .store(false, std::sync::atomic::Ordering::SeqCst);
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        Ok(value)
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
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
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
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl PayingStore {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        const PAYING: &[u8] = b"\"stage\":\"paying\"";
        let paying = batch.writes().iter().any(|write| {
            matches!(write, pubky_switch_client::store::Write::Put { key, value, .. }
                if key.starts_with(b"p/paykit/j/") && value.windows(PAYING.len()).any(|w| w == PAYING))
        });
        let deny_list = batch.writes().iter().any(|write| {
            matches!(write, pubky_switch_client::store::Write::Put { key, .. } if key == b"p/paykit/dl")
        });
        let applied = apply(batch)?;
        if applied != pubky_switch_client::store::CommitOutcome::Applied {
            return Ok(applied);
        }
        if paying {
            if let Some(hook) = self.hook.lock().unwrap().take() {
                hook();
            }
        }
        if deny_list {
            if let Some(hook) = self.lists_hook.lock().unwrap().take() {
                hook();
            }
        }
        Ok(pubky_switch_client::store::CommitOutcome::Applied)
    }
}

/// The wallet drops bob's request and keeps him blocked until the timed unblock.
async fn wallet_about_to_raise_the_floor(w: &World, wallet: &mut Paykit, shop: &mut Paykit) {
    request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(block_marks(wallet).await, 1);
    w.clock.advance_ms(11 * MINUTE_MS);
}

/// The phone starts carol's payment automatically, a first dispatch, and `during` runs
/// while its journal write of `Paying` commits, the wallet about to raise the floor. Returns
/// the attempt, left open.
async fn automatic_first_dispatch(
    w: &Arc<World>,
    during: impl FnOnce(&World, Paykit, Paykit) + Send + 'static,
) -> (Paykit, paykit_lib::AttemptId) {
    automatic_first_dispatch_with(w, true, during).await
}

/// [`automatic_first_dispatch`], the wallet about to raise the floor when `about_to_raise`;
/// `during` also gets carol's request.
async fn automatic_first_dispatch_with(
    w: &Arc<World>,
    about_to_raise: bool,
    during: impl FnOnce(&World, Paykit, Paykit) + Send + 'static,
) -> (Paykit, paykit_lib::AttemptId) {
    let phone_store = Arc::new(PayingStore::default());
    // On the heap: the setup nests deep.
    let (mut wallet, mut phone, mut shop, carols) = Box::pin(phone_paying_carol_on(
        w,
        Arc::clone(&phone_store) as Arc<dyn StateStore>,
    ))
    .await;
    if about_to_raise {
        Box::pin(wallet_about_to_raise_the_floor(w, &mut wallet, &mut shop)).await;
    }
    assert_eq!(shared_floor(&wallet).await, None);
    let world = Arc::clone(w);
    phone_store.on_paying(move || during(&world, wallet, shop));
    let outcome = phone
        .pay_request_automatically(&carols, paykit_sdk::PayOptions::default())
        .await
        .outcome
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
                    wallet.unblock(&bob).await.unwrap();
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
        // The request record is read again first, then the floor, which fails.
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "/records/".into(),
                after: 1,
            });
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_blocked_return_request_back_by_a_live_mark_not_paid_automatically() {
    // Bob's request is dropped for the block and he is unblocked; whether he is flagged
    // isn't known, so nothing asks him. The unblock has already raised the floor. He sends
    // a request again on his own: its record is `manual_only`. An automatic first
    // dispatch reads the record and holds: the method isn't called. The user pays it by
    // hand.
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(&w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    Box::pin(dropped_for_the_block(&w, &mut wallet, &mut shop)).await;
    lists.flagged_unknown(true);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let again = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert!(shared_floor(&wallet).await.is_some());
    assert!(wallet.request(&again).await.unwrap().manual_only);
    assert!(held_for_the_user(&wallet).await);
    let outcome = wallet
        .pay_request_automatically(&again, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = wallet.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
    // The resume ends the held attempt, known unsubmitted; by hand, the request is paid.
    wallet.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let paid = wallet
        .pay_request(&again, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
}

#[tokio::test]
async fn test_blocked_return_request_marked_manual_only_during_the_paying_commit_not_paid() {
    // An automatic first dispatch reads carol's record, open and not `manual_only`, and
    // journals `Paying`. While that write commits, another payer app marks the record
    // `manual_only`, raising no floor. The record read again after the commit holds the
    // payment: the method isn't called, and the attempt is known unsubmitted.
    let w = Arc::new(World::new());
    let (phone, attempt_id) = automatic_first_dispatch_with(&w, false, |w, mut wallet, _shop| {
        let carol = w.carol_id();
        // The wallet runs on its own thread while the phone's commit waits for it.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    wallet.receive(None).await.unwrap();
                    mark_manual_only(&mut wallet, &carol).await;
                    assert_eq!(shared_floor(&wallet).await, None);
                })
        })
        .join()
        .unwrap();
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_blocked_return_payee_blocked_during_the_paying_commit_not_paid() {
    // An automatic first dispatch reads the deny list, which doesn't hold carol, and journals
    // `Paying`. While that write commits, another payer app blocks her. The deny list read
    // again after the commit holds the payment: the method isn't called, and the attempt is
    // known unsubmitted.
    let w = Arc::new(World::new());
    let (phone, attempt_id) = automatic_first_dispatch_with(&w, false, |w, mut wallet, _shop| {
        let carol = w.carol_id();
        // The wallet runs on its own thread while the phone's commit waits for it.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async { wallet.app_mut().deny(&carol).await.unwrap() })
        })
        .join()
        .unwrap();
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

#[tokio::test]
async fn test_blocked_return_request_marked_manual_only_after_the_repeat_read_not_repeated() {
    // At the resume, the repeat reads carol's record, open and not `manual_only`; then
    // another payer app marks it `manual_only`, raising no floor. The record read again just
    // before the call holds the repeat.
    let w = World::new();
    Box::pin(repeat_while_the_wallet_writes(&w, OwnWrite::ManualOnly)).await;
}

#[tokio::test]
async fn test_blocked_return_request_cancelled_after_the_repeat_read_not_repeated() {
    // Carol cancels her request. At the resume, the repeat reads her record, open: no payer
    // app has received the cancellation yet. Then another payer app receives it and records
    // it. The record read again just before the call holds the repeat.
    let w = World::new();
    Box::pin(repeat_while_the_wallet_writes(&w, OwnWrite::Cancellation)).await;
}

#[tokio::test]
async fn test_blocked_return_request_cancelled_during_the_paying_commit_not_paid() {
    // An automatic first dispatch reads carol's record, open, and journals `Paying`. While
    // that write commits, carol cancels her request, and another payer app receives the
    // cancellation and records it. The record read again after the commit holds the payment:
    // the method isn't called, and the attempt is known unsubmitted.
    let w = Arc::new(World::new());
    let (phone, attempt_id) = automatic_first_dispatch_with(&w, false, |w, mut wallet, _shop| {
        let carol = w.carol_id();
        // Carol and the wallet run on their own thread while the phone's commit waits.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(async {
                        Box::pin(carol_cancels(w)).await;
                        OwnWrite::Cancellation.run(&mut wallet, &carol).await;
                    })
            });
        });
    })
    .await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 0);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

/// What another payer app writes to carol's request record while the phone pays it.
#[derive(Clone, Copy)]
enum OwnWrite {
    /// Marks it `manual_only`, raising no floor.
    ManualOnly,
    /// Records carol's cancellation, sent before.
    Cancellation,
}

impl OwnWrite {
    /// `app` reads its messages, then writes this to `payee`'s one request it holds.
    async fn run(self, app: &mut Paykit, payee: &[u8; 32]) {
        app.receive(None).await.unwrap();
        match self {
            Self::ManualOnly => {
                mark_manual_only(app, payee).await;
                assert_eq!(shared_floor(app).await, None);
            }
            Self::Cancellation => {
                let request = app
                    .requests()
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|view| view.request.payee == *payee)
                    .expect("the request")
                    .request;
                assert_eq!(
                    app.request(&request).await.unwrap().status,
                    paykit_sdk::RequestStatus::Cancelled
                );
            }
        }
    }
}

/// Carol cancels her one request to alice, from another app of hers.
async fn carol_cancels(w: &World) {
    let mut seller_wallet = w
        .join(
            &w.carol,
            "seller-wallet",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let record = seller_wallet.payee_activity().await.unwrap()[0].record;
    seller_wallet
        .cancel_sent_request(&record, None)
        .await
        .unwrap();
}

/// The phone leaves an automatic payment of carol's request in flight. At the resume, the repeat reads
/// her record; then, while the phone reads the deny list, the wallet makes `write` (carol's
/// cancellation is sent before the resume). The
/// method isn't called again, and the attempt stays open for the method's history or a
/// receipt, at this resume and the next.
async fn repeat_while_the_wallet_writes(w: &World, write: OwnWrite) {
    let phone_store = Arc::new(PayingStore::default());
    // On the heap: the setup nests deep.
    let (mut wallet, mut phone, _shop, carols) = Box::pin(phone_paying_carol_on(
        w,
        Arc::clone(&phone_store) as Arc<dyn StateStore>,
    ))
    .await;
    let attempt_id = paid_in_flight(w, &mut phone, &carols, Initiator::Automatic).await;
    w.clock.advance_ms(MINUTE_MS);
    if matches!(write, OwnWrite::Cancellation) {
        Box::pin(carol_cancels(w)).await;
    }
    let carol = w.carol_id();
    phone_store.on_deny_list_read(move || {
        // The wallet runs on its own thread while the phone's commit waits for it.
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(write.run(&mut wallet, &carol))
        })
        .join()
        .unwrap();
    });
    Box::pin(phone.resume_payments()).await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Paying);
    Box::pin(phone.resume_payments()).await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

/// `app` writes the payer record of `payee`'s one request it holds with `manual_only`.
async fn mark_manual_only(app: &mut Paykit, payee: &[u8; 32]) {
    let request = app
        .requests()
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.request.payee == *payee)
        .expect("the request")
        .request;
    let collection = paykit_lib::collections::Collection::PayerActivity;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::request_record(payee, &request.request_hash),
    );
    let key = app.app().record_key(&target).unwrap();
    let records = app.app().records_of(collection.name()).await.unwrap();
    let held = records
        .iter()
        .find(|record| record.key == key)
        .expect("the record");
    let mut value: serde_json::Value =
        serde_json::from_slice(held.content.value().expect("a value")).unwrap();
    value["manual_only"] = serde_json::Value::Bool(true);
    app.app_mut()
        .put_record(&target, &serde_json::to_vec(&value).unwrap(), Vec::new())
        .await
        .unwrap();
}

/// Which of the phone's reads at an automatic dispatch another payer app's write lands
/// after: the deny list, carol's request record, or the block floor, each answered before
/// the write.
#[derive(Clone, Copy)]
enum LandsAfter {
    /// The wallet blocks carol.
    DenyList,
    /// The wallet marks carol's request record `manual_only`, raising no floor.
    Record,
    /// The wallet raises the floor past carol's request.
    Floor,
}

impl LandsAfter {
    /// A part of the path of the phone's read.
    fn path(self, w: &World, phone: &Paykit, carols: &paykit_sdk::RequestRef) -> String {
        let target = |collection: paykit_lib::collections::Collection, natural: Vec<u8>| {
            let target = pubky_switch_client::app::RecordTarget::new(
                pubky_switch_client::app::Collection::new(
                    collection.name(),
                    collection.policy(None),
                ),
                &natural,
            );
            let key = phone.app().record_key(&target).unwrap();
            format!("/records/{}", pubky_switch_core::enc::b64(&key))
        };
        match self {
            Self::DenyList => "/allow".into(),
            Self::Record => target(
                paykit_lib::collections::Collection::PayerActivity,
                paykit_lib::derive::natural_key::request_record(
                    &carols.payee,
                    &carols.request_hash,
                ),
            ),
            Self::Floor => target(
                paykit_lib::collections::Collection::BlockFloor,
                paykit_lib::derive::natural_key::block_floor(&w.alice_id()),
            ),
        }
    }

    /// The wallet makes the write, on its own thread while the phone waits for it.
    fn write(self, w: &World, mut wallet: Paykit) {
        let carol = w.carol_id();
        let bob = w.bob_id();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(async {
                    match self {
                        Self::DenyList => wallet.app_mut().deny(&carol).await.unwrap(),
                        Self::Record => OwnWrite::ManualOnly.run(&mut wallet, &carol).await,
                        Self::Floor => {
                            wallet.unblock(&bob).await.unwrap();
                            wallet.receive(None).await.unwrap();
                            assert_eq!(asked(&wallet, &bob).await.len(), 1);
                            assert!(shared_floor(&wallet).await.is_some());
                        }
                    }
                })
        })
        .join()
        .unwrap();
    }
}

/// The phone starts carol's payment automatically, a first dispatch; after its journal
/// write of `Paying`, the wallet makes `write` right after the phone's next read of it. The
/// method isn't called, and the attempt is known unsubmitted.
async fn first_dispatch_with_a_write_after_its_read(w: &Arc<World>, write: LandsAfter) {
    let phone_store = Arc::new(PayingStore::default());
    let reads = Arc::new(ReadHook::new(Arc::clone(&w.router)));
    // On the heap: the setup nests deep.
    let (mut wallet, mut phone, mut shop, carols) = Box::pin(phone_paying_carol_through(
        w,
        Arc::clone(&phone_store) as Arc<dyn StateStore>,
        Arc::clone(&reads) as Arc<dyn Transport>,
    ))
    .await;
    if matches!(write, LandsAfter::Floor) {
        Box::pin(wallet_about_to_raise_the_floor(w, &mut wallet, &mut shop)).await;
    }
    assert_eq!(shared_floor(&wallet).await, None);
    let path = write.path(w, &phone, &carols);
    let world = Arc::clone(w);
    let armed = Arc::clone(&reads);
    phone_store.on_paying(move || {
        armed.after_read(&path, move || write.write(&world, wallet));
    });
    let outcome =
        Box::pin(phone.pay_request_automatically(&carols, paykit_sdk::PayOptions::default()))
            .await
            .outcome
            .unwrap();
    let (paykit_sdk::PayOutcome::Pending { attempt_id, .. }
    | paykit_sdk::PayOutcome::Paid { attempt_id, .. }) = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert!(reads.fired());
    assert_eq!(w.rail.pay_calls(attempt_id), 0);
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Pending { .. }),
        "{outcome:?}"
    );
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Attempted);
}

/// The phone leaves an automatic payment of carol's request in flight. At the resume, once the repeat's
/// check has read the deny list, the wallet makes `write`: at once for the deny list, which
/// that read has answered, or right after the phone's next read of the record or the floor.
/// The method isn't called again, and the attempt stays open, at this resume and the next.
async fn repeat_with_a_write_after_its_read(w: &Arc<World>, write: LandsAfter) {
    let phone_store = Arc::new(PayingStore::default());
    let reads = Arc::new(ReadHook::new(Arc::clone(&w.router)));
    // On the heap: the setup nests deep.
    let (mut wallet, mut phone, mut shop, carols) = Box::pin(phone_paying_carol_through(
        w,
        Arc::clone(&phone_store) as Arc<dyn StateStore>,
        Arc::clone(&reads) as Arc<dyn Transport>,
    ))
    .await;
    let attempt_id = paid_in_flight(w, &mut phone, &carols, Initiator::Automatic).await;
    if matches!(write, LandsAfter::Floor) {
        Box::pin(wallet_about_to_raise_the_floor(w, &mut wallet, &mut shop)).await;
    }
    w.clock.advance_ms(MINUTE_MS);
    let path = write.path(w, &phone, &carols);
    let world = Arc::clone(w);
    let armed = Arc::clone(&reads);
    let wrote = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = Arc::clone(&wrote);
    phone_store.on_deny_list_read(move || {
        if matches!(write, LandsAfter::DenyList) {
            write.write(&world, wallet);
            done.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            armed.after_read(&path, move || write.write(&world, wallet));
        }
    });
    Box::pin(phone.resume_payments()).await.unwrap();
    assert!(reads.fired() || wrote.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    let payment = phone.payments().await.unwrap().remove(0);
    assert_eq!(payment.stage, paykit_sdk::Stage::Paying);
    Box::pin(phone.resume_payments()).await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_payee_blocked_after_the_deny_list_read_not_paid() {
    // After the journal write of `Paying`, the phone reads the deny list, which doesn't hold
    // carol; right after that read, another payer app blocks her. The second read of the deny
    // list holds the payment.
    let w = Arc::new(World::new());
    Box::pin(first_dispatch_with_a_write_after_its_read(
        &w,
        LandsAfter::DenyList,
    ))
    .await;
}

#[tokio::test]
async fn test_blocked_return_request_marked_manual_only_after_the_record_read_not_paid() {
    // After the journal write of `Paying`, the phone reads carol's record, open and not
    // `manual_only`; right after that read, another payer app marks it `manual_only`. The
    // second read of the record shows another write and holds the payment.
    let w = Arc::new(World::new());
    Box::pin(first_dispatch_with_a_write_after_its_read(
        &w,
        LandsAfter::Record,
    ))
    .await;
}

#[tokio::test]
async fn test_blocked_return_floor_raised_after_the_floor_read_not_paid() {
    // After the journal write of `Paying`, the phone reads the floor, which no app has
    // raised; right after that read, another payer app raises it past carol's request. The
    // second read of the floor shows another write and holds the payment.
    let w = Arc::new(World::new());
    Box::pin(first_dispatch_with_a_write_after_its_read(
        &w,
        LandsAfter::Floor,
    ))
    .await;
}

#[tokio::test]
async fn test_blocked_return_payee_blocked_after_the_repeat_deny_list_read_not_repeated() {
    // At the resume, the repeat's check reads the deny list, which doesn't hold carol; right
    // after that read, another payer app blocks her. The second read holds the repeat.
    let w = Arc::new(World::new());
    Box::pin(repeat_with_a_write_after_its_read(&w, LandsAfter::DenyList)).await;
}

#[tokio::test]
async fn test_blocked_return_request_marked_manual_only_after_the_repeat_record_read_not_repeated()
{
    // At the resume, the repeat's check reads carol's record, open and not `manual_only`;
    // right after that read, another payer app marks it `manual_only`. The second read of
    // the record holds the repeat.
    let w = Arc::new(World::new());
    Box::pin(repeat_with_a_write_after_its_read(&w, LandsAfter::Record)).await;
}

#[tokio::test]
async fn test_blocked_return_floor_raised_after_the_repeat_floor_read_not_repeated() {
    // At the resume, the repeat's check reads the floor, which no app has raised; right
    // after that read, another payer app raises it past carol's request. The second read of
    // the floor holds the repeat.
    let w = Arc::new(World::new());
    Box::pin(repeat_with_a_write_after_its_read(&w, LandsAfter::Floor)).await;
}

#[tokio::test]
async fn test_blocked_return_floor_kept_copy_not_taken_for_the_switch_at_a_dispatch() {
    // The phone holds the floor of a first raise, under carol's request. The wallet raises it
    // again, past her request, and the phone reads nothing meanwhile. At the phone's
    // automatic dispatch, the switch answers the floor's reads `404`: the copy the phone keeps
    // isn't taken for the switch's write, and the absence of a permanent floor it holds live
    // is the rollback alarm, so the method isn't called.
    let w = World::new();
    w.rail.set_declaration(
        paykit_sdk::RailDeclaration::new(
            paykit_sdk::rail::FAKE_LONGEST_COMPLETION_MS,
            paykit_sdk::rail::FAKE_PAYMENT_TIME_TOLERANCE_MS,
        )
        .with_idempotent(true),
    );
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut phone = w
        .join_with(
            &w.alice,
            "phone",
            AppScopes::new().pay().build().unwrap(),
            crate::common::day_window(),
            store(),
        )
        .await;
    let mut shop = w.shop().await;
    Box::pin(wallet_raises_the_floor(&w, &mut wallet, &mut shop)).await;
    phone.receive(None).await.unwrap();
    let first = shared_floor(&phone).await.expect("the first raise");
    w.clock.advance_ms(MINUTE_MS);
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
    request_from(&mut shop, w.alice_id(), draft("3", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(shared_floor(&wallet)
        .await
        .is_some_and(|floor| floor > first));
    assert_eq!(shared_floor(&phone).await, Some(first));
    let path = LandsAfter::Floor.path(&w, &phone, &carols);
    for _ in 0..4 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: path.clone(),
                code: pubky_switch_core::http::ErrorCode::NotFound,
                current: serde_json::Value::Null,
            });
    }
    // The floor is permanent: a `404` for the floor the phone holds live is the rollback
    // alarm (Paykit wire 5.6, Seen floor and permanence), and the method isn't called.
    let outcome = phone
        .pay_request_automatically(&carols, paykit_sdk::PayOptions::default())
        .await
        .outcome;
    assert!(
        matches!(
            outcome,
            Err(paykit_sdk::PaykitError::Client(
                pubky_switch_client::app::AppError::Alarm(_)
            ))
        ),
        "{outcome:?}"
    );
    assert!(phone.app().alarm().is_some());
    assert!(w.rail.all_pay_calls().is_empty());
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

/// Start the named kind of attempt and leave its dispatch in flight.
async fn paid_in_flight(
    w: &World,
    phone: &mut Paykit,
    carols: &paykit_sdk::RequestRef,
    by: Initiator,
) -> paykit_lib::AttemptId {
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let options = paykit_sdk::PayOptions::default();
    let paid = if by == Initiator::User {
        phone.pay_request(carols, options).await
    } else {
        phone.pay_request_automatically(carols, options).await
    };
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = paid.outcome.unwrap() else {
        panic!("in flight");
    };
    attempt_id
}

/// Alice's wallet, holding `paykit_pay` and contacts, on `storage`, with carol blocked, a
/// subscription of bob's accepted, and the identity's floor seen divergent: the hold stands.
/// Returns the wallet, bob's shop and the subscription.
async fn held_floor_wallet(
    w: &World,
    storage: Arc<dyn StateStore>,
) -> (Paykit, Paykit, paykit_sdk::RequestRef) {
    let mut wallet = contacts_wallet(w, storage).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let mut shop = w.shop().await;
    let sent = shop
        .send_request(
            w.alice_id(),
            crate::periods::monthly(w.clock.now_ms(), None),
        )
        .await
        .unwrap();
    let subscription = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&subscription).await.unwrap();
    shop.receive(None).await.unwrap();
    let mut phone = payer_app(w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = floor_key(w, &wallet);
    w.alice
        .switch()
        .make_divergent([0x5a; 16], key, [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    (wallet, shop, subscription)
}

/// Under the floor's hold, after a raise the wallet owes and can't write: the lists stay
/// read, a new request from bob is taken and paid once by hand, and a Billing Period pays
/// by hand (Paykit wire 5.6, Divergent floor: payments by hand and Billing Periods go on
/// under their own rules).
async fn still_pays_by_hand(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
    subscription: &paykit_sdk::RequestRef,
) {
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    let request = request_from(shop, w.alice_id(), draft("9", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let options = crate::common::over_hold(wallet, &request).await;
    let paid = wallet.pay_request(&request, options).await.outcome.unwrap();
    assert!(
        matches!(paid, paykit_sdk::PayOutcome::Paid { .. }),
        "{paid:?}"
    );
    let options = crate::common::over_hold(wallet, subscription).await;
    let period = wallet
        .pay_period(subscription, &crate::periods::january(), options)
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(period, paykit_sdk::PayOutcome::Paid { .. }),
        "{period:?}"
    );
    // And a payment without a request, its proof sent.
    crate::public::publish(
        w,
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
    let unrequested = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(
        matches!(unrequested, paykit_sdk::PayOutcome::Paid { .. }),
        "{unrequested:?}"
    );
    assert_eq!(w.rail.all_executions().len(), 3);
}

#[tokio::test]
async fn test_blocked_return_held_floor_keeps_payments_after_another_apps_unblock() {
    let w = World::new();
    let (mut wallet, mut shop, subscription) = held_floor_wallet(&w, store()).await;
    // A contacts app unblocks carol: the wallet owes the raise and can't write it.
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.carol_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    // The unblock is recorded, carol's entries held with the raise owed.
    assert!(wallet
        .app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some());
    still_pays_by_hand(&w, &mut wallet, &mut shop, &subscription).await;
}

#[tokio::test]
async fn test_blocked_return_held_floor_keeps_payments_after_a_restore_that_is_not_clean() {
    let w = World::new();
    let storage = store();
    let (wallet, mut shop, subscription) = held_floor_wallet(&w, Arc::clone(&storage)).await;
    let mut wallet = wallet;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    // Restored in place, its cursor not known: the widening owes a raise it can't write.
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    restored.receive(None).await.unwrap();
    assert!(floor_held_divergent(&restored).await);
    assert!(widening_owed(&restored).await);
    reconciled(&mut restored).await;
    still_pays_by_hand(&w, &mut restored, &mut shop, &subscription).await;
}

#[tokio::test]
async fn test_blocked_return_restore_widening_held_asks_no_one() {
    // Paykit wire 5.6, Raising: restored in place with its cursor not known, the wallet owes
    // the widening's raise and can't write it under the floor's hold. The widening names no
    // sender, so no one is asked to re-send while it is owed, and its re-send request waits.
    let w = World::new();
    let storage = store();
    let (mut wallet, _shop, _subscription) = held_floor_wallet(&w, Arc::clone(&storage)).await;
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let since = w.clock.now_ms();
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    for _ in 0..2 {
        restored.receive(None).await.unwrap();
        w.clock.advance_ms(11 * MINUTE_MS);
    }
    assert!(widening_owed(&restored).await);
    assert_eq!(asked_since(&restored, &w.bob_id(), since).await, 0);
    restored.flush().await.unwrap();
    assert_eq!(asked_since(&restored, &w.bob_id(), since).await, 0);
    let error = restored
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
        "{error}"
    );
}

/// How many re-send requests `app` delivered to `to` since `since_ms`.
async fn asked_since(app: &Paykit, to: &[u8; 32], since_ms: u64) -> usize {
    app.app()
        .sent_to(to, since_ms, u64::MAX)
        .await
        .unwrap()
        .iter()
        .filter(|m| m.kind == "paykit.resend_request")
        .count()
}

/// The reconciliation a restore owes before any payment, done.
async fn reconciled(app: &mut Paykit) {
    let mut done = app.reconcile().await;
    for _ in 0..4 {
        if done.is_ok() {
            break;
        }
        app.receive(None).await.unwrap();
        done = app.reconcile().await;
    }
    done.unwrap();
    app.end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
}

/// The block floor held notices `app` shows.
async fn floor_held_notices(app: &Paykit) -> Vec<paykit_sdk::Notice> {
    app.notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == paykit_sdk::NoticeKind::BlockFloorHeld)
        .collect()
}

#[tokio::test]
async fn test_blocked_return_held_floor_told_to_the_host_once_and_after_reopen() {
    let w = World::new();
    let storage = store();
    let (mut wallet, _shop, _subscription) = held_floor_wallet(&w, Arc::clone(&storage)).await;
    let shown = floor_held_notices(&wallet).await;
    assert_eq!(shown.len(), 1);
    assert!(shown[0].counterparty.is_none() && shown[0].attempt_id.is_none());
    wallet.acknowledge_notice(&shown[0].id).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_notices(&wallet).await.is_empty());
    let app_id = wallet.app().app_id();
    drop(wallet);
    let wallet = w
        .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
        .await;
    assert_eq!(floor_held_notices(&wallet).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_seen_then_gone_after_a_restore_holds() {
    // Paykit wire 5.6, Seen floor and permanence: restored on a fresh device, the wallet
    // holds no copy of the floor it saw live; the switch then answers that it has none, with
    // no provider loss. The highest floor seen doesn't make that absence lawful: every
    // one-time request counts as dated before the floor.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    wallet
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let key = floor_key(&w, &wallet);
    drop(wallet);
    w.alice.switch().drop_record(&key).unwrap();
    let storage = store();
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    // Listed after the restore, the floor seen live is gone: the rollback alarm. The app
    // stops, so no one-time request is paid automatically, and the alarm is kept across a
    // reopen.
    let _ = restored.receive(None).await;
    assert!(restored.app().alarm().is_some());
    let request = request_from(&mut shop, w.alice_id(), draft("5", w.clock.now_ms())).await;
    let outcome = restored
        .pay_request_automatically(&request, paykit_sdk::PayOptions::default())
        .await
        .outcome;
    assert!(
        !matches!(outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{outcome:?}"
    );
    assert!(w.rail.all_executions().is_empty());
    let app_id = restored.app().app_id();
    drop(restored);
    let reopened = w
        .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
        .await;
    assert!(reopened.app().alarm().is_some());
}

#[tokio::test]
async fn test_blocked_return_held_floor_kept_over_a_missed_item() {
    // A `missed` item widens the blocked window: its raise can't be written under the hold,
    // so the widening stays owed, and the hold, the lists and payments by hand go on.
    let w = World::new();
    let (mut wallet, mut shop, subscription) = held_floor_wallet(&w, store()).await;
    let now = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, now - 1_000, now + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    assert!(widening_owed(&wallet).await);
    assert!(unblock_held(&w, &mut wallet).await);
    still_pays_by_hand(&w, &mut wallet, &mut shop, &subscription).await;
}

#[tokio::test]
async fn test_blocked_return_missed_item_under_the_hold_asks_no_one() {
    // Paykit wire 5.6, Raising: a `missed` item's widening is owed from the commit that
    // writes it. Under the hold, its raise can't be written: no one is asked to re-send, in
    // the receive that takes the item or after, and a re-send request waits.
    let w = World::new();
    let (mut wallet, _shop, _subscription) = held_floor_wallet(&w, store()).await;
    let since = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, since - 1_000, since + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked_since(&wallet, &w.bob_id(), since).await, 0);
    // Its read of the lists is due at once, the hold notwithstanding.
    assert!(missed_widening_read(&wallet).await);
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    wallet.flush().await.unwrap();
    assert!(widening_owed(&wallet).await);
    assert_eq!(asked_since(&wallet, &w.bob_id(), since).await, 0);
    let error = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
        "{error}"
    );
}

#[tokio::test]
async fn test_blocked_return_missed_item_holds_a_due_mark_ask_until_its_raise() {
    // Paykit wire 5.6, Raising: with no hold, a `missed` item comes while carol is blocked and
    // bob's mark is due. Its widening is owed before its read of the lists, so bob's ask
    // waits for that read, which the receive taking the item makes after the feed: the
    // widening and its raise applied, bob is asked in that receive.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let _shop = w.shop().await;
    let since = w.clock.now_ms();
    due_mark(&mut wallet, &w.bob_id(), since - 2_000).await;
    w.alice
        .switch()
        .push_missed(1, since - 1_000, since + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(!widening_owed(&wallet).await);
    assert!(shared_floor(&wallet).await.is_some());
    assert!(mark_asked(&wallet).await);
}

#[tokio::test]
async fn test_blocked_return_conflicting_widening_raise_keeps_the_receive_going() {
    // Paykit wire 5.6, Raising: a `missed` item comes while carol is blocked. Its widening's
    // raise meets `409 conflict` on its write and again on its retry: the widening stays owed,
    // the read of the lists stands, and the next read applies it with its raise.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let since = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, since - 1_000, since + MINUTE_MS);
    for _ in 0..2 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::Conflict,
                current: serde_json::Value::Null,
            });
    }
    // The receive that takes the item reads the lists for it, and its raise conflicts twice.
    wallet.receive(None).await.unwrap();
    assert!(missed_widening_read(&wallet).await);
    w.clock.advance_ms(5_000);
    wallet.receive(None).await.unwrap();
    assert!(!widening_owed(&wallet).await);
    assert!(shared_floor(&wallet).await.is_some());
}

#[tokio::test]
async fn test_blocked_return_missed_item_with_nothing_to_ask_read_in_its_receive() {
    // Paykit wire 5.6, Raising: a `missed` item comes while carol is blocked, with no mark due
    // and no ask queued. Its widening's read of the lists is made in the receive that takes
    // it, so the widening and its raise are applied there.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let since = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, since - 1_000, since + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(!widening_owed(&wallet).await);
    assert!(shared_floor(&wallet).await.is_some());
}

#[tokio::test]
async fn test_blocked_return_missed_item_under_the_hold_read_in_its_receive() {
    // Paykit wire 5.6, Raising: under the divergence hold, with nothing to ask, a `missed`
    // item's read of the lists is still made in the receive that takes it; its raise waits.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5e; 16], floor_key(&w, &wallet), [5; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    let since = w.clock.now_ms();
    w.alice
        .switch()
        .push_missed(1, since - 1_000, since + MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(missed_widening_read(&wallet).await);
}

/// Whether the unverified mark's ask was made: its request signed and queued.
async fn mark_asked(app: &Paykit) -> bool {
    app.app()
        .protocol_scan("paykit", b"ud/")
        .await
        .unwrap()
        .iter()
        .any(|(_, value)| {
            let value: serde_json::Value = serde_json::from_slice(value).unwrap();
            !value["d"]["queued"].is_null()
        })
}

/// Whether every `missed` item's widening `app` keeps holds its read of the lists.
async fn missed_widening_read(app: &Paykit) -> bool {
    let widenings = app.app().protocol_scan("paykit", b"bw/").await.unwrap();
    !widenings.is_empty()
        && widenings.iter().all(|(_, value)| {
            let value: serde_json::Value = serde_json::from_slice(value).unwrap();
            !value["d"]["deny"].is_null()
        })
}

/// A mark of an entry from `sender` dropped unverified, due to be asked for again.
async fn due_mark(app: &mut Paykit, sender: &[u8; 32], at: u64) {
    let value = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(sender),
            "earliest_ms": at,
            "latest_ms": at,
            "due": true,
            "asked_at_ms": null,
            "queued": null,
        }
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("ud/{}", pubky_switch_core::enc::b64(sender)).into_bytes(),
                value: serde_json::to_vec(&value).unwrap(),
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_blocked_return_unwritten_raise_told_once_and_gone_once_written() {
    // Paykit wire 5.6, Raising: another app unblocks bob while the floor holds a value that
    // doesn't read, with none seen, so the wallet's raise can't be written and no divergence
    // hold explains it. The host is told once, over later receives, each reading the lists
    // once; acknowledged after a reopen, it isn't told again in that load, and after the next
    // reopen it is.
    // Once the floor reads, the raise is written by the next flush, and the notice goes.
    let w = World::new();
    let storage = store();
    let mut wallet = contacts_wallet(&w, Arc::clone(&storage)).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(&floor_record(&w), br#"{"v":1}"#, Vec::new())
        .await
        .unwrap();
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(unblock_raise_owed(&wallet).await);
    assert_eq!(raise_waiting_notices(&wallet).await, 1);
    let reads = |w: &World| {
        w.router
            .log()
            .iter()
            .filter(|line| line.contains("/allow"))
            .count()
    };
    for _ in 0..3 {
        let before = reads(&w);
        w.clock.advance_ms(5_000);
        wallet.receive(None).await.unwrap();
        // The raise is tried with one read of the lists; the client reads them once more
        // before the feed, after the copy it took.
        assert!(reads(&w) <= before + 2);
    }
    assert_eq!(raise_waiting_notices(&wallet).await, 1);
    // Reopened, it is still shown; acknowledged before the first receive, it isn't told again
    // in this load.
    let app_id = wallet.app().app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&storage), app_id, Arc::clone(&w.clock))
        .await;
    let shown = wallet.notices().await.unwrap();
    let notice = shown
        .iter()
        .find(|notice| notice.kind == paykit_sdk::NoticeKind::FloorRaiseWaiting)
        .unwrap();
    wallet.acknowledge_notice(&notice.id).await.unwrap();
    w.clock.advance_ms(5_000);
    wallet.receive(None).await.unwrap();
    assert_eq!(raise_waiting_notices(&wallet).await, 0);
    // Reopened again, the raise still unwritten: it is told again.
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, Arc::clone(&storage), app_id, Arc::clone(&w.clock))
        .await;
    w.clock.advance_ms(5_000);
    wallet.receive(None).await.unwrap();
    assert_eq!(raise_waiting_notices(&wallet).await, 1);
    // The floor reads now: a flush writes the raise, and the notice goes with it.
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    w.clock.advance_ms(5_000);
    wallet.flush().await.unwrap();
    assert!(!unblock_raise_owed(&wallet).await);
    assert_eq!(raise_waiting_notices(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_allowance_accepted_while_a_raise_is_owed_pays_nothing_old() {
    // Paykit wire 5.6, Raising; spec 8.1: bob's one-time request is open before the block.
    // Another app unblocks him while the wallet's raise can't be written, the floor holding a
    // value that doesn't read, and he accepts the wallet's Allowance meanwhile. The acceptance is taken under its own rules, but the
    // request, dated before the raise owed, stays paid only by hand: nothing pays it.
    let w = World::new();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new()
                .allowance_execute()
                .manage_contacts()
                .build()
                .unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    let mut shop = w
        .join(
            &w.bob,
            "shop",
            AppScopes::new().allowance().request().build().unwrap(),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    Box::pin(
        wallet.propose_executable_allowance(
            w.bob_id(),
            paykit_lib::allowances::AllowanceTerms::from_json(&serde_json::json!({
                "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
                "per_payment_amount": null, "active_from": null, "expires_at": null,
                "allowed_payment_endpoint_identifiers": null
            }))
            .unwrap(),
            pubky_switch_client::app::UserConfirmed::new(),
        ),
    )
    .await
    .unwrap();
    Box::pin(shop.receive(None)).await.unwrap();
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert!(!wallet.request(&request).await.unwrap().manual_only);
    wallet.block(&w.bob_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(&floor_record(&w), br#"{"v":1}"#, Vec::new())
        .await
        .unwrap();
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(unblock_raise_owed(&wallet).await);
    let offer = shop.allowances().await.unwrap().pop().unwrap();
    Box::pin(shop.accept_allowance(&offer.record))
        .await
        .unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(1_000);
        wallet.receive(None).await.unwrap();
    }
    assert!(unblock_raise_owed(&wallet).await);
    assert_eq!(
        wallet.allowances().await.unwrap()[0].status,
        paykit_sdk::AllowanceStatus::Accepted
    );
    assert!(wallet.request(&request).await.unwrap().manual_only);
    let outcome = wallet
        .pay_request_automatically(&request, paykit_sdk::PayOptions::default())
        .await
        .outcome;
    assert!(
        !matches!(outcome, Ok(paykit_sdk::PayOutcome::Paid { .. })),
        "{outcome:?}"
    );
    assert!(w.rail.all_pay_calls().is_empty());
}

/// How many floor raise waiting notices `app` shows.
async fn raise_waiting_notices(app: &Paykit) -> usize {
    app.notices()
        .await
        .unwrap()
        .into_iter()
        .filter(|notice| notice.kind == paykit_sdk::NoticeKind::FloorRaiseWaiting)
        .count()
}

#[tokio::test]
async fn test_blocked_return_divergent_floor_holds_a_repeat_the_user_started() {
    // Paykit wire 5.6, Divergent floor: no resume repeats a one-time payment under the hold,
    // the user's own included.
    let w = World::new();
    let (mut wallet, mut phone, _shop, request) = phone_paying_carol(&w).await;
    wallet
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let outcome = phone
        .pay_request(&request, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("expected a pending payment, got {outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    w.alice
        .switch()
        .make_divergent([0x5a; 16], floor_key(&w, &phone), [1; 32])
        .unwrap();
    phone.receive(None).await.unwrap();
    assert!(floor_held_divergent(&phone).await);
    w.clock.advance_ms(MINUTE_MS);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_in_place_restore_keeps_the_highest_floor_seen() {
    // The wallet sees the floor at the hour; its backup is from before. Restored in place,
    // it keeps the floor it saw, so a request from before the hour is paid only by hand.
    let w = World::new();
    let storage = store();
    let mut wallet = contacts_wallet(&w, Arc::clone(&storage)).await;
    let mut shop = w.shop().await;
    let backup = wallet.backup().await.unwrap();
    let before = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let higher = w.clock.now_ms() + crate::common::HOUR_MS;
    wallet
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(higher)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    let kept: serde_json::Value = serde_json::from_slice(
        &restored
            .app()
            .protocol_get("paykit", b"fk")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(kept["d"]["seen_ms"], serde_json::json!(higher));
    assert!(!floor_held_divergent(&restored).await);
    restored.receive(None).await.unwrap();
    assert!(restored.request(&before).await.unwrap().manual_only);
}

/// Whether `app` still owes a widening: a `missed` item's (`bw/`) or a restore's (`rh`).
async fn widening_owed(app: &Paykit) -> bool {
    let missed = !app
        .app()
        .protocol_scan("paykit", b"bw/")
        .await
        .unwrap()
        .is_empty();
    let restore = app
        .app()
        .protocol_get("paykit", b"rh")
        .await
        .unwrap()
        .is_some_and(|bytes| {
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            !value["d"]["widening_read"].is_null() || !value["d"]["earlier_widening"].is_null()
        });
    missed || restore
}

#[tokio::test]
async fn test_blocked_return_owed_raise_asks_the_unblocked_sender_nothing() {
    // Paykit wire 5.6, Raising: under the floor's hold another app unblocks bob, and the
    // wallet can't write the raise it owes. Its lists stay read, but bob is asked nothing
    // while the raise is owed, so no copy he re-sends reaches an own app with no raised floor.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5a; 16], floor_key(&w, &wallet), [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet
        .app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some());
    assert!(wallet
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    let error = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(paykit_sdk::Refusal::NotNow(_))),
        "{error}"
    );
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // Bob's block mark, whose ask the raise holds, reads no lists at each receive: the hold
    // keeps the raise from being written whatever the lists show.
    let reads = |w: &World| {
        w.router
            .log()
            .iter()
            .filter(|line| line.contains("/allow"))
            .count()
    };
    let before = reads(&w);
    for _ in 0..3 {
        w.clock.advance_ms(5_000);
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(reads(&w), before);
}

#[tokio::test]
async fn test_blocked_return_owed_raise_holds_an_ask_already_queued() {
    // An ask to bob queued before he was blocked again waits in the outbox. Under the floor's
    // hold another app unblocks him and the wallet can't write the raise it owes: neither its
    // receive nor a flush delivers that ask (Paykit wire 5.6, Raising).
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let before = asked(&wallet, &w.bob_id()).await.len();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/inbox".into(),
            after: 0,
        });
    let _ = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await;
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), before);
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(w.clock.now_ms())
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5b; 16], floor_key(&w, &wallet), [2; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet
        .app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some());
    wallet.flush().await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), before);
}

/// Whether `app` owes the raise of an unblock (`bf`).
async fn unblock_raise_owed(app: &Paykit) -> bool {
    app.app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn test_blocked_return_owed_raise_counts_no_queued_ask_as_sent() {
    // An ask the wallet kept for bob, its request queued and not delivered before he was
    // blocked again: while the raise for another app's unblock of him is owed under the hold,
    // the receive neither sends it nor counts it as sent (Paykit wire 5.6, Raising).
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let before = asked(&wallet, &w.bob_id()).await.len();
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/inbox".into(),
            after: 0,
        });
    let _ = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(5),
        )
        .await;
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), before);
    let queued = wallet.app().queued_messages("paykit").await.unwrap();
    let [queued] = queued.as_slice() else {
        panic!("one request queued: {queued:?}");
    };
    let queued = queued.clone();
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.block(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(w.clock.now_ms())
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5b; 16], floor_key(&w, &wallet), [2; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(wallet
        .app()
        .protocol_get("paykit", b"bf")
        .await
        .unwrap()
        .is_some());
    // As a stop between queuing an ask's request and dropping the ask leaves it.
    keep_queued_ask(&mut wallet, &w.bob_id(), 5, &queued).await;
    let summary = wallet.receive(None).await.unwrap();
    assert_eq!(summary.resends, 0);
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), before);
}

#[tokio::test]
async fn test_blocked_return_owed_raise_takes_period_details_and_holds_a_one_time_request() {
    // Paykit spec 8.1; Paykit wire 5.6, Raising: under the floor's hold another app unblocks
    // bob, whose subscription the wallet accepted. While the raise for that unblock is owed,
    // his period details are taken under their own rules, and his one-time request waits.
    let w = World::new();
    assert_eq!(w.clock.now_ms(), pubky_switch_client::testing::START_MS);
    let mut shop = w.shop().await;
    let mut wallet = contacts_wallet(&w, store()).await;
    let sent = shop
        .send_request(
            w.alice_id(),
            crate::periods::monthly(pubky_switch_client::testing::START_MS, None),
        )
        .await
        .unwrap();
    let subscription = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&subscription).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    wallet.block(&w.bob_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5d; 16], floor_key(&w, &wallet), [4; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(unblock_raise_owed(&wallet).await);
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt11"),
        paykit_lib::PaymentEndpointPayload::new("lnbc1period").unwrap(),
    );
    shop.send_period_details(
        &sale,
        paykit_lib::bodies::PeriodDetails {
            billing_period: crate::periods::january(),
            payment_endpoints: Some(endpoints),
            rates: None,
            expires_at: w.clock.now_ms() + 24 * 60 * MINUTE_MS,
        },
    )
    .await
    .unwrap();
    request_from(&mut shop, w.alice_id(), draft("4", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&subscription).await.unwrap().periods.len(),
        1
    );
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_blocked_return_raise_writes_the_floor_held_over_a_lower_write_refused() {
    // Paykit wire 5.6, Seen floor and permanence: the wallet holds a floor and another app
    // writes a lower one, which the wallet refuses, keeping the higher. The switch holds the
    // lower write, so the raise the wallet owes for another app's unblock isn't spared by the
    // floor it holds: it writes that higher floor over the lower write.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    let high = w.clock.now_ms() + 24 * 60 * MINUTE_MS;
    let mut phone = payer_app(&w, "phone").await;
    for floor in [high, 1] {
        phone
            .app_mut()
            .put_record(
                &floor_record(&w),
                &paykit_lib::records::BlockFloorRecord::new(floor)
                    .to_json()
                    .unwrap(),
                Vec::new(),
            )
            .await
            .unwrap();
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(shared_floor(&wallet).await, Some(high));
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    for _ in 0..2 {
        w.clock.advance_ms(11 * MINUTE_MS);
        wallet.receive(None).await.unwrap();
    }
    assert!(!unblock_raise_owed(&wallet).await);
    let mut reader = payer_app(&w, "reader").await;
    reader.receive(None).await.unwrap();
    assert_eq!(shared_floor(&reader).await, Some(high));
}

#[tokio::test]
async fn test_blocked_return_conflicting_background_raise_keeps_the_receive_going() {
    // Paykit wire 5.6, Raising: another app unblocks bob, and the wallet's raise for it meets
    // `409 conflict` on its write and again on its retry. The lists read stands, the raise
    // stays owed for the next receive, the host told, and the feed is read: bob's period
    // details are taken.
    let w = World::new();
    let mut shop = w.shop().await;
    let mut wallet = contacts_wallet(&w, store()).await;
    let sent = shop
        .send_request(
            w.alice_id(),
            crate::periods::monthly(pubky_switch_client::testing::START_MS, None),
        )
        .await
        .unwrap();
    let subscription = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    wallet.accept_subscription(&subscription).await.unwrap();
    shop.receive(None).await.unwrap();
    let sale = shop.payee_activity().await.unwrap()[0].record;
    wallet.block(&w.bob_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        crate::common::endpoint("btc-lightning-bolt11"),
        paykit_lib::PaymentEndpointPayload::new("lnbc1period").unwrap(),
    );
    shop.send_period_details(
        &sale,
        paykit_lib::bodies::PeriodDetails {
            billing_period: crate::periods::january(),
            payment_endpoints: Some(endpoints),
            rates: None,
            expires_at: w.clock.now_ms() + 24 * 60 * MINUTE_MS,
        },
    )
    .await
    .unwrap();
    for _ in 0..2 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::Conflict,
                current: serde_json::Value::Null,
            });
    }
    wallet.receive(None).await.unwrap();
    assert!(unblock_raise_owed(&wallet).await);
    assert_eq!(
        wallet.request(&subscription).await.unwrap().periods.len(),
        1
    );
    // The host is told the raise waits, and the notice goes once it is written.
    assert_eq!(raise_waiting_notices(&wallet).await, 1);
    w.clock.advance_ms(5_000);
    wallet.receive(None).await.unwrap();
    assert!(!unblock_raise_owed(&wallet).await);
    assert_eq!(raise_waiting_notices(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_held_floor_takes_a_cancellation_from_an_unblocked_payee() {
    // Under the hold another app unblocks carol: her new requests wait for a raise that can't
    // be written, but her cancellation of a request she sent before is a stop, and goes.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut carol_shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut carol_shop, w.alice_id(), draft("3", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    let mut phone = payer_app(&w, "phone").await;
    phone
        .app_mut()
        .put_record(
            &floor_record(&w),
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5a; 16], floor_key(&w, &wallet), [1; 32])
        .unwrap();
    wallet.receive(None).await.unwrap();
    assert!(floor_held_divergent(&wallet).await);
    let mut contacts = w
        .join_with(
            &w.alice,
            "contacts",
            AppScopes::new().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            store(),
        )
        .await;
    contacts.unblock(&w.carol_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let sale = carol_shop.payee_activity().await.unwrap()[0].record;
    carol_shop.cancel_sent_request(&sale, None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        paykit_sdk::RequestStatus::Cancelled
    );
}

#[tokio::test]
async fn test_blocked_return_divergence_reported_and_cleared_before_a_backup_holds() {
    // The client reads the floor divergent and then cleared by another app's write, before
    // the wallet takes its report: the backup still carries the hold, from that report.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut phone = payer_app(&w, "phone").await;
    let target = floor_record(&w);
    phone
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    w.alice
        .switch()
        .make_divergent([0x5a; 16], floor_key(&w, &wallet), [1; 32])
        .unwrap();
    wallet.app_mut().read_record(&target).await.unwrap();
    phone.receive(None).await.unwrap();
    phone
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(2)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    let cleared = wallet.app_mut().read_record(&target).await.unwrap();
    assert!(cleared.is_some_and(|record| !record.divergent));
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let restored = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert!(floor_held_divergent(&restored).await);
}

#[tokio::test]
async fn test_blocked_return_divergent_floor_holds_an_automatic_repeat() {
    // Paykit wire 5.6, Divergent floor: no resume repeats a one-time payment while the
    // divergence hold stands, whatever the floor the view shows.
    let w = World::new();
    let (mut wallet, mut phone, _shop, request) = phone_paying_carol(&w).await;
    let target = floor_record(&w);
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let outcome = phone
        .pay_request_automatically(&request, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("expected a pending automatic payment, got {outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);

    let key = floor_key(&w, &phone);
    w.alice
        .switch()
        .make_divergent([0x5a; 16], key, [1; 32])
        .unwrap();
    w.clock.advance_ms(MINUTE_MS);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    phone.receive(None).await.unwrap();
    assert!(floor_held_divergent(&phone).await);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_gone_in_continuous_history_holds_a_repeat() {
    // Paykit wire 5.6, Seen floor and permanence: a permanent floor the phone saw live that
    // reads absent with no provider loss is the rollback alarm, and no repeat goes.
    let w = World::new();
    let phone_store = store();
    let (mut wallet, mut phone, _shop, request) =
        Box::pin(phone_paying_carol_on(&w, Arc::clone(&phone_store))).await;
    let target = floor_record(&w);
    wallet
        .app_mut()
        .put_record(
            &target,
            &paykit_lib::records::BlockFloorRecord::new(1)
                .to_json()
                .unwrap(),
            Vec::new(),
        )
        .await
        .unwrap();
    phone.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let outcome = phone
        .pay_request_automatically(&request, paykit_sdk::PayOptions::default())
        .await
        .outcome
        .unwrap();
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = outcome else {
        panic!("expected a pending automatic payment, got {outcome:?}");
    };
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);

    w.alice
        .switch()
        .drop_record(&floor_key(&w, &phone))
        .unwrap();
    w.clock.advance_ms(MINUTE_MS);
    // The read before the repeat meets the absence: the client's rollback alarm, and no
    // repeat.
    let resumed = phone.resume_payments().await;
    assert!(
        matches!(
            resumed,
            Err(paykit_sdk::PaykitError::Client(
                pubky_switch_client::app::AppError::Alarm(_)
            ))
        ),
        "{resumed:?}"
    );
    assert!(phone.app().alarm().is_some());
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    // Reopened, the alarm and the hold stay.
    let app_id = phone.app().app_id();
    drop(phone);
    let mut phone = w
        .load_on_clock(&w.alice, phone_store, app_id, Arc::clone(&w.clock))
        .await;
    assert!(phone.app().alarm().is_some());
    let _ = phone.resume_payments().await;
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
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
    let attempt_id = paid_in_flight(&w, &mut phone, &carols, Initiator::User).await;
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
    let attempt_id = paid_in_flight(&w, &mut phone, &carols, Initiator::User).await;
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
    let attempt_id = paid_in_flight(&w, &mut phone, &carols, Initiator::User).await;
    w.clock.advance_ms(MINUTE_MS);
    // The resume reads the request record again, twice, then the floor, which fails.
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Unreachable {
            path: "/records/".into(),
            after: 2,
        });
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 1);
    phone.resume_payments().await.unwrap();
    assert_eq!(w.rail.pay_calls(&attempt_id), 2);
}

#[tokio::test]
async fn test_blocked_return_floor_write_kept_by_the_client_names_its_batch() {
    // A dropped-message window closes only after the unanswered raise has applied.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let error = wallet.unblock(&w.bob_id()).await.unwrap_err();
    assert!(error.kept_batch().is_some(), "{error}");
    assert!(error.refusal().is_none(), "{error}");
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert!(shared_floor(&wallet).await.is_some());
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_blocked_return_floor_write_kept_at_unblock_is_asked_once_after_success() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    assert!(wallet
        .unblock(&w.bob_id())
        .await
        .unwrap_err()
        .kept_batch()
        .is_some());
    let marks = wallet.app().protocol_scan("paykit", b"bd/").await.unwrap();
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["queued"].is_null(), "{mark}");
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    let floor = shared_floor(&wallet).await;
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert_eq!(shared_floor(&wallet).await, floor);
    assert_eq!(block_marks(&wallet).await, 1);
}

/// Alice's wallet with bob's request dropped for the block and bob unblocked; whether he is
/// flagged isn't known, so his mark asks nothing. The unblock has already raised the
/// floor. Returns the wallet's store.
async fn wallet_with_a_quiet_mark(w: &World) -> (Paykit, Paykit, Arc<ListsStore>) {
    let lists = Arc::new(ListsStore::default());
    let mut wallet = contacts_wallet(w, Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut shop = w.shop().await;
    Box::pin(dropped_for_the_block(w, &mut wallet, &mut shop)).await;
    lists.flagged_unknown(true);
    wallet.unblock(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert!(shared_floor(&wallet).await.is_some());
    (wallet, shop, lists)
}

/// Keep an ask of `sender` from `from_ms` on (`ask/`), as entries the app couldn't read or
/// a `missed` item queue one.
async fn keep_queued_ask(
    app: &mut Paykit,
    sender: &[u8; 32],
    from_ms: u64,
    queued: &pubky_switch_client::app::QueuedMessage,
) {
    let ask = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(sender),
            "from_ms": from_ms,
            "until_ms": null,
            "queued": {
                "msg_id": queued.msg_id,
                "hash": pubky_switch_core::enc::b64(&queued.hash),
            },
        }
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("ask/{}/{from_ms:020}", pubky_switch_core::enc::b64(sender))
                    .into_bytes(),
                value: serde_json::to_vec(&ask).unwrap(),
            }],
        )
        .await
        .unwrap();
}

async fn keep_ask(app: &mut Paykit, sender: &[u8; 32], from_ms: u64) {
    let ask = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(sender),
            "from_ms": from_ms,
            "until_ms": null,
            "queued": null,
        }
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("ask/{}/{from_ms:020}", pubky_switch_core::enc::b64(sender))
                    .into_bytes(),
                value: serde_json::to_vec(&ask).unwrap(),
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_blocked_return_ask_whose_answer_is_lost_asked_once_after_a_restart() {
    // The unblock already raised the floor. Bob accepts the ask but its answer is lost.
    // Reload and delivery retry retain the same queued identity: one accepted request.
    let w = World::new();
    let (mut wallet, _shop, lists) = Box::pin(wallet_with_a_quiet_mark(&w)).await;
    keep_ask(&mut wallet, &w.bob_id(), 5).await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/inbox".into(),
        });
    let _ = wallet.receive(None).await;
    let wallet_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(
            &w.alice,
            Arc::clone(&lists) as Arc<dyn StateStore>,
            wallet_id,
            Arc::clone(&w.clock),
        )
        .await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    // Whatever the outbox still holds goes too.
    wallet.flush().await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await, vec![(Some(5), None)]);
    assert!(shared_floor(&wallet).await.is_some());
    assert!(wallet
        .app()
        .protocol_scan("paykit", b"ask/")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_blocked_return_conflicting_unblock_raise_retries_before_any_ask() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/ops".into(),
            code: pubky_switch_core::http::ErrorCode::Conflict,
            current: serde_json::Value::Null,
        });
    assert_eq!(
        wallet.unblock(&w.bob_id()).await.unwrap_err().code(),
        Some(pubky_switch_core::http::ErrorCode::Conflict)
    );
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert_eq!(shared_floor(&wallet).await, None);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    let floor = shared_floor(&wallet).await;
    assert!(floor.is_some());
    keep_ask(&mut wallet, &w.bob_id(), 5).await;
    keep_ask(&mut wallet, &w.bob_id(), 6).await;
    wallet.receive(None).await.unwrap();
    let asks = asked(&wallet, &w.bob_id()).await;
    assert!(asks.contains(&(Some(5), None)));
    assert!(asks.contains(&(Some(6), None)));
    assert_eq!(shared_floor(&wallet).await, floor);
}

#[tokio::test]
async fn test_blocked_return_floor_write_that_fails_keeps_the_unblock_and_ask_waiting() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    crate::common::refuse_writes(&w.alice, 1);
    assert!(wallet.unblock(&w.bob_id()).await.is_err());
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(shared_floor(&wallet).await, None);
    assert_eq!(block_marks(&wallet).await, 1);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let floor = shared_floor(&wallet).await;
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert_eq!(shared_floor(&wallet).await, floor);
}

#[tokio::test]
async fn test_blocked_return_floor_write_refused_for_good_returns_the_refusal() {
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    dropped_for_the_block(&w, &mut wallet, &mut shop).await;
    let code = pubky_switch_core::http::ErrorCode::PermissionMissing;
    crate::common::refuse_writes(&w.alice, 1);
    let error = wallet.unblock(&w.bob_id()).await.unwrap_err();
    assert!(error.refusal().is_none(), "{error}");
    assert!(error.kept_batch().is_none(), "{error}");
    assert_eq!(error.code(), Some(code), "{error}");
    assert!(wallet
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    assert_eq!(shared_floor(&wallet).await, None);
    wallet.unblock(&w.bob_id()).await.unwrap();
    let floor = shared_floor(&wallet).await;
    wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(0),
        )
        .await
        .unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
    assert_eq!(shared_floor(&wallet).await, floor);
}

#[tokio::test]
async fn test_blocked_return_entry_held_while_the_notices_owed_are_full() {
    // With as many notices of lost messages owed as the app keeps (`marks::MAX_LOST_OWED`),
    // carol's entry, whose count would drop bob's mark naming a request as queued, waits:
    // none owed is dropped. The pass after that read settles those owed, each told, and the
    // next read counts carol's entry, the bound dropping bob's mark.
    const MAX_LOST_OWED: u64 = 64;
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let mut shop = w.shop().await;
    Box::pin(bound_drops_bob_at_the_next_receive_step(
        &w,
        &mut wallet,
        &mut shop,
        None,
    ))
    .await;
    let other = [9u8; 32];
    let writes: Vec<_> = (0..MAX_LOST_OWED)
        .map(|i| {
            let msg_id = format!("6f1c2b8e-3d4a-4f5b-9c6d-{i:012x}");
            let value = serde_json::json!({
                "v": 1,
                "d": {
                    "sender": pubky_switch_core::enc::b64(&other),
                    "earliest_ms": i,
                    "latest_ms": i,
                    "raised_ms": i,
                    "msg_id": msg_id,
                    "hash": pubky_switch_core::enc::b64(&[3; 32]),
                }
            });
            pubky_switch_client::app::LocalWrite::Put {
                key: format!("bl/{}/{msg_id}", pubky_switch_core::enc::b64(&other)).into_bytes(),
                value: serde_json::to_vec(&value).unwrap(),
            }
        })
        .collect();
    wallet
        .app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
    Box::pin(wallet.receive(None)).await.unwrap();
    let senders = crate::common::mark_senders(&wallet, "bd/").await;
    assert!(senders.contains(&w.bob_id()));
    assert!(!senders.contains(&w.carol_id()));
    assert_eq!(lost_for(&wallet, &other).await, 64);
    assert_eq!(lost_owed(&wallet).await, 0);
    w.clock.advance_ms(11 * MINUTE_MS);
    Box::pin(wallet.receive(None)).await.unwrap();
    bob_dropped_for_carol(&w, &wallet).await;
    assert_eq!(lost_for(&wallet, &w.bob_id()).await, 0);
    assert_eq!(lost_owed(&wallet).await, 0);
}

#[tokio::test]
async fn test_blocked_return_kept_floor_raise_holds_the_restore_verdict() {
    // The restore's raise of the block floor goes unanswered and is kept, the batch carrying
    // the restore's hold as it was. Reconcile mode can't end while it is kept, so no verdict
    // is decided that the kept batch could later write back over.
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let _shop = w.shop().await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    // Every batch of this receive goes unanswered, the floor's raise among them.
    for _ in 0..4 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unanswered {
                path: "/ops".into(),
                after: 0,
            });
    }
    let _ = restored.receive(None).await;
    let before = verdict(&restored).await;
    restored.reconcile().await.unwrap();
    restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .expect_err("reconcile mode waits for the kept raise");
    assert_eq!(verdict(&restored).await, before);
}

#[tokio::test]
async fn test_blocked_return_kept_floor_batch_holds_an_open_restore_verdict() {
    // The state set up directly: a restored wallet whose restore verdict is still open while
    // a floor batch is kept. A floor batch carries the hold as it was, so a verdict decided
    // while it is kept could be written back over when it lands. Neither reconcile, which
    // takes the verdict before it writes, nor ending reconcile mode decides it while the
    // batch is kept; once it lands, the verdict is decided (Paykit spec 7.2, 8.1).
    let w = World::new();
    let mut wallet = contacts_wallet(&w, store()).await;
    let _shop = w.shop().await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * MINUTE_MS);
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    crate::common::restore_with_unknown_cursor(&mut restored).await;
    restored.receive(None).await.unwrap();
    // A batch the client keeps: a write to the deny list that can't be sent.
    for _ in 0..2 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            });
    }
    let kept = restored
        .app_mut()
        .deny(&[7; 32])
        .await
        .expect_err("the deny list's write can't be sent")
        .kept_batch()
        .expect("the client keeps the write");
    // That batch stands in for a kept floor batch, and the verdict is still open.
    let raw = restored
        .app()
        .protocol_get("paykit", b"rh")
        .await
        .unwrap()
        .unwrap();
    let mut hold: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    // An open hold as a restore holds it before its verdict: its own widening still owed,
    // and no re-send asked yet.
    hold["d"]["verdict"] = serde_json::json!("open");
    hold["d"]["asks_queued"] = serde_json::json!(false);
    hold["d"]["asked"] = serde_json::json!([]);
    if hold["d"]["widening_from_ms"].is_null() {
        hold["d"]["widening_from_ms"] = serde_json::json!(0);
        hold["d"]["widening_read"] = serde_json::Value::Null;
    }
    restored
        .app_mut()
        .protocol_commit(
            "paykit",
            &[
                pubky_switch_client::app::LocalWrite::Put {
                    key: b"rh".to_vec(),
                    value: serde_json::to_vec(&hold).unwrap(),
                },
                pubky_switch_client::app::LocalWrite::Put {
                    key: b"fb".to_vec(),
                    value: kept.to_vec(),
                },
            ],
        )
        .await
        .unwrap();
    // The kept batch can't be sent again meanwhile.
    for _ in 0..4 {
        w.alice
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Unreachable {
                path: "/ops".into(),
                after: 0,
            });
    }
    let _ = restored.reconcile().await;
    assert_eq!(
        verdict(&restored).await,
        serde_json::json!("open"),
        "reconcile takes no verdict while the floor batch is kept"
    );
    let refused = restored
        .end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .expect_err("reconcile mode waits for the kept floor batch");
    assert!(
        matches!(refused.refusal(), Some(paykit_sdk::Refusal::NotNow(why)) if why.contains("floor")),
        "{refused:?}"
    );
    assert_eq!(verdict(&restored).await, serde_json::json!("open"));
    // Once the batch lands, the verdict is decided. Faults left unused keep it a few reads
    // more.
    for _ in 0..6 {
        let _ = restored.receive(None).await;
        let _ = restored.reconcile().await;
        if verdict(&restored).await != serde_json::json!("open") {
            break;
        }
    }
    assert!(
        restored
            .app()
            .protocol_get("paykit", b"fb")
            .await
            .unwrap()
            .is_none(),
        "the floor batch landed"
    );
    assert_ne!(verdict(&restored).await, serde_json::json!("open"));
}

/// The restore's verdict as Paykit keeps it (`rh`).
async fn verdict(app: &Paykit) -> serde_json::Value {
    let raw = app
        .app()
        .protocol_get("paykit", b"rh")
        .await
        .unwrap()
        .unwrap();
    serde_json::from_slice::<serde_json::Value>(&raw).unwrap()["d"]["verdict"].clone()
}
