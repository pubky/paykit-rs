use chrono::{Duration as ChronoDuration, TimeZone, Utc};

use super::*;
use crate::{
    domain::adapters::ReceivingDetail,
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

fn receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

fn stream_item(
    stream_item_id: u64,
    raw_json: &str,
    status: PrivateStreamParseStatus,
) -> PrivateStreamItemRecord {
    PrivateStreamItemRecord {
        stream_item_id,
        counterparty: counterparty(),
        counterparty_receiver_path: receiver_path(),
        receive_batch_id: 0,
        raw_json: raw_json.into(),
        parsed_version: Some(1),
        parsed_kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        known_paykit_kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        parse_status: status,
        parse_error: None,
        received_at: timestamp(),
    }
}

fn list_json(payload: &str) -> String {
    format!(
        r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"btc-lightning-bolt11":"{payload}"}}}}"#
    )
}

fn private_message(raw_json: String) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: Some(1),
        kind: Some(PrivateMessageKind::PrivatePaymentList.as_str().into()),
        raw_json,
    }
}

#[test]
fn test_derive_private_payment_list_view_uses_latest_valid_list() {
    let first = list_json("ln-old");
    let latest = list_json("ln-new");

    let view = derive_private_payment_list_view(vec![
        stream_item(1, &latest, PrivateStreamParseStatus::Valid),
        stream_item(0, &first, PrivateStreamParseStatus::Valid),
    ])
    .unwrap()
    .unwrap();

    assert_eq!(view.latest_stream_item_id, Some(1));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-new");
    assert_eq!(view.last_refresh_at, Some(timestamp()));
}

#[test]
fn test_private_payment_list_view_debug_redacts_payloads() {
    let mut payment_endpoints = HashMap::new();
    payment_endpoints.insert("btc-lightning-bolt11".into(), "ln-private-secret".into());
    let view = PrivatePaymentListView {
        latest_stream_item_id: Some(42),
        payment_endpoints,
        last_refresh_at: Some(timestamp()),
    };

    let debug = format!("{view:?}");

    assert!(debug.contains("btc-lightning-bolt11"));
    assert!(!debug.contains("ln-private-secret"));
}

#[test]
fn test_derive_private_payment_list_view_ignores_malformed_newer_list() {
    let valid = list_json("ln-valid");
    let malformed = r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"../bad":"ln-bad"}}"#;

    let view = derive_private_payment_list_view(vec![
        stream_item(0, &valid, PrivateStreamParseStatus::Valid),
        stream_item(1, malformed, PrivateStreamParseStatus::MalformedRecognized),
    ])
    .unwrap()
    .unwrap();

    assert_eq!(view.latest_stream_item_id, Some(0));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-valid");
}

#[tokio::test]
async fn test_current_private_payment_list_loads_from_storage() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let json = list_json("ln-storage");

    persist_private_stream_batch(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![private_message(json)],
        None,
        timestamp(),
    )
    .await
    .unwrap();

    let view = current_private_payment_list(&storage, &counterparty, &receiver_path())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(view.latest_stream_item_id, Some(0));
    assert_eq!(view.payment_endpoints["btc-lightning-bolt11"], "ln-storage");
}

#[tokio::test]
async fn test_enqueue_private_payment_list_stores_exact_list_message() {
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();

    let record = enqueue_private_payment_list(
        &storage,
        counterparty.clone(),
        receiver_path(),
        vec![ReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private".into(),
        }],
        timestamp(),
    )
    .await
    .unwrap();

    let queued = queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
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
    let storage = InMemoryStorage::new();
    let counterparty = counterparty();
    let stale_lease = storage
        .transaction({
            let counterparty = counterparty.clone();
            move |tx| {
                Ok(tx
                    .claim_peer_link_operation(
                        &counterparty,
                        &receiver_path(),
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
                    &receiver_path(),
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
        vec![ReceivingDetail {
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private".into(),
        }],
        timestamp(),
        &stale_lease,
    )
    .await;

    assert!(matches!(result, Err(PaykitSdkError::Policy { .. })));
    assert!(
        queued_outbound_private_messages(&storage, &counterparty, &receiver_path())
            .await
            .unwrap()
            .is_empty()
    );
}
