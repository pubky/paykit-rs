//! Recovery checkpoints keep their current complete shape (Paykit spec 7.2, 9;
//! Switch spec 12.4), without changing which old restore-local values are discarded.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use paykit_lib::records::{ActivityRecord, ActivitySide, StoredMessage};
use paykit_sdk::{AppScopes, Paykit, PaykitBackup, PaykitConfig, Rail};
use pubky_switch_client::app::{AppError, LocalWrite};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::error::StoreError;
use pubky_switch_client::store::{Fault, FaultStore, KeyValue, MemoryStore, StateStore, Write};
use pubky_switch_client::testing::{Seeds, TestIdentity, TestSigner};
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
    let values = wallet
        .app()
        .protocol_scan("paykit", family.prefix())
        .await
        .unwrap();
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
    let data = pubky_switch_core::enc::unb64(value["layers"][0]["data"].as_str().unwrap()).unwrap();
    let mut layer = pubky_switch_client::app::decode_local_state(&data).unwrap();
    for (key, bytes) in changes {
        layer.iter_mut().find(|(k, _)| k == key).unwrap().1 = bytes.clone();
    }
    value["layers"][0]["data"] = Value::String(pubky_switch_core::enc::b64(
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
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.clone(),
                value: bad.clone(),
            }],
        )
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
    assert_eq!(
        wallet.app().protocol_get("paykit", &key).await.unwrap(),
        Some(bad)
    );
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
            .protocol_scan("paykit", family.prefix())
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
            loaded.app().protocol_get("paykit", &key).await.unwrap(),
            Some(good.clone())
        );
        loaded.receive(None).await.unwrap();
        if matches!(family, Family::RestoreHold) {
            let held: Value = serde_json::from_slice(
                &loaded
                    .app()
                    .protocol_get("paykit", &key)
                    .await
                    .unwrap()
                    .unwrap(),
            )
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
            assert!(loaded
                .app()
                .protocol_get("paykit", &key)
                .await
                .unwrap()
                .is_none());
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
        assert_eq!(
            loaded.app().protocol_get("paykit", &key).await.unwrap(),
            Some(good)
        );
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
        assert!(wallet
            .app()
            .protocol_get("paykit", key)
            .await
            .unwrap()
            .is_some());
        changes.push((key.to_vec(), b"{".to_vec()));
    }
    let backup = replace(&wallet.backup().await.unwrap(), &changes);
    let loaded = restore(&w, store(), &backup).await;
    let hold: Value = serde_json::from_slice(
        &loaded
            .app()
            .protocol_get("paykit", b"rh")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(hold["d"]["asks_queued"], false);
    assert_eq!(hold["d"]["asked"], json!([]));
    let owed: Value = serde_json::from_slice(
        &loaded
            .app()
            .protocol_get("paykit", b"ro")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert!(owed["d"].get("waiting").unwrap().is_null());
    assert!(owed["d"].get("restored_at_ms").unwrap().is_u64());
    assert!(loaded
        .app()
        .protocol_get("paykit", b"mh")
        .await
        .unwrap()
        .is_none());
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
impl RestoreStampStore {
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
        let stamps = batch.writes().iter().any(|write| match write {
            Write::Put { key, value, .. } if key == b"p/paykit/ro" => {
                let body: Value = serde_json::from_slice(value).unwrap();
                body["d"]["restored_at_ms"].is_u64()
            }
            _ => false,
        });
        if stamps && self.stop.load(Ordering::SeqCst) {
            self.stopped.store(true, Ordering::SeqCst);
            return Err(StoreError::Unavailable);
        }
        apply(batch)
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
    let bytes = loaded
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["d"].get("waiting").unwrap().is_null());
    assert!(body["d"].get("restored_at_ms").unwrap().is_null());
    storage.stop.store(false, Ordering::SeqCst);
    loaded.receive(None).await.unwrap();
    assert!(loaded
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
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
    let held = wallet.app().protocol_scan("paykit", b"ha/").await.unwrap();
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
    let bytes = restored
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let mut waiting = vec![w.bob_id(), w.carol_id()];
    waiting.sort();
    assert_eq!(body["d"]["waiting"], json!(waiting));
    assert!(body["d"]["restored_at_ms"].is_u64());
    assert_eq!(
        restored
            .app()
            .protocol_scan("paykit", b"ha/")
            .await
            .unwrap(),
        held
    );
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
    assert_eq!(
        loaded.app().protocol_get("paykit", b"ro").await.unwrap(),
        Some(bytes)
    );
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
    assert!(loaded
        .app()
        .protocol_scan("paykit", b"ha/")
        .await
        .unwrap()
        .is_empty());
    assert!(loaded
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
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

async fn checkpoint_reload(w: &World, wallet: Paykit, storage: Arc<dyn StateStore>) -> Paykit {
    let app_id = wallet.app_id();
    drop(wallet);
    Paykit::load(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        app_id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .expect("a current cursor leaves the checkpoints for their ordinary readers")
}

async fn checkpoint_put(wallet: &mut Paykit, key: &[u8], value: &Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(value).unwrap();
    wallet
        .app_mut()
        .protocol_commit(
            "paykit",
            &[LocalWrite::Put {
                key: key.to_vec(),
                value: bytes.clone(),
            }],
        )
        .await
        .unwrap();
    bytes
}

async fn checkpoint_value(wallet: &Paykit, key: &[u8]) -> Value {
    serde_json::from_slice(
        &wallet
            .app()
            .protocol_get("paykit", key)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

async fn checkpoint_own_request(w: &World) -> (Paykit, Arc<dyn StateStore>, StoredMessage) {
    let mut shop = w
        .join(
            &w.alice,
            "shop",
            AppScopes::new()
                .request()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let mut wallet = w
        .join(&w.bob, "wallet", AppScopes::new().pay().build().unwrap())
        .await;
    wallet.app_mut().invite(&w.alice_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    let mut request = draft("1", w.clock.now_ms());
    request.proposal_expires_at_ms = None;
    w.alice
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::LoseAnswer {
            path: "/ops".into(),
        });
    let sent = shop.send_request(w.bob_id(), request).await.unwrap();
    assert!(matches!(
        sent.delivery,
        paykit_sdk::MessageDelivery::Kept { .. }
    ));
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    let backup = shop.backup().await.unwrap();
    drop(shop);
    let storage = store();
    let mut shop = restore(w, Arc::clone(&storage), &backup).await;
    shop.block(&w.bob_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    let activity = shop.payee_activity().await.unwrap();
    assert_eq!(activity.len(), 1);
    let held = shop
        .app()
        .shared_record(&activity[0].record)
        .await
        .unwrap()
        .unwrap();
    let record = ActivityRecord::read(
        held.content.value().unwrap(),
        &w.alice_id(),
        ActivitySide::Payee,
    )
    .unwrap();
    let original = record.request.unwrap();
    assert_eq!(original.message().msg_id, sent.msg_id);
    assert_eq!(
        activity[0].request_hash,
        Some(sent.request.unwrap().request_hash)
    );
    assert_eq!(original.sender(), w.alice_id());
    assert_eq!(original.cert().app_id(), shop.app_id());
    assert_eq!(original.message().kind, "paykit.payment_request");
    assert!(shop
        .app()
        .delivery(&original.message().msg_id)
        .await
        .unwrap()
        .is_none());
    assert!(shop
        .app()
        .protocol_scan("paykit", b"ha/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        checkpoint_value(&shop, b"ro").await["d"]["waiting"],
        json!([w.bob_id()])
    );
    (shop, storage, original)
}

#[tokio::test]
async fn test_recovery_checkpoint_empty_waiting_after_reload_refused() {
    let w = World::new();
    let (mut shop, storage, original) = Box::pin(checkpoint_own_request(&w)).await;
    let mut value = checkpoint_value(&shop, b"ro").await;
    value["d"]["waiting"] = json!([]);
    let bad = checkpoint_put(&mut shop, b"ro", &value).await;
    let mut shop = checkpoint_reload(&w, shop, storage).await;
    let error = shop
        .unblock(&w.bob_id())
        .await
        .expect_err("the first read before unblocking validates the resend subset");
    assert!(crate::common::is_corrupt(&error), "{error:?}");
    let error = shop
        .receive(None)
        .await
        .expect_err("an empty retained resend subset was read");
    assert!(crate::common::is_corrupt(&error), "{error:?}");
    assert_eq!(
        shop.app().protocol_get("paykit", b"ro").await.unwrap(),
        Some(bad)
    );
    assert!(shop
        .app()
        .delivery(&original.message().msg_id)
        .await
        .unwrap()
        .is_none());
    assert!(shop
        .app()
        .protocol_scan("paykit", b"ha/")
        .await
        .unwrap()
        .is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_recovery_checkpoint_own_request_resumes_same_signed_message() {
    let w = World::new();
    let (shop, storage, original) = Box::pin(checkpoint_own_request(&w)).await;
    let mut shop = checkpoint_reload(&w, shop, storage).await;
    shop.receive(None).await.unwrap();
    assert!(shop
        .app()
        .delivery(&original.message().msg_id)
        .await
        .unwrap()
        .is_none());
    shop.unblock(&w.bob_id()).await.unwrap();
    shop.receive(None).await.unwrap();
    let sent = shop.app().sent_to(&w.bob_id(), 0, u64::MAX).await.unwrap();
    let sent = sent
        .iter()
        .find(|sent| sent.msg_id == original.message().msg_id)
        .unwrap();
    assert_eq!(&sent.plaintext, original.plaintext());
    assert!(shop
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
    let before = w.bob.switch().entries_from(&w.alice_id());
    shop.receive(None).await.unwrap();
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), before);
    assert!(w.rail.all_executions().is_empty());
}

struct PartialAsks {
    wallet: Paykit,
    storage: Arc<dyn StateStore>,
    asked: Vec<[u8; 32]>,
    missing: [u8; 32],
}

async fn checkpoint_partial_asks(w: &World) -> PartialAsks {
    let dave = TestSigner::at(
        TestIdentity::new(&Seeds::default(), "dave"),
        "https://dave-switch.example",
        Arc::clone(&w.clock),
        Arc::clone(&w.homeserver),
        crate::common::spec(),
    );
    w.router.add_switch(dave.origin(), dave.switch());
    let missing = dave.identity().public();
    let mut wallet = w.wallet("wallet").await;
    wallet.app_mut().invite(&w.carol_id()).await.unwrap();
    wallet.app_mut().invite(&missing).await.unwrap();
    let mut phone = w.wallet("phone").await;
    let mut shops = vec![
        w.shop().await,
        w.join(
            &w.carol,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await,
        w.join(&dave, "shop", AppScopes::new().request().build().unwrap())
            .await,
    ];
    let mut requests = Vec::new();
    for shop in &mut shops {
        requests.push(request_from(shop, w.alice_id(), draft("1", w.clock.now_ms())).await);
    }
    wallet.receive(None).await.unwrap();
    assert_eq!(wallet.requests().await.unwrap().len(), 3);
    phone.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let app_id = wallet.app_id();
    let cert = wallet.app().cert().clone();
    drop(wallet);
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    crate::unrequested::store_bundle(w, phone.app_id(), &phone.app().cert().clone());
    phone.receive(None).await.unwrap();
    phone.receive(None).await.unwrap();
    let value = crate::outcome_record::record_value(&mut phone, &requests[2])
        .await
        .unwrap();
    crate::outcome_record::phone_writes(
        &mut phone,
        &requests[2],
        Some(&serde_json::to_vec(&value).unwrap()),
    )
    .await;
    w.alice.next_doc(|body| body.data_epoch += 1).unwrap();
    let newest = w.alice.switch().identity_doc().unwrap().data_epoch();
    crate::unrequested::store_bundle_from(w, app_id, &cert, newest);
    let storage = store();
    let mut wallet = restore(w, Arc::clone(&storage), &backup).await;
    for _ in 0..2 {
        wallet.receive(None).await.unwrap();
    }
    let key = wallet
        .app()
        .record_key(&crate::outcome_record::payer_record(&requests[2]))
        .unwrap();
    let held = wallet.app().shared_record(&key).await.unwrap().unwrap();
    assert!(matches!(
        held.content,
        pubky_switch_client::app::RecordContent::Unreadable(
            pubky_switch_client::app::Unopened::KeyNotHeld { .. }
        )
    ));
    let mut asked = vec![w.bob_id(), w.carol_id()];
    asked.sort();
    let hold = checkpoint_value(&wallet, b"rh").await;
    assert_eq!(hold["d"]["asks_queued"], false);
    assert_eq!(hold["d"]["asked"], json!(asked));
    for peer in &asked {
        assert_eq!(crate::common::asked(&wallet, peer).await.len(), 1);
    }
    assert!(crate::common::asked(&wallet, &missing).await.is_empty());
    PartialAsks {
        wallet,
        storage,
        asked,
        missing,
    }
}

#[derive(Clone, Copy)]
enum AskedDamage {
    Completed,
    Duplicate,
    Descending,
}

fn checkpoint_damage_asks(value: &mut Value, how: AskedDamage) {
    match how {
        AskedDamage::Completed => value["d"]["asks_queued"] = json!(true),
        AskedDamage::Duplicate => {
            let asked = value["d"]["asked"].as_array_mut().unwrap();
            asked.insert(1, asked[0].clone());
        }
        AskedDamage::Descending => value["d"]["asked"].as_array_mut().unwrap().reverse(),
    }
}

async fn checkpoint_refuse_asks(how: AskedDamage) {
    let w = World::new();
    let PartialAsks {
        mut wallet,
        storage,
        asked,
        missing,
    } = Box::pin(checkpoint_partial_asks(&w)).await;
    let mut value = checkpoint_value(&wallet, b"rh").await;
    checkpoint_damage_asks(&mut value, how);
    let bad = checkpoint_put(&mut wallet, b"rh", &value).await;
    let wallet = checkpoint_reload(&w, wallet, storage).await;
    let error = wallet
        .requests()
        .await
        .expect_err("an inconsistent restore checkpoint was read");
    assert!(crate::common::is_corrupt(&error), "{error:?}");
    assert_eq!(
        wallet.app().protocol_get("paykit", b"rh").await.unwrap(),
        Some(bad)
    );
    for peer in asked {
        assert_eq!(crate::common::asked(&wallet, &peer).await.len(), 1);
    }
    assert!(crate::common::asked(&wallet, &missing).await.is_empty());
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_recovery_checkpoint_completed_asked_after_reload_refused() {
    Box::pin(checkpoint_refuse_asks(AskedDamage::Completed)).await;
}

#[tokio::test]
async fn test_recovery_checkpoint_duplicate_asked_after_reload_refused() {
    Box::pin(checkpoint_refuse_asks(AskedDamage::Duplicate)).await;
}

#[tokio::test]
async fn test_recovery_checkpoint_descending_asked_after_reload_refused() {
    Box::pin(checkpoint_refuse_asks(AskedDamage::Descending)).await;
}

#[tokio::test]
async fn test_recovery_checkpoint_partial_asks_finish_after_reload() {
    let w = World::new();
    let PartialAsks {
        wallet,
        storage,
        asked,
        missing,
    } = Box::pin(checkpoint_partial_asks(&w)).await;
    let original = wallet
        .app()
        .protocol_get("paykit", b"rh")
        .await
        .unwrap()
        .unwrap();
    let mut wallet = checkpoint_reload(&w, wallet, storage).await;
    assert_eq!(
        wallet.app().protocol_get("paykit", b"rh").await.unwrap(),
        Some(original)
    );
    wallet.receive(None).await.unwrap();
    for peer in &asked {
        assert_eq!(crate::common::asked(&wallet, peer).await.len(), 1);
    }
    crate::unrequested::store_bundle(&w, wallet.app_id(), &wallet.app().cert().clone());
    for _ in 0..3 {
        wallet.receive(None).await.unwrap();
    }
    let hold = checkpoint_value(&wallet, b"rh").await;
    assert_eq!(hold["d"]["asks_queued"], true);
    assert_eq!(hold["d"]["asked"], json!([]));
    for peer in asked.into_iter().chain([missing]) {
        assert_eq!(crate::common::asked(&wallet, &peer).await.len(), 1);
    }
    assert!(w.rail.all_executions().is_empty());
}

async fn checkpoint_regenerated(w: &World, backup: &PaykitBackup) {
    let mut wallet = restore(w, store(), backup).await;
    let hold = checkpoint_value(&wallet, b"rh").await;
    assert_eq!(hold["d"]["asks_queued"], false);
    assert_eq!(hold["d"]["asked"], json!([]));
    let owed = checkpoint_value(&wallet, b"ro").await;
    assert!(owed["d"]["waiting"].is_null());
    assert!(owed["d"]["restored_at_ms"].is_u64());
    wallet.receive(None).await.unwrap();
    assert!(wallet
        .app()
        .protocol_get("paykit", b"ro")
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_recovery_checkpoint_malformed_live_transients_remain_backable() {
    let w = World::new();
    let (mut wallet, _, _, _) = Box::pin(produced(&w, Family::RestoreHold)).await;
    let mut hold = checkpoint_value(&wallet, b"rh").await;
    hold["d"]["asks_queued"] = json!(true);
    hold["d"]["asked"] = json!([w.bob_id()]);
    let rh = checkpoint_put(&mut wallet, b"rh", &hold).await;
    let mut owed = checkpoint_value(&wallet, b"ro").await;
    owed["d"]["waiting"] = json!([]);
    let ro = checkpoint_put(&mut wallet, b"ro", &owed).await;
    let backup = wallet.backup().await.unwrap();
    let value: Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let data = pubky_switch_core::enc::unb64(value["layers"][0]["data"].as_str().unwrap()).unwrap();
    let layer = pubky_switch_client::app::decode_local_state(&data).unwrap();
    for (key, bytes) in [(b"rh", rh), (b"ro", ro)] {
        assert_eq!(layer.iter().find(|(k, _)| k == key).unwrap().1, bytes);
    }
    Box::pin(checkpoint_regenerated(&w, &backup)).await;
}

#[tokio::test]
async fn test_recovery_checkpoint_malformed_saved_transients_regenerate() {
    let w = World::new();
    let (mut wallet, _, _, _) = Box::pin(produced(&w, Family::RestoreHold)).await;
    let backup = wallet.backup().await.unwrap();
    let mut hold = checkpoint_value(&wallet, b"rh").await;
    hold["d"]["asked"] = json!([w.carol_id(), w.bob_id(), w.bob_id()]);
    let mut owed = checkpoint_value(&wallet, b"ro").await;
    owed["d"]["waiting"] = json!([]);
    let changed = replace(
        &backup,
        &[
            (b"rh".to_vec(), serde_json::to_vec(&hold).unwrap()),
            (b"ro".to_vec(), serde_json::to_vec(&owed).unwrap()),
        ],
    );
    Box::pin(checkpoint_regenerated(&w, &changed)).await;
}
