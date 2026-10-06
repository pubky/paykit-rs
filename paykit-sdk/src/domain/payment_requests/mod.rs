//! Payment Request lifecycle derivation.
//!
//! Derived records can include Payment Request metadata. Treat them as private
//! SDK state.

use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_payment_request_event_message, serialize_payment_request_event, AllowanceId,
    BillingPeriod, ConversionRate, EventId, PaymentConversion, PaymentConversionQuote,
    PaymentDeadline, PaymentEndpointIdentifier, PaymentProof, PaymentRequest, PaymentRequestEvent,
    PrivateApplicationMessage, PrivateMessageKind,
};
#[cfg(test)]
use paykit_lib::{PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestRejection};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

#[cfg(test)]
use crate::domain::outbound_private::enqueue_private_message;
use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    domain::private_stream::{
        is_payment_request_kind, outbound_event_carriers, payload_hash, OutboundEventCarriers,
    },
    domain::records::{AmountRecord, BillingPeriodRecord},
    storage::{
        require_paykit_app_capability, retry_storage_transaction, EventDedupRecord,
        NewOutboundPrivateMessage, OutboundPrivateMessageRecord, PaymentRequestExecutionClaim,
        PrivateStreamItemRecord, StorageAdapter, StorageTransaction,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

mod derivation;

pub(crate) use derivation::derive_payment_request_records_from_parts;

use derivation::recurrence_unit_to_str;
pub(crate) use derivation::{
    payment_proof_allowed_states, payment_request_records,
    payment_request_records_from_transaction, received_payment_request_records_from_transaction,
    request_from_record, validate_proof_conversion,
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
    /// Immutable request-bound Payment Endpoints owned by `required_app_id`.
    /// Payloads are private SDK state; preserve the full optional layout in storage.
    pub payment_endpoints: Option<HashMap<String, String>>,
    /// Payee application whose Payment Endpoint must be paid, when constrained.
    pub required_app_id: Option<paykit_lib::PaykitAppId>,
    /// Optional conversion policy copied from immutable terms.
    pub conversion: Option<PaymentConversion>,
    /// Actual-payment deadline, independent of proposal acceptance.
    pub payment_deadline: Option<PaymentDeadline>,
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
                "payment_endpoints",
                &self.payment_endpoints.as_ref().map(|_| "<redacted>"),
            )
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
            amount: AmountRecord::from(terms.amount()),
            payment_reference: terms.payment_reference().as_str().to_owned(),
            proposal_expires_at: terms.proposal_expires_at().clone(),
            recurrence: terms.recurrence().as_ref().map(|recurrence| {
                PaymentRequestRecurrenceRecord {
                    every: recurrence.every(),
                    unit: recurrence_unit_to_str(recurrence.unit()).to_owned(),
                    starts_at: recurrence.starts_at().to_owned(),
                    anchor: recurrence.anchor().to_owned(),
                    ends_at: recurrence.ends_at().to_owned(),
                }
            }),
            accepted_payment_endpoint_identifiers: terms
                .accepted_payment_endpoint_identifiers()
                .iter()
                .map(|identifier| identifier.as_str().to_owned())
                .collect(),
            required_app_id: terms.required_app_id().cloned(),
            payment_endpoints: terms.payment_endpoints().map(|endpoints| {
                endpoints
                    .iter()
                    .map(|(identifier, payload)| {
                        (identifier.as_str().to_owned(), payload.as_str().to_owned())
                    })
                    .collect()
            }),
            conversion: terms.conversion().cloned(),
            payment_deadline: terms.payment_deadline().cloned(),
            metadata: terms.metadata().clone(),
        }
    }
}

