//! Recovery checkpoints keep their current complete shape (Paykit spec 7.2, 9;
//! Switch spec 12.4), without changing which old restore-local values are discarded.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use paykit_sdk::{AppScopes, Paykit, PaykitBackup, PaykitConfig, Rail};
use pubky_switch_client::app::{AppError, LocalWrite};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{
    Fault, FaultStore, KeyValue, MemoryStore, StateStore, Write, WriteBatch,
};
use serde_json::{json, Value};

use crate::common::{draft, request_from, store, World};

#[derive(Clone, Copy)]
enum Family {
    RestoreHold,
    MissedHold,
    OwnResend,
    HeldAnswer,
    MissedItem,
}

impl Family {
    fn prefix(self) -> &'static [u8] {
        match self {
            Self::RestoreHold => b"rh",
            Self::MissedHold => b"mh",
            Self::OwnResend => b"ro",
            Self::HeldAnswer => b"ha/",
            Self::MissedItem => b"mi/",
        }
    }
}

#[derive(Clone, Copy)]
enum Damage {
    Unknown,
    MissingWaiting,
    MissingTime,
    Duplicate,
}

async fn restore(w: &World, storage: Arc<dyn StateStore>, backup: &PaykitBackup) -> Paykit {
    Paykit::restore(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap()
}

async fn produced(w: &World, family: Family) -> (Paykit, Arc<dyn StateStore>, Vec<u8>, Vec<u8>) {
    let mut storage = store();
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
    match family {
        Family::RestoreHold | Family::OwnResend => {
            let backup = wallet.backup().await.unwrap();
            storage = store();
            wallet = restore(w, Arc::clone(&storage), &backup).await;
        }
        Family::MissedHold | Family::MissedItem => {
            if matches!(family, Family::MissedHold) {
                let mut shop = w.shop().await;
                request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
            }
            wallet.receive(None).await.unwrap();
            let now = w.clock.now_ms();
            w.alice.switch().push_missed(1, now - 1_000, now);
            // Run the real callback, leaving its checkpoints for the SDK's next pass.
            wallet.app_mut().receive(None).await.unwrap();
            if matches!(family, Family::MissedHold) {
                wallet.receive(None).await.unwrap();
                assert_eq!(wallet.requests().await.unwrap().len(), 1);
            }
        }
        Family::HeldAnswer => {
            let mut shop = w.shop().await;
            let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
            wallet.receive(None).await.unwrap();
            wallet.block(&w.bob_id()).await.unwrap();
            wallet.reject_request(&request, None).await.unwrap();
        }
    }
    let values = wallet.app().protocol_scan(family.prefix()).await.unwrap();
    assert_eq!(
        values.len(),
        1,
        "the actual producer must retain its checkpoint"
    );
    let (key, value) = values.into_iter().next().unwrap();
    let body: Value = serde_json::from_slice(&value).unwrap();
    assert!(body["d"].is_object());
    if matches!(family, Family::RestoreHold) {
        assert_eq!(body["d"]["asks_queued"], false);
        assert_eq!(body["d"]["asked"], json!([]));
    }
    if matches!(family, Family::OwnResend) {
        assert!(body["d"].get("waiting").is_some());
        assert!(body["d"].get("restored_at_ms").is_some());
        assert!(body["d"]["waiting"].is_null());
        assert!(body["d"]["restored_at_ms"].is_u64());
    }
    (wallet, storage, key, value)
}

fn damage(bytes: &[u8], how: Damage) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    let body = value["d"].as_object_mut().unwrap();
    match how {
        Damage::Unknown => {
            body.insert("extra".into(), json!({"nested": [7]}));
        }
        Damage::MissingWaiting => {
            body.remove("waiting").unwrap();
        }
        Damage::MissingTime => {
            body.remove("restored_at_ms").unwrap();
        }
        Damage::Duplicate => {
            let (name, member) = body.iter().next().unwrap();
            let start = format!("\"d\":{{\"{name}\":{member},");
            return String::from_utf8(bytes.to_vec())
                .unwrap()
                .replacen("\"d\":{", &start, 1)
                .into_bytes();
        }
    }
    serde_json::to_vec(&value).unwrap()
}

