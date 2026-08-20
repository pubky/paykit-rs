use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use super::*;
use crate::{
    domain::adapters::PrivateReceivingDetail,
    domain::outbound_private::queued_outbound_private_messages,
    domain::private_stream::persist_private_stream_batch,
    storage::{InMemoryStorage, PrivateStreamItemRecord},
    PaykitSdkError,
};
use paykit_lib::PrivateApplicationMessage;

fn timestamp() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
}

fn counterparty() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
}

fn app_id() -> paykit_lib::PaykitAppId {
    paykit_lib::PaykitAppId::new("bitkit").unwrap()
}

fn registered_storage() -> InMemoryStorage {
    InMemoryStorage::with_registered_apps([app_id()])
}

fn outbound_private_list(
    outbound_message_id: u64,
    counterparty: PubkyPublicKey,
    app_id: paykit_lib::PaykitAppId,
    status: OutboundPrivateMessageStatus,
    last_attempt_at: Option<DateTime<Utc>>,
    raw_json: String,
) -> OutboundPrivateMessageRecord {
    let sent_at = (status == OutboundPrivateMessageStatus::Sent).then_some(timestamp());
    OutboundPrivateMessageRecord {
        outbound_message_id,
        counterparty,
        app_id,
        kind: PrivateMessageKind::PrivatePaymentList.as_str().into(),
        raw_json,
        status,
        attempt_count: u32::from(last_attempt_at.is_some()),
        created_at: timestamp(),
        updated_at: timestamp(),
        last_attempt_at,
        sent_at,
        last_error: None,
    }
}

fn stream_item(
    stream_item_id: u64,
    raw_json: &str,
    status: PrivateStreamParseStatus,
) -> PrivateStreamItemRecord {
    PrivateStreamItemRecord {
        stream_item_id,
        counterparty: counterparty(),
        receive_batch_id: 0,
        raw_json: raw_json.into(),
        parsed_version: Some(1),
        parsed_kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        parsed_app_id: Some(app_id().as_str().into()),
        known_paykit_kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        parse_status: status,
        parse_error: None,
        received_at: timestamp(),
    }
}

fn list_json(payload: &str) -> String {
    list_json_for_app("bitkit", payload)
}

fn list_json_for_app(app_id: &str, payload: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.private_payment_list","app_id":"{app_id}","payment_endpoints":{{"btc-lightning-bolt11":"{payload}"}}}}"#
    )
}

fn private_message(raw_json: String) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        app_id: Some(app_id().as_str().into()),
        raw_json,
    }
}

#[test]
fn test_derive_private_payment_list_views_uses_latest_valid_list() {
    let first = list_json("ln-old");
    let latest = list_json("ln-new");

    let view = derive_private_payment_list_views(vec![
        stream_item(1, &latest, PrivateStreamParseStatus::Valid),
        stream_item(0, &first, PrivateStreamParseStatus::Valid),
    ])
    .unwrap()
    .pop()
    .unwrap();

    assert_eq!(view.app_id, app_id());
    assert_eq!(view.latest_stream_item_id, Some(1));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-new");
    assert_eq!(view.last_refresh_at, Some(timestamp()));
}

#[test]
fn test_derive_private_payment_list_views_keeps_latest_list_per_app() {
    let mut bitkit_old = stream_item(
        0,
        &list_json_for_app("bitkit", "ln-bitkit-old"),
        PrivateStreamParseStatus::Valid,
    );
    bitkit_old.parsed_app_id = Some("bitkit".into());
    let mut server = stream_item(
        1,
        &list_json_for_app("paykit-server", "ln-server"),
        PrivateStreamParseStatus::Valid,
    );
    server.parsed_app_id = Some("paykit-server".into());
    let mut bitkit_new = stream_item(
        2,
        &list_json_for_app("bitkit", "ln-bitkit-new"),
        PrivateStreamParseStatus::Valid,
    );
    bitkit_new.parsed_app_id = Some("bitkit".into());

    let views = derive_private_payment_list_views(vec![bitkit_old, server, bitkit_new]).unwrap();

    assert_eq!(views.len(), 2);
    assert_eq!(views[0].app_id.as_str(), "bitkit");
    assert_eq!(
        views[0].payment_endpoints["btc-lightning-bolt11"],
        "ln-bitkit-new"
    );
    assert_eq!(views[1].app_id.as_str(), "paykit-server");
    assert_eq!(
        views[1].payment_endpoints["btc-lightning-bolt11"],
        "ln-server"
    );
}

#[test]
fn test_private_payment_list_view_debug_redacts_payloads() {
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert("btc-lightning-bolt11".into(), "ln-private-secret".into());
    let view = PrivatePaymentListView {
        app_id: app_id(),
        latest_stream_item_id: Some(42),
        payment_endpoints,
        last_refresh_at: Some(timestamp()),
    };

    let debug = format!("{view:?}");

    assert!(debug.contains("btc-lightning-bolt11"));
    assert!(!debug.contains("ln-private-secret"));
}

