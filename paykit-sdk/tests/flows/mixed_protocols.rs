//! Own certificates and state shared by Paykit and another protocol (Paykit spec 3.2).

use std::sync::Arc;

use paykit_sdk::{
    AppScopes, Paykit, PaykitBackup, PaykitConfig, PaykitProtocol, Rail, RequestedScopes,
};
use pubky_switch_client::app::{
    App, AppBackup, AppError, Effect, LocalWrite, MissedEntries, Outcome, Protocol, ProtocolSet,
    ReceiveContext, ReceivedMessage, Refusal, ResendNeeded, StoreFailure, UserConfirmed,
};
use pubky_switch_client::store::{Fault, FaultStore, StateStore};
use pubky_switch_client::testing::{AppSpec, Seeds, TestIdentity, TestSigner};
use pubky_switch_core::message::Message;
use pubky_switch_core::records::AppCert;

use crate::common::{store, World, ALICE_ORIGIN};

struct Chat;

impl Protocol for Chat {
    fn resend_needed(&self, _: &ResendNeeded) -> Result<Vec<LocalWrite>, StoreFailure> {
        Ok(Vec::new())
    }
    fn missed(&self, _: &MissedEntries) -> Result<Vec<LocalWrite>, StoreFailure> {
        Ok(Vec::new())
    }
    fn receive(&self, message: &ReceivedMessage, _: &mut ReceiveContext<'_>) -> Outcome {
        if message
            .message
            .body
            .get("wait_for_epoch")
            .and_then(serde_json::Value::as_u64)
            == Some(2)
        {
            return Outcome::AwaitKey { data_epoch: 2 };
        }
        Outcome::Apply(Effect {
            ops: Vec::new(),
            local: vec![LocalWrite::Put {
                key: message.message.msg_id.as_bytes().to_vec(),
                value: b"chat".to_vec(),
            }],
        })
    }
    fn name(&self) -> String {
        "chat".into()
    }
    fn has_keys(&self, scope: &str) -> bool {
        scope == "chat"
    }
    fn sent_between_own_apps(&self, _: &str) -> bool {
        false
    }
    fn check_scopes(&self, _: &Message, sender: &AppCert, own: &AppCert) -> Result<(), String> {
        if sender.scopes().iter().any(|s| s == "chat") && own.scopes().iter().any(|s| s == "chat") {
            Ok(())
        } else {
            Err("chat requires its own scope".into())
        }
    }
    fn accepts(&self, _: &AppCert) -> Vec<String> {
        vec!["chat.item".into()]
    }
    fn check_body(&self, _: &ReceivedMessage) -> Result<(), String> {
        Ok(())
    }
}

fn world() -> World {
    let mut w = World::new();
    let mut spec = crate::common::spec();
    spec.keyed_scopes.push("chat".into());
    w.alice = TestSigner::at(
        TestIdentity::new(&Seeds::default(), "alice"),
        ALICE_ORIGIN,
        Arc::clone(&w.clock),
        Arc::clone(&w.homeserver),
        spec,
    );
    w.router.add_switch(w.alice.origin(), w.alice.switch());
    w
}

fn scopes() -> RequestedScopes {
    let mut scopes = AppScopes::new().pay().build().unwrap();
    scopes.scopes.push("chat".into());
    scopes
}

