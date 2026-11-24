//! Entries the client stores for good, as Paykit handles them (Switch spec 8.2): an effect
//! the switch refuses is written later by the app, an entry dropped unverified is asked for
//! again, and the SDK acknowledges the duty events it took.

use std::sync::Arc;

use paykit_sdk::{AppScopes, PayOptions, PayeeStatus, Paykit, PaykitConfig, Rail, RequestStatus};
use pubky_switch_client::app::AppEvent;
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::claims::Outcome;
use pubky_switch_core::http::ErrorCode;

use crate::common::{
    asked, btc, draft, endpoint, list_version, mark_senders, refuse_writes, request_from,
    seed_marks, store, World,
};
use pubky_switch_core::records::DAY_MS;

#[derive(Clone, Copy)]
enum MarkDamage {
    Unknown,
    MissingAsked,
    MissingQueued,
    DuplicateTime,
    UnknownQueued,
}

async fn wallet_with_drop_mark(
    w: &World,
    key_wait: bool,
) -> (
    Paykit,
    Paykit,
    Arc<dyn pubky_switch_client::store::StateStore>,
    paykit_sdk::RequestRef,
) {
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let mut shop = w.shop().await;
    let request = if key_wait {
        let (request, _, at) = waiting_for_the_current_key(w, &mut wallet, &mut shop).await;
        past_the_key_wait(w, &mut wallet, at).await;
        request
    } else {
        dropped_unverified(w, &mut wallet, &mut shop).await.0
    };
    let prefix = if key_wait { b"kw/" } else { b"ud/" };
    let marks = wallet.app().protocol_scan("paykit", prefix).await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    let body = mark["d"].as_object().unwrap();
    assert!(body.contains_key("asked_at_ms"));
    assert!(body.contains_key("queued"));
    assert!(!body.contains_key("pending_forget"));
    assert!(!body.contains_key("speculative"));
    assert!(!body.contains_key("late_told"));
    assert_eq!(body.contains_key("epochs"), key_wait);
    assert!(!body.contains_key("mark"));
    (wallet, shop, storage, request)
}

fn damage_mark(bytes: &[u8], damage: MarkDamage) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let body = value["d"].as_object_mut().unwrap();
    match damage {
        MarkDamage::Unknown => {
            body.insert("extra".into(), serde_json::json!({"nested": [7]}));
        }
        MarkDamage::MissingAsked => {
            body.remove("asked_at_ms").unwrap();
        }
        MarkDamage::MissingQueued => {
            body.remove("queued").unwrap();
        }
        MarkDamage::DuplicateTime => {
            let at = body["earliest_ms"].as_u64().unwrap();
            return String::from_utf8(bytes.to_vec())
                .unwrap()
                .replacen("\"d\":{", &format!("\"d\":{{\"earliest_ms\":{at},"), 1)
                .into_bytes();
        }
        MarkDamage::UnknownQueued => {
            body.get_mut("queued")
                .unwrap()
                .as_object_mut()
                .expect("the real sender retained its queued request")
                .insert("extra".into(), serde_json::json!({"nested": [7]}));
        }
    }
    serde_json::to_vec(&value).unwrap()
}