/// Caller-supplied evidence for one Payment Proof.
///
/// This input reports a payment; it does not authorize or execute one. The
/// caller owns settlement validation and must derive any Allowance attribution
/// from its durable payment association, not the currently matching Allowance.
#[derive(Clone, PartialEq)]
pub struct PaymentProofSubmission {
    /// Required for recurring requests and absent for one-time requests.
    pub billing_period: Option<BillingPeriod>,
    /// Payee App whose endpoint was used for this payment execution.
    pub payment_app_id: paykit_lib::PaykitAppId,
    /// Payment Endpoint Identifier used by this payment execution.
    pub payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// Selected recurring conversion quote Event ID.
    pub conversion_quote_id: Option<EventId>,
    /// Method-specific evidence; Paykit does not verify its settlement claims.
    pub proof: JsonMap<String, JsonValue>,
    /// Optional report of the Allowance consumed by this payment.
    ///
    /// Absence means no attribution was supplied. This field never changes
    /// Allowance authority, reservations, or usage accounting.
    pub allowance_id: Option<AllowanceId>,
}

impl fmt::Debug for PaymentProofSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentProofSubmission")
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field("allowance_id", &self.allowance_id)
            .field(
                "proof",
                &format_args!("<redacted:{} fields>", self.proof.len()),
            )
            .finish()
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
    /// Informational Allowance attribution copied from the proof, when supplied.
    ///
    /// Unknown or ended Allowances remain reportable historical claims. This
    /// value does not prove settlement or change Allowance authority or usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowance_id: Option<String>,
    /// Selected recurring conversion quote Event ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversion_quote_id: Option<String>,
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
            .field("allowance_id", &self.allowance_id)
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

/// Immutable payee quote retained alongside the request's event history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaymentConversionQuoteRecord {
    /// Quote identity (the quote Event ID).
    pub event_id: String,
    /// Billing Period to which these rates apply.
    pub billing_period: BillingPeriodRecord,
    /// Units of payment asset per one requested asset unit.
    pub rates: Vec<ConversionRate>,
    /// Inclusive start of the payment validity interval, in RFC3339 UTC.
    pub valid_from: String,
    /// Inclusive actual-payment deadline for this quote.
    pub expires_at: String,
    /// Local outbound delivery status, when issued locally.
    pub outbound_status: Option<OutboundPrivateMessageStatus>,
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
    /// Payer application associated with the request, independent of its current execution claim.
    pub payer_app_id: Option<paykit_lib::PaykitAppId>,
    /// Paykit App that currently owns payment execution for this identity.
    pub execution_claim_app_id: Option<paykit_lib::PaykitAppId>,
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
    /// All validated quotes, including expired quotes needed for delayed payment evidence.
    pub conversion_quotes: Vec<PaymentConversionQuoteRecord>,
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
            .field("execution_claim_app_id", &self.execution_claim_app_id)
            .field("accepted_event_id", &self.accepted_event_id)
            .field("accepted_outbound_status", &self.accepted_outbound_status)
            .field("rejected_event_id", &self.rejected_event_id)
            .field("rejected_outbound_status", &self.rejected_outbound_status)
            .field("canceled_event_id", &self.canceled_event_id)
            .field("canceled_outbound_status", &self.canceled_outbound_status)
            .field("conversion_quote_count", &self.conversion_quotes.len())
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
            execution_claim_app_id: None,
            terms: None,
            accepted_event_id: None,
            accepted_outbound_status: None,
            rejected_event_id: None,
            rejected_outbound_status: None,
            canceled_event_id: None,
            canceled_outbound_status: None,
            conversion_quotes: Vec::new(),
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
    // Terminal protocol events do not release a claim while wallet execution is unresolved.
    if record.execution_claim_app_id.as_ref() == Some(app_id) {
        return true;
    }
    let owned = match record.local_role {
        Some(PaymentRequestLocalRole::Payee) => record.proposal_app_id.as_ref() == Some(app_id),
        Some(PaymentRequestLocalRole::Payer) => {
            record.execution_claim_app_id.as_ref() == Some(app_id)
        }
        None => false,
    };
    owned
        && matches!(
            record.state,
            PaymentRequestLifecycleState::Proposed
                | PaymentRequestLifecycleState::ProposalExpired
                | PaymentRequestLifecycleState::Accepted
                | PaymentRequestLifecycleState::ActiveRecurring
                | PaymentRequestLifecycleState::RecoveryRequired
        )
}

