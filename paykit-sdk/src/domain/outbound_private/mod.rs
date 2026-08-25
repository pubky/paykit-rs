//! Durable outbound Private Application Message queue.

use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::{
    inspect_private_application_message, PrivateMessageInspection, PrivateMessageKind,
    PrivateMessageStructure,
};
use serde::{Deserialize, Serialize};

use crate::{
    storage::{
        is_parked_unknown_kind_outbound_message, require_peer_link_operation_lease,
        NewOutboundPrivateMessage, OutboundPrivateMessageRecord, PeerLinkOperationLease,
        StorageAdapter,
    },
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
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

/// Reason an outbound private message is parked instead of processed.
///
/// SECURITY / REDACTION: this closed vocabulary is the entire parked-message
/// signal. It never carries payload data or the unrecognized kind text, so it
/// is stable and safe to surface across the FFI boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum OutboundPrivateParkReason {
    /// The queued payload carries a Private Message Kind this build does not
    /// recognize. A newer build wrote it; only a build that understands the
    /// kind may process it.
    UnsupportedKind,
}

/// One outbound private message left parked at the head of a peer's queue.
///
/// SECURITY / REDACTION: contains only the local outbound message id plus a
/// closed-vocabulary [`OutboundPrivateParkReason`] -- never payload bytes and
/// never the unrecognized kind string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateParkedMessage {
    /// Outbound message id of the parked queue head.
    pub outbound_message_id: u64,
    /// Why the message is parked.
    pub reason: OutboundPrivateParkReason,
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
    /// Queue heads left parked because this build does not recognize their
    /// Private Message Kind. Parked records are never claimed, mutated, or
    /// invalidated; entries carry only ids and a closed-vocabulary reason.
    #[serde(default)]
    pub parked_unsupported: Vec<OutboundPrivateParkedMessage>,
}

/// Summary for processing outbound private messages for one counterparty.
///
/// SDK-produced values contain either `report` or `error`, never both.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateCounterpartySendReport {
    /// Counterparty whose queue was processed.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
    counterparty_receiver_path: PaykitReceiverPath,
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
                counterparty_receiver_path,
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
                lease.counterparty_receiver_path.clone(),
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
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<Vec<OutboundPrivateMessageRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx.queued_outbound_private_messages(counterparty, counterparty_receiver_path))
        })
        .await
}

#[cfg(test)]
pub(crate) async fn claim_next_outbound_private_message<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
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
                counterparty_receiver_path,
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
                &lease.counterparty_receiver_path,
                now,
                stale_before,
                failed_retry_after,
            ))
        })
        .await
}

/// Report the parked unknown-kind queue head for one counterparty, if any.
///
/// One lease-checked read of the queued messages; the parked record itself is
/// never claimed, mutated, or invalidated (its `attempt_count` and timestamps
/// must not move). Returns `None` when the queue is empty or its head is not
/// parked -- for example a head that is merely backing off between retries.
/// The flush loop calls this after the claim path returns no work, so the
/// parked-head signal re-surfaces on every flush until a newer build
/// processes the message.
pub(crate) async fn parked_unsupported_queue_head<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    lease: &PeerLinkOperationLease,
) -> Result<Option<OutboundPrivateParkedMessage>>
where
    S: StorageAdapter,
{
    let head = storage
        .transaction({
            let counterparty = counterparty.clone();
            let lease = lease.clone();
            move |tx| {
                require_peer_link_operation_lease(tx, &lease)?;
                Ok(tx
                    .queued_outbound_private_messages(
                        &counterparty,
                        &lease.counterparty_receiver_path,
                    )
                    .into_iter()
                    .next())
            }
        })
        .await?;
    Ok(head
        .filter(is_parked_unknown_kind_outbound_message)
        .map(|record| OutboundPrivateParkedMessage {
            outbound_message_id: record.outbound_message_id,
            reason: OutboundPrivateParkReason::UnsupportedKind,
        }))
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
        // SECURITY / REDACTION: no kind echo. This context becomes the
        // persisted outbound `last_error` on the flush path and crosses the
        // FFI boundary, so it must stay a stable static string.
        return Err(PaykitSdkError::Protocol {
            context: "queued private message kind does not match payload kind".into(),
            source: None,
        });
    }
    Ok(())
}