async fn refuse_mark_backup(w: &World, wallet: &mut Paykit, key: &[u8], bad: Vec<u8>) {
    use pubky_switch_client::app::AppError;
    use pubky_switch_client::store::{Fault as StoreFault, FaultStore, StateStore};
    let app_id = wallet.app_id();
    let mut backup: serde_json::Value =
        serde_json::from_slice(&wallet.backup().await.unwrap().to_bytes().unwrap()).unwrap();
    let layer =
        pubky_switch_core::enc::unb64(backup["layers"][0]["data"].as_str().unwrap()).unwrap();
    let mut layer = pubky_switch_client::app::decode_local_state(&layer).unwrap();
    layer.iter_mut().find(|(k, _)| k == key).unwrap().1 = bad;
    backup["layers"][0]["data"] = serde_json::Value::String(pubky_switch_core::enc::b64(
        &pubky_switch_client::app::encode_local_state(&layer),
    ));
    let backup =
        paykit_sdk::PaykitBackup::from_bytes(&serde_json::to_vec(&backup).unwrap()).unwrap();
    let target = Arc::new(FaultStore::new(store(), StoreFault::None));
    let result = Box::pin(Paykit::restore(
        w.env(&w.alice, Arc::clone(&target) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await;
    let Err(error) = result else {
        panic!("malformed recovery state was restored")
    };
    assert!(matches!(
        error,
        paykit_sdk::PaykitError::Client(AppError::InvalidData(_))
    ));
    assert_eq!(target.commits(), 0);
    assert!(Paykit::load(
        w.env(&w.alice, target),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer()
    )
    .await
    .is_err());
}

async fn refuse_mark_state(key_wait: bool, restore: bool, damage: MarkDamage) {
    use pubky_switch_client::app::LocalWrite;
    let w = World::new();
    let (mut wallet, _, storage, _) = Box::pin(wallet_with_drop_mark(&w, key_wait)).await;
    let prefix = if key_wait { b"kw/" } else { b"ud/" };
    let (key, good) = wallet
        .app()
        .protocol_scan("paykit", prefix)
        .await
        .unwrap()
        .remove(0);
    let bad = damage_mark(&good, damage);
    assert_ne!(bad, good);
    if restore {
        refuse_mark_backup(&w, &mut wallet, &key, bad).await;
    } else {
        wallet
            .app_mut()
            .protocol_commit(
                "paykit",
                &[LocalWrite::Put {
                    key: key.clone(),
                    value: bad.clone(),
                }],
            )
            .await
            .unwrap();
        let app_id = wallet.app_id();
        let before = asked(&wallet, &w.bob_id()).await;
        // Load already reads drop marks to clamp restored times; it must reject before
        // rewriting the malformed value (Paykit spec 7.2, "What a restore loses").
        let result = Paykit::load(
            w.env(&w.alice, storage),
            PaykitConfig::default(),
            app_id,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await;
        let Err(error) = result else {
            panic!("a malformed drop mark was loaded")
        };
        assert!(crate::common::is_corrupt(&error), "{error:?}");
        assert_eq!(
            wallet.app().protocol_get("paykit", &key).await.unwrap(),
            Some(bad)
        );
        assert_eq!(asked(&wallet, &w.bob_id()).await, before);
    }
}

#[tokio::test]
async fn test_mark_state_drop_unknown_after_reload() {
    refuse_mark_state(false, false, MarkDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_drop_unknown_at_restore() {
    refuse_mark_state(false, true, MarkDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_key_wait_unknown_after_reload() {
    refuse_mark_state(true, false, MarkDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_key_wait_unknown_at_restore() {
    refuse_mark_state(true, true, MarkDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_drop_missing_asked_after_reload() {
    refuse_mark_state(false, false, MarkDamage::MissingAsked).await;
}

#[tokio::test]
async fn test_mark_state_drop_missing_queued_at_restore() {
    refuse_mark_state(false, true, MarkDamage::MissingQueued).await;
}

#[tokio::test]
async fn test_mark_state_key_wait_missing_asked_after_reload() {
    refuse_mark_state(true, false, MarkDamage::MissingAsked).await;
}

#[tokio::test]
async fn test_mark_state_key_wait_missing_queued_at_restore() {
    refuse_mark_state(true, true, MarkDamage::MissingQueued).await;
}

#[tokio::test]
async fn test_mark_state_drop_duplicate_after_reload() {
    refuse_mark_state(false, false, MarkDamage::DuplicateTime).await;
}

#[tokio::test]
async fn test_mark_state_key_wait_duplicate_at_restore() {
    refuse_mark_state(true, true, MarkDamage::DuplicateTime).await;
}

#[tokio::test]
async fn test_mark_state_queued_unknown_after_reload() {
    refuse_mark_state(false, false, MarkDamage::UnknownQueued).await;
}

#[tokio::test]
async fn test_mark_state_queued_unknown_at_restore() {
    refuse_mark_state(false, true, MarkDamage::UnknownQueued).await;
}

#[tokio::test]
async fn test_mark_state_produced_marks_reload_restore_and_recover() {
    for key_wait in [false, true] {
        let w = World::new();
        let (wallet, mut shop, storage, request) =
            Box::pin(wallet_with_drop_mark(&w, key_wait)).await;
        let prefix = if key_wait { b"kw/" } else { b"ud/" };
        let held = wallet.app().protocol_scan("paykit", prefix).await.unwrap();
        let app_id = wallet.app_id();
        drop(wallet);
        let mut wallet = w
            .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
            .await;
        assert_eq!(
            wallet.app().protocol_scan("paykit", prefix).await.unwrap(),
            held
        );
        let backup = wallet.backup().await.unwrap();
        let restored = Box::pin(Paykit::restore(
            w.env(&w.alice, store()),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        ))
        .await
        .unwrap();
        assert_eq!(
            restored
                .app()
                .protocol_scan("paykit", prefix)
                .await
                .unwrap(),
            held
        );
        drop(restored);
        w.homeserver.set_down(false);
        if key_wait {
            crate::unrequested::store_bundle(&w, wallet.app_id(), wallet.app().cert());
        }
        wallet.receive(None).await.unwrap();
        wallet.flush().await.unwrap();
        assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
        shop.receive(None).await.unwrap();
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.request(&request).await.unwrap().status,
            RequestStatus::Open
        );
    }
}

async fn wallet_with_kept_mark(
    w: &World,
) -> (Paykit, Arc<dyn pubky_switch_client::store::StateStore>) {
    let storage = store();
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&storage),
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.app_mut().deny(&w.bob_id()).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    wallet.unblock(&w.bob_id()).await.unwrap();
    w.clock.advance_ms(11 * 60_000);
    w.bob.switch().fail_next(Fault::Unreachable {
        path: "inbox".into(),
        after: 0,
    });
    wallet.receive(None).await.unwrap();
    let marks = wallet.app().protocol_scan("paykit", b"bd/").await.unwrap();
    assert_eq!(marks.len(), 1);
    let mark: serde_json::Value = serde_json::from_slice(&marks[0].1).unwrap();
    assert!(mark["d"]["queued"]["msg_id"].is_string());
    assert!(mark["d"]["queued"]["hash"].is_string());
    (wallet, storage)
}

#[tokio::test]
async fn test_mark_state_queued_request_reload_restore_and_send() {
    let w = World::new();
    let (wallet, storage) = Box::pin(wallet_with_kept_mark(&w)).await;
    let held = wallet.app().protocol_scan("paykit", b"bd/").await.unwrap();
    let app_id = wallet.app_id();
    drop(wallet);
    let mut wallet = w
        .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
        .await;
    assert_eq!(
        wallet.app().protocol_scan("paykit", b"bd/").await.unwrap(),
        held
    );
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut restored = Box::pin(Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    ))
    .await
    .unwrap();
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"bd/")
            .await
            .unwrap(),
        held
    );
    restored.receive(None).await.unwrap();
    restored.receive(None).await.unwrap();
    assert_eq!(asked(&restored, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_mark_state_kept_reference_unknown_at_restore() {
    let w = World::new();
    let (mut wallet, _) = Box::pin(wallet_with_kept_mark(&w)).await;
    let (key, good) = wallet
        .app()
        .protocol_scan("paykit", b"bd/")
        .await
        .unwrap()
        .remove(0);
    let bad = damage_mark(&good, MarkDamage::UnknownQueued);
    refuse_mark_backup(&w, &mut wallet, &key, bad).await;
}

#[tokio::test]
async fn test_mark_state_kept_reference_unknown_after_reload() {
    use pubky_switch_client::app::LocalWrite;
    let w = World::new();
    let (mut wallet, storage) = Box::pin(wallet_with_kept_mark(&w)).await;
    let (key, good) = wallet
        .app()
        .protocol_scan("paykit", b"bd/")
        .await
        .unwrap()
        .remove(0);
    let bad = damage_mark(&good, MarkDamage::UnknownQueued);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.clone(),
                value: bad.clone(),
            }],
        )
        .await
        .unwrap();
    let result = Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        wallet.app_id(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    let Err(error) = result else {
        panic!("a malformed kept request was loaded")
    };
    assert!(crate::common::is_corrupt(&error), "{error:?}");
    assert_eq!(
        wallet.app().protocol_get("paykit", &key).await.unwrap(),
        Some(bad)
    );
}

#[derive(Clone, Copy)]
enum AskDamage {
    Unknown,
    MissingUntil,
    MissingQueued,
    DuplicateUntil,
    UnknownReference,
}

#[derive(Default)]
struct FailAskDelete {
    inner: pubky_switch_client::store::MemoryStore,
    armed: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for FailAskDelete {
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
impl FailAskDelete {
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
        use pubky_switch_client::store::Write;
        if batch.writes().iter().any(
            |write| matches!(write, Write::Delete { key, .. } if key.starts_with(b"p/paykit/ask/")),
        ) && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        apply(batch)
    }
}

async fn wallet_with_queued_ask(
    w: &World,
    bounded: bool,
    with_request: bool,
) -> (Paykit, Arc<dyn pubky_switch_client::store::StateStore>) {
    let storage = Arc::new(FailAskDelete::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            AppScopes::new().pay().manage_contacts().build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&storage) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let now = w.clock.now_ms();
    // Feed items reach the public client callbacks; no local ask is inserted by the test
    // (Paykit spec 9; Switch wire 7.4.4).
    if bounded {
        let mut phone = w.wallet("departing").await;
        wallet.app_mut().receive(None).await.unwrap();
        w.clock.advance_ms(1_000);
        phone
            .app_mut()
            .leave(pubky_switch_client::app::LeaveMode::Immediate(
                pubky_switch_client::app::UserConfirmed::new(),
            ))
            .await
            .unwrap();
        let seq = w.alice.switch().push_unreadable(w.bob_id(), now);
        // Stop after the client has durably called the protocol, before Paykit sends its ask.
        let report = wallet.app_mut().receive(None).await.unwrap();
        assert!(
            report.events.iter().any(|event| matches!(event,
            AppEvent::Unreadable { seq: read, resend_asked: true } if *read == seq)),
            "{report:?}"
        );
        wallet.block(&w.bob_id()).await.unwrap();
    } else {
        wallet.block(&w.bob_id()).await.unwrap();
        w.alice.switch().push_missed(1, now - 1_000, now);
        wallet.receive(None).await.unwrap();
    }
    let asks = wallet.app().protocol_scan("paykit", b"ask/").await.unwrap();
    assert_eq!(asks.len(), 1);
    let ask: serde_json::Value = serde_json::from_slice(&asks[0].1).unwrap();
    assert_eq!(ask["d"]["until_ms"].is_u64(), bounded);
    assert!(ask["d"].as_object().unwrap().contains_key("until_ms"));
    assert!(ask["d"].as_object().unwrap().contains_key("queued"));
    assert!(ask["d"]["queued"].is_null());
    if with_request {
        wallet.unblock(&w.bob_id()).await.unwrap();
        storage
            .armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = wallet.receive(None).await.unwrap_err();
        assert!(matches!(
            error,
            paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(
                pubky_switch_client::error::StoreError::Unavailable
            ))
        ));
        assert!(!storage.armed.load(std::sync::atomic::Ordering::SeqCst));
        let asks = wallet.app().protocol_scan("paykit", b"ask/").await.unwrap();
        assert_eq!(asks.len(), 1);
        let ask: serde_json::Value = serde_json::from_slice(&asks[0].1).unwrap();
        let queued = ask["d"]["queued"].as_object().unwrap();
        assert!(queued["msg_id"].is_string());
        assert!(queued["hash"].is_string());
        assert!(!queued.contains_key("kept_batch"));
    }
    (wallet, storage)
}

async fn refuse_ask_state(bounded: bool, restore: bool, damage: AskDamage) {
    use pubky_switch_client::app::LocalWrite;
    let w = World::new();
    let (mut wallet, storage) = Box::pin(wallet_with_queued_ask(
        &w,
        bounded,
        matches!(damage, AskDamage::UnknownReference),
    ))
    .await;
    let (key, good) = wallet
        .app()
        .protocol_scan("paykit", b"ask/")
        .await
        .unwrap()
        .remove(0);
    let mut value: serde_json::Value = serde_json::from_slice(&good).unwrap();
    let body = value["d"].as_object_mut().unwrap();
    let bad = match damage {
        AskDamage::DuplicateUntil => String::from_utf8(good.clone())
            .unwrap()
            .replacen("\"d\":{", "\"d\":{\"until_ms\":null,", 1)
            .into_bytes(),
        _ => {
            match damage {
                AskDamage::Unknown => {
                    body.insert("extra".into(), serde_json::json!({"nested": [7]}));
                }
                AskDamage::MissingUntil => {
                    body.remove("until_ms").unwrap();
                }
                AskDamage::MissingQueued => {
                    body.remove("queued").unwrap();
                }
                AskDamage::UnknownReference => {
                    body.get_mut("queued")
                        .unwrap()
                        .as_object_mut()
                        .unwrap()
                        .insert("extra".into(), serde_json::json!({"nested": [7]}));
                }
                AskDamage::DuplicateUntil => unreachable!(),
            }
            serde_json::to_vec(&value).unwrap()
        }
    };
    assert_ne!(bad, good);
    if restore {
        refuse_mark_backup(&w, &mut wallet, &key, bad).await;
    } else {
        wallet
            .app_mut()
            .protocol_commit(
                "paykit",
                &[LocalWrite::Put {
                    key: key.clone(),
                    value: bad.clone(),
                }],
            )
            .await
            .unwrap();
        let app_id = wallet.app_id();
        drop(wallet);
        let mut wallet = w
            .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
            .await;
        let error = wallet.receive(None).await.unwrap_err();
        assert!(crate::common::is_corrupt(&error), "{error:?}");
        assert_eq!(
            wallet.app().protocol_get("paykit", &key).await.unwrap(),
            Some(bad)
        );
    }
}

#[tokio::test]
async fn test_mark_state_ask_unknown_after_reload() {
    refuse_ask_state(true, false, AskDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_ask_unknown_at_restore() {
    refuse_ask_state(false, true, AskDamage::Unknown).await;
}

#[tokio::test]
async fn test_mark_state_ask_missing_until_after_reload() {
    refuse_ask_state(true, false, AskDamage::MissingUntil).await;
}

#[tokio::test]
async fn test_mark_state_ask_missing_until_at_restore() {
    refuse_ask_state(false, true, AskDamage::MissingUntil).await;
}

#[tokio::test]
async fn test_mark_state_ask_missing_queued_after_reload() {
    refuse_ask_state(false, false, AskDamage::MissingQueued).await;
}

#[tokio::test]
async fn test_mark_state_ask_missing_queued_at_restore() {
    refuse_ask_state(true, true, AskDamage::MissingQueued).await;
}

#[tokio::test]
async fn test_mark_state_ask_reference_unknown_after_reload() {
    refuse_ask_state(true, false, AskDamage::UnknownReference).await;
}

#[tokio::test]
async fn test_mark_state_ask_reference_unknown_at_restore() {
    refuse_ask_state(false, true, AskDamage::UnknownReference).await;
}

#[tokio::test]
async fn test_mark_state_ask_duplicate_at_restore() {
    refuse_ask_state(true, true, AskDamage::DuplicateUntil).await;
}

#[tokio::test]
async fn test_mark_state_callback_asks_reload_restore_and_send() {
    for bounded in [false, true] {
        for with_request in [false, true] {
            let w = World::new();
            let (wallet, storage) =
                Box::pin(wallet_with_queued_ask(&w, bounded, with_request)).await;
            let held = wallet.app().protocol_scan("paykit", b"ask/").await.unwrap();
            let app_id = wallet.app_id();
            drop(wallet);
            let mut wallet = w
                .load_on_clock(&w.alice, storage, app_id, Arc::clone(&w.clock))
                .await;
            assert_eq!(
                wallet.app().protocol_scan("paykit", b"ask/").await.unwrap(),
                held
            );
            let backup = wallet.backup().await.unwrap();
            let restored = Box::pin(Paykit::restore(
                w.env(&w.alice, store()),
                PaykitConfig::default(),
                &backup,
                Arc::clone(&w.rail) as Arc<dyn Rail>,
                w.timer(),
            ))
            .await
            .unwrap();
            assert_eq!(
                restored
                    .app()
                    .protocol_scan("paykit", b"ask/")
                    .await
                    .unwrap(),
                held
            );
            drop(restored);
            wallet.unblock(&w.bob_id()).await.unwrap();
            wallet.receive(None).await.unwrap();
            wallet.receive(None).await.unwrap();
            assert!(wallet
                .app()
                .protocol_scan("paykit", b"ask/")
                .await
                .unwrap()
                .is_empty());
            assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
        }
    }
}

/// Whether `events` hold a duty event about an entry stored locally.
fn about_stored(events: &[AppEvent]) -> bool {
    events.iter().any(|event| {
        matches!(
            event,
            AppEvent::Parked { id: Some(_), .. }
                | AppEvent::EffectRefused { .. }
                | AppEvent::EffectUncertain { .. }
                | AppEvent::StoredEntryDropped { .. }
        )
    })
}

#[tokio::test]
async fn test_stored_refused_request_shown_unrecorded_then_written() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The receive step's write is refused for good, and so is the app's first write of it.
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    assert_eq!(
        wallet.requests().await.unwrap()[0].status,
        RequestStatus::Unrecorded
    );
    // Not paid while unrecorded.
    assert!(wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .is_err());
    // The SDK took the duty events and acknowledged them.
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
    // Room frees: the next receive writes the record, and the stored entry goes.
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    // Written, its note goes; the marks of the events taken go once they are acknowledged.
    assert!(kept(&wallet, "se/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "dt/").await.is_empty());
}

/// The keys of Paykit's local state under `prefix`.
pub(crate) async fn kept(app: &paykit_sdk::Paykit, prefix: &str) -> Vec<Vec<u8>> {
    app.app()
        .protocol_scan("paykit", prefix.as_bytes())
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect()
}

#[tokio::test]
async fn test_stored_refused_request_written_later_notes_its_sender() {
    // A request applied at its first read notes bob as a counterparty to ask for re-sends
    // after a `missed` item (Paykit spec 9); one written later does too, once written.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "peer/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(kept(&wallet, "peer/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_list_held_notes_its_sender() {
    // A list applied at its first read notes bob as a counterparty, its hold included; one
    // whose step is refused is held, and notes him too.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    assert_eq!(kept(&wallet, "peer/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_first_list_stays_held_over_the_absent_key() {
    // The first list's record write is refused, so no record exists: the list stays held,
    // as no held list is created over an absent key, which can be a delete the service
    // forgot (Paykit spec 6; wire 5, "Incomplete protected-record contract").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert_eq!(wallet.held_lists().await.unwrap().len(), 1);
    let payment = || {
        paykit_sdk::UnrequestedPayment::new(w.bob_id(), btc("0.0015"))
            .with_options(PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12")))
    };
    let held = wallet.pay_without_request(payment()).await.outcome;
    assert!(held.is_err(), "{held:?}");
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_stored_refused_proof_at_the_payee_written_later() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    // The acceptance's and the proof's steps are refused, and the app's first writes too.
    refuse_writes(&w.bob, 4);
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::Requested
    );
    shop.receive(None).await.unwrap();
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        PayeeStatus::ProofReceived
    );
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}

/// Bob's request to alice, stored unverified at her wallet while bob's standing can't be
/// checked, then dropped past 21 days, with the homeserver down. Returns the request and
/// when alice's inbox accepted it.
async fn dropped_unverified(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
) -> (paykit_sdk::RequestRef, u64) {
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(shop, w.alice_id(), unexpiring).await;
    w.homeserver.set_down(true);
    // Past the 10 minutes bob's `identity.json` is cached for.
    w.clock.advance_ms(11 * 60_000);
    let _ = wallet.receive(None).await;
    w.clock
        .advance_ms(pubky_switch_client::app::DEFAULT_PARK_AFTER_MS + 1);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    w.clock
        .advance_ms(pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS);
    // The bounds are judged on the service time verified: the first read verifies it.
    let mut events = wallet.receive(None).await.unwrap().report.events;
    events.extend(wallet.receive(None).await.unwrap().report.events);
    let at = events
        .iter()
        .find_map(|e| match e {
            AppEvent::StoredEntryDropped { accepted_at_ms, .. } => Some(*accepted_at_ms),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
    (request, at)
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_asked_for_again() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (request, _) = dropped_unverified(&w, &mut wallet, &mut shop).await;
    w.homeserver.set_down(false);
    // The wallet asked bob to send it again; bob's shop answers, and the request arrives.
    wallet.flush().await.unwrap();
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_mark_past_128_the_earliest_latest_goes() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let held = seed_marks(
        &mut wallet,
        "ud/",
        paykit_sdk::MAX_DROPPED_MARKS,
        w.clock.now_ms() - 1_000,
    )
    .await;
    dropped_unverified(&w, &mut wallet, &mut shop).await;
    let senders = mark_senders(&wallet, "ud/").await;
    assert_eq!(senders.len(), paykit_sdk::MAX_DROPPED_MARKS);
    assert!(senders.contains(&w.bob_id()));
    assert!(!senders.contains(&held[0]));
    assert!(senders.contains(&held[1]));
}

/// The marks of entries dropped unverified, whole.
async fn unverified_marks(app: &paykit_sdk::Paykit) -> Vec<serde_json::Value> {
    app.app()
        .protocol_scan("paykit", b"ud/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| serde_json::from_slice::<serde_json::Value>(&value).unwrap()["d"].clone())
        .collect()
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_mark_asks_its_window_once_a_day() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (_, at) = dropped_unverified(&w, &mut wallet, &mut shop).await;
    w.homeserver.set_down(false);
    wallet.flush().await.unwrap();
    // Asked for what bob's inbox accepted from the mark's earliest time to its latest.
    assert_eq!(
        asked(&wallet, &w.bob_id()).await,
        vec![(Some(at), Some(at))]
    );
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // A backup carries the mark whole, so a restore doesn't reset it.
    let held = unverified_marks(&wallet).await;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0]["earliest_ms"], at);
    assert_eq!(held[0]["latest_ms"], at);
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(unverified_marks(&wallet).await, held);
    wallet.reconcile().await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // Asked again a day later while the mark lives.
    w.clock.advance_ms(DAY_MS);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 1);
    assert_eq!(
        asked(&wallet, &w.bob_id()).await.last(),
        Some(&(Some(at), Some(at)))
    );
    // 30 days after its latest time, the mark goes.
    w.clock.set_ms(at + paykit_sdk::KEPT_MESSAGE_MS);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert!(unverified_marks(&wallet).await.is_empty());
}

/// Bob's request reaches alice's wallet, whose receive step's batch is applied and its
/// answer lost, kept past the window in which a resend's answer tells: the client stores it
/// as of unknown fate (Switch spec 12.5). `before` runs once the window has passed, before
/// the read that stores it.
async fn uncertain_request(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    shop: &mut paykit_sdk::Paykit,
    before: impl AsyncFnOnce(&mut paykit_sdk::Paykit),
) -> paykit_sdk::RequestRef {
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(shop, w.alice_id(), unexpiring).await;
    w.alice.switch().fail_next(Fault::LoseAnswer {
        path: "/ops".into(),
    });
    assert!(wallet.receive(None).await.is_err());
    w.clock.advance_ms(31 * pubky_switch_core::records::DAY_MS);
    before(wallet).await;
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        summary.report.events.iter().any(|e| matches!(
            e,
            AppEvent::Parked {
                reason: pubky_switch_client::app::ParkReason::Uncertain,
                ..
            }
        )),
        "{:?}",
        summary.report.events
    );
    request
}

#[tokio::test]
async fn test_stored_uncertain_effect_compensated_and_forgotten() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = uncertain_request(&w, &mut wallet, &mut shop, async |_| {}).await;
    // The record holds the request: the app's write found it in place, and the stored entry
    // went with its duty events.
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(!about_stored(&events), "{events:?}");
}

#[tokio::test]
async fn test_stored_uncertain_entry_goes_with_its_events_past_a_host_duty() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // A message bob's switch never admits, bob having blocked alice, is given up first: a
    // duty event for the host, which the SDK doesn't acknowledge. It holds back none of the
    // stored entry's events, which the SDK took and acknowledges one by one.
    let mut manager = w
        .join(
            &w.bob,
            "manager",
            paykit_sdk::AppScopes::new()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    manager.app_mut().deny(&w.alice_id()).await.unwrap();
    let asked = wallet
        .request_resend(
            w.bob_id(),
            paykit_sdk::ResendSelection::default().with_accepted_from_ms(1),
        )
        .await
        .unwrap();
    assert!(matches!(
        wallet.message_delivery(&asked).await.unwrap(),
        Some(paykit_sdk::MessageDelivery::WaitingForAdmission { .. })
    ));
    let request = uncertain_request(&w, &mut wallet, &mut shop, async |wallet| {
        // Given up however few tries confirmed it, past the longest wait (Switch spec 11).
        w.clock
            .advance_ms(pubky_switch_client::app::UNDELIVERABLE_MAX_MS);
        wallet.flush().await.unwrap();
    })
    .await;
    // The host's duty stays for the host; the uncertain entry, whose effect is in place, went
    // with its events, which the SDK took and acknowledged past it.
    let events = wallet.app_mut().unacknowledged_events().await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AppEvent::MessageUndelivered { .. })),
        "{events:?}"
    );
    assert!(!about_stored(&events), "{events:?}");
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

/// Whether alice's payer record of `request` holds a receipt.
async fn holds_receipt(wallet: &paykit_sdk::Paykit, request: &paykit_sdk::RequestRef) -> bool {
    let key = wallet.request(request).await.unwrap().record;
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    !value["receipt"].is_null()
}

#[tokio::test]
async fn test_stored_refused_receipt_written_beside_its_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert!(!holds_receipt(&wallet, &request).await);
    wallet.receive(None).await.unwrap();
    assert!(holds_receipt(&wallet, &request).await);
}

#[tokio::test]
async fn test_stored_refused_receipt_from_before_a_joined_apps_first_listing_written_after() {
    // Alice's wallet pays bob's request; a phone joins, and its first read takes bob's
    // receipt, whose write is refused for good, before Paykit has listed the phone's
    // records. The stored entry is kept, and written after the listing.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    let mut phone = Paykit::join(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "phone", &AppScopes::new().pay().build().unwrap()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    // No certificate is kept as listed under before the first listing.
    assert!(phone
        .app()
        .protocol_get("paykit", b"lc")
        .await
        .unwrap()
        .is_none());
    // Bob's shop reads alice's directory again, which lists the phone, so the receipt goes
    // to it too.
    w.clock.advance_ms(11 * 60_000);
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    let summary = phone.receive(None).await.unwrap();
    assert!(about_stored(&summary.report.events), "{summary:?}");
    assert!(phone
        .app()
        .protocol_get("paykit", b"lc")
        .await
        .unwrap()
        .is_some());
    assert!(!holds_receipt(&phone, &request).await);
    phone.receive(None).await.unwrap();
    assert!(holds_receipt(&phone, &request).await);
    assert!(phone.notices().await.unwrap().is_empty());
}

/// Alice's wallet pays `request`, the dispatch left in flight. Returns the attempt.
pub(crate) async fn pending(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) -> paykit_lib::AttemptId {
    wallet.receive(None).await.unwrap();
    w.rail.next(paykit_sdk::rail::Behaviour::Hang);
    let paykit_sdk::PayOutcome::Pending { attempt_id, .. } = wallet
        .pay_request(request, PayOptions::default())
        .await
        .outcome
        .unwrap()
    else {
        panic!("pending");
    };
    attempt_id
}

#[tokio::test]
async fn test_stored_receipt_waits_for_the_attempt_owners_matching_proof() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    let proof = crate::receipts::requested_proof(&w, &wallet, &shop, &request, attempt_id);
    let receipt = crate::receipts::signed_receipt(&w, &shop, &request, &proof);
    w.deliver(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        receipt.plaintext(),
    );
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "pr/").await.len(), 1);
    assert!(!holds_receipt(&wallet, &request).await);
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Open);
    crate::receipts::publish_requested_proof(&mut wallet, &request, proof).await;
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "pr/").await.is_empty());
    assert!(holds_receipt(&wallet, &request).await);
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Done);
}