/// Queue one raw Payment Request protocol event for outbound delivery.
///
/// The exact canonical JSON payload is serialized before it is stored, so retry
/// workers can resend the same Event ID and payload.
#[cfg(test)]
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

/// Queue a Payment Request proposal and derive its result in the same transaction.
pub(crate) async fn enqueue_payment_request<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    request: &PaymentRequest,
    now: DateTime<Utc>,
) -> Result<PaymentRequestRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Request(request.clone());
    let raw_json = serialize_payment_request_event(app_id, &event)?;
    let (_, kind) = crate::domain::outbound_private::validate_outbound_private_message(&raw_json)?;
    storage
        .transaction(|tx| {
            require_paykit_app_capability(tx, app_id, PrivateMessageKind::PaymentRequest)?;
            tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty.clone(),
                app_id.clone(),
                kind,
                raw_json,
                now,
            ))?;
            payment_request_records_from_transaction(tx, &counterparty, now)?
                .into_iter()
                .find(|record| record.payment_request_id == request.payment_request_id().as_str())
                .ok_or_else(|| PaykitSdkError::Protocol {
                    context: "queued Payment Request has no derived record".into(),
                    source: None,
                })
        })
        .await
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
    enqueue_checked_payment_request_action_with_identity(
        storage,
        counterparty,
        app_id,
        event,
        || now,
        None,
    )
    .await
}

/// Queue a manual action while retaining the runtime's captured payer identity.
pub(crate) async fn enqueue_checked_payment_request_action_with_identity<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: impl Fn() -> DateTime<Utc> + Send + Sync,
    expected_identity: Option<crate::IdentityState>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let kind = match event {
        PaymentRequestEvent::Acceptance(_) => PrivateMessageKind::PaymentRequestAcceptance,
        PaymentRequestEvent::Rejection(_) => PrivateMessageKind::PaymentRequestRejection,
        PaymentRequestEvent::Cancellation(_) => PrivateMessageKind::PaymentRequestCancellation,
        PaymentRequestEvent::Proof(_) => PrivateMessageKind::PaymentProof,
        PaymentRequestEvent::Request(_) | PaymentRequestEvent::ConversionQuote(_) => {
            return Err(PaykitSdkError::Protocol {
                context: "checked Payment Request action cannot queue a proposal".into(),
                source: None,
            });
        }
    };
    let raw_json = serialize_payment_request_event(app_id, event)?;
    crate::domain::outbound_private::validate_outbound_private_message(&raw_json)?;
    let app_id = app_id.clone();
    let event = event.clone();
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let app_id = app_id.clone();
        let event = event.clone();
        let raw_json = raw_json.clone();
        let expected_identity = expected_identity.clone();
        let now = &now;
        move |tx| {
            let now = now();
            if expected_identity
                .as_ref()
                .is_some_and(|expected| tx.load_identity_state().as_ref() != Some(expected))
            {
                return Err(PaykitSdkError::Policy {
                    context: "Payment accounting identity changed during operation".into(),
                    source: None,
                });
            }
            require_paykit_app_capability(tx, &app_id, PrivateMessageKind::PaymentRequest)?;
            let release_execution_claim =
                require_current_payment_request_action(tx, &counterparty, &app_id, &event, now)?;
            if !matches!(event, PaymentRequestEvent::Proof(_)) {
                crate::domain::allowance_accounting::manual_response(
                    tx,
                    crate::PaymentRequestScope {
                        counterparty: counterparty.clone(),
                        payment_request_id: event.payment_request_id().clone(),
                    },
                )?;
            }
            let outbound = tx.insert_outbound_private_message(NewOutboundPrivateMessage::new(
                counterparty.clone(),
                app_id.clone(),
                kind.as_str().to_owned(),
                raw_json,
                now,
            ))?;
            if release_execution_claim
                && !request_has_unresolved_payment(
                    tx,
                    &counterparty,
                    event.payment_request_id().as_str(),
                )
            {
                let records = payment_request_records_from_transaction(tx, &counterparty, now)?;
                let pending_proof = records.iter().any(|record| {
                    record.payment_request_id == event.payment_request_id().as_str()
                        && request_has_unreported_successful_payment(
                            tx.allowance_accounting_state().as_ref(),
                            record,
                        )
                });
                if !pending_proof {
                    tx.remove_payment_request_execution_claim(
                        &counterparty,
                        event.payment_request_id().as_str(),
                    );
                }
            }
            Ok(outbound)
        }
    })
    .await
}

