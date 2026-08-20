//! Payment Request lifecycle derivation.
//!
//! Derived records can include Payment Request metadata. Treat them as private
//! SDK state.

use std::{
    cmp::{Ordering, Reverse},
    collections::{HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_payment_request_event_message, serialize_payment_request_event, PaymentRequest,
    PaymentRequestEvent, PrivateApplicationMessage, PrivateMessageKind,
};
#[cfg(test)]
use paykit_lib::{
    PaymentProof, PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestRejection,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    domain::outbound_private::enqueue_private_message,
    domain::outbound_private::OutboundPrivateMessageStatus,
    domain::private_stream::payload_hash,
    domain::records::{AmountRecord, BillingPeriodRecord},
    storage::{
        require_paykit_app_capability, EventDedupRecord, NewOutboundPrivateMessage,
        OutboundPrivateMessageRecord, PrivateStreamItemRecord, StorageAdapter, StorageTransaction,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

mod derivation;

pub(crate) use derivation::derive_payment_request_records_from_parts;

use derivation::recurrence_unit_to_str;
pub(crate) use derivation::{
    payment_request_records, payment_request_records_from_transaction,
    received_payment_request_records, request_from_record,
};

/// Local role for one Payment Request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PaymentRequestLocalRole {
    /// Local identity is expected to pay.
    Payer,
    /// Local identity expects to receive payment.
    Payee,
}

/// SDK-derived Payment Request lifecycle state.
///
/// States are derived from the local durable stream and outbound queue. They do
/// not imply counterparty visibility unless the related outbound status is
/// [`OutboundPrivateMessageStatus::Sent`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PaymentRequestLifecycleState {
    /// Proposal is known locally and remains actionable.
    Proposed,
    /// Proposal is past its expiry.
    ProposalExpired,
    /// Acceptance is present locally.
    Accepted,
    /// Rejection is present locally.
    Rejected,
    /// Cancellation is present locally.
    Canceled,
    /// A one-time Payment Proof is present locally.
    ProofSubmitted,
    /// Recurring request acceptance is present locally.
    ActiveRecurring,
    /// A local outbound event may have advanced the private link without a durable checkpoint.
    RecoveryRequired,
    /// Event ordering, dedupe, or lifecycle validation found an invalid state.
    InvalidConflict,
}

/// Filter for listing SDK-derived Payment Requests.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequestFilter {
    /// Restrict results to one counterparty. `None` lists across all known
    /// counterparties with Payment Request activity.
    pub counterparty: Option<PubkyPublicKey>,
    /// Restrict results to one local role.
    pub local_role: Option<PaymentRequestLocalRole>,
    /// Restrict results to lifecycle states. An empty list means all states.
    pub states: Vec<PaymentRequestLifecycleState>,
    /// Restrict results by whether the request has recurrence terms.
    pub recurring: Option<bool>,
    /// Include only inbound Payment Requests received from counterparties.
    pub received_only: bool,
}

impl PaymentRequestFilter {
    pub(crate) fn matches(&self, record: &PaymentRequestRecord) -> bool {
        if let Some(counterparty) = &self.counterparty {
            if &record.counterparty != counterparty {
                return false;
            }
        }
        if let Some(local_role) = self.local_role {
            if record.local_role != Some(local_role) {
                return false;
            }
        }
        if !self.states.is_empty() && !self.states.contains(&record.state) {
            return false;
        }
        if let Some(recurring) = self.recurring {
            let record_recurring = record
                .terms
                .as_ref()
                .and_then(|terms| terms.recurrence.as_ref())
                .is_some();
            if record_recurring != recurring {
                return false;
            }
        }
        true
    }
}

/// Recurrence fields copied from Payment Request terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequestRecurrenceRecord {
    /// Positive interval count.
    pub every: u32,
    /// Recurrence unit string.
    pub unit: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub anchor: String,
    /// Optional RFC3339 UTC timestamp using `Z`, after `starts_at` when
    /// present.
    pub ends_at: Option<String>,
}

