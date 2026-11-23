//! Stored discovery inputs retain their complete current shape (Paykit spec 4, 8.4;
//! Paykit wire 4.14; Switch spec 4.2, 12.4).

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_lib::bodies::{ListEndpoint, PrivatePaymentList, PrivatePaymentListRequest};
use paykit_lib::PaymentEndpointPayload;
use paykit_sdk::{
    PayOptions, Paykit, PaykitBackup, PaykitConfig, PaykitError, Rail, Refusal, UnrequestedPayment,
};
use pubky_switch_client::app::{AppError, LocalWrite, UserConfirmed};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_core::enc::b64;
use pubky_switch_core::records::app_record_path;
use serde_json::{json, Value};

use crate::common::{btc, endpoint, is_corrupt, list_version, store, World, HOUR_MS};

#[derive(Clone, Copy)]
enum Family {
    Records,
    GenericAsk,
    AppAsk,
    HeldAsk,
}

#[derive(Clone, Copy)]
enum Damage {
    Unknown,
    EmptyRecords,
    MissingApp,
    MissingVersion,
    VersionWithoutApp,
}

struct Held {
    wallet: Paykit,
    storage: Arc<dyn StateStore>,
    family: Family,
    app: [u8; 16],
    key: Vec<u8>,
    bytes: Vec<u8>,
}

impl Held {
    async fn reload(&mut self, w: &World) {
        self.wallet = Paykit::load(
            w.env(&w.alice, Arc::clone(&self.storage)),
            PaykitConfig::default(),
            self.wallet.app_id(),
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .expect("discovery values retain their lazy read boundary");
    }

    async fn put(&mut self, bytes: Vec<u8>) {
        self.wallet
            .app_mut()
            .protocol_commit(
                "paykit",
                &[LocalWrite::Put {
                    key: self.key.clone(),
                    value: bytes,
                }],
            )
            .await
            .unwrap();
    }

    async fn read(&mut self, w: &World) -> Result<(), PaykitError> {
        match self.family {
            Family::Records => self
                .wallet
                .public_endpoints(&w.bob_id(), None)
                .await
                .map(|_| ()),
            _ => self
                .wallet
                .pay_without_request(payment(w, self.family, self.app))
                .await
                .outcome
                .map(|_| ()),
        }
    }
}

fn payment(w: &World, family: Family, app: [u8; 16]) -> UnrequestedPayment {
    let mut options = PayOptions::default().with_endpoint(endpoint("btc-lightning-bolt12"));
    if matches!(family, Family::AppAsk | Family::HeldAsk) {
        options = options.with_list_app(app);
    }
    UnrequestedPayment::new(w.bob_id(), btc("0.001")).with_options(options)
}

async fn list(w: &World, wallet: &mut Paykit, app: [u8; 16], version: paykit_lib::ListVersion) {
    let key = format!("lq/{}/{}", b64(&w.bob_id()), b64(&app));
    let asked = wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap();
    let body = PrivatePaymentList {
        version,
        payment_endpoints: BTreeMap::from([(
            endpoint("btc-lightning-bolt12"),
            ListEndpoint {
                payload: PaymentEndpointPayload::new("lno1discovery").unwrap(),
                single_use: false,
            },
        )]),
        expires_at: w.clock.now_ms() + 1_000,
    }
    .to_body()
    .unwrap();
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[wallet.app_id()],
        "paykit.private_payment_list",
        Vec::new(),
        body,
    );
    assert_eq!(wallet.receive(None).await.unwrap().applied.len(), 1);
    assert_eq!(
        wallet
            .app()
            .protocol_get("paykit", key.as_bytes())
            .await
            .unwrap(),
        asked
    );
    w.clock.advance_ms(1_001);
}