/// The outcome of `attempt_id` on its claim, as the switch holds it.
async fn outcome_of(wallet: &mut paykit_sdk::Paykit, attempt_id: paykit_lib::AttemptId) -> Outcome {
    let obligation = wallet
        .payments()
        .await
        .unwrap()
        .into_iter()
        .find(|payment| payment.attempt_id == attempt_id)
        .unwrap()
        .obligation;
    let read = wallet
        .app_mut()
        .read_claim(&obligation)
        .await
        .unwrap()
        .unwrap();
    read.view.attempt(&attempt_id.to_bytes()).unwrap().outcome
}

#[tokio::test]
async fn test_stored_refused_unrequested_proof_credited_later() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let outcome = wallet
        .pay_without_request(
            paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.001"))
                .with_options(
                    PayOptions::default()
                        .with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
                ),
        )
        .await
        .outcome
        .unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
    // The proof's step is refused, and the app's first write of it.
    refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert!(shop.payee_activity().await.unwrap().is_empty());
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1);
    assert_eq!(sales[0].status, PayeeStatus::ProofReceived);
    assert!(shop.app_mut().parked_entries().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_stored_refused_note_given_up_past_its_window() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    // Both intake and the first later write of the verified receipt are refused.
    crate::receipts::deliver_proved_receipt(&w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    // Past its window, its later write is refused again: the client's own read first takes
    // what is due at the switch, so the refusal meets the later write.
    wallet.app_mut().receive(None).await.unwrap();
    refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    // Given up: its note goes, its entry stays stored for the host, who is told.
    assert!(kept(&wallet, "se/").await.is_empty());
    let parked = wallet.app_mut().parked_entries().await.unwrap();
    assert_eq!(parked.len(), 1);
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
    assert!(notices[0].msg_id.is_some());
    // Acknowledged, it goes at once with its stored entry: nothing raises it again.
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(kept(&wallet, "nt/").await.is_empty());
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
}

/// The wallet's verified receipt for bob's request, stored with its note (`se/`) after its
/// intake and first later write were refused, then `past` after its acceptance with the
/// client's own read made: the next later write of it is the next batch at alice's switch.
async fn stored_receipt_aged(w: &World, past: u64) -> (Paykit, paykit_sdk::RequestRef) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(w, &mut wallet, &request).await;
    crate::receipts::deliver_proved_receipt(w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.clock.advance_ms(past);
    wallet.app_mut().receive(None).await.unwrap();
    (wallet, request)
}

/// The next batch at `signer`'s switch is answered `code`, applying nothing.
fn answer_next_write(signer: &pubky_switch_client::testing::TestSigner, code: ErrorCode) {
    signer.switch().fail_next(Fault::Error {
        path: "/ops".into(),
        code,
        current: serde_json::Value::Null,
    });
}

/// Past its window, the later write of a stored message is answered `code`, on which the
/// client keeps the write's batch to send again: the write may still land, so it is neither
/// counted nor given up, and lands with the batch.
async fn kept_past_its_window_on(code: ErrorCode) {
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, code);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1, "{code:?}");
    assert!(wallet.notices().await.unwrap().is_empty(), "{code:?}");
    // Once the switch's limit has passed, the next receive writes it.
    w.clock.advance_ms(11 * 60_000);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty(), "{code:?}");
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    assert!(wallet.notices().await.unwrap().is_empty(), "{code:?}");
}

#[tokio::test]
async fn test_stored_rate_limited_note_kept_past_its_window_then_written() {
    kept_past_its_window_on(ErrorCode::RateLimited).await;
}

#[tokio::test]
async fn test_stored_stale_timestamp_note_kept_past_its_window_then_written() {
    kept_past_its_window_on(ErrorCode::StaleTimestamp).await;
}

#[tokio::test]
async fn test_stored_quota_exceeded_kept_write_waits_and_lands_once_room_frees() {
    // The account is full: the client keeps the write's batch to send again once room frees.
    // Past the window the message waits, uncounted, as long as the batch is held, and lands
    // with it. At the next receive the account is still full: the client's read sends the
    // kept batch again, it stays kept, and the message, held, isn't written again.
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, ErrorCode::QuotaExceeded);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    answer_next_write(&w.alice, ErrorCode::QuotaExceeded);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(11 * 60_000);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(kept(&wallet, "hw/").await.is_empty());
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_stored_bare_conflict_given_up_past_its_window() {
    // `409 conflict`, a bare answer the client keeps no batch for: nothing of the write is
    // held, so past its 30 days the message is given up (Paykit spec 8.1), and no marker is
    // left.
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, ErrorCode::Conflict);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(kept(&wallet, "hw/").await.is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
}

#[tokio::test]
async fn test_stored_dropped_batch_past_the_window_given_up_on_the_record() {
    // Past its 30 days the write's batch is kept on a full account, so the message waits.
    // The client then drops the batch, its resend refused for good: nothing is established,
    // so the record decides. The record doesn't hold the message, and its write is refused
    // again: it is given up, the host told (Paykit spec 8.1, Stored entries, Kept batch).
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, ErrorCode::QuotaExceeded);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    answer_next_write(&w.alice, ErrorCode::PermissionMissing);
    answer_next_write(&w.alice, ErrorCode::PermissionMissing);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(kept(&wallet, "hw/").await.is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
}

#[tokio::test]
async fn test_stored_marker_of_no_kept_batch_swept_at_the_receive() {
    // A marker of a message no longer kept, naming a batch the client holds nothing of, as one
    // a message written another way leaves: the receive sweeps it.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let stale = b"hw/AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE/stale".to_vec();
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: stale,
                value: vec![3; 16],
            }],
        )
        .await
        .unwrap();
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "hw/").await.is_empty());
}

#[tokio::test]
async fn test_stored_marker_goes_with_a_batch_the_client_drops() {
    // The write's batch is kept on a full account, then its resend is refused for good and
    // the client drops it: nothing of it can land. Its marker goes, and the message is
    // written by a later try, no marker left.
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, ErrorCode::QuotaExceeded);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    answer_next_write(&w.alice, ErrorCode::PermissionMissing);
    let _ = wallet.app_mut().receive(None).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "hw/").await.is_empty());
}

#[tokio::test]
async fn test_stored_note_kept_when_its_write_meets_a_wallet_wide_stop() {
    // The certificate expired: a stop of the whole wallet, not of the message. The receive
    // ends with it and nothing is given up.
    let w = World::new();
    let (mut wallet, _) = stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS).await;
    answer_next_write(&w.alice, ErrorCode::CertExpired);
    let error = wallet.receive(None).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::CertExpired), "{error:?}");
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_stored_kept_batch_past_two_windows_not_given_up_and_lands() {
    // The client keeps a rate-limited batch to send again: the write may still land, so the
    // message waits past any bound, and lands with the batch.
    let w = World::new();
    let (mut wallet, _) =
        stored_receipt_aged(&w, paykit_sdk::KEPT_MESSAGE_MS.saturating_mul(2)).await;
    answer_next_write(&w.alice, ErrorCode::RateLimited);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(11 * 60_000);
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    assert!(wallet.notices().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_stored_write_behind_an_earlier_kept_batch_given_up_at_its_window() {
    // Two stored requests past their window. The first one's write is kept on a rate limit,
    // so it waits. The second one's write waits behind that batch: nothing of it is sent or
    // kept, and no marker is written for it, so it is a write not made, given up at its 30
    // days. Once room frees, the first lands.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let first = request_from(&mut shop, w.alice_id(), open_ended("1")).await;
    let second = request_from(&mut shop, w.alice_id(), open_ended("2")).await;
    refuse_writes(&w.alice, 4);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 2);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.app_mut().receive(None).await.unwrap();
    answer_next_write(&w.alice, ErrorCode::RateLimited);
    answer_next_write(&w.alice, ErrorCode::QuotaExceeded);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].request_hash, Some(second.request_hash));
    w.clock.advance_ms(11 * 60_000);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&first).await.unwrap().status,
        RequestStatus::Open
    );
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(kept(&wallet, "hw/").await.is_empty());
}

/// Bob's open-ended request is stored unrecorded and his cancellation of it kept as a stop
/// beside it. Past the bound the request's write is kept by the client on `code`. Given up
/// then, the stop would go with it, and the client's replay would leave the request payable.
/// It waits instead: once the batch lands, the cancellation is placed over it, and the
/// request is never payable.
async fn kept_request_keeps_its_cancellation(code: ErrorCode) {
    // Bob's open-ended request is stored unrecorded and his cancellation of it kept as a
    // stop beside it. Past the bound the request's write is kept by the client on a rate
    // limit. Given up then, the stop would go with it, and the client's replay would
    // leave the request payable. It waits instead: once the batch lands, the cancellation is
    // placed over it, and the request is never payable.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), open_ended("1")).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
    let record = shop.payee_activity().await.unwrap()[0].record;
    shop.cancel_sent_request(&record, None).await.unwrap();
    refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(kept(&wallet, "kc/").await.len(), 1);
    // Past the bound at which any message is given up but one whose batch the client keeps.
    w.clock
        .advance_ms(paykit_sdk::KEPT_MESSAGE_MS.saturating_mul(2));
    wallet.app_mut().receive(None).await.unwrap();
    answer_next_write(&w.alice, code);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "kc/").await.len(), 1);
    w.clock.advance_ms(11 * 60_000);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Cancelled
    );
    let refused = wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap_err();
    assert_eq!(refused.refusal(), Some(&paykit_sdk::Refusal::Cancelled));
}

#[tokio::test]
async fn test_stored_request_kept_by_the_client_keeps_its_cancellation() {
    kept_request_keeps_its_cancellation(ErrorCode::RateLimited).await;
}

#[tokio::test]
async fn test_stored_request_kept_over_quota_keeps_its_cancellation() {
    // The account full past both bounds: the request waits with its cancellation, and lands
    // under it once room frees.
    kept_request_keeps_its_cancellation(ErrorCode::QuotaExceeded).await;
}

