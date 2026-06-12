//! Durable outbound Private Application Message queue.

use chrono::{DateTime, Utc};
use paykit_lib::PrivateMessageKind;
use serde::{Deserialize, Serialize};

use crate::{
    storage::{
        require_peer_link_operation_lease, NewOutboundPrivateMessage, OutboundPrivateMessageRecord,
        PeerLinkOperationLease, StorageAdapter,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

/// Delivery status for one outbound Private Application Message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboundPrivateMessageStatus {
    /// Message is queued and has not been sent.
    Pending,
    /// A worker is currently trying to send this message.
    Sending,
    /// Message was sent successfully.
    Sent,
    /// Last send attempt failed.
    Failed,
    /// The stored payload is invalid and must not be retried automatically.
    Invalid,
    /// Newer latest-state data made this message unnecessary to send.
    Superseded,
}

/// Failed outbound private send attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateSendFailure {
    /// Outbound message id.
    pub outbound_message_id: u64,
    /// Error from the send attempt.
    pub error: String,
}

/// Summary returned after processing outbound private messages.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateSendReport {
    /// Messages attempted in this run.
    pub attempted: Vec<u64>,
    /// Messages marked sent in this run.
    pub sent: Vec<u64>,
    /// Messages that failed in this run.
    pub failed: Vec<OutboundPrivateSendFailure>,
}

/// Enqueue one raw JSON Private Application Message.
pub(crate) async fn enqueue_private_message<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    raw_json: String,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let kind = validate_outbound_private_message(&raw_json)?;
    storage
        .transaction(move |tx| {
            let record = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                kind,
                raw_json,
                now,
            ));
            Ok(record)
        })
        .await
}

/// Load private messages that should be attempted for one counterparty.
pub(crate) async fn queued_outbound_private_messages<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Vec<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.queued_outbound_private_messages(counterparty)))
        .await
}

#[cfg(test)]
pub(crate) async fn claim_next_outbound_private_message<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
) -> Result<Option<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx.claim_next_outbound_private_message(counterparty, now, stale_before))
        })
        .await
}

pub(crate) async fn claim_next_outbound_private_message_with_peer_lease<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
    lease: PeerLinkOperationLease,
) -> Result<Option<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            require_peer_link_operation_lease(tx, &lease)?;
            Ok(tx.claim_next_outbound_private_message(counterparty, now, stale_before))
        })
        .await
}

pub(crate) fn mark_outbound_sent(
    mut record: OutboundPrivateMessageRecord,
    now: DateTime<Utc>,
) -> OutboundPrivateMessageRecord {
    record.status = OutboundPrivateMessageStatus::Sent;
    record.sent_at = Some(now);
    record.updated_at = now;
    record.last_error = None;
    record
}

pub(crate) fn mark_outbound_failed(
    mut record: OutboundPrivateMessageRecord,
    error: String,
    now: DateTime<Utc>,
) -> OutboundPrivateMessageRecord {
    record.status = OutboundPrivateMessageStatus::Failed;
    record.updated_at = now;
    record.last_error = Some(error);
    record
}

pub(crate) fn mark_outbound_invalid(
    mut record: OutboundPrivateMessageRecord,
    error: String,
    now: DateTime<Utc>,
) -> OutboundPrivateMessageRecord {
    record.status = OutboundPrivateMessageStatus::Invalid;
    record.updated_at = now;
    record.last_error = Some(error);
    record
}

pub(crate) fn validate_queued_outbound_private_message(
    record: &OutboundPrivateMessageRecord,
) -> Result<()> {
    let kind = validate_outbound_private_message(&record.raw_json)?;
    if kind != record.kind {
        return Err(PaykitSdkError::Protocol(format!(
            "queued private message kind '{}' does not match payload kind '{kind}'",
            record.kind
        )));
    }
    Ok(())
}

pub(crate) fn validate_outbound_private_message(raw_json: &str) -> Result<String> {
    if raw_json.len() > paykit_lib::pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitSdkError::Protocol(
            "Private Application Message exceeds pubky-noise message size".into(),
        ));
    }

    let value: serde_json::Value = serde_json::from_str(raw_json)
        .map_err(|err| PaykitSdkError::Protocol(format!("invalid private message JSON: {err}")))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| PaykitSdkError::Protocol("private message version is missing".into()))?;
    if version != 1 {
        return Err(PaykitSdkError::Protocol(format!(
            "unsupported private message version {version}"
        )));
    }
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PaykitSdkError::Protocol("private message kind is missing".into()))?;
    let parsed_kind = PrivateMessageKind::parse(kind).ok_or_else(|| {
        PaykitSdkError::Protocol(format!("unsupported private message kind '{kind}'"))
    })?;
    validate_outbound_private_message_body(parsed_kind, raw_json)?;

    Ok(kind.to_owned())
}

