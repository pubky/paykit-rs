//! A restore's first read precedes outgoing requests (Switch spec 12.4).

use std::sync::Arc;

use paykit_sdk::{MessageDelivery, Paykit, PaykitConfig, Rail, Refusal};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;

use crate::common::{draft, request_from, store, ScriptedIdentityReads, World};

async fn queued_request(w: &World, shop: &mut Paykit) -> String {
    w.alice.switch().fail_next(Fault::Error {
        path: "inbox".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    assert!(matches!(sent.delivery, MessageDelivery::Queued { .. }));
    sent.msg_id
}

async fn restore_shop(w: &World, shop: &mut Paykit, reads: &Arc<ScriptedIdentityReads>) -> Paykit {
    let backup = shop.backup().await.unwrap();
    let mut env = w.env(&w.bob, store());
    env.homeserver = Arc::clone(reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>;
    let restored = Paykit::restore(
        env,
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().restore_held_cursor(), None);
    restored
}

#[tokio::test]
async fn test_restore_first_read_holds_a_queued_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    assert!(matches!(
        restored.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Queued { .. })
    ));
    reads.then(w.bob_id(), 0, true);
    let result = restored.flush().await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    let error = result.expect_err("the restore's identity read is unavailable");
    assert!(matches!(
        error.refusal(),
        Some(Refusal::IdentityUnavailable(_))
    ));
    assert!(matches!(
        restored.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Queued { .. })
    ));
}

#[tokio::test]
async fn test_restore_first_read_holds_a_new_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    reads.then(w.bob_id(), 0, true);
    let result = restored
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await;
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    let error = result.expect_err("the restore's identity read is unavailable");
    assert!(matches!(
        error.refusal(),
        Some(Refusal::IdentityUnavailable(_))
    ));
    // The failed first read leaves the public activity view unlisted. Inspect the
    // retained records without asserting that the SDK view is already available.
    assert!(restored
        .app()
        .records_of("payee_activity")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_restore_first_read_allows_the_original_request_after_confirmation() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    assert!(restored
        .app_mut()
        .refresh_identity()
        .await
        .unwrap()
        .confirmed_at()
        .is_some());
    restored.receive(None).await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    restored.flush().await.unwrap();
    let received = wallet.receive(None).await.unwrap();
    let requests = wallet.requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(received.applied.contains(&msg_id));
    assert!(matches!(
        restored.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Accepted { .. })
    ));
}

