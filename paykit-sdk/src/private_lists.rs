//! Private Payment List latest-state records.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_private_payment_list_json, serialize_private_payment_list_json, PrivateMessageKind,
    PrivatePaymentList,
};
use serde::{Deserialize, Serialize};

use crate::{
    adapters::ReceivingDetail,
    endpoints::normalize_receiving_details,
    outbound_private::enqueue_private_message,
    private_stream::PrivateStreamParseStatus,
    storage::{OutboundPrivateMessageRecord, PrivateStreamItemRecord, StorageAdapter},
    PubkyPublicKey, Result,
};

/// Derived latest-state view of a counterparty's Private Payment List.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListView {
    /// Stream item id of the latest valid list.
    pub latest_stream_item_id: Option<u64>,
    /// Current endpoint payloads keyed by identifier string.
    pub payment_endpoints: HashMap<String, String>,
    /// Receive time of the latest valid list.
    pub last_refresh_at: Option<DateTime<Utc>>,
}

/// Load the current Private Payment List view for one counterparty.
pub async fn current_private_payment_list<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Option<PrivatePaymentListView>>
where
    S: StorageAdapter,
{
    let items = storage
        .transaction(|tx| Ok(tx.private_stream_items(counterparty)))
        .await?;
    derive_private_payment_list_view(items)
}

/// Queue a complete Private Payment List for delivery to one counterparty.
pub async fn enqueue_private_payment_list<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    receiving_details: Vec<ReceivingDetail>,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let payment_endpoints = normalize_receiving_details(receiving_details)?;
    let list = PrivatePaymentList::new(payment_endpoints);
    let raw_json = serialize_private_payment_list_json(&list)?;
    enqueue_private_message(storage, counterparty, raw_json, now).await
}

/// Derive the latest valid Private Payment List view from private stream items.
pub fn derive_private_payment_list_view(
    mut items: Vec<PrivateStreamItemRecord>,
) -> Result<Option<PrivatePaymentListView>> {
    items.sort_by_key(|item| item.stream_item_id);

    let latest = items.into_iter().rev().find(|item| {
        item.parse_status == PrivateStreamParseStatus::Valid
            && item.known_paykit_kind.as_deref()
                == Some(PrivateMessageKind::PrivatePaymentList.as_str())
    });

    let Some(item) = latest else {
        return Ok(None);
    };

    let list = parse_private_payment_list_json(&item.raw_json)?;
    let payment_endpoints = list
        .payment_endpoints
        .into_iter()
        .map(|(identifier, payload)| (identifier.as_str().to_owned(), payload.as_str().to_owned()))
        .collect();

    Ok(Some(PrivatePaymentListView {
        latest_stream_item_id: Some(item.stream_item_id),
        payment_endpoints,
        last_refresh_at: Some(item.received_at),
    }))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        adapters::ReceivingDetail,
        outbound_private::queued_outbound_private_messages,
        private_stream::persist_private_stream_batch,
        storage::{InMemoryStorage, PrivateStreamItemRecord},
    };
    use paykit_lib::PrivateApplicationMessage;

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
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
            vec![private_message(json)],
            None,
            timestamp(),
        )
        .await
        .unwrap();

        let view = current_private_payment_list(&storage, &counterparty)
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
            vec![ReceivingDetail {
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
}
