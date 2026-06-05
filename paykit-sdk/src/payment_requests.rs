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
    parse_payment_request_event_message, serialize_payment_request_event, PaymentProof,
    PaymentRequest, PaymentRequestAcceptance, PaymentRequestCancellation, PaymentRequestEvent,
    PaymentRequestRejection, PrivateApplicationMessage,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    outbound_private::enqueue_private_message,
    outbound_private::OutboundPrivateMessageStatus,
    private_stream::payload_hash,
    storage::{
        EventDedupRecord, OutboundPrivateMessageRecord, PrivateStreamItemRecord, StorageAdapter,
    },
    PubkyPublicKey, Result,
};

/// Local role for one Payment Request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentRequestLocalRole {
    /// Local identity is expected to pay.
    Payer,
    /// Local identity expects to receive payment.
    Payee,
}

/// SDK-derived Payment Request lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentRequestLifecycleState {
    /// Proposal is known locally and remains actionable.
    Proposed,
    /// Proposal is past its expiry.
    ProposalExpired,
    /// Request was accepted.
    Accepted,
    /// Request was rejected.
    Rejected,
    /// Request was canceled.
    Canceled,
    /// A one-time Payment Proof was submitted.
    ProofSubmitted,
    /// Recurring request is accepted and active.
    ActiveRecurring,
    /// Event ordering, dedupe, or lifecycle validation found an invalid state.
    InvalidConflict,
}

/// Durable Payment Amount fields copied from Payment Request terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequestAmountRecord {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
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
    /// Optional RFC3339 UTC timestamp using `Z`.
    pub ends_at: Option<String>,
}

/// Immutable Payment Request terms copied into an SDK record.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequestTermsRecord {
    /// Requested amount.
    pub amount: PaymentRequestAmountRecord,
    /// Payee-provided payment correlation value.
    pub payment_reference: String,
    /// Proposal expiry before acceptance.
    pub proposal_expires_at: Option<String>,
    /// Optional recurrence.
    pub recurrence: Option<PaymentRequestRecurrenceRecord>,
    /// Accepted Payment Endpoint Identifiers.
    pub accepted_payment_endpoint_identifiers: Vec<String>,
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
            amount: PaymentRequestAmountRecord {
                value: terms.amount.value.clone(),
                asset: terms.amount.asset.clone(),
            },
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
            metadata: terms.metadata.clone(),
        }
    }
}

/// Billing Period fields copied from a Payment Proof.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequestBillingPeriodRecord {
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub ends_at: String,
}

impl From<&paykit_lib::BillingPeriod> for PaymentRequestBillingPeriodRecord {
    fn from(period: &paykit_lib::BillingPeriod) -> Self {
        Self {
            starts_at: period.starts_at.clone(),
            ends_at: period.ends_at.clone(),
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
    pub billing_period: Option<PaymentRequestBillingPeriodRecord>,
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
    /// Derived lifecycle state.
    pub state: PaymentRequestLifecycleState,
    /// Stream item id of the proposal event.
    pub proposal_stream_item_id: Option<u64>,
    /// Outbound message id of the proposal event.
    pub proposal_outbound_message_id: Option<u64>,
    /// Local outbound delivery status for the proposal event.
    pub proposal_outbound_status: Option<OutboundPrivateMessageStatus>,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
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
        self.last_event_at = Some(item.received_at);
    }

    fn touch_outbound(&mut self, message: &OutboundPrivateMessageRecord) {
        self.last_outbound_message_id = Some(message.outbound_message_id);
        self.last_outbound_status = Some(message.status.clone());
        self.last_event_at = Some(message.created_at);
    }

    fn mark_invalid(&mut self, item: &PrivateStreamItemRecord, reason: impl Into<String>) {
        self.state = PaymentRequestLifecycleState::InvalidConflict;
        if self.invalid_reason.is_none() {
            self.invalid_reason = Some(reason.into());
        }
        self.touch(item);
    }
}

/// Derive received Payment Request lifecycle records for one counterparty.
///
/// Records are derived from the persisted inbound private stream and returned
/// newest-first by the last applied stream item. Malformed recognized Payment
/// Request events without a valid `payment_request_id` remain available in the
/// raw private stream log but cannot be attached to a request-scoped record.
pub(crate) async fn received_payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    let (items, dedupe_records) = storage
        .transaction(|tx| {
            let items = tx.private_stream_items(counterparty);
            let mut dedupe_records = HashMap::new();
            for item in &items {
                let Some(message) = payment_request_message_from_item(item) else {
                    continue;
                };
                let Some(parsed) = parse_payment_request_event_message(&message) else {
                    continue;
                };
                let Some(event_id) = parsed.event_id() else {
                    continue;
                };
                if let Some(record) = tx.event_dedup_record(counterparty, event_id.as_str()) {
                    dedupe_records.insert(event_id.as_str().to_owned(), record);
                }
            }
            Ok((items, dedupe_records))
        })
        .await?;
    derive_received_payment_request_records(counterparty.clone(), items, dedupe_records, now)
}

/// Derive local Payment Request records for one counterparty.
///
/// Records merge received Payment Request events from the private stream with
/// local outbound Payment Request events from the outbound private-message log.
/// Returned records are newest-first by local record time.
pub(crate) async fn payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    let (items, outbound, dedupe_records) = storage
        .transaction(|tx| {
            let items = tx.private_stream_items(counterparty);
            let outbound = tx.outbound_private_messages(counterparty);
            let mut dedupe_records = HashMap::new();
            for item in &items {
                let Some(message) = payment_request_message_from_item(item) else {
                    continue;
                };
                let Some(parsed) = parse_payment_request_event_message(&message) else {
                    continue;
                };
                let Some(event_id) = parsed.event_id() else {
                    continue;
                };
                if let Some(record) = tx.event_dedup_record(counterparty, event_id.as_str()) {
                    dedupe_records.insert(event_id.as_str().to_owned(), record);
                }
            }
            Ok((items, outbound, dedupe_records))
        })
        .await?;
    derive_payment_request_records(counterparty.clone(), items, outbound, dedupe_records, now)
}