#[tokio::test]
async fn test_restore_first_read_retries_a_queued_reply_after_reload() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    w.bob.switch().fail_next(Fault::Error {
        path: "inbox".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    w.bob.switch().fail_next(Fault::Error {
        path: "inbox".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    let msg_id = wallet.reject_request(&request, None).await.unwrap();
    assert!(matches!(
        wallet.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Queued { .. })
    ));
    let backup = wallet.backup().await.unwrap();
    let storage = store();
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut env = w.env(&w.alice, Arc::clone(&storage));
    env.homeserver = Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>;
    let mut restored = Paykit::restore(
        env,
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let app_id = restored.app_id();
    for reload in [false, true] {
        if reload {
            drop(restored);
            restored = w
                .load_with_homeserver(
                    &w.alice,
                    Arc::clone(&storage),
                    app_id,
                    Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>,
                )
                .await;
        }
        assert_eq!(restored.app().restore_held_cursor(), None);
        reads.then(w.alice_id(), 0, true);
        let error = restored.flush().await.unwrap_err();
        assert!(matches!(
            error.refusal(),
            Some(Refusal::IdentityUnavailable(_))
        ));
        assert!(matches!(
            restored.message_delivery(&msg_id).await.unwrap(),
            Some(MessageDelivery::Queued { .. })
        ));
        let summary = shop.receive(None).await.unwrap();
        assert!(!summary.applied.contains(&msg_id));
        assert_eq!(
            shop.payee_activity().await.unwrap()[0].status,
            paykit_sdk::PayeeStatus::Requested
        );
    }
    restored.flush().await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    let summary = shop.receive(None).await.unwrap();
    assert!(summary.applied.contains(&msg_id));
    assert_eq!(
        shop.payee_activity().await.unwrap()[0].status,
        paykit_sdk::PayeeStatus::Rejected
    );
    assert!(matches!(
        restored.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Accepted { .. })
    ));
}

#[tokio::test]
async fn test_restore_first_read_retries_after_feed_failure() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    w.bob.switch().fail_next(Fault::Error {
        path: "/feed".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    assert!(restored.flush().await.is_err());
    wallet.receive(None).await.unwrap();
    assert!(wallet.requests().await.unwrap().is_empty());
    assert!(matches!(
        restored.message_delivery(&msg_id).await.unwrap(),
        Some(MessageDelivery::Queued { .. })
    ));
    restored.flush().await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.applied.contains(&msg_id));
    assert_eq!(wallet.requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_restore_first_read_preserves_the_implicit_report_through_an_error() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    drop(wallet);
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.flush().await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    assert!(restored
        .app()
        .sent_to(&w.bob_id(), 0, u64::MAX)
        .await
        .unwrap()
        .iter()
        .any(|message| message.kind == "paykit.resend_request"));
    assert!(
        matches!(sent.delivery, MessageDelivery::Accepted { .. }),
        "{:?}",
        sent.delivery
    );
    w.alice.switch().fail_next(Fault::Error {
        path: "/feed".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    assert!(restored.receive(None).await.is_err());
    let summary = restored.receive(None).await.unwrap();
    assert!(
        summary.resends >= 1,
        "the implicit read's resend request stays in its summary"
    );
    assert_eq!(
        summary
            .applied
            .iter()
            .filter(|id| *id == &sent.msg_id)
            .count(),
        1,
        "{summary:?}"
    );
    assert_eq!(
        summary
            .report
            .events
            .iter()
            .filter(|event| matches!(event,
        pubky_switch_client::app::AppEvent::MessageReceived { msg_id, .. }
        if msg_id == &sent.msg_id))
            .count(),
        1
    );
    let later = restored.receive(None).await.unwrap();
    assert!(!later.applied.contains(&sent.msg_id));
}

#[tokio::test]
async fn test_restore_first_read_does_not_confirm_a_held_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let request = request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    restored.flush().await.unwrap();
    assert!(restored.app().restore_held_cursor().is_some());
    assert!(restored.request(&request).await.unwrap().held.is_some());
    assert!(restored
        .app()
        .protocol_scan(b"hc/")
        .await
        .unwrap()
        .is_empty());
    assert!(w.rail.all_executions().is_empty());
}
#[tokio::test]
async fn test_restore_first_read_block_refusal_keeps_other_current_blocks() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let backup = wallet.backup().await.unwrap();
    let mut manager = w
        .join(
            &w.alice,
            "manager",
            paykit_sdk::AppScopes::new()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    manager.block(&w.carol_id()).await.unwrap();
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().restore_held_cursor(), None);
    let error = restored.block(&w.bob_id()).await.unwrap_err();
    assert!(
        matches!(
            error,
            paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Refused(_))
        ),
        "{error:?}"
    );
    assert!(restored.app().restore_held_cursor().is_some());
    let mark = format!("bk/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    assert!(restored
        .app()
        .protocol_get(mark.as_bytes())
        .await
        .unwrap()
        .is_none());
    let bytes = restored.app().protocol_get(b"dl").await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let denied: Vec<[u8; 32]> = serde_json::from_value(value["d"].clone()).unwrap();
    assert_eq!(denied, vec![w.carol_id()]);
    let actual = restored.app_mut().allow_list().await.unwrap();
    assert_eq!(actual.deny, denied);
}

#[tokio::test]
async fn test_restore_first_read_keeps_block_intent_when_identity_is_unavailable() {
    let w = World::new();
    let mut wallet = w
        .join(
            &w.alice,
            "manager",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await;
    let backup = wallet.backup().await.unwrap();
    let storage = store();
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut env = w.env(&w.alice, Arc::clone(&storage));
    env.homeserver = Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>;
    let mut restored = Paykit::restore(
        env,
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    reads.then(w.alice_id(), 0, true);
    let error = restored.block(&w.bob_id()).await.unwrap_err();
    assert!(matches!(
        error.refusal(),
        Some(Refusal::IdentityUnavailable(_))
    ));
    let mark = format!("bk/{}", pubky_switch_core::enc::b64(&w.bob_id()));
    let before = restored
        .app()
        .protocol_get(mark.as_bytes())
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(value["d"]["blocked"], true);
    assert!(value["d"]["batch"].is_null());
    let app_id = restored.app_id();
    drop(restored);
    let mut restored = w
        .load_with_homeserver(
            &w.alice,
            storage,
            app_id,
            Arc::clone(&reads) as Arc<dyn pubky_switch_client::app::HomeserverReader>,
        )
        .await;
    assert_eq!(
        restored.app().protocol_get(mark.as_bytes()).await.unwrap(),
        Some(before)
    );
    restored.flush().await.unwrap();
    assert!(restored
        .app_mut()
        .allow_list()
        .await
        .unwrap()
        .deny
        .contains(&w.bob_id()));
    assert!(restored
        .app()
        .protocol_get(mark.as_bytes())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn test_restore_first_read_missing_identity_holds_the_queued_request() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    w.homeserver
        .remove(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH);
    assert_eq!(restored.app().identity_confirmed_at(), None);
    let result = restored.flush().await;
    assert_eq!(restored.app().identity_confirmed_at(), None);
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        !summary.applied.contains(&msg_id),
        "queued request delivered with no identity confirmation; flush={result:?}, verdict={:?}",
        restored.app().restore_held_cursor()
    );
    let delivery = restored.message_delivery(&msg_id).await.unwrap();
    assert!(
        matches!(delivery, Some(MessageDelivery::Queued { .. })),
        "unconfirmed identity delivery={delivery:?}; flush={result:?}; verdict={:?}",
        restored.app().restore_held_cursor()
    );
}

#[tokio::test]
async fn test_restore_first_read_older_identity_holds_the_queued_request() {
    use pubky_switch_client::app::HomeserverReader;
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let old = w
        .homeserver
        .get(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH.into())
        .await
        .unwrap()
        .unwrap();
    let msg_id = queued_request(&w, &mut shop).await;
    let _spare = w
        .join(
            &w.bob,
            "spare",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    shop.receive(None).await.unwrap();
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    w.homeserver
        .put(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH, old);
    assert_eq!(restored.app().identity_confirmed_at(), None);
    assert!(matches!(
        restored.app_mut().refresh_identity().await.unwrap(),
        pubky_switch_client::app::IdentityRefresh::Older { .. }
    ));
    let result = restored.flush().await;
    assert_eq!(restored.app().identity_confirmed_at(), None);
    let delivery = restored.message_delivery(&msg_id).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(
        !summary.applied.contains(&msg_id),
        "queued request delivered after unconfirmed Older identity; flush={result:?}, verdict={:?}",
        restored.app().restore_held_cursor()
    );
    assert!(
        matches!(delivery, Some(MessageDelivery::Queued { .. })),
        "unconfirmed identity delivery={delivery:?}; flush={result:?}; verdict={:?}",
        restored.app().restore_held_cursor()
    );
}