async fn client(w: &World, storage: Arc<dyn StateStore>) -> App {
    let p = Arc::new(PaykitProtocol::with_rail(
        Arc::clone(&w.rail) as Arc<dyn Rail>
    ));
    let set = ProtocolSet::new(vec![p, Arc::new(Chat)]).unwrap();
    App::join(
        w.env(&w.alice, storage),
        set,
        PaykitConfig::default().app,
        w.approve(&w.alice, "mixed", &scopes()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn test_mixed_protocols_own_join() {
    let w = world();
    let app = Paykit::join_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "mixed", &scopes()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        |p| ProtocolSet::new(vec![p, Arc::new(Chat)]),
    )
    .await
    .expect("a Paykit app can also own chat");
    assert!(app
        .app()
        .scope_key(app.app().data_epoch(), "chat")
        .is_some());
}

#[tokio::test]
async fn test_mixed_protocols_own_load() {
    let w = world();
    let storage = store();
    let app = client(&w, Arc::clone(&storage)).await;
    let id = app.app_id();
    drop(app);
    let app = Paykit::load_with_protocols(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        |p| ProtocolSet::new(vec![p, Arc::new(Chat)]),
    )
    .await
    .expect("load retains both registered owners");
    assert!(app
        .app()
        .scope_key(app.app().data_epoch(), "chat")
        .is_some());
}

#[tokio::test]
async fn test_mixed_protocols_own_restore() {
    let w = world();
    let mut app = client(&w, store()).await;
    let backup =
        PaykitBackup::from_bytes(&app.backup_layered().await.unwrap().to_bytes().unwrap()).unwrap();
    drop(app);
    let app = Paykit::restore_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        |p| ProtocolSet::new(vec![p, Arc::new(Chat)]),
    )
    .await
    .expect("restore installs both registered owners");
    assert!(app
        .app()
        .scope_key(app.app().data_epoch(), "chat")
        .is_some());
}

/// Chat, also naming many kinds whose roots chat doesn't own. The client keeps none of them
/// in the app's record (Switch wire 3.1, 5.1), so they make it no larger.
struct ChatNamingForeignKinds;

impl Protocol for ChatNamingForeignKinds {
    fn resend_needed(&self, r: &ResendNeeded) -> Result<Vec<LocalWrite>, StoreFailure> {
        Chat.resend_needed(r)
    }
    fn missed(&self, m: &MissedEntries) -> Result<Vec<LocalWrite>, StoreFailure> {
        Chat.missed(m)
    }
    fn receive(&self, message: &ReceivedMessage, cx: &mut ReceiveContext<'_>) -> Outcome {
        Chat.receive(message, cx)
    }
    fn name(&self) -> String {
        Chat.name()
    }
    fn has_keys(&self, scope: &str) -> bool {
        Chat.has_keys(scope)
    }
    fn sent_between_own_apps(&self, kind: &str) -> bool {
        Chat.sent_between_own_apps(kind)
    }
    fn check_scopes(&self, m: &Message, sender: &AppCert, own: &AppCert) -> Result<(), String> {
        Chat.check_scopes(m, sender, own)
    }
    fn accepts(&self, cert: &AppCert) -> Vec<String> {
        // Far more than one signed operation carries, under roots of other protocols: a
        // payment kind, another protocol's, and a root that only starts with `chat`.
        let pad = "x".repeat(200);
        let mut kinds = Chat.accepts(cert);
        for i in 0..2000 {
            kinds.push(format!("news.item{i}{pad}"));
        }
        kinds.push("paykit.chat_receipt".into());
        kinds.push("chatroom.item".into());
        kinds
    }
    fn check_body(&self, m: &ReceivedMessage) -> Result<(), String> {
        Chat.check_body(m)
    }
}

#[tokio::test]
async fn test_mixed_protocols_join_counts_only_owned_kinds() {
    let w = world();
    let app = Paykit::join_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "mixed", &scopes()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        |p| ProtocolSet::new(vec![p, Arc::new(ChatNamingForeignKinds)]),
    )
    .await
    .expect("the record the client signs fits");
    let record = w
        .alice
        .switch()
        .app(&app.app_id())
        .record
        .expect("the joined app's record");
    let record = pubky_switch_core::records::AppRecord::verify(&record, &w.alice_id()).unwrap();
    let accepts = &record.body().accepts;
    assert!(accepts.iter().any(|k| k == "chat.item"), "{accepts:?}");
    assert!(
        accepts
            .iter()
            .all(|k| !k.starts_with("news.") && !k.starts_with("chatroom.")),
        "{accepts:?}"
    );
}