pub(crate) async fn claim_payment_request_execution<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    payment_request_id: &paykit_lib::PaymentRequestId,
    now: DateTime<Utc>,
) -> Result<PaymentRequestRecord>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let app_id = app_id.clone();
        let payment_request_id = payment_request_id.as_str().to_owned();
        move |tx| {
            require_paykit_app_capability(
                tx,
                &app_id,
                PrivateMessageKind::PaymentRequest,
            )?;
            if !tx
                .paykit_app_capabilities(&app_id)
                .is_some_and(|capabilities| capabilities.outgoing_payments)
            {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "Paykit app '{app_id}' is not authorized for outgoing payments"
                    ),
                    source: None,
                });
            }
            let mut record = payment_request_records_from_transaction(
                tx,
                &counterparty,
                now,
            )?
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id)
            .ok_or_else(|| PaykitSdkError::NotFound {
                context: format!(
                    "Payment Request {payment_request_id} is not known for counterparty {counterparty}"
                ),
                source: None,
            })?;
            require_local_payer(&record, "claim Payment Request for execution")?;
            require_execution_claim_state(
                &record,
                "claim Payment Request for execution",
            )?;
            require_origin_app_authorized(
                tx,
                &counterparty,
                &record,
                "claim Payment Request for execution",
            )?;
            if let Some(existing) =
                tx.payment_request_execution_claim(&counterparty, &payment_request_id)
            {
                if existing.app_id != app_id {
                    return Err(PaykitSdkError::Policy {
                        context: format!(
                            "Payment Request {payment_request_id} is already claimed by Paykit app '{}'",
                            existing.app_id
                        ),
                        source: None,
                    });
                }
            } else {
                if request_has_unresolved_payment(tx, &counterparty, &payment_request_id) {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot claim Payment Request with unresolved payment accounting".into(),
                        source: None,
                    });
                }
                tx.save_payment_request_execution_claim(PaymentRequestExecutionClaim {
                    counterparty: counterparty.clone(),
                    payment_request_id: payment_request_id.clone(),
                    app_id: app_id.clone(),
                    claimed_at: now,
                });
            }
            record.execution_claim_app_id = Some(app_id);
            Ok(record)
        }
    })
    .await
}

pub(crate) async fn release_payment_request_execution_claim<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    payment_request_id: &paykit_lib::PaymentRequestId,
    now: DateTime<Utc>,
) -> Result<PaymentRequestRecord>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let app_id = app_id.clone();
        let payment_request_id = payment_request_id.as_str().to_owned();
        move |tx| {
            let mut record = payment_request_records_from_transaction(
                tx,
                &counterparty,
                now,
            )?
            .into_iter()
            .find(|record| record.payment_request_id == payment_request_id)
            .ok_or_else(|| PaykitSdkError::NotFound {
                context: format!(
                    "Payment Request {payment_request_id} is not known for counterparty {counterparty}"
                ),
                source: None,
            })?;
            require_local_payer(&record, "release Payment Request execution claim")?;
            if request_has_unresolved_payment(tx, &counterparty, &payment_request_id)
                || request_has_unreported_successful_payment(
                    tx.allowance_accounting_state().as_ref(),
                    &record,
                )
            {
                return Err(PaykitSdkError::Policy {
                    context: "cannot release Payment Request execution claim with unresolved payment accounting or an unreported successful payment".into(),
                    source: None,
                });
            }
            require_execution_claim_release_state(
                &record,
                "release Payment Request execution claim",
            )?;
            if let Some(existing) =
                tx.payment_request_execution_claim(&counterparty, &payment_request_id)
            {
                if existing.app_id != app_id {
                    return Err(PaykitSdkError::Policy {
                        context: format!(
                            "Payment Request {payment_request_id} is claimed by Paykit app '{}'",
                            existing.app_id
                        ),
                        source: None,
                    });
                }
                tx.remove_payment_request_execution_claim(
                    &counterparty,
                    &payment_request_id,
                );
            }
            record.execution_claim_app_id = None;
            Ok(record)
        }
    })
    .await
}

