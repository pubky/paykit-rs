//! Durable private stream records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use std::collections::{HashMap, HashSet};

use crate::{
    domain::{
        outbound_private::OutboundPrivateMessageStatus,
        payment_requests::{
            payment_request_records_from_transaction, PaymentRequestLifecycleState,
        },
        receipts::ReceiptAccessRecord,
    },
    storage::{
        require_peer_link_operation_lease, retry_storage_transaction, EncryptedLinkStateRecord,
        EventDedupRecord, NewPrivateStreamItem, NewPrivateStreamItemDetails,
        OutboundPrivateMessageRecord, PeerLinkOperationLease, StorageAdapter,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

use paykit_lib::{
    parse_allowance_event_message, parse_payment_request_event_message,
    parse_private_payment_list_json, parse_receipt_access_event_message, AllowanceEventMessage,
    EventId, PaykitAppId, PaymentRequestEventMessage, PrivateApplicationMessage,
    PrivateMessageKind, ReceiptAccess, ReceiptAccessEventMessage,
};

/// Read surface shared by every typed Event Message parser in `paykit-lib`.
///
/// Intake classification, outbound validation, and backup validation only
/// need these three views, so each new Event Message family adds one `impl`
/// here instead of a new arm at every site.
pub(crate) trait ParsedEventMessage {
    fn app_id(&self) -> Option<&PaykitAppId>;
    fn event_id(&self) -> Option<&EventId>;
    fn is_valid(&self) -> bool;
    fn validation_error(&self) -> Option<&str>;
}

macro_rules! impl_parsed_event_message {
    ($($message:ty),* $(,)?) => {$(
        impl ParsedEventMessage for $message {
            fn app_id(&self) -> Option<&PaykitAppId> {
                <$message>::app_id(self)
            }
            fn event_id(&self) -> Option<&EventId> {
                <$message>::event_id(self)
            }

            fn is_valid(&self) -> bool {
                <$message>::is_valid(self)
            }

            fn validation_error(&self) -> Option<&str> {
                <$message>::validation_error(self)
            }
        }
    )*};
}

impl_parsed_event_message!(
    ReceiptAccessEventMessage,
    PaymentRequestEventMessage,
    AllowanceEventMessage,
);

/// Require a parsed Event Message whose kind matched and that validated.
///
/// `mismatch_context` is only evaluated when the parser rejected the kind.
pub(crate) fn require_valid_event_message<M: ParsedEventMessage>(
    parsed: Option<M>,
    mismatch_context: impl FnOnce() -> String,
) -> Result<M> {
    let message = parsed.ok_or_else(|| PaykitSdkError::Protocol {
        context: mismatch_context(),
        source: None,
    })?;
    if let Some(error) = message.validation_error() {
        return Err(PaykitSdkError::Protocol {
            context: error.to_owned(),
            source: None,
        });
    }
    Ok(message)
}

/// Parse status for one received Private Application Message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Receive batch id assigned by storage, or `None` when no messages arrived.
    pub receive_batch_id: Option<u64>,
    /// Stored stream item ids in input order.
    pub stream_item_ids: Vec<u64>,
    /// Event ID conflicts found while updating dedupe records.
    pub event_conflicts: Vec<EventIdConflict>,
}

/// Summary for receiving private messages from one counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateStreamCounterpartyIntakeReport {
    /// Counterparty whose private stream was received.
    pub counterparty: PubkyPublicKey,
    /// Successful intake report, when receive completed.
    pub report: Option<PrivateStreamIntakeReport>,
    /// Error text, when receive failed for this counterparty.
    pub error: Option<String>,
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

/// Values committed by one atomic private-stream storage transaction.
#[derive(Clone)]
pub(crate) struct PrivateStreamBatchWrite {
    pub(crate) counterparty: PubkyPublicKey,
    pub(crate) messages: Vec<PrivateApplicationMessage>,
    pub(crate) link_state: Option<EncryptedLinkStateRecord>,
    pub(crate) authorized_receipt_apps: Option<Vec<PaykitAppId>>,
    pub(crate) link_lease: Option<PeerLinkOperationLease>,
    pub(crate) receive_batch_id: Option<u64>,
    pub(crate) received_at: DateTime<Utc>,
}

/// Persist an ordered batch of Private Application Messages and a link checkpoint.
#[cfg(test)]
pub(crate) async fn persist_private_stream_batch<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    messages: Vec<PrivateApplicationMessage>,
    link_state: Option<EncryptedLinkStateRecord>,
    received_at: DateTime<Utc>,
) -> Result<PrivateStreamIntakeReport>
where
    S: StorageAdapter,
{
    persist_private_stream_batch_with_link_lease(
        storage,
        counterparty,
        messages,
        link_state,
        None,
        None,
        received_at,
    )
    .await
}