/// Queue one raw Payment Request protocol event for outbound delivery.
///
/// The exact canonical JSON payload is serialized before it is stored, so retry
/// workers can resend the same Event ID and payload.
pub(crate) async fn enqueue_payment_request_event<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentRequestEvent,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let raw_json = serialize_payment_request_event(event)?;
    enqueue_private_message(storage, counterparty, raw_json, now).await
}

/// Queue a raw Payment Request proposal for outbound delivery.
pub(crate) async fn enqueue_payment_request<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentRequest,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Request(event.clone());
    enqueue_payment_request_event(storage, counterparty, &event, now).await
}

/// Queue a raw Payment Request acceptance for outbound delivery.
pub(crate) async fn enqueue_payment_request_acceptance<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentRequestAcceptance,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Acceptance(event.clone());
    enqueue_payment_request_event(storage, counterparty, &event, now).await
}

/// Queue a raw Payment Request rejection for outbound delivery.
pub(crate) async fn enqueue_payment_request_rejection<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentRequestRejection,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Rejection(event.clone());
    enqueue_payment_request_event(storage, counterparty, &event, now).await
}

/// Queue a raw Payment Request cancellation for outbound delivery.
pub(crate) async fn enqueue_payment_request_cancellation<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentRequestCancellation,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Cancellation(event.clone());
    enqueue_payment_request_event(storage, counterparty, &event, now).await
}

/// Queue a raw Payment Proof for outbound delivery.
pub(crate) async fn enqueue_payment_proof<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    event: &PaymentProof,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let event = PaymentRequestEvent::Proof(event.clone());
    enqueue_payment_request_event(storage, counterparty, &event, now).await
}