async fn produced(w: &World, family: Family) -> Held {
    let storage = store();
    let mut wallet = w.wallet_on("wallet", Arc::clone(&storage)).await;
    let mut shop = w.shop().await;
    let app = shop.app_id();
    shop.app_mut().invite(&w.alice_id()).await.unwrap();
    let signed_record = if matches!(family, Family::Records) {
        let signed = crate::public::publish(
            w,
            json!({"public_endpoints": {"btc-lightning-bolt12": "lno1discovery"}}),
            Some(w.clock.now_ms() + 24 * HOUR_MS),
        );
        let endpoints = wallet.public_endpoints(&w.bob_id(), None).await.unwrap();
        assert!(endpoints.failures.is_empty());
        assert_eq!(endpoints.endpoints.len(), 1);
        assert_eq!(endpoints.endpoints[0].app_id, app);
        assert_eq!(endpoints.endpoints[0].payload.as_str(), "lno1discovery");
        Some(signed)
    } else {
        if matches!(family, Family::HeldAsk) {
            Box::pin(list(w, &mut wallet, app, list_version(1, 7))).await;
        }
        let error = wallet
            .pay_without_request(payment(w, family, app))
            .await
            .outcome
            .unwrap_err();
        assert!(
            matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
            "{error:?}"
        );
        None
    };
    let key = match family {
        Family::Records => format!("ar/{}/{}", b64(&w.bob_id()), b64(&app)),
        Family::GenericAsk => format!("lq/{}", b64(&w.bob_id())),
        _ => format!("lq/{}/{}", b64(&w.bob_id()), b64(&app)),
    }
    .into_bytes();
    let bytes = wallet
        .app()
        .protocol_get("paykit", &key)
        .await
        .unwrap()
        .expect("the public call writes its discovery value");
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    if let Some(signed) = signed_record {
        assert_eq!(
            value["d"]["records"],
            json!([serde_json::from_slice::<Value>(&signed).unwrap()])
        );
    } else {
        let app_id = (!matches!(family, Family::GenericAsk)).then_some(app);
        let held_version = matches!(family, Family::HeldAsk).then_some(list_version(1, 7));
        assert_eq!(value["d"].as_object().unwrap().len(), 4);
        assert_eq!(value["d"]["payee"], b64(&w.bob_id()));
        assert_eq!(value["d"]["app_id"], json!(app_id));
        assert_eq!(value["d"]["held_version"], json!(held_version));
        assert_eq!(value["d"]["asked_at_ms"], w.clock.now_ms());
        let sent = wallet
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap();
        assert_eq!(sent.len(), 1);
        let signed: Value = serde_json::from_slice(sent[0].plaintext.json()).unwrap();
        assert_eq!(signed["kind"], "paykit.private_payment_list_request");
        assert_eq!(
            signed["body"],
            Value::Object(
                PrivatePaymentListRequest {
                    app_id,
                    held_version
                }
                .to_body()
                .unwrap()
            )
        );
    }
    assert!(w.rail.all_executions().is_empty());
    Held {
        wallet,
        storage,
        family,
        app,
        key,
        bytes,
    }
}

fn damage(bytes: &[u8], how: Damage) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    let body = value["d"].as_object_mut().unwrap();
    match how {
        Damage::Unknown => {
            body.insert("extra".into(), json!({"nested": [7]}));
        }
        Damage::EmptyRecords => {
            body.insert("records".into(), json!([]));
        }
        Damage::MissingApp => {
            body.remove("app_id").unwrap();
        }
        Damage::MissingVersion => {
            body.remove("held_version").unwrap();
        }
        Damage::VersionWithoutApp => {
            assert!(body["app_id"].is_null());
            body.insert(
                "held_version".into(),
                json!({"generation": 1, "counter": 7}),
            );
        }
    }
    serde_json::to_vec(&value).unwrap()
}

fn replace(backup: &PaykitBackup, key: &[u8], bytes: Vec<u8>) -> PaykitBackup {
    let mut value: Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let data = pubky_switch_core::enc::unb64(value["layers"][0]["data"].as_str().unwrap()).unwrap();
    let mut layer = pubky_switch_client::app::decode_local_state(&data).unwrap();
    layer.iter_mut().find(|(k, _)| k == key).unwrap().1 = bytes;
    value["layers"][0]["data"] =
        Value::String(b64(&pubky_switch_client::app::encode_local_state(&layer)));
    PaykitBackup::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap()
}