/// Persist a batch and checkpoint only if the peer link lease is still active.
pub(crate) async fn persist_private_stream_batch_with_link_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    messages: Vec<PrivateApplicationMessage>,
    link_state: Option<EncryptedLinkStateRecord>,
    authorized_receipt_apps: Option<Vec<PaykitAppId>>,
    link_lease: Option<PeerLinkOperationLease>,
    received_at: DateTime<Utc>,
) -> Result<PrivateStreamIntakeReport>
where
    S: StorageAdapter,
{
    persist_private_stream_batch_write(
        storage,
        PrivateStreamBatchWrite {
            counterparty,
            messages,
            link_state,
            authorized_receipt_apps,
            link_lease,
            receive_batch_id: None,
            received_at,
        },
    )
    .await
}

/// Persist stream items and their resulting Encrypted Link checkpoint.
pub(crate) async fn persist_private_stream_batch_write<S>(
    storage: &S,
    write: PrivateStreamBatchWrite,
) -> Result<PrivateStreamIntakeReport>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let PrivateStreamBatchWrite {
            counterparty,
            messages,
            link_state,
            authorized_receipt_apps,
            link_lease,
            receive_batch_id,
            received_at,
        } = write.clone();
        move |tx| {
            if let Some(lease) = link_lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let receive_batch_id = if messages.is_empty() {
                None
            } else {
                Some(match receive_batch_id {
                    Some(receive_batch_id) => receive_batch_id,
                    None => tx.allocate_receive_batch_id()?,
                })
            };
            let mut report = PrivateStreamIntakeReport {
                receive_batch_id,
                stream_item_ids: Vec::with_capacity(messages.len()),
                event_conflicts: Vec::new(),
            };
            for message in messages {
                let receive_batch_id = receive_batch_id.expect("nonempty batch has an id");
                let PrivateStreamMessageClassification {
                    status,
                    parse_error,
                    event,
                    receipt_access,
                    app_id: classification_app_id,
                } = classify_private_application_message(&message);
                let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
                        receive_batch_id,
                        raw_json: message.raw_json.clone(),
                        parsed_version: message.version.map(u32::from),
                        parsed_kind: message.kind.clone(),
                        parsed_app_id: message.app_id.clone(),
                        known_paykit_kind: message
                            .known_kind()
                            .map(|kind| kind.as_str().to_owned()),
                        parse_status: status,
                        parse_error,
                        received_at,
                    },
                ))?;

                let dedupe_outcome = event.map(|event| {
                    update_event_dedupe(
                        tx,
                        &counterparty,
                        event.event_id,
                        event.event_kind,
                        payload_hash(&message.raw_json),
                        stream_item_id,
                        &mut report,
                    )
                });

                if matches!(dedupe_outcome, Some(EventDedupeOutcome::First)) {
                    if let Some(access) = receipt_access.as_ref() {
                        tx.save_receipt_access_record(ReceiptAccessRecord::from_access(
                            counterparty.clone(),
                            classification_app_id
                                .as_ref()
                                .expect("valid Receipt Access has an App ID")
                                .clone(),
                            authorized_receipt_apps.as_ref().is_some_and(|app_ids| {
                                app_ids.contains(
                                    classification_app_id
                                        .as_ref()
                                        .expect("valid Receipt Access has an App ID"),
                                )
                            }),
                            stream_item_id,
                            receive_batch_id,
                            received_at,
                            access,
                        ));
                    }
                }

                report.stream_item_ids.push(stream_item_id);
            }

            if !report.stream_item_ids.is_empty() {
                // Conflicts can invalidate earlier events, not only requests named by this batch.
                let records =
                    payment_request_records_from_transaction(tx, &counterparty, received_at)?;
                for record in records {
                    if matches!(
                        record.state,
                        PaymentRequestLifecycleState::Canceled
                            | PaymentRequestLifecycleState::Rejected
                            | PaymentRequestLifecycleState::ProofSubmitted
                            | PaymentRequestLifecycleState::InvalidConflict
                    ) {
                        tx.remove_payment_request_execution_claim(
                            &counterparty,
                            &record.payment_request_id,
                        );
                    }
                }
            }

            let checkpointed_link = link_state.is_some();
            if let Some(link_state) = link_state {
                tx.save_encrypted_link_state(link_state);
            }
            if let Some(mut peer) = tx.linked_peer(&counterparty) {
                if !report.stream_item_ids.is_empty() {
                    peer.last_private_receive_at = Some(received_at);
                }
                if checkpointed_link || !report.stream_item_ids.is_empty() {
                    peer.last_sync_at = Some(received_at);
                }
                tx.save_linked_peer(peer);
            }

            Ok(report)
        }
    })
    .await
}

