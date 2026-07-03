//! Durable outbound Private Application Message queue.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::PrivateMessageKind;
use serde::{Deserialize, Serialize};

use crate::{
    storage::{
        require_peer_link_operation_lease, NewOutboundPrivateMessage, OutboundPrivateMessageRecord,
        PeerLinkOperationLease, StorageAdapter,
    },
    PaykitReceiverId, PaykitSdkError, PubkyPublicKey, Result,
};

/// Delivery status for one outbound Private Application Message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OutboundPrivateMessageStatus {
    /// Message is queued and has not been sent.
    Pending,
    /// A worker is sending this message.
    Sending,
    /// Message was sent successfully.
    Sent,
    /// Last send attempt failed.
    Failed,
    /// The stored payload is invalid and must not be retried automatically.
    Invalid,
    /// Automatic retry is blocked until local Encrypted Link state is recovered.
    RecoveryRequired,
    /// Newer latest-state data made this message unnecessary to send.
    Superseded,
}

/// Failed outbound private send attempt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateSendFailure {
    /// Outbound message id.
    pub outbound_message_id: u64,
    /// Error from the send attempt.
    pub error: String,
}

impl fmt::Debug for OutboundPrivateSendFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error = redacted_error(&self.error);
        f.debug_struct("OutboundPrivateSendFailure")
            .field("outbound_message_id", &self.outbound_message_id)
            .field("error", &error)
            .finish()
    }
}

/// Failed cleanup of a superseded Payment Endpoint Reservation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationCleanupFailure {
    /// Reservation id, when the failure is tied to a specific reservation.
    pub reservation_id: Option<String>,
    /// Cleanup error.
    pub error: String,
}

impl fmt::Debug for ReservationCleanupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error = redacted_error(&self.error);
        f.debug_struct("ReservationCleanupFailure")
            .field("reservation_id", &self.reservation_id)
            .field("error", &error)
            .finish()
    }
}

/// Failed recovery marker publication during outbound private send recovery.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryMarkerPublishFailure {
    /// Outbound message id that triggered recovery, when available.
    pub outbound_message_id: Option<u64>,
    /// Recovery marker publication error.
    pub error: String,
}

impl fmt::Debug for RecoveryMarkerPublishFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error = redacted_error(&self.error);
        f.debug_struct("RecoveryMarkerPublishFailure")
            .field("outbound_message_id", &self.outbound_message_id)
            .field("error", &error)
            .finish()
    }
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
    /// Superseded reservation cleanup failures observed in this run.
    pub reservation_cleanup_failures: Vec<ReservationCleanupFailure>,
    /// Recovery marker publication failures observed after fail-closed recovery.
    #[serde(default)]
    pub recovery_marker_failures: Vec<RecoveryMarkerPublishFailure>,
}

/// Summary for processing outbound private messages for one counterparty.
///
/// SDK-produced values contain either `report` or `error`, never both.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateCounterpartySendReport {
    /// Counterparty whose queue was processed.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_id: PaykitReceiverId,
    /// Successful send report, when processing completed.
    pub report: Option<OutboundPrivateSendReport>,
    /// Error text, when processing failed for this counterparty.
    pub error: Option<String>,
}

impl fmt::Debug for OutboundPrivateCounterpartySendReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let error = self.error.as_deref().map(redacted_error);
        f.debug_struct("OutboundPrivateCounterpartySendReport")
            .field("counterparty", &self.counterparty)
            .field("report", &self.report)
            .field("error", &error)
            .finish()
    }
}

fn redacted_error(error: &str) -> String {
    format!("<redacted:{} bytes>", error.len())
}

/// Enqueue one raw JSON Private Application Message.
pub(crate) async fn enqueue_private_message<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_id: PaykitReceiverId,
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
                counterparty_receiver_id,
                kind,
                raw_json,
                now,
            ));
            Ok(record)
        })
        .await
}

/// Enqueue one raw JSON Private Application Message while a peer operation
/// lease is still active.
pub(crate) async fn enqueue_private_message_with_link_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    raw_json: String,
    now: DateTime<Utc>,
    lease: &PeerLinkOperationLease,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let kind = validate_outbound_private_message(&raw_json)?;
    storage
        .transaction(move |tx| {
            require_peer_link_operation_lease(tx, lease)?;
            let record = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty,
                lease.counterparty_receiver_id.clone(),
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
    counterparty_receiver_id: &PaykitReceiverId,
) -> Result<Vec<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx.queued_outbound_private_messages(counterparty, counterparty_receiver_id))
        })
        .await
}

#[cfg(test)]
pub(crate) async fn claim_next_outbound_private_message<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_id: &PaykitReceiverId,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) -> Result<Option<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx.claim_next_outbound_private_message(
                counterparty,
                counterparty_receiver_id,
                now,
                stale_before,
                failed_retry_after,
            ))
        })
        .await
}

pub(crate) async fn claim_next_outbound_private_message_with_peer_lease<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
    lease: PeerLinkOperationLease,
) -> Result<Option<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            require_peer_link_operation_lease(tx, &lease)?;
            Ok(tx.claim_next_outbound_private_message(
                counterparty,
                &lease.counterparty_receiver_id,
                now,
                stale_before,
                failed_retry_after,
            ))
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

pub(crate) fn mark_outbound_recovery_required(
    mut record: OutboundPrivateMessageRecord,
    error: String,
    now: DateTime<Utc>,
) -> OutboundPrivateMessageRecord {
    record.status = OutboundPrivateMessageStatus::RecoveryRequired;
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
mod tests;
