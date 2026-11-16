//! Stored re-send answers and cancellation sides (Paykit spec 3.4, 9;
//! Paykit wire 4.12; Switch spec 12.4).

use std::sync::Arc;

use paykit_sdk::{AppScopes, Paykit, PaykitBackup, PaykitConfig, Rail, ResendSelection};
use pubky_switch_client::app::{AppError, AppEvent, Delivery, LocalWrite};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use serde_json::{json, Value};

use crate::common::{draft, is_corrupt, store, World, HOUR_MS};

#[derive(Clone, Copy)]
enum Family {
    Answer,
    Payee,
    Payer,
}

struct Held {
    app: Paykit,
    peer: Paykit,
    storage: Arc<dyn StateStore>,
    alice: bool,
    key: Vec<u8>,
    bytes: Vec<u8>,
    sent_id: String,
    plaintext: Vec<u8>,
}

impl Held {
    async fn reload(&mut self, w: &World) {
        self.app = Paykit::load(
            w.env(
                if self.alice { &w.alice } else { &w.bob },
                Arc::clone(&self.storage),
            ),
            PaykitConfig::default(),
            self.app.app_id(),
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
        )
        .await
        .expect("load preserves the later reader's boundary");
    }

    async fn ask(&mut self) {
        self.ask_selection(ResendSelection::default().with_msg_ids(vec![self.sent_id.clone()]))
            .await;
    }

    async fn ask_selection(&mut self, selection: ResendSelection) {
        let id = self
            .peer
            .request_resend(self.app.identity(), selection)
            .await
            .unwrap();
        let sent = self
            .peer
            .app()
            .sent_to(&self.app.identity(), 0, u64::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|sent| sent.msg_id == id)
            .unwrap();
        let signed: Value = serde_json::from_slice(sent.plaintext.json()).unwrap();
        let report = self.app.app_mut().receive(None).await.unwrap();
        let seq = report
            .events
            .iter()
            .find_map(|event| match event {
                AppEvent::MessageReceived {
                    seq,
                    msg_id,
                    sender,
                    ..
                } if msg_id == &id && sender == &self.peer.identity() => Some(*seq),
                _ => None,
            })
            .unwrap_or_else(|| panic!("the real feed did not hand over {id}: {report:?}"));
        let values = self.app.app().protocol_scan(b"ans/").await.unwrap();
        assert_eq!(values.len(), 1);
        let (key, bytes) = &values[0];
        let value: Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(key, format!("ans/{seq:020}/{id}").as_bytes());
        assert_eq!(value["d"]["seq"], seq);
        assert_eq!(value["d"]["msg_id"], id);
        assert_eq!(value["d"]["body"], signed["body"]);
        assert_eq!(
            value["d"]["cert"],
            serde_json::to_value(self.peer.app().cert().signed()).unwrap()
        );
    }

    async fn put(&mut self, bytes: Vec<u8>) {
        self.app
            .app_mut()
            .protocol_commit(&[LocalWrite::Put {
                key: self.key.clone(),
                value: bytes,
            }])
            .await
            .unwrap();
    }

    async fn signed_bytes(&self) -> Vec<u8> {
        self.app
            .app()
            .sent_to(&self.peer.identity(), 0, u64::MAX)
            .await
            .unwrap()
            .into_iter()
            .find(|sent| sent.msg_id == self.sent_id)
            .unwrap()
            .plaintext
            .json()
            .to_vec()
    }

    fn entries(&self, w: &World) -> usize {
        if self.alice {
            w.bob.switch().entries_from(&w.alice_id())
        } else {
            w.alice.switch().entries_from(&w.bob_id())
        }
    }
}

async fn held(w: &World, family: Family) -> Held {
    held_with_contacts(w, family, false).await
}

