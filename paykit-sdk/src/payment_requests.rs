//! Payment Request lifecycle derivation.
//!
//! Derived records can include Payment Request metadata. Treat them as private
//! SDK state.

use std::{cmp::Reverse, collections::HashMap, fmt};

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
    storage::OutboundPrivateMessageRecord,
    storage::{EventDedupRecord, PrivateStreamItemRecord, StorageAdapter},
    PubkyPublicKey, Result,
};

/// SDK-derived received Payment Request lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentRequestLifecycleState {
    /// Proposal has been received and remains actionable.
    Proposed,
    /// Proposal is past its expiry.
    ProposalExpired,
    /// Request was canceled.
    Canceled,
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
            .field("payment_reference", &self.payment_reference)
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

/// SDK-derived received Payment Request lifecycle record.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentRequestRecord {
    /// Counterparty associated with the private stream.
    pub counterparty: PubkyPublicKey,
    /// Stable Payment Request ID.
    pub payment_request_id: String,
    /// Derived lifecycle state.
    pub state: PaymentRequestLifecycleState,
    /// Stream item id of the proposal event.
    pub proposal_stream_item_id: Option<u64>,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
    /// Immutable terms from the proposal.
    pub terms: Option<PaymentRequestTermsRecord>,
    /// Cancellation Event ID.
    pub canceled_event_id: Option<String>,
    /// Last stream item applied to this record.
    pub last_event_stream_item_id: Option<u64>,
    /// Last event receive time.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Invalid state reason, when available.
    pub invalid_reason: Option<String>,
}

impl fmt::Debug for PaymentRequestRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequestRecord")
            .field("counterparty", &self.counterparty)
            .field("payment_request_id", &self.payment_request_id)
            .field("state", &self.state)
            .field("proposal_stream_item_id", &self.proposal_stream_item_id)
            .field("proposal_event_id", &self.proposal_event_id)
            .field("canceled_event_id", &self.canceled_event_id)
            .field("last_event_stream_item_id", &self.last_event_stream_item_id)
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
            state: PaymentRequestLifecycleState::InvalidConflict,
            proposal_stream_item_id: None,
            proposal_event_id: None,
            terms: None,
            canceled_event_id: None,
            last_event_stream_item_id: None,
            last_event_at: None,
            invalid_reason: None,
        }
    }

    fn touch(&mut self, item: &PrivateStreamItemRecord) {
        self.last_event_stream_item_id = Some(item.stream_item_id);
        self.last_event_at = Some(item.received_at);
    }

    fn mark_invalid(&mut self, item: &PrivateStreamItemRecord, reason: impl Into<String>) {
        self.state = PaymentRequestLifecycleState::InvalidConflict;
        self.invalid_reason = Some(reason.into());
        self.touch(item);
    }
}

/// Derive received Payment Request lifecycle records for one counterparty.
///
/// Records are derived from the persisted inbound private stream and returned
/// newest-first by the last applied stream item. Malformed recognized Payment
/// Request events without a valid `payment_request_id` remain available in the
/// raw private stream log but cannot be attached to a request-scoped record.
pub async fn received_payment_request_records<S>(
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
                .last_event_stream_item_id
                .or(record.proposal_stream_item_id),
        )
    });
    Ok(records)
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
        && record.last_event_stream_item_id.is_some()
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
    record.state = PaymentRequestLifecycleState::Proposed;
    record.proposal_stream_item_id = Some(item.stream_item_id);
    record.proposal_event_id = Some(request.event_id.as_str().to_owned());
    record.terms = Some(PaymentRequestTermsRecord::from(&request.request));
    record.touch(item);
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

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::{
        outbound_private::queued_outbound_private_messages,
        private_stream::persist_private_stream_batch, storage::InMemoryStorage,
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

    fn cancellation_raw(event_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"{event_id}","payment_request_id":"{request_id}"}}"#
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