fn replace(backup: &PaykitBackup, changes: &[(Vec<u8>, Vec<u8>)]) -> PaykitBackup {
    let mut value: Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let data = pubky_switch_core::enc::unb64(value["protocol"]["data"].as_str().unwrap()).unwrap();
    let mut layer = pubky_switch_client::app::decode_local_state(&data).unwrap();
    for (key, bytes) in changes {
        layer.iter_mut().find(|(k, _)| k == key).unwrap().1 = bytes.clone();
    }
    value["protocol"]["data"] = Value::String(pubky_switch_core::enc::b64(
        &pubky_switch_client::app::encode_local_state(&layer),
    ));
    PaykitBackup::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap()
}

async fn refuse_current(family: Family, how: Damage) {
    let w = World::new();
    let (mut wallet, storage, key, good) = Box::pin(produced(&w, family)).await;
    let bad = damage(&good, how);
    assert_ne!(bad, good);
    wallet
        .app_mut()
        .protocol_commit(&[LocalWrite::Put {
            key: key.clone(),
            value: bad.clone(),
        }])
        .await
        .unwrap();
    let sends = w.bob.switch().entries_from(&w.alice_id());
    // With a current cursor, load leaves these checkpoints unread. The public
    // recovery pass reads rh/ro/ha/mi; an open request's status reads mh.
    let mut loaded = Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        wallet.app_id(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .expect("load must preserve this checkpoint's lazy read boundary");
    let error = if matches!(family, Family::MissedHold) {
        loaded.receive(None).await.unwrap();
        loaded
            .requests()
            .await
            .expect_err("malformed missed hold was read")
    } else {
        loaded
            .receive(None)
            .await
            .expect_err("malformed recovery state was read")
    };
    assert!(crate::common::is_corrupt(&error), "{error:?}");
    assert_eq!(wallet.app().protocol_get(&key).await.unwrap(), Some(bad));
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), sends);
    assert!(w.rail.all_executions().is_empty());
}