async fn held_with_contacts(w: &World, family: Family, contacts: bool) -> Held {
    let alice_store = store();
    let bob_store = store();
    let mut wallet = if contacts {
        let mut app = w
            .join_with(
                &w.alice,
                "wallet",
                AppScopes::new().pay().manage_contacts().build().unwrap(),
                PaykitConfig::default(),
                Arc::clone(&alice_store),
            )
            .await;
        app.app_mut().invite(&w.bob_id()).await.unwrap();
        app
    } else {
        w.wallet_on("wallet", Arc::clone(&alice_store)).await
    };
    let mut shop = w
        .join_with(
            &w.bob,
            "shop",
            if contacts {
                AppScopes::new()
                    .request()
                    .manage_contacts()
                    .build()
                    .unwrap()
            } else {
                AppScopes::new().request().build().unwrap()
            },
            PaykitConfig::default(),
            Arc::clone(&bob_store),
        )
        .await;
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    let request = sent.request.unwrap();
    wallet.receive(None).await.unwrap();
    let (app, peer, storage, alice, sent_id) = match family {
        Family::Answer => (shop, wallet, bob_store, false, sent.msg_id),
        Family::Payee => {
            let record = shop.payee_activity().await.unwrap()[0].record;
            let id = shop.cancel_sent_request(&record, None).await.unwrap();
            (shop, wallet, bob_store, false, id)
        }
        Family::Payer => {
            let id = wallet.cancel_request(&request, None).await.unwrap();
            (wallet, shop, alice_store, true, id)
        }
    };
    let mut result = Held {
        app,
        peer,
        storage,
        alice,
        key: Vec::new(),
        bytes: Vec::new(),
        sent_id,
        plaintext: Vec::new(),
    };
    result.plaintext = result.signed_bytes().await;
    if matches!(family, Family::Answer) {
        result.ask().await;
        let values = result.app.app().protocol_scan(b"ans/").await.unwrap();
        (result.key, result.bytes) = values.into_iter().next().unwrap();
        let value: Value = serde_json::from_slice(&result.bytes).unwrap();
        let body = &value["d"];
        assert_eq!(body.as_object().unwrap().len(), 5);
        assert_eq!(
            body["requester"],
            pubky_switch_core::enc::b64(&result.peer.identity())
        );
        assert_eq!(
            body["cert"],
            serde_json::to_value(result.peer.app().cert().signed()).unwrap()
        );
        assert_eq!(body["body"], json!({"msg_ids": [result.sent_id]}));
        assert_eq!(
            String::from_utf8(result.key.clone()).unwrap(),
            format!(
                "ans/{:020}/{}",
                body["seq"].as_u64().unwrap(),
                body["msg_id"].as_str().unwrap()
            )
        );
    } else {
        result.key = format!("side/{}", result.sent_id).into_bytes();
        result.bytes = result
            .app
            .app()
            .protocol_get(&result.key)
            .await
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_slice(&result.bytes).unwrap();
        assert_eq!(
            value["d"],
            json!({"msg_id": result.sent_id, "side": if result.alice { "payer" } else { "payee" }})
        );
    }
    result
}

fn extra(bytes: &[u8]) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(bytes).unwrap();
    value["d"]
        .as_object_mut()
        .unwrap()
        .insert("extra".into(), json!({"nested": [7]}));
    serde_json::to_vec(&value).unwrap()
}

fn replace(backup: &PaykitBackup, key: &[u8], bytes: Vec<u8>) -> PaykitBackup {
    let mut value: Value = serde_json::from_slice(&backup.to_bytes().unwrap()).unwrap();
    let data = pubky_switch_core::enc::unb64(value["protocol"]["data"].as_str().unwrap()).unwrap();
    let mut layer = pubky_switch_client::app::decode_local_state(&data).unwrap();
    layer.iter_mut().find(|(k, _)| k == key).unwrap().1 = bytes;
    value["protocol"]["data"] = Value::String(pubky_switch_core::enc::b64(
        &pubky_switch_client::app::encode_local_state(&layer),
    ));
    PaykitBackup::from_bytes(&serde_json::to_vec(&value).unwrap()).unwrap()
}

async fn refuses_read(family: Family) {
    let w = World::new();
    let mut value = Box::pin(held(&w, family)).await;
    let bad = extra(&value.bytes);
    value.put(bad.clone()).await;
    if !matches!(family, Family::Answer) {
        value.ask().await;
    }
    let before = value.entries(&w);
    value.reload(&w).await;
    let error = value
        .app
        .receive(None)
        .await
        .expect_err("unknown stored member must fail at the typed reader");
    assert!(is_corrupt(&error), "{error:?}");
    assert_eq!(
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(bad)
    );
    assert_eq!(
        value.app.app().protocol_scan(b"ans/").await.unwrap().len(),
        1
    );
    assert_eq!(value.entries(&w), before);
    assert_eq!(value.signed_bytes().await, value.plaintext);
    assert!(w.rail.all_executions().is_empty());
}

