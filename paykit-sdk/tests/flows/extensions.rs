//! Extension kinds (Paykit spec 3.3, Paykit wire 4.3): registered kinds accepted and kept
//! for the host until it takes them, gated by the deny list both ways.

use std::sync::Arc;

use paykit_lib::kinds::ExtensionKind;
use paykit_lib::scopes::ExtensionPrefix;
use paykit_sdk::{AppScopes, Paykit, PaykitConfig, Rail, Refusal, LISTS_REFRESH_MS};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::StateStore;
use serde_json::{json, Map, Value};

use crate::common::{store, ListsStore, World};

fn offer() -> ExtensionKind {
    ExtensionKind::new("paykit-cap.offer").unwrap()
}

fn prefix() -> ExtensionPrefix {
    ExtensionPrefix::parse("paykit-cap").unwrap()
}

fn body(n: u64) -> Map<String, Value> {
    json!({ "price": "1.5", "n": n })
        .as_object()
        .cloned()
        .unwrap()
}

fn handling() -> PaykitConfig {
    PaykitConfig::default().with_extension_kinds(vec![offer()])
}

/// Bob's shop holding `paykit_request`, `paykit_contacts` and `paykit_ext:paykit-cap`, on `store`, with `config`,
/// taking alice's messages.
async fn shop_on(w: &World, config: PaykitConfig, store: Arc<dyn StateStore>) -> Paykit {
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .extension(&prefix())
                .build()
                .unwrap(),
            config,
            store,
        )
        .await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    shop
}

/// Alice's app holding `paykit_pay`, `paykit_contacts` and `paykit_ext:paykit-cap`.
async fn sender(w: &World) -> Paykit {
    sender_on(w, PaykitConfig::default(), store()).await
}

async fn sender_on(w: &World, config: PaykitConfig, storage: Arc<dyn StateStore>) -> Paykit {
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new()
                .pay()
                .manage_contacts()
                .extension(&prefix())
                .build()
                .unwrap(),
            config,
            storage,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    wallet
}

/// The kinds `app`'s record accepts.
fn accepts(app: &Paykit, identity: [u8; 32]) -> Vec<String> {
    pubky_switch_core::records::AppRecord::verify(app.app().record(), &identity)
        .unwrap()
        .body()
        .accepts
        .clone()
}

/// The next `n` reads of `who`'s lists fail.
fn fail_list_reads(signer: &pubky_switch_client::testing::TestSigner, n: usize) {
    for _ in 0..n {
        signer
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/allow".into(),
                code: pubky_switch_core::http::ErrorCode::Unavailable,
                current: serde_json::Value::Null,
            });
    }
}

#[tokio::test]
async fn test_extensions_registered_kind_kept_until_taken() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut shop = shop_on(&w, handling(), lists.clone()).await;
    let mut wallet = sender(&w).await;
    // This shop does not pay. Its mailbox needs no access to a payer's pending unblocks.
    lists.pending_unblocks_unreadable(true);
    let msg_id = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let kept = shop.extension_messages().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].kind, "paykit-cap.offer");
    assert_eq!(kept[0].msg_id, msg_id);
    assert_eq!(kept[0].sender, w.alice_id());
    assert_eq!(kept[0].body, body(1));
    // Kept across receives until the host takes it.
    shop.receive(None).await.unwrap();
    assert_eq!(shop.extension_messages().await.unwrap().len(), 1);
    shop.acknowledge_extension_message(&w.alice_id(), &msg_id)
        .await
        .unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_payer_receives_after_the_unblock_floor_can_be_checked() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut payer = sender_on(&w, handling(), lists.clone()).await;
    let mut shop = shop_on(&w, handling(), store()).await;
    payer.block(&w.bob_id()).await.unwrap();
    payer.unblock(&w.bob_id()).await.unwrap();
    let id = shop
        .send_extension(w.alice_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();

    // Isolate delivery from the SDK's after-read work on the same floor.
    lists.pending_unblocks_unreadable(true);
    payer.app_mut().receive(None).await.unwrap();
    assert!(payer.extension_messages().await.unwrap().is_empty());
    lists.pending_unblocks_unreadable(false);
    payer.receive(None).await.unwrap();
    let kept = payer.extension_messages().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].msg_id, id);
}

