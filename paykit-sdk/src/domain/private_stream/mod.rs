//! Durable private stream records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    domain::receipts::ReceiptAccessRecord,
    storage::{
        require_peer_link_operation_lease, EncryptedLinkStateRecord, EventDedupRecord,
        NewPrivateStreamItem, NewPrivateStreamItemDetails, PeerLinkOperationLease, StorageAdapter,
    },
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
};

use paykit_lib::{
    parse_allowance_event_message, parse_payment_request_event_message,
    parse_private_payment_list_json, parse_receipt_access_event_message, AllowanceEventMessage,
    EventId, PaymentRequestEventMessage, PrivateApplicationMessage, PrivateMessageKind,
    ReceiptAccess, ReceiptAccessEventMessage,
};

/// Read surface shared by every typed Event Message parser in `paykit-lib`.
///
/// Intake classification, outbound validation, and backup validation only
/// need these three views, so each new Event Message family adds one `impl`
/// here instead of a new arm at every site.
pub(crate) trait ParsedEventMessage {
    fn event_id(&self) -> Option<&EventId>;
    fn is_valid(&self) -> bool;
    fn validation_error(&self) -> Option<&str>;
}

macro_rules! impl_parsed_event_message {
    ($($message:ty),* $(,)?) => {$(
        impl ParsedEventMessage for $message {
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
    /// Receive batch id assigned by storage.
    pub receive_batch_id: u64,
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
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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

/// Persist an ordered batch of Private Application Messages and a link checkpoint.
#[cfg(test)]
pub(crate) async fn persist_private_stream_batch<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
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
        counterparty_receiver_path,
        messages,
        link_state,
        None,
        received_at,
    )
    .await
}

/// Persist a batch and checkpoint only if the peer link lease is still active.
pub(crate) async fn persist_private_stream_batch_with_link_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    messages: Vec<PrivateApplicationMessage>,
    link_state: Option<EncryptedLinkStateRecord>,
    link_lease: Option<PeerLinkOperationLease>,
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
                let mut classification = classify_private_application_message(&message);
                enforce_receipt_access_receiver_scope(
                    &mut classification,
                    &counterparty_receiver_path,
                );
                let PrivateStreamMessageClassification {
                    status,
                    parse_error,
                    event,
                    receipt_access,
                } = classification;
                let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem::new(
                    NewPrivateStreamItemDetails {
                        counterparty: counterparty.clone(),
                        counterparty_receiver_path: counterparty_receiver_path.clone(),
                        receive_batch_id,
                        raw_json: message.raw_json.clone(),
                        parsed_version: message.version.map(u32::from),
                        parsed_kind: message.kind.clone(),
                        known_paykit_kind: message
                            .known_kind()
                            .map(|kind| kind.as_str().to_owned()),
                        parse_status: status,
                        parse_error,
                        received_at,
                    },
                ));

                let dedupe_outcome = event.map(|event| {
                    update_event_dedupe(
                        tx,
                        EventDedupeUpdate {
                            counterparty: &counterparty,
                            counterparty_receiver_path: &counterparty_receiver_path,
                            event_id: event.event_id,
                            event_kind: event.event_kind,
                            payload_hash: payload_hash(&message.raw_json),
                            stream_item_id,
                        },
                        &mut report,
                    )
                });

                if matches!(dedupe_outcome, Some(EventDedupeOutcome::First)) {
                    if let Some(access) = receipt_access.as_ref() {
                        tx.save_receipt_access_record(ReceiptAccessRecord::from_access(
                            counterparty.clone(),
                            counterparty_receiver_path.clone(),
                            stream_item_id,
                            receive_batch_id,
                            received_at,
                            access,
                        ));
                    }
                }

                report.stream_item_ids.push(stream_item_id);
            }

            let checkpointed_link = link_state.is_some();
            if let Some(link_state) = link_state {
                if let Some(lease) = link_lease.as_ref() {
                    require_peer_link_operation_lease(tx, lease)?;
                }
                tx.save_encrypted_link_state(link_state);
            }
            if let Some(mut peer) = tx.linked_peer(&counterparty, &counterparty_receiver_path) {
                if !report.stream_item_ids.is_empty() {
                    peer.last_private_receive_at = Some(received_at);
                }
                if checkpointed_link || !report.stream_item_ids.is_empty() {
                    peer.last_sync_at = Some(received_at);
                }
                tx.save_linked_peer(peer);
            }

            Ok(report)
        })
        .await
}