pub(crate) struct PrivateStreamMessageClassification {
    pub(crate) status: PrivateStreamParseStatus,
    pub(crate) parse_error: Option<String>,
    pub(crate) event: Option<PrivateStreamEventHeader>,
    pub(crate) receipt_access: Option<ReceiptAccess>,
    pub(crate) app_id: Option<PaykitAppId>,
}

pub(crate) struct PrivateStreamEventHeader {
    pub(crate) event_id: String,
    pub(crate) event_kind: String,
}

pub(crate) fn classify_private_application_message(
    message: &PrivateApplicationMessage,
) -> PrivateStreamMessageClassification {
    let Some(kind) = message.known_kind() else {
        let app_id = message
            .app_id
            .as_deref()
            .and_then(|app_id| PaykitAppId::new(app_id).ok());
        return PrivateStreamMessageClassification {
            status: if message.version.is_some() && message.kind.is_some() && app_id.is_some() {
                PrivateStreamParseStatus::UnknownKind
            } else {
                PrivateStreamParseStatus::InvalidJson
            },
            parse_error: message.invalid_utf8_error().map(str::to_owned),
            event: None,
            receipt_access: None,
            app_id,
        };
    };

    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            match parse_private_payment_list_json(&message.raw_json) {
                Ok(list) => PrivateStreamMessageClassification {
                    status: PrivateStreamParseStatus::Valid,
                    parse_error: None,
                    event: None,
                    receipt_access: None,
                    app_id: Some(list.app_id().clone()),
                },
                Err(err) => PrivateStreamMessageClassification {
                    status: PrivateStreamParseStatus::MalformedRecognized,
                    parse_error: Some(err.to_string()),
                    event: None,
                    receipt_access: None,
                    app_id: None,
                },
            }
        }
        PrivateMessageKind::ReceiptAccess => {
            let parsed = parse_receipt_access_event_message(message);
            let mut classification = classify_event_message(kind, parsed.as_ref());
            classification.receipt_access =
                parsed.and_then(|parsed| parsed.parsed_access().cloned());
            classification
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentConversionQuote
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            classify_event_message(kind, parse_payment_request_event_message(message).as_ref())
        }
        PrivateMessageKind::AllowanceProposal
        | PrivateMessageKind::AllowanceAcceptance
        | PrivateMessageKind::AllowanceRejection
        | PrivateMessageKind::AllowanceEnd => {
            classify_event_message(kind, parse_allowance_event_message(message).as_ref())
        }
    }
}