async fn refuse_read(family: Family, how: Damage) {
    let w = World::new();
    let mut held = Box::pin(produced(&w, family)).await;
    let bad = damage(&held.bytes, how);
    held.put(bad.clone()).await;
    held.reload(&w).await;
    let sends = w.bob.switch().entries_from(&w.alice_id());
    let error = held
        .read(&w)
        .await
        .expect_err("malformed discovery state was used");
    assert!(
        is_corrupt(&error),
        "malformed discovery state was accepted: {error:?}"
    );
    assert_eq!(
        held.wallet
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(bad)
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), sends);
    assert!(w.rail.all_executions().is_empty());
}

async fn refuse_backup(family: Family, how: Damage) {
    let w = World::new();
    let mut held = Box::pin(produced(&w, family)).await;
    let bad = damage(&held.bytes, how);
    held.put(bad.clone()).await;
    let error = held
        .wallet
        .backup()
        .await
        .expect_err("malformed discovery state entered a backup");
    assert!(
        matches!(error, PaykitError::Client(AppError::InvalidData(_))),
        "{error:?}"
    );
    assert_eq!(
        held.wallet
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(bad)
    );
}

async fn refuse_restore(family: Family, how: Damage) {
    let w = World::new();
    let mut held = Box::pin(produced(&w, family)).await;
    let backup = replace(
        &held.wallet.backup().await.unwrap(),
        &held.key,
        damage(&held.bytes, how),
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
        panic!("malformed discovery state was restored")
    };
    assert!(
        matches!(error, PaykitError::Client(AppError::InvalidData(_))),
        "{error:?}"
    );
    assert_eq!(target.commits(), 0);
    assert_eq!(
        held.wallet
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(held.bytes)
    );
}

