//! Certificates holding several protocols' scopes (Switch wire 3.1 "Several protocols",
//! Paykit wire 3.1): another protocol's scopes authorize nothing in Paykit.

use paykit_sdk::Paykit;
use pubky_switch_client::app::{AppEvent, Protocol};
use pubky_switch_client::testing::AppSpec;
use pubky_switch_core::scopes::Permission;
use serde_json::json;

use crate::common::World;

/// Bob's app `name`, registered at his switch with `scopes` and `permissions`, sends alice's
/// `wallet` a `paykit.payment_request`. Returns whether the wallet dropped it at check 8 and
/// whether it recorded the request.
async fn request_from_app(scopes: &[&str], permissions: &[Permission]) -> (bool, bool) {
    let w = World::new();
    let mut wallet = w.wallet("wallet").await;
    let keys = w.bob.identity().app_keys("app");
    w.bob
        .register_app(&keys, &AppSpec::new(scopes).with_permissions(permissions))
        .unwrap();
    let body = json!({
        "amount": { "asset": "btc", "value": "1" },
        "payment_reference": "order-1",
        "proposal_expires_at": null,
        "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
    });
    w.inject(
        &w.bob,
        "app",
        &w.alice,
        &[wallet.app_id()],
        "paykit.payment_request",
        Vec::new(),
        body.as_object().cloned().unwrap(),
    );
    let summary = wallet.receive(None).await.unwrap();
    let dropped = summary
        .report
        .events
        .iter()
        .any(|e| matches!(e, AppEvent::MessageDropped { check: 8, .. }));
    let recorded = !wallet.requests().await.unwrap().is_empty();
    (dropped, recorded)
}

#[tokio::test]
async fn test_protocols_chat_only_payment_request_dropped() {
    // A chat-only certificate is valid at Paykit's verifiers, but its payment_request is
    // dropped at check 8: it holds no `request` (Paykit spec 3.3).
    let (dropped, recorded) = request_from_app(&["chat"], &[Permission::ListsRead]).await;
    assert!(dropped, "dropped at check 8");
    assert!(!recorded);
}

#[tokio::test]
async fn test_protocols_payee_that_also_chats_request_recorded() {
    // `request` beside `chat`, with the chat's lists.read: a Paykit App, and its request is
    // taken.
    let (dropped, recorded) = request_from_app(
        &["request", "chat"],
        &[Permission::ListsRead, Permission::ConversationAdd],
    )
    .await;
    assert!(!dropped);
    assert!(recorded);
}

#[tokio::test]
async fn test_protocols_chat_only_app_joins_but_takes_no_paykit_kind() {
    // Paykit's verifiers take a chat-only certificate as valid, so such an app joins with
    // Paykit as its protocol; its record accepts no Paykit kind.
    let w = World::new();
    let scopes = paykit_sdk::RequestedScopes::new(vec!["chat".into()], vec![Permission::ListsRead]);
    let app: Paykit = w
        .join_with(
            &w.alice,
            "chat",
            scopes,
            paykit_sdk::PaykitConfig::default(),
            crate::common::store(),
        )
        .await;
    assert!(app.protocol().accepts(app.app().cert()).is_empty());
}