#[tokio::test]
async fn test_extensions_host_loaded_wallet_delivers_and_remembers_acknowledgment() {
    use paykit_sdk::{App, PaykitError};
    use pubky_switch_client::app::{AppError, Refusal as ClientRefusal};

    let w = World::new();
    let storage = store();
    let shop = shop_on(&w, handling(), Arc::clone(&storage)).await;
    let app_id = shop.app_id();
    drop(shop);

    // The saved App can still be opened with the intended registration after a mismatch.
    let config = handling();
    let protocol = PaykitConfig::default().protocol(Arc::clone(&w.rail) as Arc<dyn Rail>);
    let app = App::load(
        w.env(&w.bob, Arc::clone(&storage)),
        Arc::clone(&protocol),
        config.app.clone(),
        app_id,
    )
    .await
    .unwrap();
    assert!(matches!(
        Paykit::attach(app, config.clone(), protocol, w.timer()).await,
        Err(PaykitError::Client(AppError::Refused(
            ClientRefusal::Usage(_)
        )))
    ));

    let protocol = config.protocol(Arc::clone(&w.rail) as Arc<dyn Rail>);
    let app = App::load(
        w.env(&w.bob, Arc::clone(&storage)),
        Arc::clone(&protocol),
        config.app.clone(),
        app_id,
    )
    .await
    .unwrap();
    let mut shop = Paykit::attach(app, config, protocol, w.timer())
        .await
        .unwrap();
    let mut wallet = sender(&w).await;
    let id = wallet
        .send_extension(w.bob_id(), &offer(), vec![[7; 32]], body(1))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let kept = shop.extension_messages().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].sender, wallet.identity());
    assert_eq!(kept[0].sender_app, wallet.app_id());
    assert_eq!(kept[0].msg_id, id);
    assert_eq!(kept[0].refs, vec![pubky_switch_core::enc::b64(&[7; 32])]);
    assert_eq!(kept[0].body, body(1));
    shop.acknowledge_extension_message(&wallet.identity(), &id)
        .await
        .unwrap();
    drop(shop);

    let mut shop = w
        .load_with(&w.bob, storage, app_id, Arc::clone(&w.clock), handling())
        .await;
    wallet.app_mut().resend(&id).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_kind_not_registered_neither_accepted_nor_kept() {
    let w = World::new();
    let mut shop = shop_on(&w, PaykitConfig::default(), store()).await;
    assert!(!accepts(&shop, w.bob_id()).contains(&"paykit-cap.offer".to_owned()));
    let mut wallet = sender(&w).await;
    // No app of bob's accepts it: nothing reaches him, whatever the send answers.
    let _ = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await;
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_record_accepts_registered_kinds_its_scopes_hold() {
    let w = World::new();
    let shop = shop_on(&w, handling(), store()).await;
    assert!(accepts(&shop, w.bob_id()).contains(&"paykit-cap.offer".to_owned()));
    // Registered, but without `paykit_ext:paykit-cap`: not accepted.
    let till = w
        .join_with(
            &w.bob,
            "till",
            AppScopes::new().request().build().unwrap(),
            handling(),
            store(),
        )
        .await;
    assert!(!accepts(&till, w.bob_id()).contains(&"paykit-cap.offer".to_owned()));
}

#[tokio::test]
async fn test_extensions_accepts_follow_a_new_registration() {
    let w = World::new();
    let kept = store();
    let shop = shop_on(&w, PaykitConfig::default(), Arc::clone(&kept)).await;
    let app_id = shop.app_id();
    drop(shop);
    let mut shop = w
        .load_with(&w.bob, kept, app_id, Arc::clone(&w.clock), handling())
        .await;
    shop.receive(None).await.unwrap();
    assert!(accepts(&shop, w.bob_id()).contains(&"paykit-cap.offer".to_owned()));
    let mut wallet = sender(&w).await;
    wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    assert_eq!(shop.extension_messages().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_extensions_message_from_a_blocked_sender_not_kept() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    // Accepted by bob's inbox, then alice is blocked before his shop reads it.
    shop.app_mut().deny(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
    assert_eq!(shop.spam_count(&w.alice_id()).await.unwrap(), 1);
}

#[tokio::test]
async fn test_extensions_message_held_while_the_deny_copy_is_unknown() {
    let w = World::new();
    let lists = Arc::new(ListsStore::default());
    let mut shop = shop_on(&w, handling(), Arc::clone(&lists) as Arc<dyn StateStore>).await;
    let mut wallet = sender(&w).await;
    wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    // The shop can't tell alice's standing: the entry waits, neither kept nor counted.
    lists.lists_unknown(true);
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
    assert_eq!(shop.spam_count(&w.alice_id()).await.unwrap(), 0);
    // The lists are read: it is kept.
    lists.lists_unknown(false);
    shop.receive(None).await.unwrap();
    assert_eq!(shop.extension_messages().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_extensions_bounded_per_sender_oldest_dropped_and_asked_again() {
    // Past one sender's bound its oldest kept message goes as one never processed, and its
    // sender is asked for it again: the feed goes on, the newest kept.
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    let max = paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER as u64;
    for n in 0..=max + 1 {
        wallet
            .send_extension(w.bob_id(), &offer(), Vec::new(), body(n))
            .await
            .unwrap();
    }
    shop.receive(None).await.unwrap();
    let kept = shop.extension_messages().await.unwrap();
    assert_eq!(kept.len() as u64, max);
    assert!(kept.iter().any(|m| m.body == body(max + 1)));
    assert!(!kept.iter().any(|m| m.body == body(0) || m.body == body(1)));
    let mark = format!("pd/{}", pubky_switch_core::enc::b64(&w.alice_id()));
    assert!(shop
        .app()
        .protocol_get("paykit", mark.as_bytes())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_extensions_wait_for_room_to_report_an_eviction() {
    use crate::replies::{keep_loss_fixture, loss_notice};
    use pubky_switch_client::app::{AppEvent, ParkReason};

    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    for n in 0..paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER {
        wallet
            .send_extension(w.bob_id(), &offer(), Vec::new(), body(n as u64))
            .await
            .unwrap();
    }
    shop.receive(None).await.unwrap();
    let before = shop.extension_messages().await.unwrap();
    assert_eq!(before.len(), paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER);
    crate::common::seed_marks(
        &mut shop,
        "pd/",
        paykit_sdk::MAX_DROPPED_MARKS,
        w.clock.now_ms() - 1000,
    )
    .await;
    let peer = w.carol_id();
    let notices = (1..=64).map(|n| loss_notice(&peer, n, n)).collect();
    keep_loss_fixture(&mut shop, notices).await;

    let id = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1000))
        .await
        .unwrap();
    let read = shop.app_mut().receive(None).await.unwrap();
    assert!(read.events.iter().any(|e| matches!(
        e,
        AppEvent::Parked {
            reason: ParkReason::AwaitingAdmission,
            ..
        }
    )));
    assert_eq!(shop.extension_messages().await.unwrap(), before);
    let acknowledged = shop
        .notices()
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.counterparty == Some(peer))
        .unwrap();
    shop.acknowledge_notice(&acknowledged.id).await.unwrap();
    shop.receive(None).await.unwrap();
    let after = shop.extension_messages().await.unwrap();
    assert_eq!(after.len(), before.len());
    assert!(after.iter().any(|message| message.msg_id == id));
}

#[tokio::test]
async fn test_extensions_stored_copies_do_not_prevent_eviction_or_restore() {
    Box::pin(async {
        let w = World::new();
        let lists = Arc::new(ListsStore::default());
        let mut config = handling();
        config.app.park_after_ms = 0;
        let mut shop = shop_on(&w, config, Arc::clone(&lists) as Arc<dyn StateStore>).await;
        let mut wallet = sender(&w).await;
        lists.lists_unknown(true);
        let msg_id = wallet
            .send_extension(w.bob_id(), &offer(), Vec::new(), body(0))
            .await
            .unwrap();
        let count = pubky_switch_client::app::PARKED_RETRIES_PER_READ + 1;
        for _ in 1..count {
            w.clock.advance_ms(1);
            wallet.app_mut().resend(&msg_id).await.unwrap();
        }
        shop.receive(None).await.unwrap();
        assert!(shop.extension_messages().await.unwrap().is_empty());
        assert_eq!(shop.app_mut().parked_entries().await.unwrap().len(), count);

        lists.lists_unknown(false);
        for n in 1..=paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER {
            w.clock.advance_ms(1);
            wallet
                .send_extension(w.bob_id(), &offer(), Vec::new(), body(n as u64))
                .await
                .unwrap();
        }
        shop.receive(None).await.unwrap();
        let kept = shop.extension_messages().await.unwrap();
        assert_eq!(kept.len(), paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER);
        assert!(kept.iter().all(|message| message.msg_id != msg_id));
        assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
        let key = format!("pd/{}", pubky_switch_core::enc::b64(&w.alice_id()));
        assert!(shop
            .app()
            .protocol_get("paykit", key.as_bytes())
            .await
            .unwrap()
            .is_some());

        let backup = shop.backup().await.unwrap();
        drop(shop);
        let restored = Paykit::restore(
            w.env(&w.bob, store()),
            handling(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        assert_eq!(restored.extension_messages().await.unwrap(), kept);
    })
    .await;
}

#[tokio::test]
async fn test_extensions_earlier_stored_input_is_evicted_and_can_return_after_restore() {
    Box::pin(async {
        let w = World::new();
        let lists = Arc::new(ListsStore::default());
        let mut config = handling();
        config.app.park_after_ms = 0;
        let mut shop = shop_on(&w, config, Arc::clone(&lists) as Arc<dyn StateStore>).await;
        let mut wallet = sender(&w).await;
        let count = pubky_switch_client::app::PARKED_RETRIES_PER_READ + 1;
        let mut oldest_id = String::new();
        let mut oldest_at = 0;
        lists.lists_unknown(true);
        for n in 0..count {
            oldest_at = w.clock.now_ms();
            oldest_id = wallet
                .send_extension(w.bob_id(), &offer(), Vec::new(), body(n as u64))
                .await
                .unwrap();
            w.clock.advance_ms(1);
        }
        shop.receive(None).await.unwrap();
        assert!(shop.extension_messages().await.unwrap().is_empty());
        assert_eq!(shop.app_mut().parked_entries().await.unwrap().len(), count);

        // One older entry stays stored while this read fills the pool with newer input.
        lists.lists_unknown(false);
        for n in 0..paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER {
            wallet
                .send_extension(w.bob_id(), &offer(), Vec::new(), body(1000 + n as u64))
                .await
                .unwrap();
            w.clock.advance_ms(1);
        }
        shop.receive(None).await.unwrap();
        let newer = shop.extension_messages().await.unwrap();
        assert_eq!(newer.len(), paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER);
        assert!(newer
            .iter()
            .all(|message| message.body["n"].as_u64().unwrap() >= 1000));
        assert_eq!(shop.app_mut().parked_entries().await.unwrap().len(), 1);

        // Retrying that input must evict the input itself, with its own acceptance time.
        shop.receive(None).await.unwrap();
        assert_eq!(shop.extension_messages().await.unwrap(), newer);
        assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
        let key = format!("pd/{}", pubky_switch_core::enc::b64(&w.alice_id()));
        let bytes = shop
            .app()
            .protocol_get("paykit", key.as_bytes())
            .await
            .unwrap()
            .unwrap();
        let mark: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(mark["d"]["latest_ms"].as_u64(), Some(oldest_at));
        assert_eq!(mark["d"]["due"], true);

        let backup = shop.backup().await.unwrap();
        drop(shop);
        let mut restored = Paykit::restore(
            w.env(&w.bob, store()),
            handling(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        assert_eq!(restored.extension_messages().await.unwrap(), newer);
        restored
            .acknowledge_extension_message(&w.alice_id(), &newer[0].msg_id)
            .await
            .unwrap();
        wallet.app_mut().resend(&oldest_id).await.unwrap();
        restored.receive(None).await.unwrap();
        let kept = restored.extension_messages().await.unwrap();
        assert_eq!(kept.len(), paykit_sdk::MAX_EXTENSION_MESSAGES_PER_SENDER);
        assert!(kept.iter().any(|message| message.msg_id == oldest_id));
    })
    .await;
}

#[tokio::test]
async fn test_extensions_restored_sender_waits_for_its_first_feed_read() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    let backup = wallet.backup().await.unwrap();
    let wallet_id = wallet.app_id();
    drop(wallet);
    let storage = store();
    let mut restored = Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/feed".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    assert!(restored.receive(None).await.is_err());
    assert!(restored.app().restore_held_cursor().is_none());
    // The contact-list read completed even though the feed was unavailable.
    assert!(restored
        .app()
        .protocol_get("paykit", b"lr")
        .await
        .unwrap()
        .is_some());
    drop(restored);
    let reads = Arc::new(crate::common::ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut env = w.env(&w.alice, storage);
    env.homeserver = Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>;
    let mut restored = Paykit::load(
        env,
        PaykitConfig::default(),
        wallet_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let before = restored.app().queued_messages("paykit").await.unwrap();
    reads.then(w.alice_id(), 0, true);
    let error = restored
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap_err();
    assert!(matches!(
        error.refusal(),
        Some(Refusal::IdentityUnavailable(_))
    ));
    assert_eq!(
        restored.app().queued_messages("paykit").await.unwrap(),
        before
    );
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());

    // The next send can take the first page itself. The host still gets that read's summary.
    let id = restored
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    let summary = restored.receive(None).await.unwrap();
    assert!(summary.report.held_cursor.is_some());
    shop.receive(None).await.unwrap();
    let messages = shop.extension_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].msg_id, id);
    assert_eq!(messages[0].body, body(1));
    shop.acknowledge_extension_message(&restored.identity(), &id)
        .await
        .unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_send_returns_the_queued_id_after_a_local_delivery_error() {
    use pubky_switch_client::store::{Fault, FaultStore};

    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let saved = Arc::new(FaultStore::new(store(), Fault::None));
    let mut wallet = sender_on(
        &w,
        PaykitConfig::default(),
        Arc::clone(&saved) as Arc<dyn StateStore>,
    )
    .await;
    let app_id = wallet.app_id();
    // The outbox commit succeeds, then the attempt to record the delivery fails.
    saved.fail_put_once(b"sent/");
    let id = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    drop(wallet);
    let mut wallet = w
        .load_with(
            &w.alice,
            saved.inner(),
            app_id,
            Arc::clone(&w.clock),
            PaykitConfig::default(),
        )
        .await;
    assert!(matches!(
        wallet.message_delivery(&id).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::Queued { .. })
    ));
    wallet.flush().await.unwrap();
    assert!(matches!(
        wallet.message_delivery(&id).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::Accepted { .. })
    ));
    shop.receive(None).await.unwrap();
    let kept = shop.extension_messages().await.unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].msg_id, id);
    shop.acknowledge_extension_message(&wallet.identity(), &id)
        .await
        .unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_send_refused_while_sends_are_held() {
    let w = World::new();
    let _shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    wallet.block(&w.bob_id()).await.unwrap();
    let refused = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    wallet.unblock(&w.bob_id()).await.unwrap();
    // The copy isn't known after a receive whose reads of the lists fail: still refused.
    w.clock.advance_ms(LISTS_REFRESH_MS);
    fail_list_reads(&w.alice, 3);
    wallet.receive(None).await.unwrap();
    let refused = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap_err();
    assert!(
        matches!(refused.refusal(), Some(Refusal::NotNow(_))),
        "{refused:?}"
    );
    // Read again, it goes.
    wallet.receive(None).await.unwrap();
    wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_extensions_send_requires_the_extension_scope_and_contacts() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let wallet = w.wallet("wallet").await;
    let extension = w
        .join_with(
            &w.alice,
            "extension",
            AppScopes::new().extension(&prefix()).build().unwrap(),
            handling(),
            store(),
        )
        .await;
    for (mut app, missing) in [
        (wallet, "paykit_ext:paykit-cap"),
        (extension, "paykit_contacts"),
    ] {
        let before = app.app().queued_messages("paykit").await.unwrap();
        let refused = app
            .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
            .await
            .unwrap_err();
        assert!(
            matches!(refused.refusal(), Some(Refusal::ScopeMissing(scope)) if scope == missing),
            "{refused:?}"
        );
        assert_eq!(app.app().queued_messages("paykit").await.unwrap(), before);
    }
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_queued_delivery_checks_the_block_and_the_current_certificate() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/inbox".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    let id = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(1))
        .await
        .unwrap();
    async fn eligible(wallet: &Paykit, id: &str) -> bool {
        wallet
            .app()
            .queued_messages("paykit")
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.msg_id == id)
            .expect("queued extension")
            .may_send
    }
    assert!(eligible(&wallet, &id).await);
    wallet.block(&w.bob_id()).await.unwrap();
    assert!(!eligible(&wallet, &id).await);
    let me = wallet.app_id();
    let old = wallet.app().cert().clone();
    let reduced = crate::certificate::renew(
        &w,
        me,
        &AppScopes::new().pay().manage_contacts().build().unwrap(),
        1_000,
    );
    crate::certificate::list(&w, me, &old, &reduced);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.app().cert().hash(), reduced.hash());
    wallet.unblock(&w.bob_id()).await.unwrap();
    assert!(!eligible(&wallet, &id).await);
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(shop.extension_messages().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_extensions_accept_decimals_only_inside_metadata() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    let mut body = body(1);
    body.insert("price".into(), json!(1.5));
    let error = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::Usage(_))),
        "{error:?}"
    );
    body.remove("price");
    body.insert("metadata".into(), json!({"price": 1.5}));
    let msg_id = wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body.clone())
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let messages = shop.extension_messages().await.unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].msg_id, msg_id);
    assert_eq!(messages[0].body, body);
}