async fn refuses_backup(family: Family) {
    let w = World::new();
    let mut value = Box::pin(held(&w, family)).await;
    let bad = extra(&value.bytes);
    value.put(bad.clone()).await;
    let before = value.entries(&w);
    let error = value
        .app
        .backup()
        .await
        .expect_err("unknown stored member must refuse backup creation");
    assert!(
        matches!(
            error,
            paykit_sdk::PaykitError::Client(AppError::InvalidData(_))
        ),
        "{error:?}"
    );
    assert_eq!(
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(bad)
    );
    assert_eq!(value.entries(&w), before);
    assert!(w.rail.all_executions().is_empty());
}

async fn refuses_restore(family: Family) {
    let w = World::new();
    let mut value = Box::pin(held(&w, family)).await;
    let valid = value.app.backup().await.unwrap();
    let backup = replace(&valid, &value.key, extra(&value.bytes));
    let target = Arc::new(FaultStore::new(store(), Fault::None));
    let result = Paykit::restore(
        w.env(
            if value.alice { &w.alice } else { &w.bob },
            Arc::clone(&target) as Arc<dyn StateStore>,
        ),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await;
    let Err(error) = result else {
        panic!("unknown stored member must refuse restore")
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
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(value.bytes)
    );
}

async fn refuses_recipient(family: Family) {
    let w = World::new();
    let mut value = Box::pin(held(&w, family)).await;
    let bad = extra(&value.bytes);
    value.put(bad.clone()).await;
    value.reload(&w).await;
    let before = value.entries(&w);
    let delivery = value
        .app
        .app_mut()
        .resend(&value.sent_id)
        .await
        .expect("recipient filtering is not a typed-reader error");
    assert_eq!(
        value.entries(&w),
        before,
        "unreadable side must select no recipient: {delivery:?}"
    );
    assert_eq!(
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(bad)
    );
    assert_eq!(value.signed_bytes().await, value.plaintext);
    value.put(value.bytes.clone()).await;
    let delivery = value.app.app_mut().resend(&value.sent_id).await.unwrap();
    assert!(
        matches!(delivery, Delivery::Accepted { .. }),
        "{delivery:?}"
    );
    assert_eq!(value.entries(&w), before + 1);
    assert_eq!(value.signed_bytes().await, value.plaintext);
}

#[tokio::test]
async fn test_answer_state_answer_unknown_read() {
    Box::pin(refuses_read(Family::Answer)).await;
}
#[tokio::test]
async fn test_answer_state_payee_side_unknown_read() {
    Box::pin(refuses_read(Family::Payee)).await;
}
#[tokio::test]
async fn test_answer_state_payer_side_unknown_read() {
    Box::pin(refuses_read(Family::Payer)).await;
}
#[tokio::test]
async fn test_answer_state_answer_unknown_backup() {
    Box::pin(refuses_backup(Family::Answer)).await;
}
#[tokio::test]
async fn test_answer_state_payee_side_unknown_backup() {
    Box::pin(refuses_backup(Family::Payee)).await;
}
#[tokio::test]
async fn test_answer_state_payer_side_unknown_backup() {
    Box::pin(refuses_backup(Family::Payer)).await;
}
#[tokio::test]
async fn test_answer_state_answer_unknown_restore() {
    Box::pin(refuses_restore(Family::Answer)).await;
}
#[tokio::test]
async fn test_answer_state_payee_side_unknown_restore() {
    Box::pin(refuses_restore(Family::Payee)).await;
}
#[tokio::test]
async fn test_answer_state_payer_side_unknown_restore() {
    Box::pin(refuses_restore(Family::Payer)).await;
}
#[tokio::test]
async fn test_answer_state_payee_side_unknown_recipient() {
    Box::pin(refuses_recipient(Family::Payee)).await;
}
#[tokio::test]
async fn test_answer_state_payer_side_unknown_recipient() {
    Box::pin(refuses_recipient(Family::Payer)).await;
}

async fn current_reload_restore_and_resend(family: Family) {
    let w = World::new();
    let mut value = Box::pin(held(&w, family)).await;
    let backup = value.app.backup().await.unwrap();
    value.storage = store();
    value.app = Paykit::restore(
        w.env(
            if value.alice { &w.alice } else { &w.bob },
            Arc::clone(&value.storage),
        ),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(value.bytes.clone())
    );
    value.reload(&w).await;
    let before = value.entries(&w);
    value.app.receive(None).await.unwrap();
    assert!(value.entries(&w) > before);
    assert_eq!(value.signed_bytes().await, value.plaintext);
    assert!(value
        .app
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
    value.ask().await;
    value.app.receive(None).await.unwrap();
    let stamp = format!(
        "rs/{}/{}",
        pubky_switch_core::enc::b64(&value.peer.identity()),
        value.sent_id
    );
    let stamp: Value = serde_json::from_slice(
        &value
            .app
            .app()
            .protocol_get(stamp.as_bytes())
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(stamp["d"], w.clock.now_ms());
    let delivered = value.entries(&w);
    value.ask().await;
    value.app.receive(None).await.unwrap();
    assert_eq!(value.entries(&w), delivered);
    w.clock.advance_ms(HOUR_MS + 1);
    value.ask().await;
    value.app.receive(None).await.unwrap();
    assert_eq!(value.entries(&w), delivered + 1);
    assert_eq!(value.signed_bytes().await, value.plaintext);
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_answer_state_current_answer_reload_restore_and_resend() {
    Box::pin(current_reload_restore_and_resend(Family::Answer)).await;
}

#[tokio::test]
async fn test_answer_state_current_payee_reload_restore_and_resend() {
    Box::pin(current_reload_restore_and_resend(Family::Payee)).await;
}

#[tokio::test]
async fn test_answer_state_current_payer_reload_restore_and_resend() {
    Box::pin(current_reload_restore_and_resend(Family::Payer)).await;
}

#[tokio::test]
async fn test_answer_state_unknown_deny_preserves_queued_answer() {
    let w = World::new();
    let mut value = Box::pin(held_with_contacts(&w, Family::Answer, true)).await;
    let before = value.entries(&w);
    w.clock.advance_ms(paykit_sdk::LISTS_REFRESH_MS);
    w.bob
        .switch()
        .fail_next(pubky_switch_client::testing::Fault::Error {
            path: "/allow".into(),
            code: pubky_switch_core::http::ErrorCode::Unavailable,
            current: Value::Null,
        });
    value.app.receive(None).await.unwrap();
    assert_eq!(
        value.app.app().protocol_get(&value.key).await.unwrap(),
        Some(value.bytes.clone())
    );
    assert_eq!(value.entries(&w), before);
    value.app.receive(None).await.unwrap();
    assert!(value
        .app
        .app()
        .protocol_get(&value.key)
        .await
        .unwrap()
        .is_none());
    assert_eq!(value.entries(&w), before + 1);
    assert_eq!(value.signed_bytes().await, value.plaintext);
}

#[tokio::test]
async fn test_answer_state_blocked_requester_gets_no_answer() {
    let w = World::new();
    let mut value = Box::pin(held_with_contacts(&w, Family::Answer, true)).await;
    value.app.block(&value.peer.identity()).await.unwrap();
    let before = value.entries(&w);
    value.app.receive(None).await.unwrap();
    assert!(value
        .app
        .app()
        .protocol_get(&value.key)
        .await
        .unwrap()
        .is_none());
    assert_eq!(value.entries(&w), before);
    assert_eq!(value.signed_bytes().await, value.plaintext);
}

#[tokio::test]
async fn test_answer_state_selection_with_no_match_completes_without_send() {
    let w = World::new();
    let mut value = Box::pin(held(&w, Family::Payee)).await;
    value
        .ask_selection(
            ResendSelection::default()
                .with_msg_ids(vec!["123e4567-e89b-42d3-a456-426614174000".into()]),
        )
        .await;
    let before = value.entries(&w);
    value.app.receive(None).await.unwrap();
    assert!(value
        .app
        .app()
        .protocol_scan(b"ans/")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(value.entries(&w), before);
    assert_eq!(value.signed_bytes().await, value.plaintext);
}