#[tokio::test]
async fn test_mixed_protocols_withdrawal_refused_by_a_lapsed_expiry_keeps_the_member_readable() {
    let w = world();
    let mut scopes = AppScopes::new().request().build().unwrap();
    scopes.scopes.push("chat".into());
    let mut app = Paykit::join_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default().with_app(pubky_switch_client::app::AppConfig {
            publishes_record: true,
            ..pubky_switch_client::app::AppConfig::default()
        }),
        w.approve(&w.alice, "mixed", &scopes),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    let id = paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt12").unwrap();
    app.set_public_endpoint(
        id.clone(),
        paykit_lib::PaymentEndpointPayload::new("lno1tips").unwrap(),
    )
    .await
    .unwrap();
    // Chat's member asks for a fixed expiry, an hour on, which then lapses.
    let chat = || {
        let mut member = serde_json::Map::new();
        member.insert("room".into(), serde_json::json!("lobby"));
        Some(member)
    };
    let hour = 3_600_000;
    let deadline = pubky_switch_client::clock::Clock::now_ms(w.clock.as_ref()) + hour;
    app.app_mut()
        .set_protocol_member("chat", chat(), Some(deadline))
        .await
        .unwrap();
    w.clock.advance_ms(2 * hour);
    // The last withdrawal can't be signed while chat's expiry has lapsed: the endpoint stays
    // under Paykit's lifetime, and the record still reads.
    assert!(app.remove_public_endpoint(&id).await.is_err());
    assert_eq!(app.own_public_endpoints().unwrap().len(), 1);
    // Chat asks for no expiry: the record signed then carries Paykit's lifetime with the
    // endpoint, so it still reads, and a receive goes through.
    app.app_mut()
        .set_protocol_member("chat", chat(), None)
        .await
        .unwrap();
    assert_eq!(app.own_public_endpoints().unwrap().len(), 1);
    app.receive(None).await.unwrap();
    let held =
        pubky_switch_core::records::AppRecord::verify(app.app().record(), &w.alice_id()).unwrap();
    assert!(held.expires_at().is_some());
    // The withdrawal goes through now, and Paykit's ask goes with it: no expiry is left.
    app.remove_public_endpoint(&id).await.unwrap();
    assert!(app.own_public_endpoints().unwrap().is_empty());
    let held =
        pubky_switch_core::records::AppRecord::verify(app.app().record(), &w.alice_id()).unwrap();
    assert_eq!(held.expires_at(), None);
    app.receive(None).await.unwrap();
    // Chat asks for an expiry again: a record without public endpoints whose expiry is
    // chat's alone is left as it is, receive after receive.
    let deadline = pubky_switch_client::clock::Clock::now_ms(w.clock.as_ref()) + 2 * hour;
    app.app_mut()
        .set_protocol_member("chat", chat(), Some(deadline))
        .await
        .unwrap();
    let signed = app.app().record().clone();
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    assert_eq!(app.app().record(), &signed);
}

/// Alice's app owning chat and Paykit, holding `paykit_request`, publishing its record.
async fn publishing_mixed(w: &World) -> Paykit {
    let mut scopes = AppScopes::new().request().build().unwrap();
    scopes.scopes.push("chat".into());
    Paykit::join_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default().with_app(pubky_switch_client::app::AppConfig {
            publishes_record: true,
            ..pubky_switch_client::app::AppConfig::default()
        }),
        w.approve(&w.alice, "mixed", &scopes),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap()
}

/// A chat member.
fn chat_member() -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut member = serde_json::Map::new();
    member.insert("room".into(), serde_json::json!("lobby"));
    Some(member)
}