/// Immutable Payment Request terms copied into an SDK record.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequestTermsRecord {
    /// Requested amount.
    pub amount: AmountRecord,
    /// Payee-provided payment correlation value.
    pub payment_reference: String,
    /// Proposal expiry before acceptance.
    pub proposal_expires_at: Option<String>,
    /// Optional recurrence.
    pub recurrence: Option<PaymentRequestRecurrenceRecord>,
    /// Accepted Payment Endpoint Identifiers.
    pub accepted_payment_endpoint_identifiers: Vec<String>,
    /// Payee application whose Payment Endpoint must be paid, when constrained.
    pub required_app_id: Option<paykit_lib::PaykitAppId>,
    /// Application-specific metadata.
    pub metadata: JsonMap<String, JsonValue>,
}

impl fmt::Debug for PaymentRequestTermsRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequestTermsRecord")
            .field("amount", &"<redacted>")
            .field("payment_reference", &"<redacted>")
            .field("proposal_expires_at", &self.proposal_expires_at)
            .field("recurrence", &self.recurrence)
            .field(
                "accepted_payment_endpoint_identifiers",
                &self.accepted_payment_endpoint_identifiers,
            )
            .field("required_app_id", &self.required_app_id)
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .finish()
    }
}

impl From<&paykit_lib::PaymentRequestTerms> for PaymentRequestTermsRecord {
    fn from(terms: &paykit_lib::PaymentRequestTerms) -> Self {
        Self {
            amount: AmountRecord::from(&terms.amount),
            payment_reference: terms.payment_reference.as_str().to_owned(),
            proposal_expires_at: terms.proposal_expires_at.clone(),
            recurrence: terms.recurrence.as_ref().map(|recurrence| {
                PaymentRequestRecurrenceRecord {
                    every: recurrence.every,
                    unit: recurrence_unit_to_str(recurrence.unit).to_owned(),
                    starts_at: recurrence.starts_at.clone(),
                    anchor: recurrence.anchor.clone(),
                    ends_at: recurrence.ends_at.clone(),
                }
            }),
            accepted_payment_endpoint_identifiers: terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .map(|identifier| identifier.as_str().to_owned())
                .collect(),
            required_app_id: terms.required_app_id.clone(),
            metadata: terms.metadata.clone(),
        }
    }
}

/// Payment Proof captured in a derived Payment Request record.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentProofRecord {
    /// Event ID.
    pub event_id: String,
    /// Outbound message id, when proof was sent locally.
    pub outbound_message_id: Option<u64>,
    /// Local outbound delivery status, when proof was queued locally.
    pub outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Stream item id, when proof was received from the counterparty.
    pub stream_item_id: Option<u64>,
    /// Payment Reference copied from the proof.
    pub payment_reference: String,
    /// Optional Billing Period copied from the proof.
    pub billing_period: Option<BillingPeriodRecord>,
    /// Application whose endpoint was used for the payment.
    pub payment_app_id: paykit_lib::PaykitAppId,
    /// Payment Endpoint Identifier used for payment.
    pub payment_endpoint_identifier: String,
    /// Method-specific proof object.
    pub proof: JsonMap<String, JsonValue>,
    /// Local record time for this proof.
    pub recorded_at: DateTime<Utc>,
}

impl fmt::Debug for PaymentProofRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentProofRecord")
            .field("event_id", &self.event_id)
            .field("outbound_message_id", &self.outbound_message_id)
            .field("outbound_status", &self.outbound_status)
            .field("stream_item_id", &self.stream_item_id)
            .field("payment_reference", &"<redacted>")
            .field("billing_period", &self.billing_period)
            .field("payment_app_id", &self.payment_app_id)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field(
                "proof",
                &format_args!("<redacted:{} fields>", self.proof.len()),
            )
            .field("recorded_at", &self.recorded_at)
            .finish()
    }
}