fn derive_received_payment_request_records(
    counterparty: PubkyPublicKey,
    mut items: Vec<PrivateStreamItemRecord>,
    dedupe_records: HashMap<String, EventDedupRecord>,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>> {
    items.sort_by_key(|item| item.stream_item_id);
    let mut records = HashMap::<String, PaymentRequestRecord>::new();

    for item in items {
        let Some(message) = payment_request_message_from_item(&item) else {
            continue;
        };
        let Some(parsed) = parse_payment_request_event_message(&message) else {
            continue;
        };
        let payment_request_id = parsed.payment_request_id().map(|id| id.as_str().to_owned());
        let event_id = parsed.event_id().map(|id| id.as_str().to_owned());

        if let Some(event_id) = event_id.as_deref() {
            if let Some(dedupe) = dedupe_records.get(event_id) {
                if dedupe
                    .duplicate_stream_item_ids
                    .contains(&item.stream_item_id)
                {
                    continue;
                }
                if dedupe
                    .conflicting_stream_item_ids
                    .contains(&item.stream_item_id)
                    || (dedupe.first_stream_item_id == item.stream_item_id
                        && !dedupe.conflicting_stream_item_ids.is_empty())
                {
                    if let Some(payment_request_id) = payment_request_id {
                        record_for(&mut records, &counterparty, payment_request_id)
                            .mark_invalid(&item, "Event ID reused with different payload");
                    }
                    continue;
                }
            }
        }

        let Some(payment_request_id) = payment_request_id else {
            continue;
        };
        let record = record_for(&mut records, &counterparty, payment_request_id);
        if !parsed.is_valid() {
            record.mark_invalid(
                &item,
                parsed
                    .validation_error()
                    .unwrap_or("malformed Payment Request event"),
            );
            continue;
        }
        let event = parsed.parsed_event().expect("valid event");
        apply_event(record, event, &item, now);
    }

    for record in records.values_mut() {
        if record.state == PaymentRequestLifecycleState::Proposed && proposal_expired(record, now) {
            record.state = PaymentRequestLifecycleState::ProposalExpired;
        }
    }

    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by_key(|record| {
        Reverse(
            record
                .last_stream_item_id
                .or(record.proposal_stream_item_id),
        )
    });
    Ok(records)
}

#[derive(Clone)]
enum StoredPaymentRequestEvent {
    Received {
        item: PrivateStreamItemRecord,
        event: PaymentRequestEvent,
    },
    Outbound {
        message: OutboundPrivateMessageRecord,
        event: PaymentRequestEvent,
    },
}

impl StoredPaymentRequestEvent {
    fn record_time(&self) -> DateTime<Utc> {
        match self {
            Self::Received { item, .. } => item.received_at,
            Self::Outbound { message, .. } => message.created_at,
        }
    }

    fn kind_order(&self) -> u8 {
        match self.event() {
            PaymentRequestEvent::Request(_) => 0,
            PaymentRequestEvent::Acceptance(_) | PaymentRequestEvent::Rejection(_) => 1,
            PaymentRequestEvent::Cancellation(_) => 2,
            PaymentRequestEvent::Proof(_) => 3,
        }
    }

    fn source_order(&self) -> u64 {
        match self {
            Self::Received { item, .. } => item.stream_item_id,
            Self::Outbound { message, .. } => message.outbound_message_id,
        }
    }

    fn source_rank(&self) -> u8 {
        match self {
            Self::Received { .. } => 0,
            Self::Outbound { .. } => 1,
        }
    }

    fn payload_hash(&self) -> String {
        match self {
            Self::Received { item, .. } => payload_hash(&item.raw_json),
            Self::Outbound { message, .. } => payload_hash(&message.raw_json),
        }
    }

    fn event(&self) -> &PaymentRequestEvent {
        match self {
            Self::Received { event, .. } | Self::Outbound { event, .. } => event,
        }
    }

    fn payment_request_id(&self) -> String {
        self.event().payment_request_id().as_str().to_owned()
    }

    fn event_id(&self) -> String {
        self.event().event_id().as_str().to_owned()
    }
}

fn derive_payment_request_records(
    counterparty: PubkyPublicKey,
    mut items: Vec<PrivateStreamItemRecord>,
    outbound: Vec<OutboundPrivateMessageRecord>,
    dedupe_records: HashMap<String, EventDedupRecord>,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>> {
    let mut records = HashMap::<String, PaymentRequestRecord>::new();
    let mut events = Vec::<StoredPaymentRequestEvent>::new();
    let mut pre_invalid_request_ids = HashSet::<String>::new();
    let mut tainted_event_seeds = Vec::<TaintedEventSeed>::new();

    items.sort_by_key(|item| item.stream_item_id);
    for item in items {
        let Some(message) = payment_request_message_from_item(&item) else {
            continue;
        };
        let Some(parsed) = parse_payment_request_event_message(&message) else {
            continue;
        };
        let payment_request_id = parsed.payment_request_id().map(|id| id.as_str().to_owned());
        let event_id = parsed.event_id().map(|id| id.as_str().to_owned());

        if let Some(event_id) = event_id.as_deref() {
            if let Some(dedupe) = dedupe_records.get(event_id) {
                if dedupe
                    .duplicate_stream_item_ids
                    .contains(&item.stream_item_id)
                {
                    continue;
                }
                if dedupe
                    .conflicting_stream_item_ids
                    .contains(&item.stream_item_id)
                    || (dedupe.first_stream_item_id == item.stream_item_id
                        && !dedupe.conflicting_stream_item_ids.is_empty())
                {
                    if let Some(payment_request_id) = payment_request_id {
                        tainted_event_seeds.push(TaintedEventSeed {
                            event_id: event_id.to_owned(),
                            payment_request_id: payment_request_id.clone(),
                            payload_hash: payload_hash(&item.raw_json),
                            source_rank: 0,
                        });
                        pre_invalid_request_ids.insert(payment_request_id.clone());
                        record_for(&mut records, &counterparty, payment_request_id)
                            .mark_invalid(&item, "Event ID reused with different payload");
                    } else {
                        tainted_event_seeds.push(TaintedEventSeed {
                            event_id: event_id.to_owned(),
                            payment_request_id: String::new(),
                            payload_hash: payload_hash(&item.raw_json),
                            source_rank: 0,
                        });
                    }
                    continue;
                }
            }
        }

        let Some(payment_request_id) = payment_request_id else {
            if let Some(event_id) = event_id {
                tainted_event_seeds.push(TaintedEventSeed {
                    event_id,
                    payment_request_id: String::new(),
                    payload_hash: payload_hash(&item.raw_json),
                    source_rank: 0,
                });
            }
            continue;
        };
        if !parsed.is_valid() {
            if let Some(event_id) = event_id {
                tainted_event_seeds.push(TaintedEventSeed {
                    event_id,
                    payment_request_id: payment_request_id.clone(),
                    payload_hash: payload_hash(&item.raw_json),
                    source_rank: 0,
                });
            }
            pre_invalid_request_ids.insert(payment_request_id.clone());
            record_for(&mut records, &counterparty, payment_request_id).mark_invalid(
                &item,
                parsed
                    .validation_error()
                    .unwrap_or("malformed Payment Request event"),
            );
            continue;
        }
        events.push(StoredPaymentRequestEvent::Received {
            item,
            event: parsed.parsed_event().expect("valid event").clone(),
        });
    }

    for message in outbound {
        if matches!(
            message.status,
            OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::Superseded
        ) {
            continue;
        }
        let Some(parsed) = parse_payment_request_event_message(&PrivateApplicationMessage {
            version: Some(1),
            kind: Some(message.kind.clone()),
            raw_json: message.raw_json.clone(),
        }) else {
            continue;
        };
        let Some(event) = parsed.parsed_event() else {
            continue;
        };
        let stored = StoredPaymentRequestEvent::Outbound {
            message,
            event: event.clone(),
        };
        if dedupe_records
            .get(event.event_id().as_str())
            .is_some_and(|dedupe| !dedupe.conflicting_stream_item_ids.is_empty())
        {
            let record = record_for(&mut records, &counterparty, stored.payment_request_id());
            mark_invalid_stored(record, &stored, "Event ID reused with different payload");
            continue;
        }
        events.push(stored);
    }

    events.sort_by(compare_stored_events);
    let tainted_events = events
        .iter()
        .filter(|event| pre_invalid_request_ids.contains(&event.payment_request_id()))
        .cloned()
        .collect::<Vec<_>>();
    events.retain(|event| !pre_invalid_request_ids.contains(&event.payment_request_id()));
    events = dedupe_stored_events(
        &counterparty,
        &mut records,
        events,
        tainted_events,
        tainted_event_seeds,
    );
    for event in events {
        let payment_request_id = event.payment_request_id();
        let record = record_for(&mut records, &counterparty, payment_request_id);
        apply_stored_event(record, &event);
    }

    for record in records.values_mut() {
        if record.state == PaymentRequestLifecycleState::Proposed && proposal_expired(record, now) {
            record.state = PaymentRequestLifecycleState::ProposalExpired;
        }
    }

    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by_key(|record| {
        Reverse((
            record.last_event_at,
            record
                .last_outbound_message_id
                .or(record.last_stream_item_id),
        ))
    });
    Ok(records)
}

fn compare_stored_events(a: &StoredPaymentRequestEvent, b: &StoredPaymentRequestEvent) -> Ordering {
    a.record_time().cmp(&b.record_time()).then_with(|| {
        if a.source_rank() == b.source_rank() {
            a.source_order().cmp(&b.source_order())
        } else {
            a.kind_order()
                .cmp(&b.kind_order())
                .then_with(|| a.source_rank().cmp(&b.source_rank()))
        }
    })
}

#[derive(Clone)]
struct StoredEventDedupeEntry {
    payload_hash: String,
    event: Option<StoredPaymentRequestEvent>,
    payment_request_id: String,
    source_rank: u8,
    tainted: bool,
}

struct TaintedEventSeed {
    event_id: String,
    payment_request_id: String,
    payload_hash: String,
    source_rank: u8,
}

fn dedupe_stored_events(
    counterparty: &PubkyPublicKey,
    records: &mut HashMap<String, PaymentRequestRecord>,
    events: Vec<StoredPaymentRequestEvent>,
    tainted_events: Vec<StoredPaymentRequestEvent>,
    tainted_event_seeds: Vec<TaintedEventSeed>,
) -> Vec<StoredPaymentRequestEvent> {
    let mut first_by_event_id = HashMap::<String, StoredEventDedupeEntry>::new();
    let mut conflicted_event_ids = HashSet::<String>::new();
    let mut conflicted_request_ids = HashSet::<String>::new();
    let mut deduped = Vec::new();

    for event in tainted_events {
        first_by_event_id
            .entry(event.event_id())
            .or_insert_with(|| {
                let payload_hash = event.payload_hash();
                let payment_request_id = event.payment_request_id();
                let source_rank = event.source_rank();
                StoredEventDedupeEntry {
                    payload_hash,
                    event: Some(event),
                    payment_request_id,
                    source_rank,
                    tainted: true,
                }
            });
    }
    for seed in tainted_event_seeds {
        first_by_event_id
            .entry(seed.event_id)
            .or_insert_with(|| StoredEventDedupeEntry {
                payload_hash: seed.payload_hash,
                event: None,
                payment_request_id: seed.payment_request_id,
                source_rank: seed.source_rank,
                tainted: true,
            });
    }

    for event in events {
        let event_id = event.event_id();
        let current_hash = event.payload_hash();
        if let Some(first) = first_by_event_id.get(&event_id) {
            if first.payload_hash == current_hash
                && !first.tainted
                && first.source_rank == event.source_rank()
            {
                continue;
            }
            if !first.tainted {
                let first_event = first.event.as_ref().expect("valid stored event");
                let first_record =
                    record_for(records, counterparty, first.payment_request_id.clone());
                mark_invalid_stored(
                    first_record,
                    first_event,
                    "Event ID reused with different payload",
                );
                conflicted_request_ids.insert(first.payment_request_id.clone());
            }
            let current_record = record_for(records, counterparty, event.payment_request_id());
            mark_invalid_stored(
                current_record,
                &event,
                "Event ID reused with different payload",
            );
            conflicted_event_ids.insert(event_id);
            conflicted_request_ids.insert(event.payment_request_id());
            continue;
        }

        first_by_event_id.insert(
            event_id,
            StoredEventDedupeEntry {
                payload_hash: current_hash,
                payment_request_id: event.payment_request_id(),
                source_rank: event.source_rank(),
                event: Some(event.clone()),
                tainted: false,
            },
        );
        deduped.push(event);
    }

    deduped.retain(|event| {
        !conflicted_event_ids.contains(&event.event_id())
            && !conflicted_request_ids.contains(&event.payment_request_id())
    });
    deduped
}

fn record_for<'a>(
    records: &'a mut HashMap<String, PaymentRequestRecord>,
    counterparty: &PubkyPublicKey,
    payment_request_id: String,
) -> &'a mut PaymentRequestRecord {
    records
        .entry(payment_request_id.clone())
        .or_insert_with(|| PaymentRequestRecord::new(counterparty.clone(), payment_request_id))
}