/// Validate one outbound payload and return the kind string enqueue stamps
/// into the record's `kind` column.
///
/// Stamping invariant: the returned kind is read from the payload document
/// itself and validation fails unless it names a recognized body kind, so a
/// stored `kind` column always mirrors the payload body kind. The body stays
/// authoritative everywhere a record is re-judged later.
pub(crate) fn validate_outbound_private_message(raw_json: &str) -> Result<String> {
    if raw_json.len() > paykit_lib::pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitSdkError::Protocol {
            context: "Private Application Message exceeds pubky-noise message size".into(),
            source: None,
        });
    }

    // One shared inspection call drives kind recognition and body validity
    // below; the raw header probe stays because outbound header policy
    // distinguishes decisions inspection deliberately conflates (invalid JSON
    // versus a well-formed document missing a header field, and a header
    // version outside the u8 range still reports "unsupported"), and those
    // error strings are a frozen contract.
    let inspection = inspect_private_application_message(raw_json);

    // SECURITY / REDACTION: the contexts below are persisted as outbound
    // `last_error` values and cross the FFI boundary. They must be stable
    // static strings with no serde detail and no payload/kind/version echo
    // (a malformed payload can place attacker-chosen bytes in any of those).
    let value: serde_json::Value =
        serde_json::from_str(raw_json).map_err(|_| PaykitSdkError::Protocol {
            context: "invalid private message JSON".into(),
            source: None,
        })?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "private message version is missing".into(),
            source: None,
        })?;
    if version != 1 {
        return Err(PaykitSdkError::Protocol {
            context: "unsupported private message version".into(),
            source: None,
        });
    }
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "private message kind is missing".into(),
            source: None,
        })?;
    // The probe and inspection read the same top-level `kind` field of the
    // same document, so recognition cannot diverge between them.
    let parsed_kind = inspection
        .known_kind
        .ok_or_else(|| PaykitSdkError::Protocol {
            context: "unsupported private message kind".into(),
            source: None,
        })?;
    validate_outbound_private_message_body(parsed_kind, &inspection, raw_json)?;

    Ok(kind.to_owned())
}

fn validate_outbound_private_message_body(
    kind: PrivateMessageKind,
    inspection: &PrivateMessageInspection,
    raw_json: &str,
) -> Result<()> {
    // Deliberately exhaustive per-kind match at this security boundary: a new
    // Private Message Kind fails to compile until it is routed explicitly.
    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            // The typed parse is kept so the propagated error context
            // ("failed to parse Private Payment List JSON") stays
            // byte-identical; inspection carries only the parse category.
            paykit_lib::parse_private_payment_list_json(raw_json)?;
        }
        PrivateMessageKind::ReceiptAccess => {
            validate_recognized_outbound_body(
                inspection,
                "Receipt Access payload does not match private message kind",
            )?;
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            validate_recognized_outbound_body(
                inspection,
                "Payment Request event payload does not match private message kind",
            )?;
        }
    }

    Ok(())
}

/// Map an inspected recognized-kind body onto the outbound validation result.
///
/// `mismatch_context` preserves the pre-inspection error text for the
/// parser-returned-nothing case, which is unreachable in practice because the
/// typed parsers accept every message whose body kind matches their own.
fn validate_recognized_outbound_body(
    inspection: &PrivateMessageInspection,
    mismatch_context: &'static str,
) -> Result<()> {
    match inspection.structure {
        PrivateMessageStructure::Valid => Ok(()),
        PrivateMessageStructure::MalformedRecognized => Err(PaykitSdkError::Protocol {
            context: match inspection.error_category {
                // The category string is exactly the wrapper's
                // `validation_error()` text, so the persisted `last_error`
                // stays byte-identical to the pre-inspection behavior.
                Some(category) => category.as_str().to_owned(),
                None => mismatch_context.to_owned(),
            },
            source: None,
        }),
        // Unreachable: a recognized kind always inspects as Valid or
        // MalformedRecognized. Kept wildcard-free without panicking on
        // caller-supplied payload bytes.
        PrivateMessageStructure::UnknownKind | PrivateMessageStructure::InvalidJson => {
            Err(PaykitSdkError::Protocol {
                context: mismatch_context.to_owned(),
                source: None,
            })
        }
    }
}

#[cfg(test)]
mod tests;