/// SDK-derived Payment Request lifecycle record.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequestRecord {
    /// Counterparty associated with the private stream.
    pub counterparty: PubkyPublicKey,
    /// Stable Payment Request ID.
    pub payment_request_id: String,
    /// Local role, when known.
    pub local_role: Option<PaymentRequestLocalRole>,
    /// Derived local lifecycle state.
    pub state: PaymentRequestLifecycleState,
    /// Stream item id of the proposal event.
    pub proposal_stream_item_id: Option<u64>,
    /// Outbound message id of the proposal event.
    pub proposal_outbound_message_id: Option<u64>,
    /// Local outbound delivery status for the proposal event.
    pub proposal_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
    /// Application that created the proposal.
    pub proposal_app_id: Option<paykit_lib::PaykitAppId>,
    /// Payer application that first accepted the proposal.
    pub payer_app_id: Option<paykit_lib::PaykitAppId>,
    /// Immutable terms from the proposal.
    pub terms: Option<PaymentRequestTermsRecord>,
    /// Acceptance Event ID.
    pub accepted_event_id: Option<String>,
    /// Local outbound delivery status for an acceptance event.
    pub accepted_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Rejection Event ID.
    pub rejected_event_id: Option<String>,
    /// Local outbound delivery status for a rejection event.
    pub rejected_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Cancellation Event ID.
    pub canceled_event_id: Option<String>,
    /// Local outbound delivery status for a cancellation event.
    pub canceled_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Payment Proof records in local record order.
    pub payment_proofs: Vec<PaymentProofRecord>,
    /// Last inbound stream item applied to this record.
    pub last_stream_item_id: Option<u64>,
    /// Last outbound message applied to this record.
    pub last_outbound_message_id: Option<u64>,
    /// Local delivery status of the last outbound message applied to this record.
    pub last_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Last event local record time.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Invalid state reason, when available.
    pub invalid_reason: Option<String>,
}

impl fmt::Debug for PaymentRequestRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequestRecord")
            .field("counterparty", &self.counterparty)
            .field("payment_request_id", &self.payment_request_id)
            .field("local_role", &self.local_role)
            .field("state", &self.state)
            .field("proposal_stream_item_id", &self.proposal_stream_item_id)
            .field(
                "proposal_outbound_message_id",
                &self.proposal_outbound_message_id,
            )
            .field("proposal_outbound_status", &self.proposal_outbound_status)
            .field("proposal_event_id", &self.proposal_event_id)
            .field("proposal_app_id", &self.proposal_app_id)
            .field("payer_app_id", &self.payer_app_id)
            .field("accepted_event_id", &self.accepted_event_id)
            .field("accepted_outbound_status", &self.accepted_outbound_status)
            .field("rejected_event_id", &self.rejected_event_id)
            .field("rejected_outbound_status", &self.rejected_outbound_status)
            .field("canceled_event_id", &self.canceled_event_id)
            .field("canceled_outbound_status", &self.canceled_outbound_status)
            .field("payment_proof_count", &self.payment_proofs.len())
            .field("last_stream_item_id", &self.last_stream_item_id)
            .field("last_outbound_message_id", &self.last_outbound_message_id)
            .field("last_outbound_status", &self.last_outbound_status)
            .field("last_event_at", &self.last_event_at)
            .field("invalid_reason", &self.invalid_reason)
            .finish()
    }
}

impl PaymentRequestRecord {
    fn new(counterparty: PubkyPublicKey, payment_request_id: String) -> Self {
        Self {
            counterparty,
            payment_request_id,
            local_role: None,
            state: PaymentRequestLifecycleState::InvalidConflict,
            proposal_stream_item_id: None,
            proposal_outbound_message_id: None,
            proposal_outbound_status: None,
            proposal_event_id: None,
            proposal_app_id: None,
            payer_app_id: None,
            terms: None,
            accepted_event_id: None,
            accepted_outbound_status: None,
            rejected_event_id: None,
            rejected_outbound_status: None,
            canceled_event_id: None,
            canceled_outbound_status: None,
            payment_proofs: Vec::new(),
            last_stream_item_id: None,
            last_outbound_message_id: None,
            last_outbound_status: None,
            last_event_at: None,
            invalid_reason: None,
        }
    }

    fn touch(&mut self, item: &PrivateStreamItemRecord) {
        self.last_stream_item_id = Some(item.stream_item_id);
        self.touch_at(item.received_at);
    }

