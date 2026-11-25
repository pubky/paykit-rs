//! A restore's first read precedes outgoing requests (Switch spec 12.4).

use std::sync::Arc;

use paykit_sdk::{MessageDelivery, Paykit, PaykitConfig, Rail, Refusal};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::testing::Fault;
use pubky_switch_core::http::ErrorCode;

use crate::common::{draft, request_from, store, ScriptedIdentityReads, World};

async fn queued_request(w: &World, shop: &mut Paykit) -> String {
    w.alice.switch().fail_next(Fault::Error {
        path: "/inbox".into(),
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
        path: "/inbox".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    w.bob.switch().fail_next(Fault::Error {
        path: "/inbox".into(),
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
async fn test_restore_first_read_releases_a_held_message_only_once_it_is_complete() {
    // The restore holds the request queued before the backup (Switch spec 12.4, "The
    // outbox"). A flush whose first read fails releases nothing; once the first read is
    // complete, the next flush releases and delivers it, and nothing is left held.
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let reads = Arc::new(ScriptedIdentityReads::new(
        Arc::clone(&w.homeserver),
        Arc::clone(&w.clock),
    ));
    let mut restored = restore_shop(&w, &mut shop, &reads).await;
    assert_eq!(
        restored.app().held_messages("paykit").await.unwrap(),
        vec![msg_id.clone()]
    );
    w.bob.switch().fail_next(Fault::Error {
        path: "/feed".into(),
        code: ErrorCode::Unavailable,
        current: serde_json::Value::Null,
    });
    assert!(restored.flush().await.is_err());
    assert_eq!(
        restored.app().held_messages("paykit").await.unwrap(),
        vec![msg_id.clone()]
    );
    restored.flush().await.unwrap();
    assert!(restored
        .app()
        .held_messages("paykit")
        .await
        .unwrap()
        .is_empty());
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.applied.contains(&msg_id));
}

#[tokio::test]
async fn test_restore_first_read_preserves_the_implicit_report_through_an_error() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    request_from(&mut shop, w.alice_id(), draft("1", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    let backup = wallet.backup().await.unwrap();
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
    drop(wallet);
    // The backup is behind the service cursor. This request is still unread by either wallet.
    let sent = shop
        .send_request(w.alice_id(), draft("3", w.clock.now_ms()))
        .await
        .unwrap();
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
    assert!(matches!(
        restored.app().restore_held_cursor(),
        Some(pubky_switch_client::app::HeldCursor::Ahead { .. })
    ));
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
    request_from(&mut shop, w.alice_id(), draft("2", w.clock.now_ms())).await;
    wallet.receive(None).await.unwrap();
    wallet.receive(None).await.unwrap();
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
    assert!(matches!(
        restored.app().restore_held_cursor(),
        Some(pubky_switch_client::app::HeldCursor::Ahead { .. })
    ));
    assert!(restored.request(&request).await.unwrap().held.is_some());
    assert!(restored
        .app()
        .protocol_scan("paykit", b"hc/")
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
        .protocol_get("paykit", mark.as_bytes())
        .await
        .unwrap()
        .is_none());
    let bytes = restored
        .app()
        .protocol_get("paykit", b"dl")
        .await
        .unwrap()
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let denied: Vec<[u8; 32]> = serde_json::from_value(value["d"].clone()).unwrap();
    assert_eq!(denied, vec![w.carol_id()]);
    let actual = restored.app_mut().allow_list().await.unwrap();
    assert_eq!(actual.deny, denied);
}

#[tokio::test]
async fn test_restore_first_read_block_refusal_preserves_the_page_report() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let backup = wallet.backup().await.unwrap();
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    assert!(matches!(sent.delivery, MessageDelivery::Accepted { .. }));
    let mut restored = Paykit::restore(
        w.env(&w.alice, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    let error = restored.block(&w.carol_id()).await.unwrap_err();
    assert!(matches!(
        error,
        paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Refused(_))
    ));
    assert!(restored.app().restore_held_cursor().is_some());
    let summary = restored.receive(None).await.unwrap();
    assert_eq!(
        summary
            .applied
            .iter()
            .filter(|id| *id == &sent.msg_id)
            .count(),
        1
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
    assert!(!later.report.events.iter().any(|event| matches!(event,
        pubky_switch_client::app::AppEvent::MessageReceived { msg_id, .. }
        if msg_id == &sent.msg_id)));
    assert!(w.rail.all_executions().is_empty());
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
        .protocol_get("paykit", mark.as_bytes())
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
        restored
            .app()
            .protocol_get("paykit", mark.as_bytes())
            .await
            .unwrap(),
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
        .protocol_get("paykit", mark.as_bytes())
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

#[tokio::test]
async fn test_restore_first_read_newer_directory_identity_requires_confirmation() {
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
    let backup = shop.backup().await.unwrap();
    let _spare = w
        .join(
            &w.bob,
            "spare",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    w.homeserver
        .put(w.bob_id(), pubky_switch_core::records::IDENTITY_PATH, old);
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().restore_held_cursor(), None);
    assert_eq!(restored.app().identity_confirmed_at(), None);
    let result = restored.flush().await;
    assert_eq!(
        restored.app().identity_confirmed_at(),
        None,
        "newer directory copy must not be mistaken for the prior Homeserver confirmation"
    );
    let delivery = restored.message_delivery(&msg_id).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(!summary.applied.contains(&msg_id), "request delivered while newest own identity is unconfirmed; flush={result:?}; verdict={:?}; delivery={delivery:?}", restored.app().restore_held_cursor());
    assert!(
        matches!(delivery, Some(MessageDelivery::Queued { .. })),
        "delivery={delivery:?}; flush={result:?}; verdict={:?}",
        restored.app().restore_held_cursor()
    );
}

#[tokio::test]
async fn test_restore_first_read_newer_directory_identity_confirmed_sends() {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let mut shop = w.shop().await;
    let msg_id = queued_request(&w, &mut shop).await;
    let backup = shop.backup().await.unwrap();
    let _spare = w
        .join(
            &w.bob,
            "spare",
            paykit_sdk::AppScopes::new().request().build().unwrap(),
        )
        .await;
    let mut restored = Paykit::restore(
        w.env(&w.bob, store()),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    assert_eq!(restored.app().restore_held_cursor(), None);
    assert_eq!(restored.app().identity_confirmed_at(), None);
    let result = restored.flush().await;
    assert!(restored.app().identity_confirmed_at().is_some());
    let delivery = restored.message_delivery(&msg_id).await.unwrap();
    let summary = wallet.receive(None).await.unwrap();
    assert!(summary.applied.contains(&msg_id), "request delivered while newest own identity is unconfirmed; flush={result:?}; verdict={:?}; delivery={delivery:?}", restored.app().restore_held_cursor());
    assert!(
        matches!(delivery, Some(MessageDelivery::Accepted { .. })),
        "delivery={delivery:?}; flush={result:?}; verdict={:?}",
        restored.app().restore_held_cursor()
    );
}

#[derive(Clone, Copy)]
enum BlockFailure {
    None,
    Keep,
    Commit,
}

struct BlockFailureStore {
    inner: pubky_switch_client::store::MemoryStore,
    target: Vec<u8>,
    failure: BlockFailure,
    finalization_fails: bool,
    armed: std::sync::atomic::AtomicBool,
    failures: std::sync::atomic::AtomicUsize,
}

impl pubky_switch_client::store::StateStore for BlockFailureStore {
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
        use std::sync::atomic::Ordering;
        if self.finalization_fails
            && prefix == b"duty/"
            && self
                .failures
                .compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(pubky_switch_client::error::StoreError::Corrupt);
        }
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
impl BlockFailureStore {
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
        use std::sync::atomic::Ordering;
        let matches = batch
            .writes()
            .iter()
            .any(|write| match (self.failure, write) {
                (BlockFailure::Commit, Write::Delete { key, .. }) => key == &self.target,
                (BlockFailure::Keep, Write::Put { key, value, .. }) if key.starts_with(b"opq/") => {
                    let value: serde_json::Value = serde_json::from_slice(value).unwrap();
                    let bytes = pubky_switch_core::enc::unb64(value["d"]["body"].as_str().unwrap())
                        .unwrap();
                    pubky_switch_client::client::Batch::from_body(bytes)
                        .unwrap()
                        .ops()
                        .iter()
                        .any(|op| {
                            matches!(
                                op.body().kind,
                                pubky_switch_core::ops::OpKind::DenyPut { .. }
                            )
                        })
                }
                _ => false,
            });
        if matches && self.armed.swap(false, Ordering::SeqCst) {
            self.failures.fetch_add(1, Ordering::SeqCst);
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        apply(batch)
    }
}

async fn block_after_first_page(failure: BlockFailure, manages: bool, finalization_fails: bool) {
    use pubky_switch_client::error::StoreError;
    use pubky_switch_client::store::StateStore;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let w = World::new();
    let mut wallet = if manages {
        w.join(
            &w.alice,
            "wallet",
            paykit_sdk::AppScopes::new()
                .pay()
                .manage_contacts()
                .build()
                .unwrap(),
        )
        .await
    } else {
        w.wallet("wallet").await
    };
    if manages {
        wallet.app_mut().invite(&w.bob_id()).await.unwrap();
    }
    let mut shop = w.shop().await;
    let backup = wallet.backup().await.unwrap();
    let sent = shop
        .send_request(w.alice_id(), draft("1", w.clock.now_ms()))
        .await
        .unwrap();
    assert!(matches!(sent.delivery, MessageDelivery::Accepted { .. }));
    let target = format!("p/paykit/bk/{}", pubky_switch_core::enc::b64(&w.carol_id())).into_bytes();
    let faults = Arc::new(BlockFailureStore {
        inner: Default::default(),
        target,
        failure,
        finalization_fails,
        armed: AtomicBool::new(false),
        failures: AtomicUsize::new(0),
    });
    let mut restored = Paykit::restore(
        w.env(&w.alice, Arc::clone(&faults) as Arc<dyn StateStore>),
        PaykitConfig::default(),
        &backup,
        Arc::clone(&w.rail) as Arc<dyn Rail>,
        w.timer(),
    )
    .await
    .unwrap();
    faults.armed.store(true, Ordering::SeqCst);
    let result = restored.block(&w.carol_id()).await;
    match (failure, finalization_fails) {
        (BlockFailure::None, false) => result.unwrap(),
        (_, true) => assert!(
            matches!(
                result,
                Err(paykit_sdk::PaykitError::Client(
                    pubky_switch_client::app::AppError::Store(StoreError::Corrupt)
                ))
            ),
            "{result:?}"
        ),
        _ => assert!(
            matches!(
                result,
                Err(paykit_sdk::PaykitError::Client(
                    pubky_switch_client::app::AppError::Store(StoreError::Unavailable)
                ))
            ),
            "{result:?}"
        ),
    }
    let expected = if matches!(failure, BlockFailure::None) {
        0
    } else {
        1 + usize::from(finalization_fails)
    };
    assert_eq!(faults.failures.load(Ordering::SeqCst), expected);
    assert!(restored.app().restore_held_cursor().is_some());
    if !finalization_fails {
        let summary = restored.receive(None).await.unwrap();
        assert_eq!(
            summary
                .applied
                .iter()
                .filter(|id| *id == &sent.msg_id)
                .count(),
            1
        );
        assert_eq!(summary.report.events.iter().filter(|event| matches!(event,
            pubky_switch_client::app::AppEvent::MessageReceived { msg_id, .. } if msg_id == &sent.msg_id)).count(), 1);
        let later = restored.receive(None).await.unwrap();
        assert!(!later.applied.contains(&sent.msg_id));
        assert!(!later.report.events.iter().any(|event| matches!(event,
            pubky_switch_client::app::AppEvent::MessageReceived { msg_id, .. } if msg_id == &sent.msg_id)));
    }
    assert!(w.rail.all_executions().is_empty());
}

#[tokio::test]
async fn test_restore_first_read_block_success_reports_the_page_once() {
    Box::pin(block_after_first_page(BlockFailure::None, true, false)).await;
}

#[tokio::test]
async fn test_restore_first_read_block_store_error_reports_the_page_once() {
    Box::pin(block_after_first_page(BlockFailure::Keep, true, false)).await;
}

#[tokio::test]
async fn test_restore_first_read_block_commit_error_reports_the_page_once() {
    Box::pin(block_after_first_page(BlockFailure::Commit, true, false)).await;
}

#[tokio::test]
async fn test_restore_first_read_block_rollback_error_reports_the_page_once() {
    Box::pin(block_after_first_page(BlockFailure::Commit, false, false)).await;
}

#[tokio::test]
async fn test_restore_first_read_finalization_error_precedes_block_error() {
    Box::pin(block_after_first_page(BlockFailure::Commit, false, true)).await;
}
