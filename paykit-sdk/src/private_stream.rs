//! Durable private stream records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    storage::{EncryptedLinkStateRecord, EventDedupRecord, NewPrivateStreamItem, StorageAdapter},
    PubkyPublicKey, Result,
};

use paykit_lib::{
    parse_payment_request_event_message, parse_private_payment_list_json,
    parse_receipt_access_event_message, PrivateApplicationMessage, PrivateMessageKind,
};

/// Parse status for one received Private Application Message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivateStreamParseStatus {
    /// Message parsed as a valid recognized Paykit message.
    Valid,
    /// Message kind is recognized, but payload is malformed.
    MalformedRecognized,
    /// Message has a valid private header but unknown kind.
    UnknownKind,
    /// Message is not valid JSON or does not have a usable private header.
    InvalidJson,
}

/// Summary of a persisted private stream batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateStreamIntakeReport {
    /// Receive batch id assigned by storage.
    pub receive_batch_id: u64,
    /// Stored stream item ids in input order.
    pub stream_item_ids: Vec<u64>,
    /// Event ID conflicts found while updating dedupe records.
    pub event_conflicts: Vec<EventIdConflict>,
}

/// Reused Event ID with a different payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventIdConflict {
    /// Conflicting Event ID.
    pub event_id: String,
    /// First stream item that used this Event ID.
    pub first_stream_item_id: u64,
    /// Stream item that reused this Event ID with a different payload.
    pub conflicting_stream_item_id: u64,
}

/// Persist an ordered batch of Private Application Messages and a link checkpoint.
pub async fn persist_private_stream_batch<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    messages: Vec<PrivateApplicationMessage>,
    link_state: Option<EncryptedLinkStateRecord>,
    received_at: DateTime<Utc>,
) -> Result<PrivateStreamIntakeReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let receive_batch_id = tx.allocate_receive_batch_id();
            let mut report = PrivateStreamIntakeReport {
                receive_batch_id,
                stream_item_ids: Vec::with_capacity(messages.len()),
                event_conflicts: Vec::new(),
            };

            for message in messages {
                let classification = classify_message(&message);
                let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem {
                    counterparty: counterparty.clone(),
                    receive_batch_id,
                    raw_json: message.raw_json.clone(),
                    parsed_version: message.version.map(u32::from),
                    parsed_kind: message.kind.clone(),
                    known_paykit_kind: message.known_kind().map(|kind| kind.as_str().to_owned()),
                    parse_status: classification.status,
                    parse_error: classification.parse_error,
                    received_at,
                });

                if let Some(event) = classification.event {
                    update_event_dedupe(
                        tx,
                        event.event_id,
                        event.event_kind,
                        payload_hash(&message.raw_json),
                        stream_item_id,
                        &mut report,
                    );
                }

                report.stream_item_ids.push(stream_item_id);
            }

            if let Some(link_state) = link_state {
                tx.save_encrypted_link_state(link_state);
            }

            Ok(report)
        })
        .await
}

struct MessageClassification {
    status: PrivateStreamParseStatus,
    parse_error: Option<String>,
    event: Option<EventHeader>,
}

struct EventHeader {
    event_id: String,
    event_kind: String,
}

fn classify_message(message: &PrivateApplicationMessage) -> MessageClassification {
    let Some(kind) = message.known_kind() else {
        return MessageClassification {
            status: if message.version.is_some() && message.kind.is_some() {
                PrivateStreamParseStatus::UnknownKind
            } else {
                PrivateStreamParseStatus::InvalidJson
            },
            parse_error: None,
            event: None,
        };
    };

    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            match parse_private_payment_list_json(&message.raw_json) {
                Ok(_) => MessageClassification {
                    status: PrivateStreamParseStatus::Valid,
                    parse_error: None,
                    event: None,
                },
                Err(err) => MessageClassification {
                    status: PrivateStreamParseStatus::MalformedRecognized,
                    parse_error: Some(err.to_string()),
                    event: None,
                },
            }
        }
        PrivateMessageKind::ReceiptAccess => {
            let parsed = parse_receipt_access_event_message(message);
            let event = parsed
                .as_ref()
                .and_then(|parsed| parsed.event_id())
                .map(|event_id| EventHeader {
                    event_id: event_id.as_str().to_owned(),
                    event_kind: kind.as_str().to_owned(),
                });
            MessageClassification {
                status: status_from_event_validity(
                    parsed.as_ref().is_some_and(|parsed| parsed.is_valid()),
                ),
                parse_error: parsed
                    .as_ref()
                    .and_then(|parsed| parsed.validation_error())
                    .map(str::to_owned),
                event,
            }
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            let parsed = parse_payment_request_event_message(message);
            let event = parsed
                .as_ref()
                .and_then(|parsed| parsed.event_id())
                .map(|event_id| EventHeader {
                    event_id: event_id.as_str().to_owned(),
                    event_kind: kind.as_str().to_owned(),
                });
            MessageClassification {
                status: status_from_event_validity(
                    parsed.as_ref().is_some_and(|parsed| parsed.is_valid()),
                ),
                parse_error: parsed
                    .as_ref()
                    .and_then(|parsed| parsed.validation_error())
                    .map(str::to_owned),
                event,
            }
        }
    }
}