fn payment_request_message_from_item(
    item: &PrivateStreamItemRecord,
) -> Option<PrivateApplicationMessage> {
    let kind = item.known_paykit_kind.as_deref()?;
    if !is_payment_request_kind(kind) {
        return None;
    }
    Some(PrivateApplicationMessage {
        version: item
            .parsed_version
            .and_then(|version| u8::try_from(version).ok()),
        kind: item.parsed_kind.clone(),
        raw_json: item.raw_json.clone(),
    })
}

fn is_payment_request_kind(kind: &str) -> bool {
    matches!(
        kind,
        "paykit.payment_request"
            | "paykit.payment_request_acceptance"
            | "paykit.payment_request_rejection"
            | "paykit.payment_request_cancellation"
            | "paykit.payment_proof"
    )
}

fn apply_event(
    record: &mut PaymentRequestRecord,
    event: &PaymentRequestEvent,
    item: &PrivateStreamItemRecord,
    now: DateTime<Utc>,
) {
    if record.state == PaymentRequestLifecycleState::InvalidConflict
        && record.last_stream_item_id.is_some()
    {
        record.touch(item);
        return;
    }

    match event {
        PaymentRequestEvent::Request(request) => apply_request(record, request, item),
        PaymentRequestEvent::Acceptance(_) => {
            if record.terms.is_none() {
                record.mark_invalid(item, "Payment Request acceptance arrived before proposal");
                return;
            }
            record.mark_invalid(
                item,
                "Payment Request acceptance is an outbound payer event for a received proposal",
            );
        }
        PaymentRequestEvent::Rejection(_) => {
            if record.terms.is_none() {
                record.mark_invalid(item, "Payment Request rejection arrived before proposal");
                return;
            }
            record.mark_invalid(
                item,
                "Payment Request rejection is an outbound payer event for a received proposal",
            );
        }
        PaymentRequestEvent::Cancellation(cancellation) => {
            if record.terms.is_none() {
                record.mark_invalid(item, "Payment Request cancellation arrived before proposal");
                return;
            }
            if matches!(
                record.state,
                PaymentRequestLifecycleState::Canceled
                    | PaymentRequestLifecycleState::InvalidConflict
            ) {
                record.mark_invalid(
                    item,
                    "Payment Request cancellation arrived after terminal state",
                );
                return;
            }
            record.canceled_event_id = Some(cancellation.event_id.as_str().to_owned());
            record.state = PaymentRequestLifecycleState::Canceled;
            record.touch(item);
        }
        PaymentRequestEvent::Proof(_) => {
            if record.terms.is_none() {
                record.mark_invalid(item, "Payment Proof arrived before acceptance");
                return;
            }
            record.mark_invalid(
                item,
                "Payment Proof is an outbound payer event for a received proposal",
            );
        }
    }

    if record.state == PaymentRequestLifecycleState::Proposed && proposal_expired(record, now) {
        record.state = PaymentRequestLifecycleState::ProposalExpired;
    }
}

fn apply_request(
    record: &mut PaymentRequestRecord,
    request: &PaymentRequest,
    item: &PrivateStreamItemRecord,
) {
    if record.state == PaymentRequestLifecycleState::InvalidConflict
        && record.last_stream_item_id.is_some()
    {
        record.touch(item);
        return;
    }
    if record.terms.is_some() {
        record.mark_invalid(
            item,
            "multiple Payment Request proposals used the same Payment Request ID",
        );
        return;
    }
    record.local_role = Some(PaymentRequestLocalRole::Payer);
    record.state = PaymentRequestLifecycleState::Proposed;
    record.proposal_stream_item_id = Some(item.stream_item_id);
    record.proposal_event_id = Some(request.event_id.as_str().to_owned());
    record.terms = Some(PaymentRequestTermsRecord::from(&request.request));
    record.touch(item);
}