#[tokio::test]
async fn test_stored_write_kept_after_a_preflight_failure_waits_and_lands() {
    // The client keeps the write's batch, then the service time it reads before the first
    // send is refused (`401 bad_signature`): the batch was never sent and stays kept, so the
    // write may still land. Its note stays, no give-up is told, and the same batch applies at
    // a later read.
    let w = World::new();
    let reads = std::sync::Arc::new(crate::common::ReadHook::new(std::sync::Arc::clone(
        &w.router,
    )));
    let mut wallet = w
        .join_through(
            &w.alice,
            "wallet",
            AppScopes::new().pay().build().unwrap(),
            PaykitConfig::default(),
            store(),
            std::sync::Arc::clone(&reads)
                as std::sync::Arc<dyn pubky_switch_client::transport::Transport>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), open_ended("1")).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.app_mut().receive(None).await.unwrap();
    // Right after the receive's own read, the held service time ages past a day, so the
    // write's first send reads it again, and that read is refused.
    let clock = std::sync::Arc::clone(&w.clock);
    let switch = std::sync::Arc::clone(w.alice.switch());
    reads.after_read("feed", move || {
        clock.advance_ms(24 * 60 * 60 * 1000 + 1);
        switch.fail_next(Fault::Error {
            path: "feed".into(),
            code: ErrorCode::BadSignature,
            current: serde_json::Value::Null,
        });
    });
    wallet.receive(None).await.unwrap();
    assert!(reads.fired());
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert_eq!(kept(&wallet, "hw/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    // A later read sends the kept batch: the request is written, from that batch.
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(kept(&wallet, "hw/").await.is_empty());
    assert!(wallet.notices().await.unwrap().is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

/// A request of `value` with no deadline of its own.
fn open_ended(value: &str) -> paykit_sdk::RequestDraft {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        paykit_sdk::PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    paykit_sdk::RequestDraft::new(
        btc(value),
        paykit_lib::PaymentReference::new("order-1").unwrap(),
        paykit_sdk::RequestDestination::Endpoints(endpoints),
    )
}

#[tokio::test]
async fn test_stored_given_up_notice_kept_while_its_entry_cant_be_forgotten() {
    let w = World::new();
    let fault = Arc::new(pubky_switch_client::store::FaultStore::new(
        store(),
        pubky_switch_client::store::Fault::None,
    ));
    let mut wallet = w
        .wallet_on(
            "wallet",
            Arc::clone(&fault) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    crate::receipts::deliver_proved_receipt(&w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    // Past its window, its later write is refused again: the client's own read first takes
    // what is due at the switch, so the refusal meets the later write.
    wallet.app_mut().receive(None).await.unwrap();
    refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    // The entry can't be forgotten now: the acknowledgement fails, and the notice stays.
    fault.fail_scan(b"duty/", 0);
    assert!(wallet.acknowledge_notice(&notices[0].id).await.is_err());
    assert_eq!(wallet.notices().await.unwrap(), notices);
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    // Acknowledged again, the entry goes, then the notice.
    wallet.acknowledge_notice(&notices[0].id).await.unwrap();
    assert!(wallet.notices().await.unwrap().is_empty());
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
}

/// Alice's wallet with a message given up: bob's receipt for the payer proof, refused at
/// its step and at each later write past its window. Returns the wallet and the notice.
pub(crate) async fn given_up(w: &World) -> (paykit_sdk::Paykit, paykit_sdk::Notice) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(w, &mut wallet, &request).await;
    crate::receipts::deliver_proved_receipt(w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    // Past its window, its later write is refused again: the client's own read first takes
    // what is due at the switch, so the refusal meets the later write.
    wallet.app_mut().receive(None).await.unwrap();
    refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    let notice = wallet.notices().await.unwrap().remove(0);
    (wallet, notice)
}

/// The value of the notice `id` copied to the key of `other`, another message's notice.
async fn copy_notice(wallet: &mut paykit_sdk::Paykit, id: &str, other: &str) {
    let value = wallet
        .app()
        .protocol_get("paykit", format!("nt/{id}").as_bytes())
        .await
        .unwrap()
        .unwrap();
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("nt/{other}").into_bytes(),
                value,
            }],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn test_stored_notice_under_another_key_isnt_listed_or_followed() {
    let w = World::new();
    let (mut wallet, notice) = given_up(&w).await;
    // A value bound to one message's key, under another message's key: corrupt state.
    let (sender, _) = notice.id.split_once('/').unwrap();
    let other = format!("{sender}/223e4567-e89b-42d3-a456-426614174000");
    copy_notice(&mut wallet, &notice.id, &other).await;
    assert!(wallet.notices().await.is_err());
    // Its acknowledgement doesn't follow the copied handle: the entry stays.
    assert!(wallet.acknowledge_notice(&other).await.is_err());
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_stored_note_past_its_window_kept_when_its_write_errs() {
    // Past its window, the receipt's later write reads its payment's journal entry, which
    // doesn't read: the write wasn't evaluated, so the note stays and nothing is given up.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    crate::receipts::deliver_proved_receipt(&w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("j/{}", attempt_id.to_text()).into_bytes(),
                value: br#"{"v":1,"d":{}}"#.to_vec(),
            }],
        )
        .await
        .unwrap();
    assert!(wallet.receive(None).await.is_err());
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert!(kept(&wallet, "nt/").await.is_empty());
}

#[tokio::test]
async fn test_stored_note_under_another_key_is_corrupt() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    let (key, value) = wallet
        .app()
        .protocol_scan("paykit", b"se/")
        .await
        .unwrap()
        .remove(0);
    let key = String::from_utf8(key).unwrap();
    let (sender, _) = key.trim_start_matches("se/").split_once('/').unwrap();
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("se/{sender}/223e4567-e89b-42d3-a456-426614174000").into_bytes(),
                value,
            }],
        )
        .await
        .unwrap();
    assert!(wallet.requests().await.is_err());
}

#[tokio::test]
async fn test_stored_note_past_its_window_kept_when_the_switch_doesnt_answer() {
    // Past its window, the later write's batch doesn't reach the switch: it wasn't
    // evaluated, so the note stays and nothing is given up.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    crate::receipts::deliver_proved_receipt(&w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.app_mut().receive(None).await.unwrap();
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    assert!(wallet.receive(None).await.is_err());
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    assert!(kept(&wallet, "nt/").await.is_empty());
}

#[tokio::test]
async fn test_stored_refused_lists_hold_the_newer() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    // Each list's step is refused, and so is the write that would place the held list.
    for version in [list_version(1, 1), list_version(1, 2)] {
        w.list_to_alice(&mut shop, &[wallet.app_id()], version)
            .await;
        refuse_writes(&w.alice, 2);
        wallet.receive(None).await.unwrap();
        let held = wallet.held_lists().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].version, version);
    }
}

/// Alice's wallet rejects bob's `request` while her switch takes no write, then refuses the
/// batch kept for it for good: a duty event about a kept batch, for the host.
async fn batch_refused(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) {
    w.alice.switch().fail_next(Fault::Unreachable {
        path: "/ops".into(),
        after: 0,
    });
    let _ = wallet.reject_request(request, None).await;
    let before = refused_batches(wallet).await;
    refuse_writes(&w.alice, 1);
    let _ = wallet.receive(None).await;
    assert_eq!(refused_batches(wallet).await, before + 1);
}

/// How many duty events about a kept batch refused the wallet's host hasn't acknowledged.
async fn refused_batches(wallet: &mut paykit_sdk::Paykit) -> usize {
    wallet
        .app_mut()
        .unacknowledged_events()
        .await
        .unwrap()
        .iter()
        .filter(|e| matches!(e, AppEvent::BatchRefused { .. }))
        .count()
}

#[tokio::test]
async fn test_stored_dropped_entry_between_host_events_asked_for_once() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let first = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    // A host event first, which the host doesn't acknowledge.
    batch_refused(&w, &mut wallet, &first).await;
    // Bob's next request is stored unverified, then dropped past 21 days.
    let mut unexpiring = draft("2", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(&mut shop, w.alice_id(), unexpiring).await;
    w.homeserver.set_down(true);
    // Past the 10 minutes bob's `identity.json` is cached for.
    w.clock.advance_ms(11 * 60_000);
    let _ = wallet.receive(None).await;
    w.clock
        .advance_ms(pubky_switch_client::app::DEFAULT_PARK_AFTER_MS + 1);
    let _ = wallet.receive(None).await;
    assert_eq!(wallet.app_mut().parked_entries().await.unwrap().len(), 1);
    w.clock
        .advance_ms(pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS);
    let (one, two) = (
        wallet.receive(None).await.unwrap(),
        wallet.receive(None).await.unwrap(),
    );
    assert!(
        one.report
            .events
            .iter()
            .chain(&two.report.events)
            .any(|e| matches!(e, AppEvent::StoredEntryDropped { .. })),
        "{:?} {:?}",
        one.report.events,
        two.report.events
    );
    // Asked once at first sight, though the host acknowledges nothing: a read again asks
    // nothing more.
    assert_eq!(one.resends + two.resends, 1);
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    // A host event after it; the host acknowledges up to it once the receive returned.
    batch_refused(&w, &mut wallet, &first).await;
    let last = wallet
        .app_mut()
        .unacknowledged_events()
        .await
        .unwrap()
        .iter()
        .filter_map(AppEvent::id)
        .next_back()
        .unwrap();
    wallet.app_mut().ack_events(last).await.unwrap();
    // The re-send was asked once, at first sight: later reads within the day ask nothing
    // more.
    w.homeserver.set_down(false);
    wallet.flush().await.unwrap();
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_event_taken_again_after_its_commit_failed() {
    let w = World::new();
    let fault = std::sync::Arc::new(pubky_switch_client::store::FaultStore::new(
        crate::common::store(),
        pubky_switch_client::store::Fault::None,
    ));
    let mut wallet = w
        .wallet_on(
            "wallet",
            std::sync::Arc::clone(&fault)
                as std::sync::Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    // The commit that keeps the request's note, and the event's mark with it, fails, as a
    // crash before it would.
    refuse_writes(&w.alice, 1);
    fault.refuse_puts_as_too_large_once(b"p/paykit/se/");
    assert!(wallet.receive(None).await.is_err());
    assert!(kept(&wallet, "dt/").await.is_empty());
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_refused_request_never_written_over_a_value_that_doesnt_read() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    // Meanwhile a value that doesn't read is written at the request's record.
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let entry = wallet
        .app_mut()
        .record_op(&target, Some(b"{\"v\":1}"), Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(entry))
        .await
        .unwrap();
    wallet.receive(None).await.unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(&b"{\"v\":1}"[..]));
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Unrecorded
    );
}

#[tokio::test]
async fn test_stored_refused_request_never_written_over_a_value_awaiting_its_key() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // The data epoch rotates, and the phone, holding the new epoch's key, writes the
    // request's record under it: the wallet doesn't hold that key yet.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    // Its first read takes the new key, its next the request.
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let key = wallet.app().record_key(&target).unwrap();
    let written = w.record_writes(&w.alice, &key);
    assert!(written >= 1);
    // The epoch rotates again, and the wallet gets the newest key alone: it could write the
    // record, but can't open what the phone wrote.
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(
        &w,
        wallet.app_id(),
        &wallet.app().cert().clone(),
        newest,
    );
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(
        matches!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::KeyNotHeld { .. }
            )
        ),
        "{:?}",
        held.content
    );
    // Not written over while its key is awaited: the message stays kept.
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // Once its key comes the record reads, already holding the request: nothing is
    // written, and the kept message goes.
    crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
}

#[tokio::test]
async fn test_stored_refused_receipt_never_opens_a_deleted_record() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    // Meanwhile the payer's record of the request is deleted: a receipt never opens it.
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(
            paykit_lib::collections::Collection::PayerActivity.name(),
            paykit_lib::collections::Collection::PayerActivity.policy(None),
        ),
        &paykit_lib::derive::natural_key::request_record(&w.bob_id(), &request.request_hash),
    );
    let entry = wallet
        .app_mut()
        .record_op(&target, None, Vec::new())
        .await
        .unwrap();
    wallet
        .app_mut()
        .submit_batch(pubky_switch_client::app::OpBatch::for_protocol("paykit").entry(entry))
        .await
        .unwrap();
    let key = wallet.app().record_key(&target).unwrap();
    let written = w.record_writes(&w.alice, &key);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &key), written);
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(
        held.content,
        pubky_switch_client::app::RecordContent::Deleted
    );
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
}

#[tokio::test]
async fn test_stored_refused_older_list_never_replaces_a_newer_held() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let newer = list_version(1, 2);
    // The newer list's step is refused, then the older's, and the writes that would place
    // the held list: the newer stays held.
    for version in [newer, list_version(1, 1)] {
        w.list_to_alice(&mut shop, &[wallet.app_id()], version)
            .await;
        refuse_writes(&w.alice, 2);
        wallet.receive(None).await.unwrap();
        let held = wallet.held_lists().await.unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].version, newer);
    }
}

/// The wallet's kept receipt for `paykit_request`, waiting for a scope key: kept a day short of 30
/// days from its acceptance, then given up with the host's notice.
async fn given_up_after_30_days(
    w: &World,
    wallet: &mut paykit_sdk::Paykit,
    request: &paykit_sdk::RequestRef,
) {
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS - DAY_MS);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(wallet, "se/").await.len(), 1);
    assert!(wallet.notices().await.unwrap().is_empty());
    w.clock.advance_ms(2 * DAY_MS);
    wallet.receive(None).await.unwrap();
    assert!(kept(wallet, "se/").await.is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
    assert_eq!(notices[0].request_hash, Some(request.request_hash));
}

#[tokio::test]
async fn test_stored_refused_receipt_waiting_for_its_records_key_given_up_after_30_days() {
    // A refused receipt is kept while the payer's record is held unopened, written again by
    // the phone under a key the wallet doesn't hold yet. It waits for the key a day short of
    // 30 days from its acceptance, and is then given up, the host told once: a key that comes
    // later finds nothing kept (Paykit spec 8.1). The phone took the receipt too, so the
    // record holds it whatever the wallet does.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    phone.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let value = crate::outcome_record::record_value(&mut phone, &request)
        .await
        .unwrap();
    crate::outcome_record::phone_writes(
        &mut phone,
        &request,
        Some(&serde_json::to_vec(&value).unwrap()),
    )
    .await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, newest);
    wallet.receive(None).await.unwrap();
    given_up_after_30_days(&w, &mut wallet, &request).await;
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(kept(&wallet, "se/").await.is_empty());
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_stored_refused_receipt_waiting_for_the_current_epochs_key_given_up_after_30_days() {
    // A refused receipt is kept, and then the data epoch rotates with no key for the wallet:
    // nothing can be sealed at the record yet. The receipt waits for the key a day short of
    // 30 days from its acceptance, and is then given up, the host told: a key that comes
    // later doesn't bring it back (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet
        .pay_request(&request, PayOptions::default())
        .await
        .outcome
        .unwrap();
    shop.receive(None).await.unwrap();
    let record = shop.payee_activity().await.unwrap()[0].record;
    assert!(shop.confirm_payment(&record).await.unwrap().is_some());
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "se/").await.len(), 1);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    wallet.receive(None).await.unwrap();
    given_up_after_30_days(&w, &mut wallet, &request).await;
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    assert!(kept(&wallet, "se/").await.is_empty());
    assert!(!holds_receipt(&wallet, &request).await);
}

/// `app` backed up, dropped and restored onto a new store; the restore's reconcile run, and
/// one receive, in which the client tells of the parked entries the backup carries again.
pub(crate) async fn restored(w: &World, mut app: Paykit) -> Paykit {
    let backup = app.backup().await.unwrap();
    let backup = paykit_sdk::PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    drop(app);
    let mut app = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    app.reconcile().await.unwrap();
    app.end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    app.receive(None).await.unwrap();
    app
}

/// A message given up, as [`given_up`], its receipt signed once so it can be sent again.
struct GivenUp {
    notice: paykit_sdk::Notice,
    receipt: paykit_lib::records::StoredMessage,
    attempt_id: paykit_lib::AttemptId,
    obligation: [u8; 32],
}

impl GivenUp {
    async fn assert_restored_notices(&self, app: &Paykit, expected: &[paykit_sdk::Notice]) {
        let mut notices = app.notices().await.unwrap();
        // The fresh store sees the payer's signed proof without its original delivery.
        let proof = self.receipt.message().refs[0];
        let delivery = notices
            .iter()
            .position(|notice| {
                notice.kind == paykit_sdk::NoticeKind::PossiblyDelivered
                    && notice.message_hash == Some(proof)
                    && notice.counterparty == self.notice.counterparty
            })
            .expect("the restored proof has its own delivery notice");
        notices.remove(delivery);
        assert_eq!(notices, expected);
    }
}