fn validate_outbound_private_message_body(kind: PrivateMessageKind, raw_json: &str) -> Result<()> {
    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            paykit_lib::parse_private_payment_list_json(raw_json)?;
        }
        PrivateMessageKind::ReceiptAccess => {
            let message = private_application_message(kind, raw_json);
            let event =
                paykit_lib::parse_receipt_access_event_message(&message).ok_or_else(|| {
                    PaykitSdkError::Protocol(
                        "Receipt Access payload does not match private message kind".into(),
                    )
                })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol(error.to_owned()));
            }
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            let message = private_application_message(kind, raw_json);
            let event =
                paykit_lib::parse_payment_request_event_message(&message).ok_or_else(|| {
                    PaykitSdkError::Protocol(
                        "Payment Request event payload does not match private message kind".into(),
                    )
                })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol(error.to_owned()));
            }
        }
    }

    Ok(())
}

fn private_application_message(
    kind: PrivateMessageKind,
    raw_json: &str,
) -> paykit_lib::PrivateApplicationMessage {
    paykit_lib::PrivateApplicationMessage {
        version: Some(1),
        kind: Some(kind.as_str().to_owned()),
        raw_json: raw_json.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::storage::InMemoryStorage;

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn raw_private_list() -> String {
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into()
    }

    #[tokio::test]
    async fn test_enqueue_private_message_stores_pending_record() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let record = enqueue_private_message(
            &storage,
            counterparty.clone(),
            raw_private_list(),
            timestamp(),
        )
        .await
        .unwrap();

        let queued = queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert_eq!(record.outbound_message_id, 0);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
    }

    #[tokio::test]
    async fn test_enqueue_private_message_rejects_unknown_kind() {
        let result = enqueue_private_message(
            &InMemoryStorage::new(),
            counterparty(),
            r#"{"version":1,"kind":"paykit.unknown"}"#.into(),
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_enqueue_private_message_rejects_malformed_known_body() {
        let result = enqueue_private_message(
            &InMemoryStorage::new(),
            counterparty(),
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"../bad":"ln"}}"#
                .into(),
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[test]
    fn test_validate_queued_outbound_private_message_rejects_malformed_known_body() {
        let record = OutboundPrivateMessageRecord {
            outbound_message_id: 7,
            counterparty: counterparty(),
            kind: "paykit.private_payment_list".into(),
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"../bad":"ln"}}"#
                    .into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        };

        let result = validate_queued_outbound_private_message(&record);

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_claim_next_outbound_private_message_blocks_until_stale() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        enqueue_private_message(
            &storage,
            counterparty.clone(),
            raw_private_list(),
            timestamp(),
        )
        .await
        .unwrap();

        let claimed = claim_next_outbound_private_message(
            &storage,
            &counterparty,
            timestamp(),
            timestamp() - chrono::Duration::seconds(60),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
        assert_eq!(claimed.attempt_count, 1);

        let duplicate_claim = claim_next_outbound_private_message(
            &storage,
            &counterparty,
            timestamp(),
            timestamp() - chrono::Duration::seconds(60),
        )
        .await
        .unwrap();
        assert!(duplicate_claim.is_none());

        let stale_claim = claim_next_outbound_private_message(
            &storage,
            &counterparty,
            timestamp() + chrono::Duration::seconds(61),
            timestamp(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(stale_claim.outbound_message_id, claimed.outbound_message_id);
        assert_eq!(stale_claim.attempt_count, 2);
    }

    #[tokio::test]
    async fn test_claim_next_outbound_private_message_rejects_stale_peer_lease() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        enqueue_private_message(
            &storage,
            counterparty.clone(),
            raw_private_list(),
            timestamp(),
        )
        .await
        .unwrap();

        let first_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx
                        .claim_peer_link_operation(
                            &counterparty,
                            timestamp(),
                            timestamp() + chrono::Duration::seconds(10),
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
                    tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    );
                    Ok(())
                }
            })
            .await
            .unwrap();

        let result = claim_next_outbound_private_message_with_peer_lease(
            &storage,
            &counterparty,
            timestamp() + chrono::Duration::seconds(12),
            timestamp(),
            first_lease,
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        let queued = queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert_eq!(queued[0].status, OutboundPrivateMessageStatus::Pending);
        assert_eq!(queued[0].attempt_count, 0);
    }
}