pub(crate) fn require_current_payment_request_action(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<bool> {
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
            require_outgoing_payment_app(tx, app_id, "accept Payment Request")?;
            require_request_state(
                &record,
                &[PaymentRequestLifecycleState::Proposed],
                "accept Payment Request",
            )?;
            require_origin_app_authorized(tx, counterparty, &record, "accept Payment Request")?;
            require_execution_claim_owner(
                tx,
                counterparty,
                payment_request_id.as_str(),
                app_id,
                "accept Payment Request",
            )?;
            Ok(false)
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
            require_origin_app_authorized(tx, counterparty, &record, "reject Payment Request")?;
            if tx
                .payment_request_execution_claim(counterparty, payment_request_id.as_str())
                .is_some_and(|claim| claim.app_id != *app_id)
            {
                return Err(PaykitSdkError::Policy {
                    context: "cannot reject Payment Request: another Paykit app owns payment execution"
                        .into(),
                    source: None,
                });
            }
            Ok(true)
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
            if record.local_role == Some(PaymentRequestLocalRole::Payer) {
                require_origin_app_authorized(tx, counterparty, &record, "cancel Payment Request")?;
                let completed_execution = record.state == PaymentRequestLifecycleState::ProofSubmitted
                    && record.payer_app_id.as_ref() == Some(app_id)
                    && tx
                        .payment_request_execution_claim(counterparty, payment_request_id.as_str())
                        .is_none()
                    && !request_has_unresolved_payment(tx, counterparty, payment_request_id.as_str());
                let unclaimed_expired_proposal = record.state
                    == PaymentRequestLifecycleState::ProposalExpired
                    && record.execution_claim_app_id.is_none();
                if !completed_execution && !unclaimed_expired_proposal {
                    require_execution_claim_owner(
                        tx,
                        counterparty,
                        payment_request_id.as_str(),
                        app_id,
                        "cancel Payment Request",
                    )?;
                }
            } else {
                require_local_action_app(&record, app_id, "cancel Payment Request")?;
            }
            Ok(true)
        }
        PaymentRequestEvent::Proof(proof) => {
            require_local_payer(&record, "submit Payment Proof")?;
            require_request_state(
                &record,
                payment_proof_allowed_states(&record),
                "submit Payment Proof",
            )?;
            let correction = if let Some(existing) =
                proof_for_billing_period(&record, proof.billing_period().as_ref())
            {
                let sender = tx
                    .outbound_private_messages(counterparty)
                    .into_iter()
                    .find(|message| Some(message.outbound_message_id) == existing.outbound_message_id)
                    .map(|message| message.app_id);
                if sender.as_ref() != Some(app_id) {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot submit Payment Proof: another Paykit app reported this billing period".into(),
                        source: None,
                    });
                }
                true
            } else {
                false
            };
            let historical_evidence = record.state == PaymentRequestLifecycleState::Canceled
                && record.payer_app_id.as_ref() == Some(app_id)
                && record.execution_claim_app_id.is_none()
                && !tx.allowance_accounting_state().is_some_and(|state| {
                    state.history.occurrences.iter().any(|occurrence| {
                        occurrence.key.request.counterparty == *counterparty
                            && occurrence.key.request.payment_request_id == payment_request_id.as_str()
                            && !occurrence.attempts.is_empty()
                    })
                });
            if !correction && !historical_evidence {
                require_execution_claim_owner(
                    tx,
                    counterparty,
                    payment_request_id.as_str(),
                    app_id,
                    "submit Payment Proof",
                )?;
            }
            // Evidence reports a past payment; withdrawing the payee App cannot
            // revoke the payer's claim or its authority to correct that evidence.
            let request = request_from_record(&record).ok_or_else(|| PaykitSdkError::Protocol {
                context: "Payment Request terms are unavailable".into(),
                source: None,
            })?;
            validate_proof_conversion(&record, proof, &request)?;
            Ok(!correction
                && (record.state == PaymentRequestLifecycleState::Canceled
                    || record.terms.as_ref().is_some_and(|terms| terms.recurrence.is_none())))
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