#[tokio::test]
async fn test_extensions_kept_messages_carried_through_a_restore() {
    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    let refs = vec![[7; 32], [8; 32]];
    let id = wallet
        .send_extension(w.bob_id(), &offer(), refs.clone(), body(1))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let original = shop.extension_messages().await.unwrap();
    assert_eq!(original.len(), 1);
    assert_eq!(original[0].sender, w.alice_id());
    assert_eq!(original[0].sender_app, wallet.app_id());
    assert_eq!(original[0].kind, offer().as_string());
    assert_eq!(original[0].msg_id, id);
    assert_eq!(
        original[0].refs,
        refs.iter()
            .map(|r| pubky_switch_core::enc::b64(r))
            .collect::<Vec<_>>()
    );
    assert_eq!(original[0].body, body(1));
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let restored = Paykit::restore(
        w.env(&w.bob, store()),
        handling(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.extension_messages().await.unwrap(), original);
}

#[tokio::test]
async fn test_extensions_unverifiable_mailbox_entry_refused_on_read_and_restore() {
    use pubky_switch_client::app::LocalWrite;

    let w = World::new();
    let mut shop = shop_on(&w, handling(), store()).await;
    let mut wallet = sender(&w).await;
    wallet
        .send_extension(w.bob_id(), &offer(), Vec::new(), body(2))
        .await
        .unwrap();
    shop.receive(None).await.unwrap();
    let backup = shop.backup().await.unwrap();
    let clean: Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let entries = shop.app().protocol_scan("paykit", b"xm/").await.unwrap();
    let [(key, bytes)] = entries.as_slice() else {
        panic!("expected one extension")
    };
    let mut unreadable: Value = serde_json::from_slice(bytes).unwrap();
    unreadable["d"]["message"]["plaintext"] = json!("");
    let write = LocalWrite::Put {
        key: key.clone(),
        value: serde_json::to_vec(&unreadable).unwrap(),
    };
    shop.app_mut()
        .protocol_commit("paykit", &[write])
        .await
        .unwrap();
    assert!(shop.extension_messages().await.is_err());
    assert!(shop.backup().await.is_err());
    drop(shop);

    let mut layer = crate::restore::layer_of(&clean);
    let (_, value) = layer.iter_mut().find(|(k, _)| k == key).unwrap();
    *value = unreadable;
    let incomplete = crate::restore::with_layer(&clean, layer);
    let backup =
        paykit_sdk::PaykitBackup::from_bytes(&serde_json::to_vec(&incomplete).unwrap()).unwrap();
    let restored = Box::pin(Paykit::restore(
        w.env(&w.bob, store()),
        handling(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await;
    assert!(matches!(restored, Err(error) if error.to_string().contains("kept extension message")));
}