#[tokio::test]
async fn test_mixed_protocols_unread_paykit_member_kept_beside_chat_expiry() {
    let w = world();
    let mut app = publishing_mixed(&w).await;
    // A Paykit member of a field this version doesn't read only, and chat's expiry.
    let mut member = serde_json::Map::new();
    member.insert("future_field".into(), serde_json::json!("kept"));
    app.app_mut()
        .set_protocol_member_with_lifetime("paykit", Some(member), None)
        .await
        .unwrap();
    let deadline = pubky_switch_client::clock::Clock::now_ms(w.clock.as_ref()) + 3_600_000;
    app.app_mut()
        .set_protocol_member("chat", chat_member(), Some(deadline))
        .await
        .unwrap();
    // No withdrawal is owed: the record keeps its bytes, receive after receive.
    let signed = app.app().record().clone();
    app.receive(None).await.unwrap();
    app.receive(None).await.unwrap();
    app.update_own_record().await.unwrap();
    assert_eq!(app.app().record(), &signed);
}

#[tokio::test]
async fn test_mixed_protocols_update_beside_chat_expiry_checks_the_homeserver() {
    use pubky_switch_client::app::Publication;
    let w = world();
    let mut app = publishing_mixed(&w).await;
    // No Paykit member, and chat's expiry: nothing of Paykit's is owed, so the update is
    // the explicit check of the Homeserver copy, which can't be read now.
    let deadline = pubky_switch_client::clock::Clock::now_ms(w.clock.as_ref()) + 3_600_000;
    app.app_mut()
        .set_protocol_member("chat", chat_member(), Some(deadline))
        .await
        .unwrap();
    let signed = app.app().record().clone();
    w.homeserver.set_down(true);
    let publication = app.update_own_record().await.unwrap();
    w.homeserver.set_down(false);
    assert!(
        matches!(publication, Publication::Unchecked { .. }),
        "{publication:?}"
    );
    assert_eq!(app.app().record(), &signed);
}

fn register(p: Arc<PaykitProtocol>) -> Result<ProtocolSet, AppError> {
    ProtocolSet::new(vec![p, Arc::new(Chat)])
}

/// A chat item from Bob's chat app to Alice's app `id`. Returns its `msg_id`.
fn chat_item(w: &World, id: [u8; 16]) -> String {
    w.inject(
        &w.bob,
        "chat",
        &w.alice,
        &[id],
        "chat.item",
        Vec::new(),
        serde_json::Map::new(),
    )
}

/// A payment request from Bob's shop to Alice's app `id` under `reference`. Returns its
/// `msg_id`.
fn payment_request(w: &World, id: [u8; 16], reference: &str) -> String {
    w.inject(
        &w.bob,
        "shop",
        &w.alice,
        &[id],
        "paykit.payment_request",
        Vec::new(),
        serde_json::json!({"amount":{"asset":"btc","value":"1"}, "payment_reference":reference,
        "proposal_expires_at":null,"payment_endpoints":{"btc-lightning-bolt11":"lnbc1invoice"}})
        .as_object()
        .unwrap()
        .clone(),
    )
}

/// Chat kept the item `msg_id` in its own state, and Paykit kept nothing under it.
async fn chat_took(app: &Paykit, msg_id: &str) {
    assert_eq!(
        app.app()
            .protocol_get("chat", msg_id.as_bytes())
            .await
            .unwrap(),
        Some(b"chat".to_vec())
    );
    assert_eq!(
        app.app()
            .protocol_get("paykit", msg_id.as_bytes())
            .await
            .unwrap(),
        None
    );
}

fn bad_registration(p: Arc<PaykitProtocol>, case: u8) -> Result<ProtocolSet, AppError> {
    match case {
        0 => ProtocolSet::single(Arc::new(Chat)),
        1 => ProtocolSet::new(vec![Arc::new(PaykitProtocol::new()), Arc::new(Chat)]),
        _ => ProtocolSet::new(vec![p, Arc::new(Chat), Arc::new(Chat)]),
    }
}

fn usage(result: paykit_sdk::Result<Paykit>) {
    assert!(matches!(
        result,
        Err(paykit_sdk::PaykitError::Client(AppError::Refused(
            Refusal::Usage(_)
        )))
    ));
}