async fn given_up_signed(w: &World) -> (Paykit, GivenUp) {
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(w, &mut wallet, &request).await;
    let obligation = wallet.payments().await.unwrap()[0].obligation;
    let receipt =
        crate::receipts::deliver_proved_receipt(w, &mut wallet, &shop, &request, attempt_id).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    w.clock.advance_ms(paykit_sdk::KEPT_MESSAGE_MS);
    wallet.app_mut().receive(None).await.unwrap();
    refuse_writes(&w.alice, 1);
    wallet.receive(None).await.unwrap();
    let notice = wallet.notices().await.unwrap().remove(0);
    assert_eq!(notice.kind, paykit_sdk::NoticeKind::MessageGivenUp);
    (
        wallet,
        GivenUp {
            notice,
            receipt,
            attempt_id,
            obligation,
        },
    )
}

/// The outcome of the attempt on its claim, which the given-up receipt would settle.
async fn claim_outcome(app: &mut Paykit, given: &GivenUp) -> Outcome {
    let read = app
        .app_mut()
        .read_claim(&given.obligation)
        .await
        .unwrap()
        .unwrap();
    read.view
        .attempt(&given.attempt_id.to_bytes())
        .unwrap()
        .outcome
}

#[tokio::test]
async fn test_stored_given_up_message_stays_given_up_after_a_restore() {
    // Given up, then backed up and restored: the restore keeps the notice, not the note,
    // and the client tells of the parked entry again. The message stays given up: no note
    // is kept and its effect, settling the attempt, is never applied; the notice stays,
    // and its acknowledgement forgets the restored entry.
    let w = World::new();
    let (wallet, given) = given_up_signed(&w).await;
    let mut app = restored(&w, wallet).await;
    app.receive(None).await.unwrap();
    assert!(kept(&app, "se/").await.is_empty());
    assert_eq!(claim_outcome(&mut app, &given).await, Outcome::Open);
    given
        .assert_restored_notices(&app, std::slice::from_ref(&given.notice))
        .await;
    assert_eq!(app.app_mut().parked_entries().await.unwrap().len(), 1);
    app.acknowledge_notice(&given.notice.id).await.unwrap();
    assert!(app.app_mut().parked_entries().await.unwrap().is_empty());
    given.assert_restored_notices(&app, &[]).await;
}

#[tokio::test]
async fn test_stored_lists_told_again_before_a_given_up_message_dont_stop_its_forget() {
    // A restore tells again, first, that the lists it brought back are unverified, then the
    // given-up message's stored entry. The first is the host's to show the user, and stays
    // owed until the host acknowledges it; the SDK acknowledges the second, which it took,
    // by itself, so the notice's acknowledgement forgets the entry (Paykit spec 1, "Duties
    // and host events").
    use pubky_switch_client::app::AppEvent;
    let w = World::new();
    let (wallet, given) = given_up_signed(&w).await;
    let mut app = restored_unlisted(&w, wallet).await;
    let told = app.app_mut().unacknowledged_events().await.unwrap();
    let lists = told
        .iter()
        .position(|e| matches!(e, AppEvent::ListsUnverified { .. }))
        .unwrap();
    let parked = told
        .iter()
        .position(|e| matches!(e, AppEvent::Parked { .. }))
        .unwrap();
    assert!(lists < parked);
    app.reconcile().await.unwrap();
    app.end_reconcile(pubky_switch_client::app::UserConfirmed::new())
        .await
        .unwrap();
    app.receive(None).await.unwrap();
    let owed = app.app_mut().pending_duty_events().await.unwrap();
    assert!(
        matches!(owed.as_slice(), [duty] if matches!(duty.event, AppEvent::ListsUnverified { .. })),
        "{owed:?}"
    );
    app.acknowledge_notice(&given.notice.id).await.unwrap();
    assert!(app.app_mut().parked_entries().await.unwrap().is_empty());
    // The host acknowledges what it showed.
    let id = owed[0].event.id().unwrap();
    app.app_mut().ack_events_for("paykit", id).await.unwrap();
    assert!(app
        .app_mut()
        .pending_duty_events()
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_stored_given_up_message_sent_again_after_a_restore_has_no_effect() {
    // The sender sends the given-up message again after the restore: it stays processed.
    let w = World::new();
    let (wallet, given) = given_up_signed(&w).await;
    let mut app = restored(&w, wallet).await;
    w.deliver(
        &w.bob,
        "shop",
        &w.alice,
        &[app.app_id()],
        given.receipt.plaintext(),
    );
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(claim_outcome(&mut app, &given).await, Outcome::Open);
    assert!(kept(&app, "se/").await.is_empty());
    given
        .assert_restored_notices(&app, std::slice::from_ref(&given.notice))
        .await;
}

#[tokio::test]
async fn test_stored_given_up_message_cant_be_released_after_a_restore() {
    // The host can't release the restored entry of a message given up: the client releases
    // only an entry whose effect is uncertain, and this one's was refused. Nothing runs it.
    let w = World::new();
    let (wallet, given) = given_up_signed(&w).await;
    let mut app = restored(&w, wallet).await;
    let entry = app.app_mut().parked_entries().await.unwrap().remove(0);
    assert!(app
        .app_mut()
        .release_parked(entry.stored_id, &entry.hash)
        .await
        .is_err());
    app.receive(None).await.unwrap();
    assert_eq!(claim_outcome(&mut app, &given).await, Outcome::Open);
    assert!(kept(&app, "se/").await.is_empty());
}

#[tokio::test]
async fn test_stored_given_up_and_acknowledged_message_sent_again_after_a_restore_has_no_effect() {
    // Acknowledged, the notice goes with its entry; the message stays processed at the
    // client, in the backup too, so a copy sent again after a restore applies nothing.
    let w = World::new();
    let (mut wallet, given) = given_up_signed(&w).await;
    wallet.acknowledge_notice(&given.notice.id).await.unwrap();
    let mut app = restored(&w, wallet).await;
    w.deliver(
        &w.bob,
        "shop",
        &w.alice,
        &[app.app_id()],
        given.receipt.plaintext(),
    );
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(claim_outcome(&mut app, &given).await, Outcome::Open);
    assert!(kept(&app, "se/").await.is_empty());
    given.assert_restored_notices(&app, &[]).await;
}

#[tokio::test]
async fn test_stored_forgotten_entry_sent_again_after_a_restore_writes_nothing_twice() {
    // A request stored for good, written later and its entry forgotten. After a restore the
    // payee sends it again: the record holds it already, so nothing is
    // written again and nothing is stored again.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    refuse_writes(&w.alice, 2);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    let record = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    let mut app = restored(&w, wallet).await;
    let writes = w.record_writes(&w.alice, &record);
    crate::common::send_request_again(&w, &mut shop, &request).await;
    app.receive(None).await.unwrap();
    assert_eq!(w.record_writes(&w.alice, &record), writes);
    assert_eq!(app.requests().await.unwrap().len(), 1);
    assert!(app.app_mut().parked_entries().await.unwrap().is_empty());
    assert!(kept(&app, "se/").await.is_empty());
}

#[tokio::test]
async fn test_stored_entry_dropped_unverified_mark_gone_stays_gone_after_a_restore() {
    // The mark of an entry dropped unverified goes 30 days after its latest time. A restore
    // from a backup taken then tells the client's events again: the dropped entry counts
    // with its own old time, so no mark lives again and nothing is asked (Paykit spec 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let (_, at) = dropped_unverified(&w, &mut wallet, &mut shop).await;
    w.homeserver.set_down(false);
    wallet.flush().await.unwrap();
    w.clock.set_ms(at + paykit_sdk::KEPT_MESSAGE_MS);
    wallet.receive(None).await.unwrap();
    assert!(unverified_marks(&wallet).await.is_empty());
    let before = asked(&wallet, &w.bob_id()).await.len();
    let backup = wallet.backup().await.unwrap();
    drop(wallet);
    let mut wallet = paykit_sdk::Paykit::restore(
        w.env(&w.alice, crate::common::store()),
        paykit_sdk::PaykitConfig::default(),
        &backup,
        std::sync::Arc::clone(&w.rail) as std::sync::Arc<dyn paykit_sdk::Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    wallet.reconcile().await.unwrap();
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    assert!(unverified_marks(&wallet).await.is_empty());
    let after = asked(&wallet, &w.bob_id()).await;
    assert!(
        after.iter().skip(before).all(|(_, until)| until.is_none()),
        "only the restore's own ask: {after:?}"
    );
}

/// The data epochs the wallet's entries stored for a key wait for, in order.
async fn awaiting(wallet: &mut Paykit) -> Vec<u64> {
    wallet
        .app_mut()
        .parked_entries()
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry.reason {
            pubky_switch_client::app::ParkReason::AwaitingKey { data_epoch } => Some(data_epoch),
            _ => None,
        })
        .collect()
}

/// Bob's request reaches alice's wallet once the data epoch has rotated with no key for the
/// wallet: its receive step can't seal the request record, so the client stores the entry
/// at once to wait for the key, reported as waiting for it (Switch spec 8.2). Returns the
/// request, the epoch it waits for and when it was sent, by the clock every party shares.
async fn waiting_for_the_current_key(
    w: &World,
    wallet: &mut Paykit,
    shop: &mut Paykit,
) -> (paykit_sdk::RequestRef, u64, u64) {
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let epoch = w.alice.switch().identity_doc().unwrap().data_epoch();
    wallet.receive(None).await.unwrap();
    let mut unexpiring = draft("1", w.clock.now_ms());
    unexpiring.proposal_expires_at_ms = None;
    let request = request_from(shop, w.alice_id(), unexpiring).await;
    let sent = w.clock.now_ms();
    let mut events = Vec::new();
    for _ in 0..pubky_switch_client::app::RETRY_EVENT_READS + 1 {
        let summary = wallet.receive(None).await.unwrap();
        assert!(summary.key_epochs_awaited.contains(&epoch), "{summary:?}");
        assert!(summary.scope_keys_awaited);
        events.extend(summary.report.events);
    }
    assert!(awaiting(wallet).await.contains(&epoch));
    // Stored at once, never as a store failing.
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AppEvent::StoreFailing { .. }
                | AppEvent::Parked {
                    reason: pubky_switch_client::app::ParkReason::StoreUnavailable,
                    ..
                }
        )),
        "{events:?}"
    );
    (request, epoch, sent)
}

/// Run receives past 21 days from `at`, when the entries waiting for a key were sent, so the
/// client drops them (Switch spec 8.2). Returns the events the reads raised.
async fn past_the_key_wait(w: &World, wallet: &mut Paykit, at: u64) -> Vec<AppEvent> {
    w.clock
        .set_ms(at + pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS + DAY_MS);
    // The bounds are judged on the service time verified: the first read verifies it.
    let mut events = wallet.receive(None).await.unwrap().report.events;
    events.extend(wallet.receive(None).await.unwrap().report.events);
    events
}