    fn touch_outbound(&mut self, message: &OutboundPrivateMessageRecord) {
        self.last_outbound_message_id = Some(message.outbound_message_id);
        self.last_outbound_status = Some(message.status.clone());
        self.touch_at(message.updated_at);
    }

    fn touch_at(&mut self, timestamp: DateTime<Utc>) {
        self.last_event_at = Some(
            self.last_event_at
                .map(|current| current.max(timestamp))
                .unwrap_or(timestamp),
        );
    }

    fn mark_invalid(&mut self, item: &PrivateStreamItemRecord, reason: impl Into<String>) {
        self.state = PaymentRequestLifecycleState::InvalidConflict;
        if self.invalid_reason.is_none() {
            self.invalid_reason = Some(reason.into());
        }
        self.touch(item);
    }
}

pub(crate) fn payment_request_record_blocks_app_removal(
    record: &PaymentRequestRecord,
    app_id: &paykit_lib::PaykitAppId,
) -> bool {
    let owned = match record.local_role {
        Some(PaymentRequestLocalRole::Payee) => record.proposal_app_id.as_ref() == Some(app_id),
        Some(PaymentRequestLocalRole::Payer) => record.payer_app_id.as_ref() == Some(app_id),
        None => false,
    };
    owned
        && matches!(
            record.state,
            PaymentRequestLifecycleState::Proposed
                | PaymentRequestLifecycleState::ProposalExpired
                | PaymentRequestLifecycleState::Accepted
                | PaymentRequestLifecycleState::ProofSubmitted
                | PaymentRequestLifecycleState::ActiveRecurring
                | PaymentRequestLifecycleState::RecoveryRequired
        )
}

/// Queue one raw Payment Request protocol event for outbound delivery.
///
/// The exact canonical JSON payload is serialized before it is stored, so retry
/// workers can resend the same Event ID and payload.
pub(crate) async fn enqueue_payment_request_event<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let raw_json = serialize_payment_request_event(app_id, event)?;
    enqueue_private_message(storage, counterparty, raw_json, now).await
}

/// Queue a raw Payment Request proposal for outbound delivery.
pub(crate) async fn enqueue_payment_request<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequest,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Request(event.clone());
    enqueue_payment_request_event(storage, counterparty, app_id, &event, now).await
}

/// Queue a raw Payment Request acceptance for outbound delivery.
#[cfg(test)]
pub(crate) async fn enqueue_payment_request_acceptance<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestAcceptance,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Acceptance(event.clone());
    enqueue_payment_request_event(storage, counterparty, app_id, &event, now).await
}

/// Queue a raw Payment Request rejection for outbound delivery.
#[cfg(test)]
pub(crate) async fn enqueue_payment_request_rejection<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestRejection,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Rejection(event.clone());
    enqueue_payment_request_event(storage, counterparty, app_id, &event, now).await
}

/// Atomically queue the first local acceptance or rejection for a request.
#[cfg(test)]
pub(crate) async fn enqueue_payment_request_response<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    if !matches!(
        event,
        PaymentRequestEvent::Acceptance(_) | PaymentRequestEvent::Rejection(_)
    ) {
        return Err(PaykitSdkError::Protocol {
            context: "Payment Request response must be an acceptance or rejection".into(),
            source: None,
        });
    }
    enqueue_first_local_payment_request_transition(storage, counterparty, app_id, event, now).await
}