fn require_execution_claim_state(record: &PaymentRequestRecord, action: &str) -> Result<()> {
    let claimable = match record.state {
        PaymentRequestLifecycleState::Proposed => true,
        PaymentRequestLifecycleState::Accepted => {
            record
                .terms
                .as_ref()
                .is_some_and(|terms| terms.recurrence.is_none())
                && record.payment_proofs.is_empty()
        }
        PaymentRequestLifecycleState::ActiveRecurring => record
            .terms
            .as_ref()
            .is_some_and(|terms| terms.recurrence.is_some()),
        _ => false,
    };
    if claimable {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!(
                "cannot {action}: Payment Request {} has no unpaid claimable work in state {:?}",
                record.payment_request_id, record.state
            ),
            source: None,
        })
    }
}

fn require_execution_claim_release_state(
    record: &PaymentRequestRecord,
    action: &str,
) -> Result<()> {
    if matches!(
        record.state,
        PaymentRequestLifecycleState::ProposalExpired
            | PaymentRequestLifecycleState::RecoveryRequired
    ) {
        return Ok(());
    }
    require_execution_claim_state(record, action)
}

pub(crate) fn require_payment_execution_authority(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    app_id: &paykit_lib::PaykitAppId,
    record: &PaymentRequestRecord,
) -> Result<()> {
    require_paykit_app_capability(tx, app_id, PrivateMessageKind::PaymentRequest)?;
    require_outgoing_payment_app(tx, app_id, "execute Payment Request")?;
    require_origin_app_authorized(tx, counterparty, record, "execute Payment Request")?;
    require_execution_claim_owner(
        tx,
        counterparty,
        &record.payment_request_id,
        app_id,
        "execute Payment Request",
    )
}

pub(crate) fn request_has_unresolved_payment(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    payment_request_id: &str,
) -> bool {
    tx.allowance_accounting_state().is_some_and(|state| {
        state.history.occurrences.iter().any(|occurrence| {
            occurrence.key.request.counterparty == *counterparty
                && occurrence.key.request.payment_request_id == payment_request_id
                && occurrence.attempts.iter().any(|attempt| {
                    matches!(
                        attempt.status,
                        crate::PaymentExecutionStatus::Prepared
                            | crate::PaymentExecutionStatus::Submitted
                            | crate::PaymentExecutionStatus::Unknown
                    )
                })
        })
    })
}

/// Terminal protocol actions retain their execution claim until wallet outcomes are resolved.
pub(crate) fn release_resolved_payment_execution_claims(
    tx: &mut dyn StorageTransaction,
    now: DateTime<Utc>,
) -> Result<()> {
    for claim in tx
        .export_storage_state()
        .payment_request_execution_claims
        .into_values()
    {
        if payment_execution_claim_is_resolved(tx, &claim, now)? {
            tx.remove_payment_request_execution_claim(
                &claim.counterparty,
                &claim.payment_request_id,
            );
        }
    }
    Ok(())
}

pub(crate) fn payment_execution_claim_is_resolved(
    tx: &dyn StorageTransaction,
    claim: &PaymentRequestExecutionClaim,
    now: DateTime<Utc>,
) -> Result<bool> {
    if request_has_unresolved_payment(tx, &claim.counterparty, &claim.payment_request_id) {
        return Ok(false);
    }
    let records = payment_request_records_from_transaction(tx, &claim.counterparty, now)?;
    Ok(records.iter().any(|record| {
        record.payment_request_id == claim.payment_request_id
            && !request_has_unreported_successful_payment(
                tx.allowance_accounting_state().as_ref(),
                record,
            )
            && matches!(
                record.state,
                PaymentRequestLifecycleState::Canceled
                    | PaymentRequestLifecycleState::Rejected
                    | PaymentRequestLifecycleState::ProofSubmitted
                    | PaymentRequestLifecycleState::InvalidConflict
            )
    }))
}