/// The marks of key waits, whole.
async fn key_wait_marks(app: &Paykit) -> Vec<serde_json::Value> {
    app.app()
        .protocol_scan("paykit", b"kw/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| serde_json::from_slice::<serde_json::Value>(&value).unwrap()["d"].clone())
        .collect()
}

#[tokio::test]
async fn test_stored_key_wait_dropped_given_up_once_and_asked_again_once_the_key_comes() {
    // The client drops bob's request past 21 days while it waits for the current epoch's
    // key: never processed, the host is told once, naming it, and bob is asked to send it
    // again only once the key is held, from a mark of its own (Paykit spec 8.1). The copy
    // bob sends again is taken as a new message, the notice kept notwithstanding.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let (request, epoch, sent) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    // A day short of the 21 days it still waits, and nothing is told.
    w.clock
        .set_ms(sent + pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS - DAY_MS);
    wallet.receive(None).await.unwrap();
    assert!(awaiting(&mut wallet).await.contains(&epoch));
    assert!(wallet.notices().await.unwrap().is_empty());
    assert!(key_wait_marks(&wallet).await.is_empty());
    let events = past_the_key_wait(&w, &mut wallet, sent).await;
    let (msg_id, at) = events
        .iter()
        .find_map(|e| match e {
            AppEvent::StoredEntryDropped {
                waiting:
                    pubky_switch_client::app::WaitingReason::Key {
                        data_epoch: awaited,
                    },
                message,
                accepted_at_ms,
                ..
            } if *awaited == epoch => Some((message.msg_id.clone(), *accepted_at_ms)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(awaiting(&mut wallet).await.is_empty());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].msg_id.as_deref(), Some(msg_id.as_str()));
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
    assert_eq!(notices[0].request_hash, None);
    // Its own mark, with the epoch it waited for; none of entries dropped unverified, which
    // would ask at once.
    let marks = key_wait_marks(&wallet).await;
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!(marks[0]["epochs"], serde_json::json!([epoch]));
    assert_eq!(marks[0]["earliest_ms"], at);
    assert!(kept(&wallet, "ud/").await.is_empty());
    // Told once, and bob not asked while the key is missing.
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    // The key comes: bob is asked for what his inbox accepted at the mark's times.
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        asked(&wallet, &w.bob_id()).await,
        vec![(Some(at), Some(at))]
    );
    // Bob's shop sends it again; the wallet takes it.
    shop.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(
        wallet.request(&request).await.unwrap().status,
        RequestStatus::Open
    );
    assert_eq!(wallet.notices().await.unwrap().len(), 1);
}

/// `app` backed up, dropped and restored onto a new store, with no reconcile: a restored app
/// that lacks the current epoch's key can't list its records yet.
async fn restored_unlisted(w: &World, mut app: Paykit) -> Paykit {
    let backup = app.backup().await.unwrap();
    let backup = paykit_sdk::PaykitBackup::from_bytes(&backup.to_bytes().unwrap()).unwrap();
    drop(app);
    Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_stored_key_wait_dropped_while_the_records_are_not_listed_given_up_at_once() {
    // The wallet is restored while bob's request waits for the current epoch's key: the
    // restored app can't list its records without the key, so its copy may lack the
    // request's record. The client drops the entry past 21 days, never processed, and the
    // host is told all the same, in that receive (Paykit spec 8.1, "Stored entries").
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let (_, epoch, sent) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    let mut wallet = restored_unlisted(&w, wallet).await;
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.scope_keys_awaited, "{summary:?}");
    assert!(awaiting(&mut wallet).await.contains(&epoch));
    assert!(wallet.notices().await.unwrap().is_empty());
    past_the_key_wait(&w, &mut wallet, sent).await;
    assert!(wallet
        .app()
        .protocol_get("paykit", b"nl")
        .await
        .unwrap()
        .is_some());
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
    assert_eq!(key_wait_marks(&wallet).await.len(), 1);
}

#[tokio::test]
async fn test_stored_key_wait_mark_asks_only_once_every_awaited_key_is_held() {
    // Two of bob's requests wait for the keys of two data epochs and are dropped: the mark
    // names both epochs, and asks bob only once the wallet holds both keys, not when only
    // the later one comes.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let (_, first, _) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    let (_, second, sent) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    assert!(first < second);
    past_the_key_wait(&w, &mut wallet, sent).await;
    let marks = key_wait_marks(&wallet).await;
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!(marks[0]["epochs"], serde_json::json!([first, second]));
    let cert = wallet.app().cert().clone();
    crate::unrequested::store_bundle_from(&w, wallet.app_id(), &cert, second);
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.receive(None).await.unwrap();
    assert_eq!(asked(&wallet, &w.bob_id()).await.len(), 1);
}

#[tokio::test]
async fn test_stored_key_wait_mark_and_notice_carried_through_a_restore() {
    // A backup carries the mark whole and the notice: the restored app tells nothing again,
    // asks bob only once the key is held, and takes the copy he sends again as a new
    // message.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    wallet.receive(None).await.unwrap();
    let (request, _, sent) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    past_the_key_wait(&w, &mut wallet, sent).await;
    let held = key_wait_marks(&wallet).await;
    assert_eq!(held.len(), 1);
    let at = held[0]["earliest_ms"].as_u64().unwrap();
    let notices = wallet.notices().await.unwrap();
    assert_eq!(notices.len(), 1);
    let cert = wallet.app().cert().clone();
    let mut wallet = restored_unlisted(&w, wallet).await;
    assert_eq!(key_wait_marks(&wallet).await, held);
    assert_eq!(wallet.notices().await.unwrap(), notices);
    wallet.receive(None).await.unwrap();
    assert!(asked(&wallet, &w.bob_id()).await.is_empty());
    crate::unrequested::store_bundle(&w, wallet.app_id(), &cert);
    wallet.receive(None).await.unwrap();
    assert_eq!(
        asked(&wallet, &w.bob_id()).await,
        vec![(Some(at), Some(at))]
    );
    shop.receive(None).await.unwrap();
    // The restored app's first read lists what it couldn't before; the copy comes after.
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    // Taken, and held as a restore holds every open request.
    let status = wallet.request(&request).await.unwrap().status;
    assert!(
        matches!(status, RequestStatus::Open | RequestStatus::Held),
        "{status:?}"
    );
    assert_eq!(wallet.notices().await.unwrap(), notices);
}

/// How alice's payer record of bob's request can't be read when his receipt comes.
#[derive(Clone, Copy, Debug)]
enum Unread {
    /// Its value doesn't read as an activity record.
    Value,
    /// Its value doesn't open with the key the wallet holds.
    Sealed,
}

/// Bob's receipt for alice's open attempt comes while her wallet holds the payer record of
/// the request but can't read it (`unread`): the receipt is kept, never taken with no
/// effect, nothing is held on the feed, and the attempt stays open. Once the phone writes
/// the record back, a later write of the wallet's settles the attempt by the receipt, as
/// its first read would have (Paykit spec 8.1, "Stored entries"; Switch spec 10.2). With
/// `reads` false the record never reads again: 30 days after its acceptance the receipt is
/// given up, and a notice tells the host.
async fn receipt_over_unread_record(unread: Unread, reads: bool) {
    let w = World::new();
    let store = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let mut wallet = w
        .wallet_on(
            "wallet",
            Arc::clone(&store) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    phone.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let key = phone.app().record_key(&target).unwrap();
    let good = phone
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let proof = crate::receipts::requested_proof(&w, &wallet, &shop, &request, attempt_id);
    let receipt = crate::receipts::signed_receipt(&w, &shop, &request, &proof);
    let good = crate::receipts::record_with_proof(&good, &wallet, proof);
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    match unread {
        Unread::Value => {
            crate::outcome_record::phone_writes(
                &mut phone,
                &request,
                Some(crate::outcome_record::INVALID),
            )
            .await;
        }
        Unread::Sealed => {
            // The data epoch rotates, and the phone takes the new epoch's key first. Its
            // write, sealed at that epoch under another key, waits at the wallet for the
            // key, then doesn't open with it.
            w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
            crate::unrequested::store_bundle(&w, phone.app_id(), &phone.app().cert().clone());
            phone.receive(None).await.unwrap();
            let (_, kind) = phone
                .app_mut()
                .record_op(&target, Some(&good), Vec::new())
                .await
                .unwrap();
            let pubky_switch_core::ops::OpKind::RecordPut(mut put) = kind else {
                panic!("a record write");
            };
            put.value_sealed = pubky_switch_core::sealed::SealedValue::seal(
                &[3; 32],
                &w.alice_id(),
                phone.app().data_epoch(),
                &target.collection.policy.read_scope,
                &put.key,
                &good,
            )
            .unwrap();
            w.alice
                .switch()
                .app_op(
                    &w.alice.identity().app_keys("phone"),
                    pubky_switch_core::ops::OpKind::RecordPut(put),
                )
                .unwrap();
            wallet.receive(None).await.unwrap();
            crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
        }
    }
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    match unread {
        Unread::Value => assert_eq!(held.content.value(), Some(crate::outcome_record::INVALID)),
        Unread::Sealed => assert_eq!(
            held.content,
            pubky_switch_client::app::RecordContent::Unreadable(
                pubky_switch_client::app::Unopened::DoesNotOpen
            )
        ),
    }
    w.deliver(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        receipt.plaintext(),
    );
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "pr/").await.len(), 1, "{unread:?}");
    assert!(wallet.app_mut().parked_entries().await.unwrap().is_empty());
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Open);
    assert!(wallet.notices().await.unwrap().is_empty());

    if !reads {
        w.clock.advance_ms(31 * DAY_MS);
        wallet.receive(None).await.unwrap();
        assert!(kept(&wallet, "pr/").await.is_empty(), "{unread:?}");
        let notices = wallet.notices().await.unwrap();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
        assert_eq!(notices[0].counterparty, Some(w.bob_id()));
        return;
    }
    phone.receive(None).await.unwrap();
    crate::outcome_record::phone_writes(&mut phone, &request, Some(&good)).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    assert!(kept(&wallet, "pr/").await.is_empty(), "{unread:?}");
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Done);
}

#[tokio::test]
async fn test_stored_receipt_over_a_record_value_that_doesnt_read_kept_until_it_reads() {
    receipt_over_unread_record(Unread::Value, true).await;
}

#[tokio::test]
async fn test_stored_receipt_over_a_record_that_never_reads_given_up_with_a_notice() {
    receipt_over_unread_record(Unread::Value, false).await;
}

#[tokio::test]
async fn test_stored_receipt_over_a_record_that_doesnt_open_kept_until_it_reads() {
    receipt_over_unread_record(Unread::Sealed, true).await;
}

#[tokio::test]
async fn test_stored_proof_from_another_identity_never_written_into_a_settlement_record() {
    // Carol's proof of a payment without a request reaches bob's shop first; the service
    // refuses its write, so it is kept to write later. Alice's proof of the same settlement
    // then opens the shop's record of it, naming alice. Carol's kept proof is never written
    // there, not even reflected: the record names its counterparty.
    let w = World::new();
    let _wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let _carol = w
        .join(&w.carol, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    let proof = |from: &pubky_switch_client::testing::TestSigner| {
        let body = paykit_lib::bodies::PaymentProof {
            attempt_id: paykit_lib::AttemptId::generate().unwrap(),
            billing_period: None,
            payment_endpoint_identifier: crate::common::endpoint("btc-lightning-bolt12"),
            payment_app_id: shop.app_id(),
            list_version: None,
            amount: Some(crate::common::btc("0.0015")),
            allowance_id: None,
            label: None,
            payment_reference: None,
            proof: serde_json::json!({ "rail": "fake", "tx": "t1" })
                .as_object()
                .cloned()
                .unwrap(),
        }
        .to_body(0)
        .unwrap();
        w.inject(
            from,
            "wallet",
            &w.bob,
            &[shop.app_id()],
            "paykit.payment_proof",
            Vec::new(),
            body,
        )
    };
    let from_carol = proof(&w.carol);
    w.clock.advance_ms(1);
    proof(&w.alice);
    crate::common::refuse_writes(&w.bob, 1);
    shop.receive(None).await.unwrap();
    shop.receive(None).await.unwrap();
    assert!(kept(&shop, "se/").await.is_empty());
    let collection = paykit_lib::collections::Collection::PayeeActivity;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::payee_payment_record(
            &crate::common::endpoint("btc-lightning-bolt12"),
            b"fake:t1",
        ),
    );
    let record = shop.app().record_key(&target).unwrap();
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert_eq!(
        value["counterparty"],
        serde_json::json!(pubky_switch_core::enc::pubky_id(&w.alice_id())),
        "{value}"
    );
    assert!(!value.to_string().contains(&from_carol), "{value}");
}

#[tokio::test]
async fn test_stored_request_over_an_unread_record_held_then_given_up_at_30_days() {
    // Alice's wallet records bob's request; her tablet then writes the request record as a
    // value that doesn't read. Her phone, which keeps no mark of what it has seen, gets bob's
    // request: the record's key binds bob, so the entry is held until the record reads,
    // never written over the value; 30 days after its acceptance it is given up, the host
    // told, and nothing stays stored.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut tablet = w.wallet("tablet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let seen = Arc::new(crate::outcome_record::NoSeenMarks::default());
    seen.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut phone = w
        .wallet_on(
            "phone",
            Arc::clone(&seen) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    phone.receive(None).await.unwrap();
    tablet.receive(None).await.unwrap();
    crate::outcome_record::phone_writes(
        &mut tablet,
        &request,
        Some(crate::outcome_record::INVALID),
    )
    .await;
    phone.receive(None).await.unwrap();
    let record = phone
        .app()
        .record_key(&crate::outcome_record::payer_record(&request))
        .unwrap();
    async fn value_at(phone: &paykit_sdk::Paykit, record: &[u8; 32]) -> Option<Vec<u8>> {
        phone
            .app()
            .shared_record(record)
            .await
            .unwrap()
            .unwrap()
            .content
            .value()
            .map(<[u8]>::to_vec)
    }
    assert_eq!(
        value_at(&phone, &record).await.as_deref(),
        Some(crate::outcome_record::INVALID)
    );
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap()
        .into_iter()
        .find(|sent| sent.kind == "paykit.payment_request")
        .unwrap();
    w.deliver(&w.bob, "shop", &w.alice, &[phone.app_id()], &sent.plaintext);
    phone.receive(None).await.unwrap();
    assert_eq!(
        value_at(&phone, &record).await.as_deref(),
        Some(crate::outcome_record::INVALID)
    );
    assert!(phone.notices().await.unwrap().is_empty());
    w.clock.advance_ms(DAY_MS + 1);
    phone.receive(None).await.unwrap();
    w.clock.advance_ms(31 * DAY_MS);
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    assert_eq!(
        value_at(&phone, &record).await.as_deref(),
        Some(crate::outcome_record::INVALID)
    );
    assert!(phone.app_mut().parked_entries().await.unwrap().is_empty());
    let notices = phone.notices().await.unwrap();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, paykit_sdk::NoticeKind::MessageGivenUp);
    assert_eq!(notices[0].counterparty, Some(w.bob_id()));
    assert_eq!(notices[0].msg_id.as_deref(), Some(sent.msg_id.as_str()));
}