#[cfg(test)]
async fn enqueue_first_local_payment_request_transition<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let payment_request_id = event.payment_request_id().clone();
    let raw_json = serialize_payment_request_event(app_id, event)?;
    let kind = match event {
        PaymentRequestEvent::Acceptance(_) => "paykit.payment_request_acceptance",
        PaymentRequestEvent::Rejection(_) => "paykit.payment_request_rejection",
        PaymentRequestEvent::Cancellation(_) => "paykit.payment_request_cancellation",
        _ => {
            return Err(PaykitSdkError::Protocol {
                context: "local Payment Request transition must be an acceptance, rejection, or cancellation"
                    .into(),
                source: None,
            });
        }
    }
    .to_owned();
    let is_cancellation = matches!(event, PaymentRequestEvent::Cancellation(_));
    let app_id = app_id.clone();

    storage
        .transaction(move |tx| {
            require_paykit_app_capability(tx, &app_id, PrivateMessageKind::PaymentRequest)?;
            let conflicting_transition = tx
                .outbound_private_messages(&counterparty)
                .into_iter()
                .filter(|message| {
                    !matches!(
                        message.status,
                        OutboundPrivateMessageStatus::Invalid
                            | OutboundPrivateMessageStatus::Superseded
                    )
                })
                .any(|message| {
                    let stored_app_id = message.app_id.clone();
                    let parsed = parse_payment_request_event_message(&PrivateApplicationMessage {
                        version: Some(1),
                        kind: Some(message.kind),
                        app_id: Some(message.app_id.to_string()),
                        raw_json: message.raw_json,
                    });
                    parsed.is_some_and(|parsed| {
                        parsed.parsed_event().is_some_and(|stored| {
                            if stored.payment_request_id() != &payment_request_id {
                                return false;
                            }
                            match stored {
                                PaymentRequestEvent::Acceptance(_) if is_cancellation => {
                                    stored_app_id != app_id
                                }
                                PaymentRequestEvent::Acceptance(_)
                                | PaymentRequestEvent::Rejection(_)
                                | PaymentRequestEvent::Cancellation(_) => true,
                                _ => false,
                            }
                        })
                    })
                });
            if conflicting_transition {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "Payment Request {payment_request_id} already has a local transition"
                    ),
                    source: None,
                });
            }
            Ok(
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id,
                    kind,
                    raw_json,
                    now,
                )),
            )
        })
        .await
}

/// Atomically validate current request state and queue one local action.
pub(crate) async fn enqueue_checked_payment_request_action<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let kind = match event {
        PaymentRequestEvent::Acceptance(_) => PrivateMessageKind::PaymentRequestAcceptance,
        PaymentRequestEvent::Rejection(_) => PrivateMessageKind::PaymentRequestRejection,
        PaymentRequestEvent::Cancellation(_) => PrivateMessageKind::PaymentRequestCancellation,
        PaymentRequestEvent::Proof(_) => PrivateMessageKind::PaymentProof,
        PaymentRequestEvent::Request(_) => {
            return Err(PaykitSdkError::Protocol {
                context: "checked Payment Request action cannot queue a proposal".into(),
                source: None,
            });
        }
    };
    let raw_json = serialize_payment_request_event(app_id, event)?;
    let app_id = app_id.clone();
    let event = event.clone();
    storage
        .transaction(move |tx| {
            require_paykit_app_capability(tx, &app_id, PrivateMessageKind::PaymentRequest)?;
            require_current_payment_request_action(tx, &counterparty, &app_id, &event, now)?;
            Ok(
                tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                    counterparty,
                    app_id,
                    kind.as_str().to_owned(),
                    raw_json,
                    now,
                )),
            )
        })
        .await
}