fn apply_stored_event(record: &mut PaymentRequestRecord, stored: &StoredPaymentRequestEvent) {
    if record.state == PaymentRequestLifecycleState::InvalidConflict
        && (record.last_stream_item_id.is_some() || record.last_outbound_message_id.is_some())
    {
        touch_stored(record, stored);
        return;
    }

    match stored.event() {
        PaymentRequestEvent::Request(request) => apply_stored_request(record, stored, request),
        PaymentRequestEvent::Acceptance(acceptance) => {
            if !payer_action_source_allowed(record, stored) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request acceptance came from the wrong side",
                );
                return;
            }
            if !has_terms(
                record,
                stored,
                "Payment Request acceptance arrived before proposal",
            ) {
                return;
            }
            if proposal_expired(record, stored.record_time()) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request acceptance arrived after proposal expiry",
                );
                return;
            }
            if !matches!(record.state, PaymentRequestLifecycleState::Proposed) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request acceptance arrived after transition",
                );
                return;
            }
            record.accepted_event_id = Some(acceptance.event_id.as_str().to_owned());
            record.accepted_outbound_status = outbound_status(stored);
            record.state = if record
                .terms
                .as_ref()
                .and_then(|terms| terms.recurrence.as_ref())
                .is_some()
            {
                PaymentRequestLifecycleState::ActiveRecurring
            } else {
                PaymentRequestLifecycleState::Accepted
            };
            touch_stored(record, stored);
        }
        PaymentRequestEvent::Rejection(rejection) => {
            if !payer_action_source_allowed(record, stored) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request rejection came from the wrong side",
                );
                return;
            }
            if !has_terms(
                record,
                stored,
                "Payment Request rejection arrived before proposal",
            ) {
                return;
            }
            if proposal_expired(record, stored.record_time()) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request rejection arrived after proposal expiry",
                );
                return;
            }
            if !matches!(record.state, PaymentRequestLifecycleState::Proposed) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request rejection arrived after transition",
                );
                return;
            }
            record.rejected_event_id = Some(rejection.event_id.as_str().to_owned());
            record.rejected_outbound_status = outbound_status(stored);
            record.state = PaymentRequestLifecycleState::Rejected;
            touch_stored(record, stored);
        }
        PaymentRequestEvent::Cancellation(cancellation) => {
            if !has_terms(
                record,
                stored,
                "Payment Request cancellation arrived before proposal",
            ) {
                return;
            }
            if matches!(
                record.state,
                PaymentRequestLifecycleState::Rejected
                    | PaymentRequestLifecycleState::Canceled
                    | PaymentRequestLifecycleState::InvalidConflict
            ) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request cancellation arrived after terminal state",
                );
                return;
            }
            record.canceled_event_id = Some(cancellation.event_id.as_str().to_owned());
            record.canceled_outbound_status = outbound_status(stored);
            record.state = PaymentRequestLifecycleState::Canceled;
            touch_stored(record, stored);
        }
        PaymentRequestEvent::Proof(proof) => {
            if !payer_action_source_allowed(record, stored) {
                mark_invalid_stored(record, stored, "Payment Proof came from the wrong side");
                return;
            }
            if !matches!(
                record.state,
                PaymentRequestLifecycleState::Accepted
                    | PaymentRequestLifecycleState::ActiveRecurring
            ) {
                mark_invalid_stored(record, stored, "Payment Proof arrived before acceptance");
                return;
            }
            let Some(request) = request_from_record(record) else {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Proof cannot be correlated without proposal",
                );
                return;
            };
            if let Err(err) = proof.validate_for_request(&request) {
                mark_invalid_stored(record, stored, err.to_string());
                return;
            }
            record.payment_proofs.push(PaymentProofRecord {
                event_id: proof.event_id.as_str().to_owned(),
                outbound_message_id: match stored {
                    StoredPaymentRequestEvent::Outbound { message, .. } => {
                        Some(message.outbound_message_id)
                    }
                    StoredPaymentRequestEvent::Received { .. } => None,
                },
                outbound_status: outbound_status(stored),
                stream_item_id: match stored {
                    StoredPaymentRequestEvent::Received { item, .. } => Some(item.stream_item_id),
                    StoredPaymentRequestEvent::Outbound { .. } => None,
                },
                payment_reference: proof.payment_reference.as_str().to_owned(),
                billing_period: proof
                    .billing_period
                    .as_ref()
                    .map(PaymentRequestBillingPeriodRecord::from),
                payment_endpoint_identifier: proof.payment_endpoint_identifier.as_str().to_owned(),
                proof: proof.proof.clone(),
                recorded_at: stored.record_time(),
            });
            record.state = if record
                .terms
                .as_ref()
                .and_then(|terms| terms.recurrence.as_ref())
                .is_some()
            {
                PaymentRequestLifecycleState::ActiveRecurring
            } else {
                PaymentRequestLifecycleState::ProofSubmitted
            };
            touch_stored(record, stored);
        }
    }
}

fn apply_stored_request(
    record: &mut PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
    request: &PaymentRequest,
) {
    if record.state == PaymentRequestLifecycleState::InvalidConflict
        && (record.last_stream_item_id.is_some() || record.last_outbound_message_id.is_some())
    {
        touch_stored(record, stored);
        return;
    }
    if record.terms.is_some() {
        mark_invalid_stored(
            record,
            stored,
            "multiple Payment Request proposals used the same Payment Request ID",
        );
        return;
    }
    record.local_role = Some(match stored {
        StoredPaymentRequestEvent::Received { .. } => PaymentRequestLocalRole::Payer,
        StoredPaymentRequestEvent::Outbound { .. } => PaymentRequestLocalRole::Payee,
    });
    record.state = PaymentRequestLifecycleState::Proposed;
    match stored {
        StoredPaymentRequestEvent::Received { item, .. } => {
            record.proposal_stream_item_id = Some(item.stream_item_id);
        }
        StoredPaymentRequestEvent::Outbound { message, .. } => {
            record.proposal_outbound_message_id = Some(message.outbound_message_id);
            record.proposal_outbound_status = Some(message.status.clone());
        }
    }
    record.proposal_event_id = Some(request.event_id.as_str().to_owned());
    record.terms = Some(PaymentRequestTermsRecord::from(&request.request));
    touch_stored(record, stored);
}