/// The body of bob's receipt for `attempt_id`.
fn receipt_body(attempt_id: paykit_lib::AttemptId) -> serde_json::Map<String, serde_json::Value> {
    paykit_lib::bodies::PaymentReceipt {
        attempt_id,
        billing_period: None,
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap()
}

/// The marks of dropped replies `app` holds, as (sender, earliest time, due).
async fn reply_marks(app: &Paykit) -> Vec<([u8; 32], u64, bool)> {
    app.app()
        .protocol_scan("paykit", b"pd/")
        .await
        .unwrap()
        .into_iter()
        .map(|(_, value)| {
            let value: serde_json::Value = serde_json::from_slice(&value).unwrap();
            let mark = &value["d"];
            let sender =
                pubky_switch_core::enc::unb64_array::<32>(mark["sender"].as_str().unwrap())
                    .unwrap();
            (
                sender,
                mark["earliest_ms"].as_u64().unwrap(),
                mark["due"].as_bool().unwrap(),
            )
        })
        .collect()
}

/// Alice's wallet, her attempt on bob's request open, holds the payer record of the
/// request with a value her phone wrote that doesn't read. Bob's first receipt comes,
/// accepted at `at`, and is kept over the record; a second later `others` more of
/// his receipts for the same record, naming attempts of no claim, are kept the same way.
struct KeptOverUnread {
    w: World,
    wallet: Paykit,
    phone: Paykit,
    request: paykit_sdk::RequestRef,
    attempt_id: paykit_lib::AttemptId,
    /// The readable record the phone brings back, including the open attempt's proof.
    good: Vec<u8>,
    /// The first receipt Bob sent, as he signed it.
    receipt: pubky_switch_core::message::MessagePlaintext,
    /// Bob's receipt naming the signed proof of the open attempt in the readable record.
    open_receipt: paykit_lib::records::StoredMessage,
    at: u64,
}

enum ReceiptFor {
    OpenAttempt,
    AnotherAttempt,
}

async fn receipts_kept_over_unread(
    first: ReceiptFor,
    others: usize,
    scopes: AppScopes,
) -> KeptOverUnread {
    let w = World::new();
    let store = Arc::new(crate::outcome_record::NoSeenMarks::default());
    let mut wallet = w
        .join_with(
            &w.alice,
            "wallet",
            scopes.build().unwrap(),
            PaykitConfig::default(),
            Arc::clone(&store) as Arc<dyn pubky_switch_client::store::StateStore>,
        )
        .await;
    wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    let attempt_id = pending(&w, &mut wallet, &request).await;
    phone.receive(None).await.unwrap();
    let target = crate::outcome_record::payer_record(&request);
    let key = phone.app().record_key(&target).unwrap();
    let good = phone
        .app()
        .shared_record(&key)
        .await
        .unwrap()
        .unwrap()
        .content
        .value()
        .unwrap()
        .to_vec();
    let proof = crate::receipts::requested_proof(&w, &wallet, &shop, &request, attempt_id);
    let open_receipt = crate::receipts::signed_receipt(&w, &shop, &request, &proof);
    let good = crate::receipts::record_with_proof(&good, &wallet, proof);
    store.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    crate::outcome_record::phone_writes(&mut phone, &request, Some(crate::outcome_record::INVALID))
        .await;
    wallet.receive(None).await.unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert_eq!(held.content.value(), Some(crate::outcome_record::INVALID));
    let at = w.clock.now_ms();
    let receipt = match first {
        ReceiptFor::OpenAttempt => open_receipt.plaintext().clone(),
        ReceiptFor::AnotherAttempt => {
            let other = paykit_lib::AttemptId::from_bytes([0xff; 16]);
            assert_ne!(other, attempt_id);
            w.sign_as(
                &w.bob,
                "shop",
                &w.alice,
                "paykit.payment_receipt",
                vec![[7; 32], request.request_hash],
                receipt_body(other),
            )
        }
    };
    w.deliver(&w.bob, "shop", &w.alice, &[wallet.app_id()], &receipt);
    wallet.receive(None).await.unwrap();
    assert_eq!(kept(&wallet, "pr/").await.len(), 1);
    w.clock.advance_ms(1_000);
    for i in 0..others {
        let other = paykit_lib::AttemptId::from_bytes([u8::try_from(i).unwrap(); 16]);
        assert_ne!(other, attempt_id);
        w.inject(
            &w.bob,
            "shop",
            &w.alice,
            &[wallet.app_id()],
            "paykit.payment_receipt",
            vec![[7; 32], request.request_hash],
            receipt_body(other),
        );
    }
    // Consume the inputs before SDK upkeep so the host can still block the first ask.
    wallet.app_mut().receive(None).await.unwrap();
    wallet.app_mut().receive(None).await.unwrap();
    assert_eq!(outcome_of(&mut wallet, attempt_id).await, Outcome::Open);
    KeptOverUnread {
        w,
        wallet,
        phone,
        request,
        attempt_id,
        good,
        receipt,
        open_receipt,
        at,
    }
}

impl KeptOverUnread {
    /// The phone writes the record's value back, so it reads; the wallet's next receive
    /// takes it. Returns how many re-sends that receive asked for.
    async fn record_reads(&mut self) -> u64 {
        self.phone.receive(None).await.unwrap();
        crate::outcome_record::phone_writes(&mut self.phone, &self.request, Some(&self.good)).await;
        self.wallet.receive(None).await.unwrap().resends
    }

    /// Bob's inbox answers the wallet's request by re-sending his receipt, as his outbox
    /// would: the same message, accepted again now. Returns whether the wallet took it as a
    /// new message.
    async fn receipt_sent_again(&mut self) -> bool {
        let w = &self.w;
        w.deliver(
            &w.bob,
            "shop",
            &w.alice,
            &[self.wallet.app_id()],
            &self.receipt,
        );
        let msg_id = pubky_switch_core::message::Message::parse(
            self.receipt.json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )
        .unwrap()
        .msg_id;
        let events = self.wallet.receive(None).await.unwrap().report.events;
        events.iter().any(
            |event| matches!(event, AppEvent::MessageReceived { msg_id: id, .. } if *id == msg_id),
        )
    }
}

#[tokio::test]
async fn test_stored_receipt_dropped_over_an_unread_record_asked_for_before_it_reads() {
    // The earliest ordinary receipt falls outside the bound. Its accepted time starts
    // the recovery window even while the payer record can't place a receipt.
    let mut k = receipts_kept_over_unread(
        ReceiptFor::AnotherAttempt,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
        AppScopes::new().pay(),
    )
    .await;
    let bob = k.w.bob_id();
    assert_eq!(
        kept(&k.wallet, "pr/").await.len(),
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER
    );
    assert_eq!(reply_marks(&k.wallet).await, vec![(bob, k.at, true)]);
    assert_eq!(k.wallet.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(&k.wallet, &bob).await, vec![(Some(k.at), None)]);
    let held = kept(&k.wallet, "pr/").await;
    assert_eq!(k.record_reads().await, 0);
    // Reading the record still leaves these receipts waiting for their own proofs.
    assert_eq!(kept(&k.wallet, "pr/").await, held);
    assert_eq!(outcome_of(&mut k.wallet, k.attempt_id).await, Outcome::Open);
    // Asked once: a later read within the day asks nothing more.
    assert_eq!(k.wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(asked(&k.wallet, &bob).await.len(), 1);
    assert!(k.receipt_sent_again().await);
    assert_eq!(kept(&k.wallet, "pr/").await.len(), held.len());
    assert_eq!(outcome_of(&mut k.wallet, k.attempt_id).await, Outcome::Open);
    k.w.deliver(
        &k.w.bob,
        "shop",
        &k.w.alice,
        &[k.wallet.app_id()],
        k.open_receipt.plaintext(),
    );
    k.wallet.receive(None).await.unwrap();
    assert_eq!(outcome_of(&mut k.wallet, k.attempt_id).await, Outcome::Done);
}

#[tokio::test]
async fn test_stored_receipts_kept_over_an_unread_record_up_to_the_bound_drop_nothing() {
    // 64 receipts kept: none dropped, no mark, nothing asked; the record reads, and the
    // kept receipt for the attempt settles it.
    let mut k = receipts_kept_over_unread(
        ReceiptFor::OpenAttempt,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 1,
        AppScopes::new().pay(),
    )
    .await;
    let bob = k.w.bob_id();
    let mut waiting = kept(&k.wallet, "pr/").await;
    assert_eq!(waiting.len(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER);
    assert!(reply_marks(&k.wallet).await.is_empty());
    assert_eq!(k.record_reads().await, 0);
    assert!(asked(&k.wallet, &bob).await.is_empty());
    waiting.retain(|key| !key.ends_with(k.open_receipt.message().msg_id.as_bytes()));
    assert_eq!(waiting.len(), paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER - 1);
    assert_eq!(kept(&k.wallet, "pr/").await, waiting);
    assert!(holds_receipt(&k.wallet, &k.request).await);
    assert_eq!(outcome_of(&mut k.wallet, k.attempt_id).await, Outcome::Done);
}

#[tokio::test]
async fn test_stored_receipt_dropped_over_a_record_that_stays_unread_asks_once_a_day() {
    let mut k = receipts_kept_over_unread(
        ReceiptFor::AnotherAttempt,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
        AppScopes::new().pay(),
    )
    .await;
    let bob = k.w.bob_id();
    for count in 1..=3 {
        assert_eq!(k.wallet.receive(None).await.unwrap().resends, 1);
        assert_eq!(
            asked(&k.wallet, &bob).await,
            vec![(Some(k.at), None); count]
        );
        k.w.clock.advance_ms(DAY_MS - 1);
        assert_eq!(k.wallet.receive(None).await.unwrap().resends, 0);
        k.w.clock.advance_ms(1);
    }
    assert_eq!(reply_marks(&k.wallet).await, vec![(bob, k.at, true)]);
    assert_eq!(outcome_of(&mut k.wallet, k.attempt_id).await, Outcome::Open);
}

#[tokio::test]
async fn test_stored_receipt_dropped_payee_blocked_asked_only_once_unblocked() {
    // Block Bob after intake counted his receipt, before SDK upkeep can ask him.
    let mut k = receipts_kept_over_unread(
        ReceiptFor::AnotherAttempt,
        paykit_sdk::MAX_KEPT_REPLIES_PER_SENDER,
        AppScopes::new().pay().manage_contacts(),
    )
    .await;
    let bob = k.w.bob_id();
    assert_eq!(reply_marks(&k.wallet).await, vec![(bob, k.at, true)]);
    assert!(asked(&k.wallet, &bob).await.is_empty());
    k.wallet.block(&bob).await.unwrap();
    assert_eq!(k.record_reads().await, 0);
    assert_eq!(reply_marks(&k.wallet).await, vec![(bob, k.at, true)]);
    assert_eq!(k.wallet.receive(None).await.unwrap().resends, 0);
    assert!(asked(&k.wallet, &bob).await.is_empty());
    k.wallet.unblock(&bob).await.unwrap();
    assert_eq!(k.wallet.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(&k.wallet, &bob).await, vec![(Some(k.at), None)]);
}

/// A stored mark with no previously prepared ask; its original due flag was false.
async fn seed_reply_mark(w: &World, app: &mut Paykit, sender: &[u8; 32]) -> u64 {
    let at = w.clock.now_ms();
    let value = serde_json::json!({
        "v": 1,
        "d": {
            "sender": pubky_switch_core::enc::b64(sender),
            "earliest_ms": at,
            "latest_ms": at,
            "due": false,
            "asked_at_ms": null,
            "queued": null,
        }
    });
    app.app_mut()
        .protocol_commit(
            "paykit",
            &[pubky_switch_client::app::LocalWrite::Put {
                key: format!("pd/{}", pubky_switch_core::enc::b64(sender)).into_bytes(),
                value: serde_json::to_vec(&value).unwrap(),
            }],
        )
        .await
        .unwrap();
    at
}

/// The first receive asks for the original window, independently of the activity record.
async fn asks_for_stored_mark(app: &mut Paykit, sender: &[u8; 32], at: u64) {
    assert_eq!(app.receive(None).await.unwrap().resends, 1);
    assert_eq!(asked(app, sender).await, vec![(Some(at), None)]);
}

/// Alice pays bob from `wallet`, without a request, to his list's reusable endpoint.
async fn paid_without_a_request(w: &World, wallet: &mut Paykit) {
    let payment = paykit_sdk::UnrequestedPayment::new(w.bob_id(), crate::common::btc("0.0015"))
        .with_options(
            PayOptions::default().with_endpoint(crate::common::endpoint("btc-lightning-bolt12")),
        );
    let outcome = wallet.pay_without_request(payment).await.outcome.unwrap();
    assert!(
        matches!(outcome, paykit_sdk::PayOutcome::Paid { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn test_stored_receipt_dropped_mark_asks_over_a_payment_without_a_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let bob = w.bob_id();
    paid_without_a_request(&w, &mut wallet).await;
    let at = seed_reply_mark(&w, &mut wallet, &bob).await;
    asks_for_stored_mark(&mut wallet, &bob, at).await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(asked(&wallet, &bob).await, vec![(Some(at), None)]);
}

#[tokio::test]
async fn test_stored_receipt_dropped_mark_asks_over_a_billing_period_record() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let bob = w.bob_id();
    let request_hash = [5; 32];
    let period = paykit_lib::periods::PeriodRef {
        starts_at: 1_769_817_600_000,
        ends_at: 1_772_236_800_000,
    };
    let receipt = paykit_lib::bodies::PaymentReceipt {
        attempt_id: paykit_lib::AttemptId::from_bytes([3; 16]),
        billing_period: Some(period),
        metadata: None,
        payment_reference: None,
    }
    .to_body(2)
    .unwrap();
    let plaintext = w.sign_as(
        &w.bob,
        "shop",
        &w.alice,
        "paykit.payment_receipt",
        vec![[7; 32], request_hash],
        receipt,
    );
    let keys = w.bob.identity().app_keys("shop");
    let cert = w.bob.switch().app(&keys.app_id_bytes()).cert.unwrap();
    let stored = paykit_lib::records::StoredMessage::new(
        bob,
        cert.signed(),
        plaintext,
        Some(w.clock.now_ms()),
        &w.alice_id(),
    )
    .unwrap();
    let mut record =
        paykit_lib::records::ActivityRecord::new(paykit_lib::records::ActivitySide::Payer, bob);
    record.request_hash = Some(request_hash);
    record.billing_period = Some(period);
    record.receipt = Some(stored);
    assert!(matches!(
        record.binding(),
        Ok(paykit_lib::records::RecordBinding::Period { .. })
    ));
    let collection = paykit_lib::collections::Collection::PayerActivity;
    let target = pubky_switch_client::app::RecordTarget::new(
        pubky_switch_client::app::Collection::new(collection.name(), collection.policy(None)),
        &paykit_lib::derive::natural_key::period_record(
            &bob,
            &request_hash,
            period.starts_at / 1000,
        ),
    );
    wallet
        .app_mut()
        .put_record(&target, &record.to_json().unwrap(), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        wallet
            .app()
            .records_of(collection.name())
            .await
            .unwrap()
            .len(),
        1
    );
    let at = seed_reply_mark(&w, &mut wallet, &bob).await;
    asks_for_stored_mark(&mut wallet, &bob, at).await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    assert_eq!(wallet.receive(None).await.unwrap().resends, 0);
    assert_eq!(asked(&wallet, &bob).await, vec![(Some(at), None)]);
}

#[tokio::test]
async fn test_stored_reply_dropped_mark_asks_at_the_payee_over_a_payment_without_a_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    w.list_to_alice(&mut shop, &[wallet.app_id()], list_version(1, 1))
        .await;
    wallet.receive(None).await.unwrap();
    let alice = w.alice_id();
    paid_without_a_request(&w, &mut wallet).await;
    shop.receive(None).await.unwrap();
    let sales = shop.payee_activity().await.unwrap();
    assert_eq!(sales.len(), 1, "{sales:?}");
    assert_eq!((sales[0].payer, sales[0].request_hash), (alice, None));
    let at = seed_reply_mark(&w, &mut shop, &alice).await;
    asks_for_stored_mark(&mut shop, &alice, at).await;
    request_from(&mut shop, alice, draft("1", w.clock.now_ms())).await;
    assert_eq!(shop.receive(None).await.unwrap().resends, 0);
    assert_eq!(asked(&shop, &alice).await, vec![(Some(at), None)]);
}

/// The `msg_id` of the response bob's shop holds in its record of `request`.
async fn payee_response(
    w: &World,
    shop: &Paykit,
    request: &paykit_sdk::RequestRef,
) -> Option<String> {
    let record = shop
        .payee_activity()
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.request_hash == Some(request.request_hash))
        .unwrap()
        .record;
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let read = paykit_lib::records::ActivityRecord::read(
        held.content.value().unwrap(),
        &w.bob_id(),
        paykit_lib::records::ActivitySide::Payee,
    )
    .unwrap();
    read.response.map(|m| m.message().msg_id.clone())
}

#[tokio::test]
async fn test_stored_reply_takes_the_response_before_a_later_different_reply() {
    // Alice's acceptance of bob's request is refused at the shop, its step and the shop's
    // first later write: the entry is stored. A different acceptance of hers comes later
    // and its write is admissible: it is only reflected while the earlier one is kept to
    // write, and the earlier is the record's response once written (Paykit spec 6, 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let shop_id = shop.app_id();
    let acceptance = || {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_id],
            "paykit.payment_request_acceptance",
            vec![request.request_hash],
            serde_json::json!({ "label": null })
                .as_object()
                .cloned()
                .unwrap(),
        )
    };
    let first = acceptance();
    refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert_eq!(Box::pin(payee_response(&w, &shop, &request)).await, None);
    w.clock.advance_ms(1_000);
    let second = acceptance();
    shop.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(payee_response(&w, &shop, &request)).await,
        Some(first)
    );
    let record = shop.payee_activity().await.unwrap()[0].record;
    let held = shop.app().shared_record(&record).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(held.content.value().unwrap()).unwrap();
    assert!(value["msg_ids"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id.as_str() == Some(second.as_str())));
}