pub(crate) fn enforce_receipt_access_receiver_scope(
    classification: &mut PrivateStreamMessageClassification,
    counterparty_receiver_path: &PaykitReceiverPath,
) {
    let Some(access) = classification.receipt_access.as_ref() else {
        return;
    };
    if access.has_location_for_receiver(counterparty_receiver_path) {
        return;
    }
    classification.status = PrivateStreamParseStatus::MalformedRecognized;
    classification.parse_error = Some(format!(
        "Receipt Access location does not match counterparty receiver {counterparty_receiver_path}"
    ));
    classification.event = None;
    classification.receipt_access = None;
}

pub(crate) struct PrivateStreamMessageClassification {
    pub(crate) status: PrivateStreamParseStatus,
    pub(crate) parse_error: Option<String>,
    pub(crate) event: Option<PrivateStreamEventHeader>,
    pub(crate) receipt_access: Option<ReceiptAccess>,
}

pub(crate) struct PrivateStreamEventHeader {
    pub(crate) event_id: String,
    pub(crate) event_kind: String,
}

pub(crate) fn classify_private_application_message(
    message: &PrivateApplicationMessage,
) -> PrivateStreamMessageClassification {
    let Some(kind) = message.known_kind() else {
        return PrivateStreamMessageClassification {
            status: if message.version.is_some() && message.kind.is_some() {
                PrivateStreamParseStatus::UnknownKind
            } else {
                PrivateStreamParseStatus::InvalidJson
            },
            parse_error: message.invalid_utf8_error().map(str::to_owned),
            event: None,
            receipt_access: None,
        };
    };

    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            match parse_private_payment_list_json(&message.raw_json) {
                Ok(_) => PrivateStreamMessageClassification {
                    status: PrivateStreamParseStatus::Valid,
                    parse_error: None,
                    event: None,
                    receipt_access: None,
                },
                Err(err) => PrivateStreamMessageClassification {
                    status: PrivateStreamParseStatus::MalformedRecognized,
                    parse_error: Some(err.to_string()),
                    event: None,
                    receipt_access: None,
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

struct EventDedupeUpdate<'a> {
    counterparty: &'a PubkyPublicKey,
    counterparty_receiver_path: &'a PaykitReceiverPath,
    event_id: String,
    event_kind: String,
    payload_hash: String,
    stream_item_id: u64,
}

fn update_event_dedupe(
    tx: &mut dyn crate::storage::StorageTransaction,
    update: EventDedupeUpdate<'_>,
    report: &mut PrivateStreamIntakeReport,
) -> EventDedupeOutcome {
    let Some(mut record) = tx.event_dedup_record(
        update.counterparty,
        update.counterparty_receiver_path,
        &update.event_id,
    ) else {
        tx.save_event_dedup_record(EventDedupRecord {
            counterparty: update.counterparty.clone(),
            counterparty_receiver_path: update.counterparty_receiver_path.clone(),
            event_id: update.event_id,
            event_kind: update.event_kind,
            payload_hash: update.payload_hash,
            first_stream_item_id: update.stream_item_id,
            duplicate_stream_item_ids: Vec::new(),
            conflicting_stream_item_ids: Vec::new(),
        });
        return EventDedupeOutcome::First;
    };

    let outcome = if record.payload_hash == update.payload_hash {
        record.duplicate_stream_item_ids.push(update.stream_item_id);
        EventDedupeOutcome::Duplicate
    } else {
        record
            .conflicting_stream_item_ids
            .push(update.stream_item_id);
        report.event_conflicts.push(EventIdConflict {
            event_id: record.event_id.clone(),
            first_stream_item_id: record.first_stream_item_id,
            conflicting_stream_item_id: update.stream_item_id,
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

#[cfg(test)]
mod tests;