#[test]
fn test_derive_private_payment_list_views_ignores_malformed_newer_list() {
    let valid = list_json("ln-valid");
    let malformed = r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"../bad":"ln-bad"}}"#;

    let view = derive_private_payment_list_views(vec![
        stream_item(0, &valid, PrivateStreamParseStatus::Valid),
        stream_item(1, malformed, PrivateStreamParseStatus::MalformedRecognized),
    ])
    .unwrap()
    .pop()
    .unwrap();

    assert_eq!(view.latest_stream_item_id, Some(0));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-valid");
}

#[tokio::test]
async fn test_current_private_payment_lists_loads_from_storage() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let json = list_json("ln-storage");

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        vec![private_message(json)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let view = current_private_payment_lists(&storage, &counterparty)
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert_eq!(view.latest_stream_item_id, Some(0));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-storage");
}

#[tokio::test]
async fn test_enqueue_private_payment_list_stores_exact_list_message() {
    let storage = registered_storage();
    let counterparty = counterparty();

    let record = enqueue_private_payment_list(
        &storage,
        counterparty.clone(),
        app_id(),
        vec![PrivateReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private".into(),
        }],
        timestamp(),
    )
    .await
    .unwrap();

    let queued = queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap();
    assert_eq!(record.outbound_message_id, queued[0].outbound_message_id);
    let list = parse_private_payment_list_json(&queued[0].raw_json).unwrap();
    assert_eq!(list.kind(), PrivateMessageKind::PrivatePaymentList);
    assert_eq!(
        list.get(&paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap())
            .unwrap()
            .as_str(),
        "ln-private"
    );
}

#[tokio::test]
async fn test_enqueue_private_payment_list_with_link_lease_rejects_stale_lease() {
    let storage = registered_storage();
    let counterparty = counterparty();
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + ChronoDuration::seconds(10),
                    )
                    .unwrap())
            }
        })
        .await
        .unwrap();
    storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                let _ = tx.claim_peer_link_operation(
                    &counterparty,
                    timestamp() + ChronoDuration::seconds(11),
                    timestamp() + ChronoDuration::seconds(71),
                );
                Ok(())
            }
        })
        .await
        .unwrap();

    let result = enqueue_private_payment_list_with_link_lease(
        &storage,
        counterparty.clone(),
        app_id(),
        vec![PrivateReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private".into(),
        }],
        timestamp(),
        &stale_lease,
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(queued_outbound_private_messages(&storage, &counterparty)
        .await
        .unwrap()
        .is_empty());
}

#[test]
fn test_shared_private_payment_list_requires_confirmed_newer_clear() {
    let counterparty = counterparty();
    let app_id = app_id();
    let mut messages = Vec::new();
    messages.push(outbound_private_list(
        0,
        counterparty.clone(),
        app_id.clone(),
        OutboundPrivateMessageStatus::Sent,
        Some(timestamp()),
        list_json("ln-shared"),
    ));
    assert_eq!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id).unwrap(),
        HashSet::from([counterparty.clone()])
    );

    messages.push(outbound_private_list(
        1,
        counterparty.clone(),
        app_id.clone(),
        OutboundPrivateMessageStatus::Superseded,
        Some(timestamp()),
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#.into(),
    ));
    assert_eq!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id).unwrap(),
        HashSet::from([counterparty.clone()])
    );

    messages.push(outbound_private_list(
        2,
        counterparty.clone(),
        app_id.clone(),
        OutboundPrivateMessageStatus::Sent,
        Some(timestamp()),
        r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#.into(),
    ));
    assert!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id)
            .unwrap()
            .is_empty()
    );

    messages.push(outbound_private_list(
        3,
        counterparty.clone(),
        app_id.clone(),
        OutboundPrivateMessageStatus::RecoveryRequired,
        Some(timestamp()),
        list_json("ln-maybe-shared"),
    ));
    assert_eq!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id).unwrap(),
        HashSet::from([counterparty])
    );
}

#[test]
fn test_attempted_empty_private_payment_list_blocks_until_confirmed() {
    let counterparty = counterparty();
    let app_id = app_id();
    let empty_list = r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#;
    let mut messages = vec![outbound_private_list(
        0,
        counterparty.clone(),
        app_id.clone(),
        OutboundPrivateMessageStatus::Failed,
        Some(timestamp()),
        empty_list.into(),
    )];

    assert_eq!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id).unwrap(),
        HashSet::from([counterparty.clone()])
    );

    messages.push(outbound_private_list(
        1,
        counterparty,
        app_id.clone(),
        OutboundPrivateMessageStatus::Sent,
        Some(timestamp()),
        empty_list.into(),
    ));
    assert!(
        counterparties_with_shared_private_payment_lists(&messages, &app_id)
            .unwrap()
            .is_empty()
    );
}