fn status_from_event_validity(is_valid: bool) -> PrivateStreamParseStatus {
    if is_valid {
        PrivateStreamParseStatus::Valid
    } else {
        PrivateStreamParseStatus::MalformedRecognized
    }
}

fn update_event_dedupe(
    tx: &mut dyn crate::storage::StorageTransaction,
    event_id: String,
    event_kind: String,
    payload_hash: String,
    stream_item_id: u64,
    report: &mut PrivateStreamIntakeReport,
) {
    let Some(mut record) = tx.event_dedup_record(&event_id) else {
        tx.save_event_dedup_record(EventDedupRecord {
            event_id,
            event_kind,
            payload_hash,
            first_stream_item_id: stream_item_id,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        });
        return;
    };

    if record.payload_hash == payload_hash {
        record.duplicate_stream_item_ids.push(stream_item_id);
    } else {
        record.conflicting_stream_item_ids.push(stream_item_id);
        report.event_conflicts.push(EventIdConflict {
            event_id: record.event_id.clone(),
            first_stream_item_id: record.first_stream_item_id,
            conflicting_stream_item_id: stream_item_id,
        });
    }

    tx.save_event_dedup_record(record);
}

fn payload_hash(raw_json: &str) -> String {
    let digest = Sha256::digest(raw_json.as_bytes());
    format!("sha256:{digest:x}")
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{storage::InMemoryStorage, EncryptedLinkStateRecord, PrivateStreamParseStatus};

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::new("pk-peer").unwrap()
    }

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn private_message(raw_json: &str) -> PrivateApplicationMessage {
        let value: serde_json::Value = serde_json::from_str(raw_json).unwrap();
        PrivateApplicationMessage {
            version: value
                .get("version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u8::try_from(version).ok()),
            kind: value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            raw_json: raw_json.to_owned(),
        }
    }

    fn payment_request_raw(reference: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}"#
        )
    }

    #[tokio::test]
    async fn test_persist_private_stream_batch_stores_messages_and_checkpoint() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let link_state = EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: Some(vec![1, 2, 3]),
            handshake_snapshot: None,
            generation: 1,
            checkpointed_at: timestamp(),
        };
        let messages = vec![
            private_message(r#"{"version":1,"kind":"paykit.unknown","body":{}}"#),
            private_message(&payment_request_raw("invoice-2026-0001")),
        ];

        let report = persist_private_stream_batch(
            &storage,
            counterparty.clone(),
            messages,
            Some(link_state.clone()),
            timestamp(),
        )
        .await
        .unwrap();

        let snapshot = storage.snapshot().unwrap();
        assert_eq!(report.receive_batch_id, 0);
        assert_eq!(report.stream_item_ids, vec![0, 1]);
        assert_eq!(snapshot.private_stream_items.len(), 2);
        assert_eq!(
            snapshot.private_stream_items[0].parse_status,
            PrivateStreamParseStatus::UnknownKind
        );
        assert_eq!(
            snapshot.private_stream_items[1].parse_status,
            PrivateStreamParseStatus::Valid
        );
        assert_eq!(snapshot.encrypted_link_states[&counterparty], link_state);
        assert_eq!(snapshot.event_dedup_records.len(), 1);
    }

    #[tokio::test]
    async fn test_persist_private_stream_batch_marks_event_id_conflicts() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let messages = vec![
            private_message(&payment_request_raw("invoice-2026-0001")),
            private_message(&payment_request_raw("invoice-2026-0002")),
        ];

        let report =
            persist_private_stream_batch(&storage, counterparty, messages, None, timestamp())
                .await
                .unwrap();

        let snapshot = storage.snapshot().unwrap();
        let record = snapshot
            .event_dedup_records
            .get("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
            .unwrap();
        assert_eq!(report.event_conflicts.len(), 1);
        assert_eq!(record.first_stream_item_id, 0);
        assert_eq!(record.conflicting_stream_item_ids, vec![1]);
    }

    #[tokio::test]
    async fn test_persist_private_stream_batch_keeps_malformed_recognized_messages() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let malformed = r#"{"version":1,"kind":"paykit.payment_request","event_id":"8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#;

        persist_private_stream_batch(
            &storage,
            counterparty,
            vec![private_message(malformed)],
            None,
            timestamp(),
        )
        .await
        .unwrap();

        let snapshot = storage.snapshot().unwrap();
        let item = &snapshot.private_stream_items[0];
        assert_eq!(
            item.parse_status,
            PrivateStreamParseStatus::MalformedRecognized
        );
        assert!(item
            .parse_error
            .as_ref()
            .is_some_and(|error| error.contains("amount.value")));
        assert_eq!(snapshot.event_dedup_records.len(), 1);
    }
}