async fn refuse_backup(family: Family, how: Damage) {
    let w = World::new();
    let (mut wallet, _, key, good) = Box::pin(produced(&w, family)).await;
    let backup = replace(
        &wallet.backup().await.unwrap(),
        &[(key, damage(&good, how))],
    );
    let target = Arc::new(FaultStore::new(store(), Fault::None));
    let result = Paykit::restore(
        w.env(&w.alice, Arc::clone(&target) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    let Err(error) = result else {
        panic!("malformed retained checkpoint was restored")
    };
    assert!(
        matches!(
            error,
            paykit_sdk::PaykitError::Client(AppError::InvalidData(_))
        ),
        "{error:?}"
    );
    assert_eq!(target.commits(), 0);
    assert_eq!(
        wallet
            .app()
            .protocol_scan(family.prefix())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_recovery_state_restore_hold_unknown_after_reload() {
    Box::pin(refuse_current(Family::RestoreHold, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_missed_hold_unknown_after_reload() {
    Box::pin(refuse_current(Family::MissedHold, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_own_resend_unknown_after_reload() {
    Box::pin(refuse_current(Family::OwnResend, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_held_answer_unknown_after_reload() {
    Box::pin(refuse_current(Family::HeldAnswer, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_missed_item_unknown_after_reload() {
    Box::pin(refuse_current(Family::MissedItem, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_held_answer_unknown_at_restore() {
    Box::pin(refuse_backup(Family::HeldAnswer, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_held_answer_duplicate_at_restore() {
    Box::pin(refuse_backup(Family::HeldAnswer, Damage::Duplicate)).await;
}

#[tokio::test]
async fn test_recovery_state_missed_item_unknown_at_restore() {
    Box::pin(refuse_backup(Family::MissedItem, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_recovery_state_missed_item_duplicate_at_restore() {
    Box::pin(refuse_backup(Family::MissedItem, Damage::Duplicate)).await;
}

#[tokio::test]
async fn test_recovery_state_own_resend_missing_waiting_after_reload() {
    Box::pin(refuse_current(Family::OwnResend, Damage::MissingWaiting)).await;
}

#[tokio::test]
async fn test_recovery_state_own_resend_missing_time_after_reload() {
    Box::pin(refuse_current(Family::OwnResend, Damage::MissingTime)).await;
}

#[tokio::test]
async fn test_recovery_state_produced_values_reload_and_progress() {
    for family in [
        Family::RestoreHold,
        Family::MissedHold,
        Family::OwnResend,
        Family::HeldAnswer,
        Family::MissedItem,
    ] {
        let w = World::new();
        let (wallet, storage, key, good) = Box::pin(produced(&w, family)).await;
        let mut loaded = Paykit::load(
            w.env(&w.alice, storage),
            PaykitConfig::default(),
            wallet.app_id(),
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .unwrap();
        assert_eq!(
            loaded.app().protocol_get(&key).await.unwrap(),
            Some(good.clone())
        );
        loaded.receive(None).await.unwrap();
        if matches!(family, Family::RestoreHold) {
            let held: Value =
                serde_json::from_slice(&loaded.app().protocol_get(&key).await.unwrap().unwrap())
                    .unwrap();
            assert_eq!(held["d"]["asks_queued"], true);
            assert_eq!(held["d"]["asked"], json!([]));
        }
        if matches!(family, Family::HeldAnswer) {
            let body: Value = serde_json::from_slice(&good).unwrap();
            let msg_id = body["d"]["msg_id"].as_str().unwrap();
            assert!(loaded.app().delivery(msg_id).await.unwrap().is_none());
            loaded.unblock(&w.bob_id()).await.unwrap();
            loaded.receive(None).await.unwrap();
            assert!(loaded.app().delivery(msg_id).await.unwrap().is_some());
            assert!(loaded.app().protocol_get(&key).await.unwrap().is_none());
        }
        assert!(w.rail.all_executions().is_empty());
    }
}

#[tokio::test]
async fn test_recovery_state_retained_values_restore_unchanged() {
    for family in [Family::HeldAnswer, Family::MissedItem] {
        let w = World::new();
        let (mut wallet, _, key, good) = Box::pin(produced(&w, family)).await;
        let backup = wallet.backup().await.unwrap();
        let loaded = restore(&w, store(), &backup).await;
        assert_eq!(loaded.app().protocol_get(&key).await.unwrap(), Some(good));
    }
}

#[tokio::test]
async fn test_recovery_state_restore_regenerates_old_checkpoints() {
    let w = World::new();
    let (mut wallet, _, _, _) = Box::pin(produced(&w, Family::RestoreHold)).await;
    let now = w.clock.now_ms();
    w.alice.switch().push_missed(1, now - 1_000, now);
    wallet.app_mut().receive(None).await.unwrap();
    let mut changes = Vec::new();
    for key in [b"rh", b"mh", b"ro"] {
        assert!(wallet.app().protocol_get(key).await.unwrap().is_some());
        changes.push((key.to_vec(), b"{".to_vec()));
    }
    let backup = replace(&wallet.backup().await.unwrap(), &changes);
    let loaded = restore(&w, store(), &backup).await;
    let hold: Value =
        serde_json::from_slice(&loaded.app().protocol_get(b"rh").await.unwrap().unwrap()).unwrap();
    assert_eq!(hold["d"]["asks_queued"], false);
    assert_eq!(hold["d"]["asked"], json!([]));
    let owed: Value =
        serde_json::from_slice(&loaded.app().protocol_get(b"ro").await.unwrap().unwrap()).unwrap();
    assert!(owed["d"].get("waiting").unwrap().is_null());
    assert!(owed["d"].get("restored_at_ms").unwrap().is_u64());
    assert!(loaded.app().protocol_get(b"mh").await.unwrap().is_none());
}

#[derive(Default)]
struct RestoreStampStore {
    inner: MemoryStore,
    stop: AtomicBool,
    stopped: AtomicBool,
}

impl StateStore for RestoreStampStore {
    fn get(&self, ns: &str, key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<KeyValue>, StoreError> {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(&self, batch: WriteBatch) -> Result<(), StoreError> {
        let stamps = batch.writes.iter().any(|write| match write {
            Write::Put { key, value, .. } if key == b"p/ro" => {
                let body: Value = serde_json::from_slice(value).unwrap();
                body["d"]["restored_at_ms"].is_u64()
            }
            _ => false,
        });
        if stamps && self.stop.load(Ordering::SeqCst) {
            self.stopped.store(true, Ordering::SeqCst);
            return Err(StoreError::Unavailable);
        }
        self.inner.commit(batch)
    }
}

#[tokio::test]
async fn test_recovery_state_restore_commit_before_stamp_reloads() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let backup = wallet.backup().await.unwrap();
    let storage = Arc::new(RestoreStampStore::default());
    storage.stop.store(true, Ordering::SeqCst);
    let result = Paykit::restore(
        w.env(&w.alice, Arc::clone(&storage) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    assert!(matches!(
        result,
        Err(paykit_sdk::PaykitError::Client(AppError::Store(
            StoreError::Unavailable
        )))
    ));
    assert!(storage.stopped.load(Ordering::SeqCst));
    let mut loaded = Paykit::load(
        w.env(&w.alice, Arc::clone(&storage) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        wallet.app_id(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let bytes = loaded.app().protocol_get(b"ro").await.unwrap().unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["d"].get("waiting").unwrap().is_null());
    assert!(body["d"].get("restored_at_ms").unwrap().is_null());
    storage.stop.store(false, Ordering::SeqCst);
    loaded.receive(None).await.unwrap();
    assert!(loaded.app().protocol_get(b"ro").await.unwrap().is_none());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_recovery_state_restore_retains_sorted_waiting_counterparties() {
    let w = World::new();
    let (mut wallet, _, _, _) = Box::pin(produced(&w, Family::HeldAnswer)).await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    let mut shop = w
        .join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await;
    let request = request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.block(&w.carol_id()).await.unwrap();
    wallet.reject_request(&request, None).await.unwrap();
    let held = wallet.app().protocol_scan(b"ha/").await.unwrap();
    assert_eq!(held.len(), 2);
    let messages: Vec<String> = held
        .iter()
        .map(|(_, value)| {
            let body: Value = serde_json::from_slice(value).unwrap();
            body["d"]["msg_id"].as_str().unwrap().to_owned()
        })
        .collect();
    let backup = wallet.backup().await.unwrap();
    let storage = store();
    let mut restored = restore(&w, Arc::clone(&storage), &backup).await;
    restored.receive(None).await.unwrap();
    let bytes = restored.app().protocol_get(b"ro").await.unwrap().unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let mut waiting = vec![w.bob_id(), w.carol_id()];
    waiting.sort();
    assert_eq!(body["d"]["waiting"], json!(waiting));
    assert!(body["d"]["restored_at_ms"].is_u64());
    assert_eq!(restored.app().protocol_scan(b"ha/").await.unwrap(), held);
    drop(restored);
    let mut loaded = Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        wallet.app_id(),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(loaded.app().protocol_get(b"ro").await.unwrap(), Some(bytes));
    loaded.receive(None).await.unwrap();
    for msg_id in &messages {
        assert!(loaded.app().delivery(msg_id).await.unwrap().is_none());
    }
    for counterparty in waiting {
        loaded.unblock(&counterparty).await.unwrap();
    }
    loaded.receive(None).await.unwrap();
    for msg_id in &messages {
        assert!(loaded.app().delivery(msg_id).await.unwrap().is_some());
    }
    assert!(loaded.app().protocol_scan(b"ha/").await.unwrap().is_empty());
    assert!(loaded.app().protocol_get(b"ro").await.unwrap().is_none());
    let delivered = (
        w.bob.switch().entries_from(&w.alice_id()),
        w.carol.switch().entries_from(&w.alice_id()),
    );
    loaded.receive(None).await.unwrap();
    assert_eq!(
        (
            w.bob.switch().entries_from(&w.alice_id()),
            w.carol.switch().entries_from(&w.alice_id())
        ),
        delivered
    );
    assert!(w.rail.all_executions().is_empty());
}