#[tokio::test]
async fn test_mixed_protocols_join_requires_the_supplied_paykit() {
    for case in 0..3 {
        let w = world();
        let fault = Arc::new(FaultStore::new(store(), Fault::None));
        let request = w.approve(&w.alice, "mixed", &scopes());
        let tip = w.alice.switch().tip_seq();
        usage(
            Paykit::join_with_protocols(
                w.env(&w.alice, Arc::clone(&fault) as Arc<dyn StateStore>),
                PaykitConfig::default(),
                request,
                Arc::clone(&w.rail) as Arc<dyn Rail>,
                w.timer(),
                |p| bad_registration(p, case),
            )
            .await,
        );
        assert_eq!(fault.commits(), 0);
        assert_eq!(w.alice.switch().tip_seq(), tip);
    }
}

#[tokio::test]
async fn test_mixed_protocols_load_and_restore_require_the_supplied_paykit() {
    // Each bad registration, then (`None`) the Paykit-only constructor, for an app that
    // also owns chat: refused, with nothing written.
    let w = world();
    let fault = Arc::new(FaultStore::new(store(), Fault::None));
    let mut app = client(&w, Arc::clone(&fault) as Arc<dyn StateStore>).await;
    let id = app.app_id();
    let backup =
        PaykitBackup::from_bytes(&app.backup_layered().await.unwrap().to_bytes().unwrap()).unwrap();
    drop(app);
    for case in [Some(0), Some(1), Some(2), None] {
        let commits = fault.commits();
        let env = w.env(&w.alice, Arc::clone(&fault) as Arc<dyn StateStore>);
        let rail = Arc::clone(&w.rail) as Arc<dyn Rail>;
        usage(match case {
            Some(case) => {
                Paykit::load_with_protocols(
                    env,
                    PaykitConfig::default(),
                    id,
                    rail,
                    w.timer(),
                    |p| bad_registration(p, case),
                )
                .await
            }
            None => Paykit::load(env, PaykitConfig::default(), id, rail, w.timer()).await,
        });
        assert_eq!(fault.commits(), commits, "load case {case:?}");

        let restored = Arc::new(FaultStore::new(store(), Fault::None));
        let env = w.env(&w.alice, Arc::clone(&restored) as Arc<dyn StateStore>);
        let rail = Arc::clone(&w.rail) as Arc<dyn Rail>;
        usage(match case {
            Some(case) => {
                Paykit::restore_with_protocols(
                    env,
                    PaykitConfig::default(),
                    &backup,
                    rail,
                    w.timer(),
                    |p| bad_registration(p, case),
                )
                .await
            }
            None => Paykit::restore(env, PaykitConfig::default(), &backup, rail, w.timer()).await,
        });
        assert_eq!(restored.commits(), 0, "restore case {case:?}");
    }
}