async fn preserves(family: Family) {
    let w = World::new();
    let mut held = Box::pin(produced(&w, family)).await;
    held.reload(&w).await;
    let sends = w.bob.switch().entries_from(&w.alice_id());
    match family {
        Family::Records => held.read(&w).await.unwrap(),
        _ => {
            let error = held.read(&w).await.unwrap_err();
            assert!(
                matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
                "{error:?}"
            );
        }
    }
    assert_eq!(
        held.wallet
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(held.bytes.clone())
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), sends);
    let backup = held.wallet.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(held.bytes.clone())
    );
    restored.reconcile().await.unwrap();
    restored.end_reconcile(UserConfirmed::new()).await.unwrap();
    held.wallet = restored;
    let sends = w.bob.switch().entries_from(&w.alice_id());
    match family {
        Family::Records => held.read(&w).await.unwrap(),
        _ => {
            let error = held.read(&w).await.unwrap_err();
            assert!(
                matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
                "{error:?}"
            );
        }
    }
    assert_eq!(
        held.wallet
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(held.bytes)
    );
    assert_eq!(w.bob.switch().entries_from(&w.alice_id()), sends);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_discovery_state_records_unknown_read() {
    Box::pin(refuse_read(Family::Records, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_records_unknown_backup() {
    Box::pin(refuse_backup(Family::Records, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_records_unknown_restore() {
    Box::pin(refuse_restore(Family::Records, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_records_empty_read() {
    Box::pin(refuse_read(Family::Records, Damage::EmptyRecords)).await;
}

#[tokio::test]
async fn test_discovery_state_records_empty_backup() {
    Box::pin(refuse_backup(Family::Records, Damage::EmptyRecords)).await;
}

#[tokio::test]
async fn test_discovery_state_records_empty_restore() {
    Box::pin(refuse_restore(Family::Records, Damage::EmptyRecords)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_unknown_read() {
    Box::pin(refuse_read(Family::GenericAsk, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_unknown_backup() {
    Box::pin(refuse_backup(Family::GenericAsk, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_unknown_restore() {
    Box::pin(refuse_restore(Family::GenericAsk, Damage::Unknown)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_app_read() {
    Box::pin(refuse_read(Family::GenericAsk, Damage::MissingApp)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_app_backup() {
    Box::pin(refuse_backup(Family::GenericAsk, Damage::MissingApp)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_app_restore() {
    Box::pin(refuse_restore(Family::GenericAsk, Damage::MissingApp)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_version_read() {
    Box::pin(refuse_read(Family::HeldAsk, Damage::MissingVersion)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_version_backup() {
    Box::pin(refuse_backup(Family::HeldAsk, Damage::MissingVersion)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_missing_version_restore() {
    Box::pin(refuse_restore(Family::HeldAsk, Damage::MissingVersion)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_version_without_app_read() {
    Box::pin(refuse_read(Family::GenericAsk, Damage::VersionWithoutApp)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_version_without_app_backup() {
    Box::pin(refuse_backup(Family::GenericAsk, Damage::VersionWithoutApp)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_version_without_app_restore() {
    Box::pin(refuse_restore(
        Family::GenericAsk,
        Damage::VersionWithoutApp,
    ))
    .await;
}

#[tokio::test]
async fn test_discovery_state_records_reload_and_restore() {
    Box::pin(preserves(Family::Records)).await;
}

#[tokio::test]
async fn test_discovery_state_generic_ask_reload_and_restore() {
    Box::pin(preserves(Family::GenericAsk)).await;
}

#[tokio::test]
async fn test_discovery_state_app_ask_reload_and_restore() {
    Box::pin(preserves(Family::AppAsk)).await;
}

#[tokio::test]
async fn test_discovery_state_held_ask_reload_and_restore() {
    Box::pin(preserves(Family::HeldAsk)).await;
}

#[tokio::test]
async fn test_discovery_state_ask_clock_and_version_progress() {
    let w = World::new();
    let mut held = Box::pin(produced(&w, Family::HeldAsk)).await;
    let asked = w.clock.now_ms();
    w.clock.set_ms(asked - 1);
    let error = held.read(&w).await.unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
        "{error:?}"
    );
    assert_eq!(
        held.wallet
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap()
            .len(),
        2
    );
    w.clock.set_ms(asked + 24 * HOUR_MS);
    let error = held.read(&w).await.unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
        "{error:?}"
    );
    assert_eq!(
        held.wallet
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap()
            .len(),
        3
    );
    // The newly received version is named immediately, before another day passes.
    Box::pin(list(&w, &mut held.wallet, held.app, list_version(1, 8))).await;
    let error = held.read(&w).await.unwrap_err();
    assert!(
        matches!(error.refusal(), Some(Refusal::WaitingForList { .. })),
        "{error:?}"
    );
    let key = format!("lq/{}/{}", b64(&w.bob_id()), b64(&held.app));
    let bytes = held
        .wallet
        .app()
        .protocol_get("paykit", key.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let mark: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        mark["d"]["held_version"],
        json!({"generation": 1, "counter": 8})
    );
    assert_eq!(
        held.wallet
            .app()
            .sent_to(&w.bob_id(), 0, u64::MAX)
            .await
            .unwrap()
            .len(),
        4
    );
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_discovery_state_record_history_survives_restore() {
    let w = World::new();
    let mut held = Box::pin(produced(&w, Family::Records)).await;
    let first: Value = serde_json::from_slice(&held.bytes).unwrap();
    let first = serde_json::to_vec(&first["d"]["records"][0]).unwrap();
    w.clock.advance_ms(1_000);
    crate::public::publish(
        &w,
        json!({"public_endpoints": {"btc-lightning-bolt12": "lno1later"}}),
        Some(w.clock.now_ms() + 24 * HOUR_MS),
    );
    assert_eq!(
        held.wallet
            .public_endpoints(&w.bob_id(), None)
            .await
            .unwrap()
            .endpoints[0]
            .payload
            .as_str(),
        "lno1later"
    );
    let bytes = held
        .wallet
        .app()
        .protocol_get("paykit", &held.key)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap()["d"]["records"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let backup = held.wallet.backup().await.unwrap();
    w.homeserver
        .put(w.bob_id(), &app_record_path(&held.app), first);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        restored
            .public_endpoints(&w.bob_id(), None)
            .await
            .unwrap()
            .endpoints[0]
            .payload
            .as_str(),
        "lno1later"
    );
    assert_eq!(
        restored
            .app()
            .protocol_get("paykit", &held.key)
            .await
            .unwrap(),
        Some(bytes)
    );
    assert!(w.rail.all_executions().is_empty());
}