fn has_terms(
    record: &mut PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
    reason: &str,
) -> bool {
    if record.terms.is_some() {
        true
    } else {
        mark_invalid_stored(record, stored, reason);
        false
    }
}

fn payer_action_source_allowed(
    record: &PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
) -> bool {
    matches!(
        (record.local_role, stored),
        (
            Some(PaymentRequestLocalRole::Payer),
            StoredPaymentRequestEvent::Outbound { .. }
        ) | (
            Some(PaymentRequestLocalRole::Payee),
            StoredPaymentRequestEvent::Received { .. }
        )
    )
}

fn touch_stored(record: &mut PaymentRequestRecord, stored: &StoredPaymentRequestEvent) {
    match stored {
        StoredPaymentRequestEvent::Received { item, .. } => record.touch(item),
        StoredPaymentRequestEvent::Outbound { message, .. } => record.touch_outbound(message),
    }
}

fn outbound_status(stored: &StoredPaymentRequestEvent) -> Option<OutboundPrivateMessageStatus> {
    match stored {
        StoredPaymentRequestEvent::Outbound { message, .. } => Some(message.status.clone()),
        StoredPaymentRequestEvent::Received { .. } => None,
    }
}

fn mark_invalid_stored(
    record: &mut PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
    reason: impl Into<String>,
) {
    record.state = PaymentRequestLifecycleState::InvalidConflict;
    if record.invalid_reason.is_none() {
        record.invalid_reason = Some(reason.into());
    }
    touch_stored(record, stored);
}

pub(crate) fn request_from_record(record: &PaymentRequestRecord) -> Option<PaymentRequest> {
    let terms = record.terms.as_ref()?;
    let proposal_event_id = record.proposal_event_id.as_ref()?;
    let recurrence = if let Some(recurrence) = &terms.recurrence {
        Some(paykit_lib::Recurrence {
            every: recurrence.every,
            unit: parse_recurrence_unit(&recurrence.unit)?,
            starts_at: recurrence.starts_at.clone(),
            anchor: recurrence.anchor.clone(),
            ends_at: recurrence.ends_at.clone(),
        })
    } else {
        None
    };
    Some(PaymentRequest::new(
        paykit_lib::EventId::new(proposal_event_id).ok()?,
        paykit_lib::PaymentRequestId::new(record.payment_request_id.clone()).ok()?,
        paykit_lib::PaymentRequestTerms {
            amount: paykit_lib::PaymentAmount::new(
                terms.amount.value.clone(),
                terms.amount.asset.clone(),
            )
            .ok()?,
            payment_reference: paykit_lib::PaymentReference::new(terms.payment_reference.clone())
                .ok()?,
            proposal_expires_at: terms.proposal_expires_at.clone(),
            recurrence,
            accepted_payment_endpoint_identifiers: terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .map(|identifier| paykit_lib::PaymentEndpointIdentifier::new(identifier).ok())
                .collect::<Option<Vec<_>>>()?,
            metadata: terms.metadata.clone(),
        },
    ))
}

fn proposal_expired(record: &PaymentRequestRecord, now: DateTime<Utc>) -> bool {
    record
        .terms
        .as_ref()
        .and_then(|terms| terms.proposal_expires_at.as_ref())
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires_at| now >= expires_at.with_timezone(&Utc))
        .unwrap_or(false)
}

fn recurrence_unit_to_str(unit: paykit_lib::RecurrenceUnit) -> &'static str {
    match unit {
        paykit_lib::RecurrenceUnit::Minute => "minute",
        paykit_lib::RecurrenceUnit::Hour => "hour",
        paykit_lib::RecurrenceUnit::Day => "day",
        paykit_lib::RecurrenceUnit::Week => "week",
        paykit_lib::RecurrenceUnit::Month => "month",
        paykit_lib::RecurrenceUnit::Year => "year",
    }
}