/// Classify one recognized Event Message kind from its typed parse result.
///
/// `None` means the parser rejected the kind it was handed, which is treated
/// as a malformed recognized message with no header to dedupe on.
fn classify_event_message<M: ParsedEventMessage>(
    kind: PrivateMessageKind,
    parsed: Option<&M>,
) -> PrivateStreamMessageClassification {
    let is_valid = parsed.is_some_and(ParsedEventMessage::is_valid);
    PrivateStreamMessageClassification {
        app_id: parsed.and_then(ParsedEventMessage::app_id).cloned(),
        status: if is_valid {
            PrivateStreamParseStatus::Valid
        } else {
            PrivateStreamParseStatus::MalformedRecognized
        },
        parse_error: parsed
            .and_then(ParsedEventMessage::validation_error)
            .map(str::to_owned),
        event: parsed
            .and_then(ParsedEventMessage::event_id)
            .map(|event_id| PrivateStreamEventHeader {
                event_id: event_id.as_str().to_owned(),
                event_kind: kind.as_str().to_owned(),
            }),
        receipt_access: None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventDedupeOutcome {
    First,
    Duplicate,
    Conflict,
}

fn update_event_dedupe(
    tx: &mut dyn crate::storage::StorageTransaction,
    counterparty: &PubkyPublicKey,
    event_id: String,
    event_kind: String,
    payload_hash: String,
    stream_item_id: u64,
    report: &mut PrivateStreamIntakeReport,
) -> EventDedupeOutcome {
    let Some(mut record) = tx.event_dedup_record(counterparty, &event_id) else {
        tx.save_event_dedup_record(EventDedupRecord {
            counterparty: counterparty.clone(),
            event_id,
            event_kind,
            payload_hash,
            first_stream_item_id: stream_item_id,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        });
        return EventDedupeOutcome::First;
    };

    let outcome = if record.payload_hash == payload_hash {
        record.duplicate_stream_item_ids.push(stream_item_id);
        EventDedupeOutcome::Duplicate
    } else {
        record.conflicting_stream_item_ids.push(stream_item_id);
        report.event_conflicts.push(EventIdConflict {
            event_id: record.event_id.clone(),
            first_stream_item_id: record.first_stream_item_id,
            conflicting_stream_item_id: stream_item_id,
        });
        EventDedupeOutcome::Conflict
    };

    tx.save_event_dedup_record(record);
    outcome
}

pub(crate) fn payload_hash(raw_json: &str) -> String {
    let digest = Sha256::digest(raw_json.as_bytes());
    format!("sha256:{digest:x}")
}

/// Return a canonical Event ID from a JSON carrier when one is present.
pub(crate) fn canonical_event_id(raw_json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(raw_json).ok()?;
    let value = value.get("event_id")?.as_str()?;
    EventId::new(value)
        .ok()
        .map(|event_id| event_id.as_str().to_owned())
}

/// Whether a recognized Private Message Kind uses Event Message semantics.
///
/// Keep this match exhaustive so adding a new recognized kind requires an
/// explicit Event ID policy decision at compile time.
pub(crate) fn is_event_message_kind(kind: &str) -> bool {
    match PrivateMessageKind::parse(kind) {
        None | Some(PrivateMessageKind::PrivatePaymentList) => false,
        Some(
            PrivateMessageKind::ReceiptAccess
            | PrivateMessageKind::PaymentRequest
            | PrivateMessageKind::PaymentRequestAcceptance
            | PrivateMessageKind::PaymentRequestRejection
            | PrivateMessageKind::PaymentConversionQuote
            | PrivateMessageKind::PaymentRequestCancellation
            | PrivateMessageKind::PaymentProof
            | PrivateMessageKind::AllowanceProposal
            | PrivateMessageKind::AllowanceAcceptance
            | PrivateMessageKind::AllowanceRejection
            | PrivateMessageKind::AllowanceEnd,
        ) => true,
    }
}

/// Whether a recognized kind carries a Payment Request lifecycle event.
pub(crate) fn is_payment_request_kind(kind: Option<&str>) -> bool {
    match kind.and_then(PrivateMessageKind::parse) {
        Some(
            PrivateMessageKind::PaymentRequest
            | PrivateMessageKind::PaymentRequestAcceptance
            | PrivateMessageKind::PaymentRequestRejection
            | PrivateMessageKind::PaymentRequestCancellation
            | PrivateMessageKind::PaymentConversionQuote
            | PrivateMessageKind::PaymentProof,
        ) => true,
        None
        | Some(
            PrivateMessageKind::PrivatePaymentList
            | PrivateMessageKind::ReceiptAccess
            | PrivateMessageKind::AllowanceProposal
            | PrivateMessageKind::AllowanceAcceptance
            | PrivateMessageKind::AllowanceRejection
            | PrivateMessageKind::AllowanceEnd,
        ) => false,
    }
}

/// Whether a Private Message Kind carries an Allowance lifecycle event.
pub(crate) fn is_allowance_kind(kind: &str) -> bool {
    match PrivateMessageKind::parse(kind) {
        None
        | Some(
            PrivateMessageKind::PrivatePaymentList
            | PrivateMessageKind::ReceiptAccess
            | PrivateMessageKind::PaymentRequest
            | PrivateMessageKind::PaymentRequestAcceptance
            | PrivateMessageKind::PaymentRequestRejection
            | PrivateMessageKind::PaymentConversionQuote
            | PrivateMessageKind::PaymentRequestCancellation
            | PrivateMessageKind::PaymentProof,
        ) => false,
        Some(
            PrivateMessageKind::AllowanceProposal
            | PrivateMessageKind::AllowanceAcceptance
            | PrivateMessageKind::AllowanceRejection
            | PrivateMessageKind::AllowanceEnd,
        ) => true,
    }
}

/// Event IDs carried by the local outbound queue on one exact Encrypted Link.
///
/// Inbound Event IDs are indexed durably at intake; outbound ones are not, so
/// every derivation that must detect Event ID reuse across the two sending
/// directions folds the outbound queue through this one policy.
#[derive(Default)]
pub(crate) struct OutboundEventCarriers {
    /// Every Event ID carried by a live outbound Event Message.
    pub(crate) event_ids: HashSet<String>,
    /// Event IDs reused by outbound Event Messages with different payloads.
    pub(crate) conflicted_event_ids: HashSet<String>,
}

/// Fold one link's outbound queue into its Event ID carriers.
///
/// `Invalid` and `Superseded` records never advance the link, so they do not
/// count as carriers.
pub(crate) fn outbound_event_carriers(
    outbound: &[OutboundPrivateMessageRecord],
) -> OutboundEventCarriers {
    let mut payloads_by_event_id = HashMap::<String, HashSet<&str>>::new();
    for message in outbound {
        if matches!(
            message.status,
            OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::Superseded
        ) || !is_event_message_kind(&message.kind)
        {
            continue;
        }
        if let Some(event_id) = canonical_event_id(&message.raw_json) {
            payloads_by_event_id
                .entry(event_id)
                .or_default()
                .insert(&message.raw_json);
        }
    }
    OutboundEventCarriers {
        event_ids: payloads_by_event_id.keys().cloned().collect(),
        conflicted_event_ids: payloads_by_event_id
            .into_iter()
            .filter_map(|(event_id, payloads)| (payloads.len() > 1).then_some(event_id))
            .collect(),
    }
}

#[cfg(test)]
mod tests;