#[tokio::test]
async fn test_stored_later_reply_stored_too_never_takes_the_response_of_an_earlier_one() {
    // Both of alice's acceptances are refused at their steps, a read apart, and so is the
    // earlier one's later write each time, while the later one's later write is admissible:
    // the later one is only reflected while the earlier is kept to write, and the earlier
    // is the record's response once written (Paykit spec 6, 8.1).
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let shop_id = shop.app_id();
    let acceptance = || {
        w.inject(
            &w.alice,
            "wallet",
            &w.bob,
            &[shop_id],
            "paykit.payment_request_acceptance",
            vec![request.request_hash],
            serde_json::json!({ "label": null })
                .as_object()
                .cloned()
                .unwrap(),
        )
    };
    let first = acceptance();
    refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    w.clock.advance_ms(1_000);
    acceptance();
    // The later one's step, then the earlier one's later write.
    refuse_writes(&w.bob, 2);
    shop.receive(None).await.unwrap();
    assert_eq!(Box::pin(payee_response(&w, &shop, &request)).await, None);
    shop.receive(None).await.unwrap();
    assert_eq!(
        Box::pin(payee_response(&w, &shop, &request)).await,
        Some(first)
    );
}

#[tokio::test]
async fn test_stored_key_drop_duty_requires_its_owner_and_survives_reload() {
    use pubky_switch_client::app::{AppError, Refusal, WaitingReason};

    let w = World::new();
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let app_id = wallet.app_id();
    let mut shop = w.shop().await;
    let (_, epoch, at) = waiting_for_the_current_key(&w, &mut wallet, &mut shop).await;
    w.clock
        .set_ms(at + pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS + DAY_MS);
    wallet.app_mut().receive(None).await.unwrap();
    wallet.app_mut().receive(None).await.unwrap();
    let events = wallet
        .app_mut()
        .unacknowledged_events_for("paykit")
        .await
        .unwrap();
    let event = events
        .iter()
        .find(|event| {
            matches!(event,
        AppEvent::StoredEntryDropped { waiting: WaitingReason::Key { data_epoch }, .. }
        if *data_epoch == epoch)
        })
        .unwrap();
    let id = event.id().unwrap();
    assert_eq!(wallet.app().event_owners(id).unwrap(), ["paykit"]);
    assert!(key_wait_marks(&wallet).await.is_empty());
    assert!(matches!(
        wallet.app_mut().ack_events_for("chat", id).await,
        Err(AppError::Refused(Refusal::Usage(_)))
    ));
    assert!(wallet
        .app_mut()
        .unacknowledged_events_for("paykit")
        .await
        .unwrap()
        .iter()
        .any(|event| event.id() == Some(id)));
    drop(wallet);
    let mut wallet = w
        .load_with(
            &w.alice,
            storage,
            app_id,
            Arc::clone(&w.clock),
            PaykitConfig::default(),
        )
        .await;
    assert!(wallet
        .app_mut()
        .unacknowledged_events_for("paykit")
        .await
        .unwrap()
        .iter()
        .any(|event| event.id() == Some(id)));
    wallet.receive(None).await.unwrap();
    assert_eq!(key_wait_marks(&wallet).await.len(), 1);
    assert!(!wallet
        .app_mut()
        .unacknowledged_events_for("paykit")
        .await
        .unwrap()
        .iter()
        .any(|event| event.id() == Some(id)));
}

mod admission {
    use super::*;
    use pubky_switch_client::app::{
        App, Collection, LocalWrite, MissedEntries, OpaqueRule, Outcome, ParkReason, Protocol,
        ReceiveContext, ReceivedMessage, ResendNeeded, StoreFailure,
    };
    use pubky_switch_core::message::Message;
    use pubky_switch_core::records::AppCert;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A client custody fixture: normal Paykit checks, but one request is not admitted.
    /// The SDK itself has no producer of this outcome.
    struct Waiting {
        inner: Arc<paykit_sdk::PaykitProtocol>,
        msg_id: String,
        calls: Arc<AtomicUsize>,
    }

    impl Protocol for Waiting {
        fn name(&self) -> String {
            self.inner.name()
        }

        fn opaque_members(&self) -> Vec<OpaqueRule> {
            self.inner.opaque_members()
        }

        fn has_keys(&self, scope: &str) -> bool {
            self.inner.has_keys(scope)
        }

        fn sent_between_own_apps(&self, kind: &str) -> bool {
            self.inner.sent_between_own_apps(kind)
        }

        fn check_scopes(
            &self,
            message: &Message,
            sender: &AppCert,
            own: &AppCert,
        ) -> Result<(), String> {
            self.inner.check_scopes(message, sender, own)
        }

        fn accepts(&self, cert: &AppCert) -> Vec<String> {
            self.inner.accepts(cert)
        }

        fn check_body(&self, message: &ReceivedMessage) -> Result<(), String> {
            self.inner.check_body(message)
        }

        fn receive(&self, message: &ReceivedMessage, cx: &mut ReceiveContext<'_>) -> Outcome {
            if message.message.msg_id == self.msg_id {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Outcome::AwaitAdmission
            } else {
                self.inner.receive(message, cx)
            }
        }

        fn resend_needed(&self, needed: &ResendNeeded) -> Result<Vec<LocalWrite>, StoreFailure> {
            self.inner.resend_needed(needed)
        }

        fn missed(&self, missed: &MissedEntries) -> Result<Vec<LocalWrite>, StoreFailure> {
            self.inner.missed(missed)
        }

        fn collections(&self) -> Vec<Collection> {
            self.inner.collections()
        }

        fn record_seen(
            &self,
            collection: &str,
            seen: Option<&[u8]>,
            value: Option<&[u8]>,
        ) -> Option<Vec<u8>> {
            self.inner.record_seen(collection, seen, value)
        }
    }

    async fn keep_waiting(
        w: &World,
        wallet: &mut Paykit,
        storage: Arc<dyn pubky_switch_client::store::StateStore>,
        msg_id: &str,
        calls: Arc<AtomicUsize>,
    ) {
        let protocol = Arc::new(Waiting {
            inner: Arc::clone(wallet.protocol()),
            msg_id: msg_id.to_owned(),
            calls,
        });
        let app = App::load(
            w.env(&w.alice, storage),
            protocol,
            PaykitConfig::default().app,
            wallet.app_id(),
        )
        .await
        .unwrap();
        // Keep this test-only callback installed while the SDK handles its real duties.
        // No call is made through the superseded handle after the new owner is acquired.
        drop(std::mem::replace(wallet.app_mut(), app));
    }

    #[tokio::test]
    async fn test_stored_admission_custody_and_drop_do_not_authorize_a_later_write() {
        use pubky_switch_client::app::WaitingReason;

        let w = World::new();
        let storage = store();
        let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
        let app_id = wallet.app_id();
        let ns = pubky_switch_client::store::client_namespace(&app_id);
        let mut shop = w.shop().await;
        let accepted = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
        wallet.receive(None).await.unwrap();
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.request(&accepted).await.unwrap().status,
            RequestStatus::Open
        );
        let processed = storage.scan(&ns, b"proc/", None, 100).unwrap();
        assert!(!processed.is_empty());
        assert!(kept(&wallet, "se/").await.is_empty());
        assert!(kept(&wallet, "dt/").await.is_empty());

        let mut proposal = draft("2", w.clock.now_ms());
        proposal.proposal_expires_at_ms = None;
        let sent = shop.send_request(w.alice_id(), proposal).await.unwrap();
        let request = sent.request.unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        keep_waiting(
            &w,
            &mut wallet,
            Arc::clone(&storage),
            &sent.msg_id,
            Arc::clone(&calls),
        )
        .await;
        let report = wallet.app_mut().receive(None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let stored_id = report
            .events
            .iter()
            .find_map(|event| match event {
                AppEvent::Parked {
                    id: None,
                    stored_id,
                    reason: ParkReason::AwaitingAdmission,
                    ..
                } => Some(*stored_id),
                _ => None,
            })
            .expect("the client reported informational admission custody");
        let entry = wallet
            .app_mut()
            .parked_entry(stored_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.plaintext.hash(), request.request_hash);
        assert_eq!(entry.message.as_ref().unwrap().msg_id, sent.msg_id);
        let accepted_at = entry.accepted_at_ms;
        assert!(wallet
            .app_mut()
            .unacknowledged_events_for("paykit")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(storage.scan(&ns, b"proc/", None, 100).unwrap(), processed);

        for pass in 0..2 {
            if pass == 1 {
                drop(wallet);
                wallet = w
                    .load_with(
                        &w.alice,
                        Arc::clone(&storage),
                        app_id,
                        Arc::clone(&w.clock),
                        PaykitConfig::default(),
                    )
                    .await;
                keep_waiting(
                    &w,
                    &mut wallet,
                    Arc::clone(&storage),
                    &sent.msg_id,
                    Arc::clone(&calls),
                )
                .await;
            }
            wallet.receive(None).await.unwrap();
            assert!(
                kept(&wallet, "se/").await.is_empty(),
                "custody must not create a later-write note"
            );
            assert!(kept(&wallet, "dt/").await.is_empty());
            assert!(wallet
                .app_mut()
                .unacknowledged_events_for("paykit")
                .await
                .unwrap()
                .is_empty());
            let entry = wallet
                .app_mut()
                .parked_entry(stored_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(entry.reason, ParkReason::AwaitingAdmission);
            assert_eq!(entry.plaintext.hash(), request.request_hash);
            assert_eq!(storage.scan(&ns, b"proc/", None, 100).unwrap(), processed);
            assert!(wallet.request(&request).await.is_err());
            assert_eq!(
                wallet.request(&accepted).await.unwrap().status,
                RequestStatus::Open
            );
        }
        assert!(calls.load(Ordering::SeqCst) >= 3);

        // A verified service time, not the host clock alone, expires the real custody.
        w.clock
            .set_ms(accepted_at + pubky_switch_client::app::MAX_UNVERIFIED_AGE_MS + DAY_MS);
        wallet.app_mut().receive(None).await.unwrap();
        wallet.app_mut().receive(None).await.unwrap();
        let duties = wallet
            .app_mut()
            .unacknowledged_events_for("paykit")
            .await
            .unwrap();
        let dropped = duties
            .iter()
            .find(|event| {
                matches!(event,
                    AppEvent::StoredEntryDropped { waiting: WaitingReason::Admission, message, .. }
                    if message.msg_id == sent.msg_id
                )
            })
            .expect("expiry creates the owned admission drop duty");
        let drop_id = dropped.id().unwrap();
        assert_eq!(wallet.app().event_owners(drop_id).unwrap(), ["paykit"]);
        assert!(wallet
            .app_mut()
            .parked_entry(stored_id)
            .await
            .unwrap()
            .is_none());
        assert_eq!(storage.scan(&ns, b"proc/", None, 100).unwrap(), processed);
        wallet.receive(None).await.unwrap();
        assert!(
            kept(&wallet, "ud/").await.is_empty(),
            "an admission drop is not an unverified sender loss"
        );
        assert!(kept(&wallet, "kw/").await.is_empty());
        assert!(kept(&wallet, "se/").await.is_empty());
        assert!(kept(&wallet, "dt/").await.is_empty());
        assert!(wallet
            .app_mut()
            .unacknowledged_events_for("paykit")
            .await
            .unwrap()
            .iter()
            .any(|event| event.id() == Some(drop_id)));

        // A later supported duty is taken once, but cannot acknowledge past the earlier
        // admission drop. The request and the refused effect are both genuine producers.
        let later = request_from(&mut shop, w.alice_id(), draft("3", w.clock.now_ms())).await;
        refuse_writes(&w.alice, 1);
        wallet.app_mut().receive(None).await.unwrap();
        let duties = wallet
            .app_mut()
            .unacknowledged_events_for("paykit")
            .await
            .unwrap();
        let (refused_id, refused_stored) = duties
            .iter()
            .find_map(|event| match event {
                AppEvent::EffectRefused { id, stored_id, .. } => Some((*id, *stored_id)),
                _ => None,
            })
            .expect("the later record effect was refused");
        let parked_id = duties
            .iter()
            .find_map(|event| match event {
                AppEvent::Parked {
                    id: Some(id),
                    stored_id,
                    reason: ParkReason::Refused,
                    ..
                } if *stored_id == refused_stored => Some(*id),
                _ => None,
            })
            .expect("the refused effect also reports its stored entry");
        let refused_entry = wallet
            .app_mut()
            .parked_entry(refused_stored)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refused_entry.plaintext.hash(), later.request_hash);
        let later_msg_id = refused_entry.message.as_ref().unwrap().msg_id.clone();
        assert_eq!(
            drop_id.cmp_same_store(&refused_id),
            Some(std::cmp::Ordering::Less)
        );
        wallet.receive(None).await.unwrap();
        assert_eq!(
            wallet.request(&later).await.unwrap().status,
            RequestStatus::Open
        );
        let target = wallet.request(&later).await.unwrap().record;
        let record = wallet.app().shared_record(&target).await.unwrap().unwrap();
        let marks = wallet.app().protocol_scan("paykit", b"dt/").await.unwrap();
        let mut expected_marks = [
            format!("dt/{parked_id}").into_bytes(),
            format!("dt/{refused_id}").into_bytes(),
        ];
        expected_marks.sort();
        assert_eq!(
            marks.iter().map(|(key, _)| key.clone()).collect::<Vec<_>>(),
            expected_marks
        );
        // The SDK acknowledges the stored entry's events it took, one by one, past the
        // admission drop left to the host: the entry's placed note goes with them, and the
        // successful write is never repeated.
        let _ = later_msg_id;
        assert!(wallet
            .app()
            .protocol_scan("paykit", b"se/")
            .await
            .unwrap()
            .is_empty());
        let processed_after = storage.scan(&ns, b"proc/", None, 100).unwrap();
        let _ = processed;
        for pass in 0..2 {
            if pass == 1 {
                drop(wallet);
                wallet = w
                    .load_with(
                        &w.alice,
                        Arc::clone(&storage),
                        app_id,
                        Arc::clone(&w.clock),
                        PaykitConfig::default(),
                    )
                    .await;
            }
            wallet.receive(None).await.unwrap();
            let duties = wallet
                .app_mut()
                .unacknowledged_events_for("paykit")
                .await
                .unwrap();
            assert!(duties.iter().any(|event| event.id() == Some(drop_id)));
            assert!(duties.iter().all(|event| event.id() != Some(refused_id)));
            assert!(duties.iter().all(|event| event.id() != Some(parked_id)));
            assert!(wallet
                .app()
                .protocol_scan("paykit", b"dt/")
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                wallet.app().shared_record(&target).await.unwrap().unwrap(),
                record
            );
            assert!(wallet
                .app()
                .protocol_scan("paykit", b"se/")
                .await
                .unwrap()
                .is_empty());
            assert!(kept(&wallet, "ud/").await.is_empty());
            assert_eq!(
                storage.scan(&ns, b"proc/", None, 100).unwrap(),
                processed_after
            );
            assert!(wallet.request(&request).await.is_err());
        }
    }
}