#[tokio::test]
async fn test_mixed_protocols_keyed_approval_checks_paykit_before_writes() {
    let w = world();
    let fault = Arc::new(FaultStore::new(store(), Fault::None));
    let mut requested = scopes();
    requested.permissions.clear();
    let request = w.approve(&w.alice, "mixed", &requested);
    let tip = w.alice.switch().tip_seq();
    let result = Paykit::join_with_protocols(
        w.env(&w.alice, Arc::clone(&fault) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        request,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await;
    assert!(matches!(
        result,
        Err(paykit_sdk::PaykitError::Refused(
            paykit_sdk::Refusal::CertificateRefused(_)
        ))
    ));
    assert_eq!(fault.commits(), 0);
    assert_eq!(w.alice.switch().tip_seq(), tip);
}

#[tokio::test]
async fn test_mixed_protocols_callbacks_and_layers_survive_load_and_restore() {
    let w = world();
    let storage = store();
    let mut app = Paykit::join_with_protocols(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        w.approve(&w.alice, "mixed", &scopes()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    let id = app.app_id();
    let _shop = w.shop().await;
    let keys = w.bob.identity().app_keys("chat");
    w.bob.register_app(&keys, &AppSpec::new(&["chat"])).unwrap();
    let chat_id = chat_item(&w, id);
    let request_id = payment_request(&w, id, "mixed");
    app.receive(None).await.unwrap();
    chat_took(&app, &chat_id).await;
    assert_eq!(
        app.app()
            .protocol_get("chat", request_id.as_bytes())
            .await
            .unwrap(),
        None
    );
    assert_eq!(app.requests().await.unwrap().len(), 1);
    let chat_state = app.app().protocol_scan("chat", b"").await.unwrap();
    drop(app);
    let mut app = Paykit::load_with_protocols(
        w.env(&w.alice, storage),
        PaykitConfig::default(),
        id,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    assert_eq!(
        app.app().protocol_scan("chat", b"").await.unwrap(),
        chat_state
    );
    assert_eq!(app.requests().await.unwrap().len(), 1);
    // After the load, Paykit takes a payment request and chat its item from the same read.
    let loaded_chat = chat_item(&w, id);
    payment_request(&w, id, "after-load");
    app.receive(None).await.unwrap();
    chat_took(&app, &loaded_chat).await;
    assert_eq!(app.requests().await.unwrap().len(), 2);
    let chat_state = app.app().protocol_scan("chat", b"").await.unwrap();
    let backup = app.backup().await.unwrap();
    let layers = AppBackup::from_bytes(&backup.to_bytes().unwrap())
        .unwrap()
        .layers()
        .unwrap();
    assert_eq!(
        layers
            .iter()
            .map(|l| l.protocol.as_str())
            .collect::<Vec<_>>(),
        vec!["paykit", "chat"]
    );
    drop(app);
    let mut app = Paykit::restore_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    assert_eq!(
        app.app().protocol_scan("chat", b"").await.unwrap(),
        chat_state
    );
    app.receive(None).await.unwrap();
    app.reconcile().await.unwrap();
    app.end_reconcile(UserConfirmed::new()).await.unwrap();
    assert_eq!(app.requests().await.unwrap().len(), 2);
    // After the restore too, each owner takes its own message from the same read.
    let restored_chat = chat_item(&w, id);
    payment_request(&w, id, "after-restore");
    app.receive(None).await.unwrap();
    chat_took(&app, &restored_chat).await;
    assert_eq!(app.requests().await.unwrap().len(), 3);
    assert!(matches!(
        app.app().protocol_get("absent", b"anything").await,
        Err(AppError::Refused(Refusal::Usage(_)))
    ));
    app.app_mut()
        .protocol_commit("chat", &[LocalWrite::DeletePrefix { prefix: Vec::new() }])
        .await
        .unwrap();
    assert!(app
        .app()
        .protocol_scan("chat", b"")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(app.requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn test_mixed_protocols_foreign_drop_stays_with_its_owner() {
    use pubky_switch_client::app::{AppEvent, WaitingReason, MAX_UNVERIFIED_AGE_MS};
    use pubky_switch_client::clock::Clock;
    let w = world();
    let storage = store();
    let mut app = Paykit::join_with_protocols(
        w.env(&w.alice, Arc::clone(&storage)),
        PaykitConfig::default(),
        w.approve(&w.alice, "mixed", &scopes()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    app.receive(None).await.unwrap();
    let id = app.app_id();
    let keys = w.bob.identity().app_keys("chat");
    w.bob.register_app(&keys, &AppSpec::new(&["chat"])).unwrap();
    let message = w.inject(
        &w.bob,
        "chat",
        &w.alice,
        &[id],
        "chat.item",
        Vec::new(),
        serde_json::json!({"wait_for_epoch":2})
            .as_object()
            .unwrap()
            .clone(),
    );
    let report = app.app_mut().receive(None).await.unwrap();
    assert!(report.events.iter().any(|e| matches!(
        e,
        AppEvent::Parked {
            reason: pubky_switch_client::app::ParkReason::AwaitingKey { data_epoch: 2 },
            ..
        }
    )));
    w.clock
        .set_ms(w.clock.now_ms() + MAX_UNVERIFIED_AGE_MS + 86_400_000);
    app.app_mut().receive(None).await.unwrap();
    app.app_mut().receive(None).await.unwrap();
    let chat = app
        .app_mut()
        .unacknowledged_events_for("chat")
        .await
        .unwrap();
    let drop_id = chat
        .iter()
        .find_map(|e| match e {
            AppEvent::StoredEntryDropped {
                waiting: WaitingReason::Key { data_epoch: 2 },
                message: msg,
                ..
            } if msg.msg_id == message => e.id(),
            _ => None,
        })
        .expect("the real chat key wait expires into its owned drop duty");
    assert_eq!(app.app().event_owners(drop_id).unwrap(), ["chat"]);
    assert!(!app
        .app_mut()
        .unacknowledged_events_for("paykit")
        .await
        .unwrap()
        .iter()
        .any(|e| e.id() == Some(drop_id)));
    for pass in 0..2 {
        if pass == 1 {
            drop(app);
            app = Paykit::load_with_protocols(
                w.env(&w.alice, Arc::clone(&storage)),
                PaykitConfig::default(),
                id,
                Arc::clone(&w.rail) as Arc<dyn Rail>,
                w.timer(),
                register,
            )
            .await
            .unwrap();
        }
        let handled = app.receive(None).await;
        for prefix in ["kw/", "ud/", "pd/", "se/", "dt/", "nt/", "dv/"] {
            assert!(
                app.app()
                    .protocol_scan("paykit", prefix.as_bytes())
                    .await
                    .unwrap()
                    .is_empty(),
                "a foreign duty must not change Paykit {prefix}"
            );
        }
        handled.unwrap();
        assert!(app
            .app_mut()
            .unacknowledged_events_for("chat")
            .await
            .unwrap()
            .iter()
            .any(|e| e.id() == Some(drop_id)));
    }
    app.app_mut().ack_events_for("chat", drop_id).await.unwrap();
    assert!(!app
        .app_mut()
        .unacknowledged_events_for("chat")
        .await
        .unwrap()
        .iter()
        .any(|e| e.id() == Some(drop_id)));
}

#[tokio::test]
async fn test_mixed_protocols_reconciliation_requires_each_owner() {
    let w = world();
    let mut original = Paykit::join_with_protocols(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        w.approve(&w.alice, "mixed", &scopes()),
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
        register,
    )
    .await
    .unwrap();
    original.receive(None).await.unwrap();
    let backup = original.backup().await.unwrap();
    let id = original.app_id();
    drop(original);
    for chat_first in [false, true] {
        let storage = store();
        let mut app = Paykit::restore_with_protocols(
            w.env(&w.alice, Arc::clone(&storage)),
            PaykitConfig::default(),
            &backup,
            Arc::clone(&w.rail) as Arc<dyn Rail>,
            w.timer(),
            register,
        )
        .await
        .unwrap();
        app.receive(None).await.unwrap();
        app.reconcile().await.unwrap();
        assert!(app.app().reconcile_mode());
        if chat_first {
            assert!(!app
                .app_mut()
                .end_reconcile_for("chat", UserConfirmed::new())
                .await
                .unwrap());
        }
        app.end_reconcile(UserConfirmed::new()).await.unwrap();
        assert_eq!(
            app.app().reconcile_mode(),
            !chat_first,
            "Paykit cannot finish another owner's reconciliation"
        );
        if !chat_first {
            assert!(app.app().may_attempt(false).is_err());
            drop(app);
            app = Paykit::load_with_protocols(
                w.env(&w.alice, storage),
                PaykitConfig::default(),
                id,
                Arc::clone(&w.rail) as Arc<dyn Rail>,
                w.timer(),
                register,
            )
            .await
            .unwrap();
            assert!(app.app().reconcile_mode());
            assert!(app
                .app_mut()
                .end_reconcile_for("chat", UserConfirmed::new())
                .await
                .unwrap());
        }
        assert!(!app.app().reconcile_mode());
        app.app().may_attempt(false).unwrap();
    }
}