fn parse_recurrence_unit(unit: &str) -> Option<paykit_lib::RecurrenceUnit> {
    match unit {
        "minute" => Some(paykit_lib::RecurrenceUnit::Minute),
        "hour" => Some(paykit_lib::RecurrenceUnit::Hour),
        "day" => Some(paykit_lib::RecurrenceUnit::Day),
        "week" => Some(paykit_lib::RecurrenceUnit::Week),
        "month" => Some(paykit_lib::RecurrenceUnit::Month),
        "year" => Some(paykit_lib::RecurrenceUnit::Year),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        outbound_private::{
            enqueue_private_message as enqueue_untyped_private_message,
            queued_outbound_private_messages,
        },
        private_stream::persist_private_stream_batch,
        storage::InMemoryStorage,
    };

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn private_message(raw_json: String) -> PrivateApplicationMessage {
        let value: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
        PrivateApplicationMessage {
            version: value
                .get("version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u8::try_from(version).ok()),
            kind: value
                .get("kind")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            raw_json,
        }
    }

    fn parsed_event(raw_json: String) -> PaymentRequestEvent {
        parse_payment_request_event_message(&private_message(raw_json))
            .unwrap()
            .parsed_event()
            .unwrap()
            .clone()
    }

    fn request_raw(
        event_id: &str,
        request_id: &str,
        reference: &str,
        expires_at: Option<&str>,
        recurrence: Option<&str>,
    ) -> String {
        let expiry = expires_at
            .map(|value| format!(r#""{value}""#))
            .unwrap_or_else(|| "null".into());
        let recurrence = recurrence.unwrap_or("null");
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"{request_id}","request":{{"amount":{{"value":"0.001","asset":"btc"}},"payment_reference":"{reference}","proposal_expires_at":{expiry},"recurrence":{recurrence},"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{"note":"private"}}}}}}"#
        )
    }

    fn acceptance_raw(event_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
        )
    }

    fn rejection_raw(event_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_rejection","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
        )
    }

    fn cancellation_raw(event_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
        )
    }

    fn malformed_cancellation_raw(event_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{event_id}","payment_request_id":"{request_id}","reason":null}}"#
        )
    }

    fn malformed_missing_request_id_raw(event_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"{event_id}"}}"#
        )
    }

    fn proof_raw(event_id: &str, request_id: &str, reference: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_proof","event_id":"{event_id}","payment_request_id":"{request_id}","payment_reference":"{reference}","billing_period":null,"payment_endpoint_identifier":"btc-lightning-bolt11","proof":{{"txid":"secret"}}}}"#
        )
    }

    async fn persist_messages(
        storage: &InMemoryStorage,
        counterparty: PubkyPublicKey,
        messages: Vec<String>,
    ) {
        persist_private_stream_batch(
            storage,
            counterparty,
            messages.into_iter().map(private_message).collect(),
            None,
            timestamp(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_enqueue_payment_request_event_stores_canonical_payload() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let event = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "invoice-2026-0001",
            None,
            None,
        ));

        let record =
            enqueue_payment_request_event(&storage, counterparty.clone(), &event, timestamp())
                .await
                .unwrap();
        let queued = queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();

        assert_eq!(queued, vec![record.clone()]);
        assert_eq!(record.kind, "paykit.payment_request");
        assert_eq!(
            record.raw_json,
            serialize_payment_request_event(&event).unwrap()
        );
    }

    #[tokio::test]
    async fn test_enqueue_payment_request_acceptance_sets_kind() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let PaymentRequestEvent::Acceptance(event) = parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
        )) else {
            panic!("expected acceptance event");
        };

        let record =
            enqueue_payment_request_acceptance(&storage, counterparty, &event, timestamp())
                .await
                .unwrap();

        assert_eq!(record.kind, "paykit.payment_request_acceptance");
    }

    #[tokio::test]
    async fn test_enqueue_payment_request_rejects_invalid_terms() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let PaymentRequestEvent::Request(mut event) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "invoice-2026-0001",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        event.request.accepted_payment_endpoint_identifiers.clear();

        let err = enqueue_payment_request(&storage, counterparty.clone(), &event, timestamp())
            .await
            .unwrap_err();
        let queued = queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();

        assert!(matches!(err, crate::PaykitSdkError::Protocol(_)));
        assert!(queued.is_empty());
    }

    #[tokio::test]
    async fn test_payment_request_records_merge_outbound_acceptance() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            )],
        )
        .await;
        let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )) else {
            panic!("expected acceptance event");
        };
        enqueue_payment_request_acceptance(
            &storage,
            counterparty.clone(),
            &acceptance,
            timestamp(),
        )
        .await
        .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payer));
        assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
        assert_eq!(
            records[0].accepted_event_id.as_deref(),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
        );
        assert_eq!(
            records[0].accepted_outbound_status,
            Some(OutboundPrivateMessageStatus::Pending)
        );
        assert_eq!(
            records[0].last_outbound_status,
            Some(OutboundPrivateMessageStatus::Pending)
        );
    }

    #[tokio::test]
    async fn test_payment_request_records_merge_sent_request_acceptance() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![acceptance_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                request_id,
            )],
        )
        .await;

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records[0].local_role, Some(PaymentRequestLocalRole::Payee));
        assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
        assert!(records[0].proposal_outbound_message_id.is_some());
    }

    #[tokio::test]
    async fn test_payment_request_records_do_not_compare_independent_source_ids() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        enqueue_untyped_private_message(
            &storage,
            counterparty.clone(),
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into(),
            timestamp(),
        )
        .await
        .unwrap();
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![acceptance_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                request_id,
            )],
        )
        .await;

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
        assert_eq!(records[0].proposal_outbound_message_id, Some(1));
        assert_eq!(
            records[0].proposal_outbound_status,
            Some(OutboundPrivateMessageStatus::Pending)
        );
        assert_eq!(records[0].last_stream_item_id, Some(0));
    }

    #[tokio::test]
    async fn test_payment_request_records_merge_outbound_proof() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            )],
        )
        .await;
        let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )) else {
            panic!("expected acceptance event");
        };
        enqueue_payment_request_acceptance(
            &storage,
            counterparty.clone(),
            &acceptance,
            timestamp(),
        )
        .await
        .unwrap();
        let PaymentRequestEvent::Proof(proof) = parsed_event(proof_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            request_id,
            "invoice-2026-0001",
        )) else {
            panic!("expected proof event");
        };
        enqueue_payment_proof(&storage, counterparty.clone(), &proof, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::ProofSubmitted
        );
        assert_eq!(records[0].payment_proofs.len(), 1);
        assert_eq!(
            records[0].payment_proofs[0].outbound_status,
            Some(OutboundPrivateMessageStatus::Pending)
        );
        assert_eq!(
            records[0].last_outbound_status,
            Some(OutboundPrivateMessageStatus::Pending)
        );
        assert!(!format!("{:?}", records[0].payment_proofs[0]).contains("secret"));
    }

    #[tokio::test]
    async fn test_payment_request_records_preserve_inbound_fifo_with_same_timestamp() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                proof_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                    request_id,
                    "invoice-2026-0001",
                ),
                acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", request_id),
            ],
        )
        .await;

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("before acceptance")));
    }

    #[tokio::test]
    async fn test_payment_request_records_ignore_duplicate_outbound_event() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            )],
        )
        .await;
        let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )) else {
            panic!("expected acceptance event");
        };
        enqueue_payment_request_acceptance(
            &storage,
            counterparty.clone(),
            &acceptance,
            timestamp(),
        )
        .await
        .unwrap();
        enqueue_payment_request_acceptance(
            &storage,
            counterparty.clone(),
            &acceptance,
            timestamp(),
        )
        .await
        .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records[0].state, PaymentRequestLifecycleState::Accepted);
        assert_eq!(
            records[0].accepted_event_id.as_deref(),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102")
        );
    }

    #[tokio::test]
    async fn test_payment_request_records_flag_outbound_event_id_conflict() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                request_id,
                "invoice-2026-0001",
                None,
                None,
            )],
        )
        .await;
        let PaymentRequestEvent::Acceptance(acceptance) = parsed_event(acceptance_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )) else {
            panic!("expected acceptance event");
        };
        let PaymentRequestEvent::Rejection(rejection) = parsed_event(rejection_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            request_id,
        )) else {
            panic!("expected rejection event");
        };
        enqueue_payment_request_acceptance(
            &storage,
            counterparty.clone(),
            &acceptance,
            timestamp(),
        )
        .await
        .unwrap();
        enqueue_payment_request_rejection(&storage, counterparty.clone(), &rejection, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
        assert_eq!(records[0].last_outbound_message_id, Some(1));
        assert!(records[0].last_stream_item_id.is_none());
    }

    #[tokio::test]
    async fn test_payment_request_records_flag_outbound_reuse_of_tainted_event_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    inbound_request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    inbound_request_id,
                    "invoice-2026-0002",
                    None,
                    None,
                ),
            ],
        )
        .await;
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            outbound_request_id,
            "invoice-2026-0003",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();
        let outbound = records
            .iter()
            .find(|record| record.payment_request_id == outbound_request_id)
            .unwrap();

        assert_eq!(
            outbound.state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(outbound
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }

    #[tokio::test]
    async fn test_payment_request_records_flag_cross_direction_duplicate_event_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let raw = request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            request_id,
            "invoice-2026-0001",
            None,
            None,
        );
        let PaymentRequestEvent::Request(request) = parsed_event(raw.clone()) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();
        persist_messages(&storage, counterparty.clone(), vec![raw]).await;

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }

    #[tokio::test]
    async fn test_payment_request_records_keep_preinvalid_position_during_later_conflict() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    inbound_request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                malformed_cancellation_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
                    inbound_request_id,
                ),
            ],
        )
        .await;
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            outbound_request_id,
            "invoice-2026-0002",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();
        let inbound = records
            .iter()
            .find(|record| record.payment_request_id == inbound_request_id)
            .unwrap();
        let outbound = records
            .iter()
            .find(|record| record.payment_request_id == outbound_request_id)
            .unwrap();

        assert_eq!(inbound.state, PaymentRequestLifecycleState::InvalidConflict);
        assert_eq!(inbound.last_stream_item_id, Some(1));
        assert_eq!(
            outbound.state,
            PaymentRequestLifecycleState::InvalidConflict
        );
    }

    #[tokio::test]
    async fn test_payment_request_records_flag_outbound_reuse_of_malformed_event_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let inbound_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![malformed_cancellation_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
                inbound_request_id,
            )],
        )
        .await;
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            outbound_request_id,
            "invoice-2026-0002",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();
        let outbound = records
            .iter()
            .find(|record| record.payment_request_id == outbound_request_id)
            .unwrap();

        assert_eq!(
            outbound.state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(outbound
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }

    #[tokio::test]
    async fn test_payment_request_records_taint_event_id_without_request_id() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let outbound_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![malformed_missing_request_id_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            )],
        )
        .await;
        let PaymentRequestEvent::Request(request) = parsed_event(request_raw(
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            outbound_request_id,
            "invoice-2026-0002",
            None,
            None,
        )) else {
            panic!("expected request event");
        };
        enqueue_payment_request(&storage, counterparty.clone(), &request, timestamp())
            .await
            .unwrap();

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }

    #[tokio::test]
    async fn test_payment_request_records_keep_malformed_inbound_audit_position() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
            ],
        )
        .await;

        let records = payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert_eq!(records[0].last_stream_item_id, Some(1));
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("reason must be a string")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_flag_inbound_acceptance_for_received_proposal() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                acceptance_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102", request_id),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert_eq!(
            records[0].terms.as_ref().unwrap().payment_reference,
            "invoice-2026-0001"
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("outbound payer event")));
        assert!(!format!("{:?}", records[0]).contains("private"));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_mark_proposal_expired() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![request_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                "invoice-2026-0001",
                Some("2026-06-03T11:59:59Z"),
                None,
            )],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::ProposalExpired
        );
    }

    #[tokio::test]
    async fn test_received_payment_request_records_flag_inbound_proof_for_received_proposal() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                proof_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                    request_id,
                    "invoice-2026-0001",
                ),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("outbound payer event")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_flag_event_id_conflict() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0002",
                    None,
                    None,
                ),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("Event ID")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_flag_invalid_transition() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![proof_raw(
                "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
                "invoice-2026-0001",
            )],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("before acceptance")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_preserve_first_invalid_reason() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                proof_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                    request_id,
                    "invoice-2026-0001",
                ),
                malformed_cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("before acceptance")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_keep_invalid_before_later_proposal() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                proof_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
                    request_id,
                    "invoice-2026-0001",
                ),
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert!(records[0].terms.is_none());
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("before acceptance")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_derive_cancellation() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records[0].state, PaymentRequestLifecycleState::Canceled);
        assert_eq!(
            records[0].canceled_event_id.as_deref(),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104")
        );
    }

    #[tokio::test]
    async fn test_received_payment_request_records_flag_second_cancellation() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104", request_id),
                cancellation_raw("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105", request_id),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(
            records[0].state,
            PaymentRequestLifecycleState::InvalidConflict
        );
        assert_eq!(
            records[0].canceled_event_id.as_deref(),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104")
        );
        assert!(records[0]
            .invalid_reason
            .as_ref()
            .is_some_and(|reason| reason.contains("terminal state")));
    }

    #[tokio::test]
    async fn test_received_payment_request_records_return_newest_first() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let older_request_id = "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        let newer_request_id = "c7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33";
        persist_messages(
            &storage,
            counterparty.clone(),
            vec![
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
                    older_request_id,
                    "invoice-2026-0001",
                    None,
                    None,
                ),
                request_raw(
                    "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
                    newer_request_id,
                    "invoice-2026-0002",
                    None,
                    None,
                ),
            ],
        )
        .await;

        let records = received_payment_request_records(&storage, &counterparty, timestamp())
            .await
            .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].payment_request_id, newer_request_id);
        assert_eq!(records[1].payment_request_id, older_request_id);
    }
}
