//! The spam count and extension messages, through the bindings.

use super::method::Account;
use super::refusal::categorized;
use super::wallet::{enrolled_payer, host, run};
use crate::{ExtensionMessage, NativeWallet, RefusalKind, StorageError, WalletError};
use pubky_dispatch_client::app::LocalWrite;
use pubky_dispatch_core::enc::b64;
use pubky_dispatch_harness::{Backend, World};
use serde_json::{json, Value};
use std::{error::Error, sync::Arc};

const KIND: &str = "paykit-cap.offer";
const MSG_ID: &str = "00000000-0000-4000-8000-000000000001";

fn assert_invalid_input<T>(result: Result<T, WalletError>, field: &str) {
    assert!(
        matches!(result, Err(WalletError::InvalidInput { field: name }) if name == field),
        "expected InvalidInput for {field}"
    );
}

async fn check_send_inputs(wallet: &NativeWallet, payee: &[u8]) {
    for (kind, body, field) in [
        ("paykit-cap.", "{}", "kind"),
        ("paykit.payment_request", "{}", "kind"),
        (KIND, r#"["private"]"#, "body_json"),
        (KIND, "null", "body_json"),
        (KIND, "{", "body_json"),
        (KIND, r#"{"x":1,"x":2}"#, "body_json"),
        (KIND, r#"{"price":2.5}"#, "body_json"),
        // Under opaque metadata the wire check passes, so the number reaches the bindings' own.
        (KIND, r#"{"metadata":{"price":1.50}}"#, "body_json"),
    ] {
        let result = wallet
            .send_extension(payee.to_vec(), kind.into(), vec![], body.into())
            .await;
        assert_invalid_input(result, field);
    }
    for (to, refs, field) in [
        (vec![0; 31], vec![], "to"),
        (payee.to_vec(), vec![vec![0; 32], vec![0; 31]], "refs"),
    ] {
        let result = wallet
            .send_extension(to, KIND.into(), refs, "{}".into())
            .await;
        assert_invalid_input(result, field);
    }
}

async fn inbox(world: &World) -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let (_, payer, app_id, _) =
        Box::pin(enrolled_payer(world, root.path(), account.clone())).await?;
    let payee = world.identity("shop")?.public();

    assert_eq!(payer.spam_count(payee.into()).await?, 0);
    assert_invalid_input(payer.spam_count(vec![0; 31]).await, "sender");
    assert!(payer.extension_messages().await?.is_empty());
    payer
        .acknowledge_extension_message(payee.into(), MSG_ID.into())
        .await?;
    assert_invalid_input(
        payer
            .acknowledge_extension_message(vec![0; 33], MSG_ID.into())
            .await,
        "sender",
    );

    check_send_inputs(&payer, &payee).await;
    let result = payer
        .send_extension(
            payee.into(),
            KIND.into(),
            vec![vec![0; 32]],
            r#"{"metadata":{"price":2.5}}"#.into(),
        )
        .await;
    let error = result.err().ok_or("sent without the extension scope")?;
    let (case, failure) = categorized(error).ok_or("expected a categorized send refusal")?;
    assert_eq!(case, "needs_user");
    assert_eq!(failure.code(), "scope_missing");
    let refusal = failure.refusal().ok_or("missing SDK refusal")?;
    assert!(
        matches!(refusal.kind(), RefusalKind::ScopeMissing { scope } if scope == "paykit_ext:paykit-cap")
    );

    payer.shutdown().await?;
    assert!(matches!(
        payer.spam_count(payee.into()).await,
        Err(WalletError::Closed)
    ));
    assert!(matches!(
        payer.extension_messages().await,
        Err(WalletError::Closed)
    ));
    assert!(matches!(
        payer
            .acknowledge_extension_message(payee.into(), MSG_ID.into())
            .await,
        Err(WalletError::Closed)
    ));
    assert!(matches!(
        payer
            .send_extension(payee.into(), KIND.into(), vec![], "{}".into())
            .await,
        Err(WalletError::Closed)
    ));
    let reopened = host(world, root.path(), account).load(app_id).await?;
    assert!(reopened.extension_messages().await?.is_empty());
    // A kept entry that fails to decode must surface a storage error on the host's read.
    let unreadable = LocalWrite::Put {
        key: format!("xm/{}/{MSG_ID}", b64(&payee)).into_bytes(),
        value: b"{}".to_vec(),
    };
    reopened
        .lend(async move |app| app.app_mut().protocol_commit("paykit", &[unreadable]).await)
        .await??;
    assert!(matches!(
        reopened.extension_messages().await,
        Err(WalletError::Storage {
            failure: StorageError::Corrupt
        })
    ));
    reopened.shutdown().await?;
    Ok(())
}

#[test]
fn test_extensions_payer_inbox_on_scripted() {
    run(Backend::Scripted, inbox);
}

#[test]
fn test_extensions_payer_inbox_on_service() {
    run(Backend::Service, inbox);
}

async fn send_extension(world: &World) -> super::TestResult {
    let root = tempfile::tempdir()?;
    let account = Arc::new(Account::open(root.path(), world.clock().clone()));
    let scopes = crate::WalletScopes {
        contacts: true,
        extensions: vec!["paykit-cap".into()],
        ..Default::default()
    };
    let (_, wallet, _, _) = Box::pin(super::wallet::enrolled_wallet(
        world,
        root.path(),
        account,
        scopes,
        None,
    ))
    .await?;
    let recipient = world.identity("other-buyer")?.public();
    let id = wallet
        .send_extension(recipient.into(), KIND.into(), vec![], "{}".into())
        .await?;
    pubky_dispatch_core::json::check_uuid_v4(&id)?;
    assert!(wallet.message_delivery(id).await?.is_some());
    wallet.shutdown().await?;
    Ok(())
}

#[test]
fn test_extensions_send_returns_a_message_id() {
    run(Backend::Scripted, send_extension);
    run(Backend::Service, send_extension);
}

#[test]
fn test_extensions_message_preserves_fields_and_redacts_body() -> Result<(), Box<dyn Error>> {
    let refs = [b64(&[3; 32]), b64(&[4; 32])];
    let body = json!({"metadata": {"private": [2.5, "body sentinel"]}});
    let sdk: paykit_sdk::ExtensionMessage = serde_json::from_value(json!({
        "sender": b64(&[1; 32]), "sender_app": b64(&[2; 16]), "kind": KIND, "msg_id": MSG_ID,
        "created_at_ms": 10, "accepted_at_ms": 20, "generation": 3, "seq": 4,
        "refs": refs, "body": body,
    }))?;
    let message = ExtensionMessage::from(sdk);
    assert_eq!(message.sender, [1; 32]);
    assert_eq!(message.sender_app, [2; 16]);
    assert_eq!(message.kind, KIND);
    assert_eq!(message.msg_id, MSG_ID);
    assert_eq!(message.created_at_ms, 10);
    assert_eq!(message.accepted_at_ms, 20);
    assert_eq!(message.generation, 3);
    assert_eq!(message.seq, 4);
    assert_eq!(message.refs, refs);
    assert_eq!(serde_json::from_str::<Value>(&message.body_json)?, body);
    let debug = format!("{message:?}");
    assert!(debug.contains(&format!("body_json: {}", message.body_json.len())));
    assert!(!debug.contains("private") && !debug.contains("body sentinel"));
    Ok(())
}