fn require_current_payment_request_action(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<()> {
    let payment_request_id = event.payment_request_id();
    let record = payment_request_records_from_transaction(tx, counterparty, now)?
        .into_iter()
        .find(|record| record.payment_request_id == payment_request_id.as_str())
        .ok_or_else(|| PaykitSdkError::NotFound {
            context: format!(
                "Payment Request {payment_request_id} is not known for counterparty {counterparty}"
            ),
            source: None,
        })?;

    match event {
        PaymentRequestEvent::Acceptance(_) => {
            require_local_payer(&record, "accept Payment Request")?;
            require_request_state(
                &record,
                &[PaymentRequestLifecycleState::Proposed],
                "accept Payment Request",
            )?;
            require_origin_app_authorized(tx, counterparty, &record, "accept Payment Request")
        }
        PaymentRequestEvent::Rejection(_) => {
            require_local_payer(&record, "reject Payment Request")?;
            require_request_state(
                &record,
                &[
                    PaymentRequestLifecycleState::Proposed,
                    PaymentRequestLifecycleState::ProposalExpired,
                ],
                "reject Payment Request",
            )?;
            require_origin_app_authorized(tx, counterparty, &record, "reject Payment Request")
        }
        PaymentRequestEvent::Cancellation(_) => {
            require_request_state(
                &record,
                &[
                    PaymentRequestLifecycleState::Proposed,
                    PaymentRequestLifecycleState::ProposalExpired,
                    PaymentRequestLifecycleState::Accepted,
                    PaymentRequestLifecycleState::ActiveRecurring,
                    PaymentRequestLifecycleState::ProofSubmitted,
                ],
                "cancel Payment Request",
            )?;
            require_local_action_app(&record, app_id, "cancel Payment Request")?;
            if record.local_role == Some(PaymentRequestLocalRole::Payer) {
                require_origin_app_authorized(tx, counterparty, &record, "cancel Payment Request")?;
            }
            Ok(())
        }
        PaymentRequestEvent::Proof(proof) => {
            require_local_payer(&record, "submit Payment Proof")?;
            require_request_state(
                &record,
                &[
                    PaymentRequestLifecycleState::Accepted,
                    PaymentRequestLifecycleState::ActiveRecurring,
                ],
                "submit Payment Proof",
            )?;
            if record.payer_app_id.as_ref() != Some(app_id) {
                return Err(PaykitSdkError::Policy {
                    context:
                        "cannot submit Payment Proof: another Paykit app owns the payer response"
                            .into(),
                    source: None,
                });
            }
            require_origin_app_authorized(tx, counterparty, &record, "submit Payment Proof")?;
            let request = request_from_record(&record).ok_or_else(|| PaykitSdkError::Protocol {
                context: "Payment Request terms are unavailable".into(),
                source: None,
            })?;
            proof.validate_for_request(&request).map_err(Into::into)
        }
        _ => Err(PaykitSdkError::Protocol {
            context:
                "local Payment Request action must be an acceptance, rejection, cancellation, or proof"
                    .into(),
            source: None,
        }),
    }
}

fn require_local_payer(record: &PaymentRequestRecord, action: &str) -> Result<()> {
    if record.local_role == Some(PaymentRequestLocalRole::Payer) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: local identity is not the payer"),
            source: None,
        })
    }
}

fn require_request_state(
    record: &PaymentRequestRecord,
    allowed: &[PaymentRequestLifecycleState],
    action: &str,
) -> Result<()> {
    if allowed.contains(&record.state) {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: Payment Request {} is in state {:?}",
                record.payment_request_id, record.state
            ),
            source: None,
        })
    }
}

fn require_local_action_app(
    record: &PaymentRequestRecord,
    app_id: &paykit_lib::PaykitAppId,
    action: &str,
) -> Result<()> {
    let authorized = match record.local_role {
        Some(PaymentRequestLocalRole::Payee) => record.proposal_app_id.as_ref() == Some(app_id),
        Some(PaymentRequestLocalRole::Payer) => record
            .payer_app_id
            .as_ref()
            .is_none_or(|payer_app_id| payer_app_id == app_id),
        None => false,
    };
    if authorized {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: another Paykit app owns this request action"),
            source: None,
        })
    }
}

fn require_origin_app_authorized(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    record: &PaymentRequestRecord,
    action: &str,
) -> Result<()> {
    let proposal_app_id =
        record
            .proposal_app_id
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "cannot {action}: Payment Request {} has no originating Paykit App",
                    record.payment_request_id
                ),
                source: None,
            })?;
    if tx
        .authorized_payment_request_apps(counterparty)
        .is_some_and(|app_ids| app_ids.contains(proposal_app_id))
    {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: originating Paykit app is not currently authorized for Payment Requests"
            ),
            source: None,
        })
    }
}

/// Queue a raw Payment Request cancellation for outbound delivery.
#[cfg(test)]
pub(crate) async fn enqueue_payment_request_cancellation<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestCancellation,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Cancellation(event.clone());
    enqueue_first_local_payment_request_transition(storage, counterparty, app_id, &event, now).await
}

/// Queue a raw Payment Proof for outbound delivery.
#[cfg(test)]
pub(crate) async fn enqueue_payment_proof<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentProof,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Proof(event.clone());
    enqueue_payment_request_event(storage, counterparty, app_id, &event, now).await
}

#[cfg(test)]
mod tests;