fn require_execution_claim_owner(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    payment_request_id: &str,
    app_id: &paykit_lib::PaykitAppId,
    action: &str,
) -> Result<()> {
    match tx.payment_request_execution_claim(counterparty, payment_request_id) {
        Some(claim) if claim.app_id == *app_id => Ok(()),
        Some(_) => Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: another Paykit app owns payment execution"),
            source: None,
        }),
        None => Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: claim payment execution first"),
            source: None,
        }),
    }
}

fn proof_for_billing_period<'a>(
    record: &'a PaymentRequestRecord,
    period: Option<&BillingPeriod>,
) -> Option<&'a PaymentProofRecord> {
    record.payment_proofs.iter().find(|existing| {
        same_billing_period(
            existing
                .billing_period
                .as_ref()
                .map(|period| (period.starts_at.as_str(), period.ends_at.as_str())),
            period.map(|period| (period.starts_at(), period.ends_at())),
        )
    })
}

fn same_billing_period(left: Option<(&str, &str)>, right: Option<(&str, &str)>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some((left_start, left_end)), Some((right_start, right_end))) => {
            match (
                DateTime::parse_from_rfc3339(left_start),
                DateTime::parse_from_rfc3339(left_end),
                DateTime::parse_from_rfc3339(right_start),
                DateTime::parse_from_rfc3339(right_end),
            ) {
                (Ok(left_start), Ok(left_end), Ok(right_start), Ok(right_end)) => {
                    left_start == right_start && left_end == right_end
                }
                _ => false,
            }
        }
        _ => false,
    }
}

/// An executor retains proof authority for settled, unreported occurrences.
pub(crate) fn request_has_unreported_successful_payment(
    accounting: Option<&crate::AllowanceAccountingState>,
    record: &PaymentRequestRecord,
) -> bool {
    // Keep the executor's claim as proof authority until its settled occurrences
    // have evidence in the event log. Terminal lifecycle still forbids execution.
    accounting.is_some_and(|state| {
        state.history.occurrences.iter().any(|occurrence| {
            occurrence.key.request.counterparty == record.counterparty
                && occurrence.key.request.payment_request_id == record.payment_request_id
                && occurrence
                    .attempts
                    .iter()
                    .any(|attempt| attempt.status == crate::PaymentExecutionStatus::Succeeded)
                && !record.payment_proofs.iter().any(|proof| {
                    match (&proof.billing_period, &occurrence.key.billing_period) {
                        (None, None) => true,
                        (Some(proof), Some(period)) => {
                            DateTime::parse_from_rfc3339(&proof.starts_at)
                                .is_ok_and(|start| start == period.starts_at)
                                && DateTime::parse_from_rfc3339(&proof.ends_at)
                                    .is_ok_and(|end| end == period.ends_at)
                        }
                        _ => false,
                    }
                })
        })
    })
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
    if tx.authorized_paykit_apps(counterparty).is_some_and(|apps| {
        apps.get(proposal_app_id)
            .is_some_and(|capabilities| capabilities.payment_requests)
    }) {
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

fn require_outgoing_payment_app(
    tx: &dyn StorageTransaction,
    app_id: &paykit_lib::PaykitAppId,
    action: &str,
) -> Result<()> {
    if tx
        .paykit_app_capabilities(app_id)
        .is_some_and(|capabilities| capabilities.outgoing_payments)
    {
        Ok(())
    } else {
        Err(PaykitSdkError::Policy {
            context: format!("cannot {action}: Paykit app '{app_id}' cannot execute payments"),
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
    enqueue_payment_request_event(storage, counterparty, app_id, &event, now).await
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
