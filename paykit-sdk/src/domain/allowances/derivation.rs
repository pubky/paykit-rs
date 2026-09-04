use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_allowance_event_message, AllowanceEnd, AllowanceEvent, AllowanceId, AllowanceProposal,
    EventId, PrivateApplicationMessage,
};
use serde_json::Value as JsonValue;

use crate::{
    domain::{
        linked_peers::LinkedPeerState,
        outbound_private::OutboundPrivateMessageStatus,
        private_stream::{
            canonical_event_id, is_allowance_kind, outbound_event_carriers, OutboundEventCarriers,
            PrivateStreamParseStatus,
        },
    },
    storage::{
        EventDedupRecord, OutboundPrivateMessageRecord, PrivateStreamItemRecord, StorageAdapter,
        StorageState, StorageTransaction,
    },
    PaykitReceiverPath, PubkyPublicKey, Result,
};

use super::{
    AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceRecord, AllowanceTermsRecord,
};

/// Durable history of one exact Encrypted Link.
///
/// Loaded inside one storage transaction so that derivation and any command
/// append observe the same state.
pub(super) struct AllowanceLinkHistory {
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    /// Every inbound private stream item on the link.
    items: Vec<PrivateStreamItemRecord>,
    /// Outbound messages that still carry local intent. `Invalid` and
    /// `Superseded` records never advance the link and are excluded.
    outbound: Vec<OutboundPrivateMessageRecord>,
    /// Event dedupe records keyed by Event ID for every inbound Event Message
    /// on the link, regardless of kind.
    dedupe_records: HashMap<String, EventDedupRecord>,
    link_recovery_required: bool,
}

impl AllowanceLinkHistory {
    pub(super) fn load(
        tx: &dyn StorageTransaction,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Self {
        let items = tx.private_stream_items(counterparty, counterparty_receiver_path);
        let outbound = tx
            .outbound_private_messages(counterparty, counterparty_receiver_path)
            .into_iter()
            .filter(|message| {
                !matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Invalid
                        | OutboundPrivateMessageStatus::Superseded
                )
            })
            .collect();
        let dedupe_records = items
            .iter()
            .filter_map(|item| canonical_event_id(&item.raw_json))
            .filter_map(|event_id| {
                tx.event_dedup_record(counterparty, counterparty_receiver_path, &event_id)
                    .map(|record| (event_id, record))
            })
            .collect();
        let link_recovery_required = tx
            .linked_peer(counterparty, counterparty_receiver_path)
            .is_some_and(|peer| peer.state == LinkedPeerState::RecoveryRequired);
        Self {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: counterparty_receiver_path.clone(),
            items,
            outbound,
            dedupe_records,
            link_recovery_required,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum EventSourceKind {
    Inbound,
    Outbound,
}

#[derive(Clone, Copy)]
enum EventSource<'a> {
    Inbound(&'a PrivateStreamItemRecord),
    Outbound(&'a OutboundPrivateMessageRecord),
}

impl<'a> EventSource<'a> {
    fn kind(self) -> EventSourceKind {
        match self {
            Self::Inbound(_) => EventSourceKind::Inbound,
            Self::Outbound(_) => EventSourceKind::Outbound,
        }
    }

    /// FIFO position within this sender's own direction.
    fn order(self) -> u64 {
        match self {
            Self::Inbound(item) => item.stream_item_id,
            Self::Outbound(message) => message.outbound_message_id,
        }
    }

    fn recorded_at(self) -> DateTime<Utc> {
        match self {
            Self::Inbound(item) => item.received_at,
            Self::Outbound(message) => message.created_at,
        }
    }

    fn raw_json(self) -> &'a str {
        match self {
            Self::Inbound(item) => &item.raw_json,
            Self::Outbound(message) => &message.raw_json,
        }
    }

    fn message(self) -> PrivateApplicationMessage {
        match self {
            Self::Inbound(item) => PrivateApplicationMessage {
                version: item
                    .parsed_version
                    .and_then(|version| u8::try_from(version).ok()),
                kind: item.parsed_kind.clone(),
                raw_json: item.raw_json.clone(),
            },
            Self::Outbound(message) => PrivateApplicationMessage {
                version: Some(1),
                kind: Some(message.kind.clone()),
                raw_json: message.raw_json.clone(),
            },
        }
    }

    fn outbound_status(self) -> Option<OutboundPrivateMessageStatus> {
        match self {
            Self::Inbound(_) => None,
            Self::Outbound(message) => Some(message.status.clone()),
        }
    }
}

struct StoredAllowanceEvent<'a> {
    source: EventSource<'a>,
    event: AllowanceEvent,
    tainted_event_id: bool,
}

impl StoredAllowanceEvent<'_> {
    fn event_id(&self) -> &EventId {
        self.event.event_id()
    }

    fn allowance_id(&self) -> &AllowanceId {
        self.event.allowance_id()
    }
}

struct InvalidEvidence<'a> {
    allowance_id: String,
    source: EventSource<'a>,
    reason: &'static str,
    conflict_event_id: Option<String>,
}

/// Return exact link scopes containing recognized Allowance activity.
pub(crate) fn allowance_scopes(state: &StorageState) -> Vec<(PubkyPublicKey, PaykitReceiverPath)> {
    let mut scopes = HashSet::new();
    for item in &state.private_stream_items {
        if item
            .known_paykit_kind
            .as_deref()
            .is_some_and(is_allowance_kind)
        {
            scopes.insert((
                item.counterparty.clone(),
                item.counterparty_receiver_path.clone(),
            ));
        }
    }
    for message in &state.outbound_private_messages {
        if is_allowance_kind(&message.kind) {
            scopes.insert((
                message.counterparty.clone(),
                message.counterparty_receiver_path.clone(),
            ));
        }
    }
    let mut scopes = scopes.into_iter().collect::<Vec<_>>();
    scopes.sort_by(|(left_key, left_path), (right_key, right_path)| {
        left_key
            .as_str()
            .cmp(right_key.as_str())
            .then_with(|| left_path.as_str().cmp(right_path.as_str()))
    });
    scopes
}

/// Derive every Allowance for one exact authenticated Encrypted Link.
///
/// Records are newest-first by local record time.
pub(crate) async fn allowance_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<Vec<AllowanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            let history = AllowanceLinkHistory::load(tx, counterparty, counterparty_receiver_path);
            Ok(derive_records(&history, None))
        })
        .await
}

/// Derive one Allowance by exact link scope and Allowance ID.
pub(crate) async fn allowance_record<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
) -> Result<Option<AllowanceRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            let history = AllowanceLinkHistory::load(tx, counterparty, counterparty_receiver_path);
            Ok(derive_allowance_record(&history, allowance_id))
        })
        .await
}

/// Derive one Allowance from already-loaded link history.
pub(super) fn derive_allowance_record(
    history: &AllowanceLinkHistory,
    allowance_id: &AllowanceId,
) -> Option<AllowanceRecord> {
    derive_records(history, Some(allowance_id.as_str())).pop()
}

fn derive_records(
    history: &AllowanceLinkHistory,
    only_allowance_id: Option<&str>,
) -> Vec<AllowanceRecord> {
    let carriers = outbound_event_carriers(&history.outbound);
    let (events, invalid_evidence) = collect_allowance_events(history, &carriers);
    let link_event_ids = history
        .dedupe_records
        .keys()
        .chain(&carriers.event_ids)
        .map(String::as_str)
        .collect::<HashSet<_>>();

    let mut events_by_allowance = HashMap::<String, Vec<StoredAllowanceEvent<'_>>>::new();
    for event in events {
        let allowance_id = event.allowance_id().as_str();
        if only_allowance_id.is_some_and(|only| only != allowance_id) {
            continue;
        }
        events_by_allowance
            .entry(allowance_id.to_owned())
            .or_default()
            .push(event);
    }
    let mut invalid_by_allowance = HashMap::<String, Vec<InvalidEvidence<'_>>>::new();
    for evidence in invalid_evidence {
        invalid_by_allowance
            .entry(evidence.allowance_id.clone())
            .or_default()
            .push(evidence);
    }

    let mut records = events_by_allowance
        .into_iter()
        .filter_map(|(allowance_id, events)| {
            let evidence = invalid_by_allowance
                .remove(&allowance_id)
                .unwrap_or_default();
            derive_allowance(history, allowance_id, events, evidence, &link_event_ids)
        })
        .collect::<Vec<_>>();
    sort_allowances_newest_first(&mut records);
    records
}

fn collect_allowance_events<'a>(
    history: &'a AllowanceLinkHistory,
    carriers: &OutboundEventCarriers,
) -> (Vec<StoredAllowanceEvent<'a>>, Vec<InvalidEvidence<'a>>) {
    let mut events = Vec::new();
    let mut invalid = Vec::new();

    for item in &history.items {
        let source = EventSource::Inbound(item);
        if item
            .known_paykit_kind
            .as_deref()
            .is_some_and(is_allowance_kind)
        {
            collect_recognized_allowance_message(source, &mut events, &mut invalid);
        } else if matches!(
            item.parse_status,
            PrivateStreamParseStatus::UnknownKind | PrivateStreamParseStatus::MalformedRecognized
        ) {
            // Any other message that names this Allowance but cannot be
            // interpreted blocks it, whether its kind is unknown or a
            // recognized kind that failed validation.
            if let Some(allowance_id) = canonical_allowance_id(&item.raw_json) {
                invalid.push(InvalidEvidence {
                    allowance_id,
                    source,
                    reason: "unsupported Allowance-correlated private message",
                    conflict_event_id: None,
                });
            }
        }
    }

    for message in &history.outbound {
        if is_allowance_kind(&message.kind) {
            collect_recognized_allowance_message(
                EventSource::Outbound(message),
                &mut events,
                &mut invalid,
            );
        }
    }

    dedupe_and_taint_events(history, carriers, events, invalid)
}

fn collect_recognized_allowance_message<'a>(
    source: EventSource<'a>,
    events: &mut Vec<StoredAllowanceEvent<'a>>,
    invalid: &mut Vec<InvalidEvidence<'a>>,
) {
    let Some(parsed) = parse_allowance_event_message(&source.message()) else {
        return;
    };
    if let Some(event) = parsed.parsed_event() {
        events.push(StoredAllowanceEvent {
            source,
            event: event.clone(),
            tainted_event_id: false,
        });
    } else if let Some(allowance_id) = parsed.allowance_id() {
        invalid.push(InvalidEvidence {
            allowance_id: allowance_id.as_str().to_owned(),
            source,
            reason: "malformed recognized Allowance event",
            conflict_event_id: None,
        });
    }
}

fn dedupe_and_taint_events<'a>(
    history: &AllowanceLinkHistory,
    carriers: &OutboundEventCarriers,
    events: Vec<StoredAllowanceEvent<'a>>,
    mut invalid: Vec<InvalidEvidence<'a>>,
) -> (Vec<StoredAllowanceEvent<'a>>, Vec<InvalidEvidence<'a>>) {
    let mut first_index_by_sender_and_id = HashMap::<(EventSourceKind, String), usize>::new();
    let mut payloads_by_sender_and_id =
        HashMap::<(EventSourceKind, String), HashSet<&'a str>>::new();
    let mut tainted_keys = HashSet::<(EventSourceKind, String)>::new();
    let mut deduped = Vec::new();

    for event in events {
        let key = (event.source.kind(), event.event_id().as_str().to_owned());
        // An exact same-sender retry carries no new evidence.
        if !payloads_by_sender_and_id
            .entry(key.clone())
            .or_default()
            .insert(event.source.raw_json())
        {
            continue;
        }
        match first_index_by_sender_and_id.get(&key) {
            Some(&first) => {
                tainted_keys.insert(key);
                invalid.push(conflict_evidence(&deduped[first]));
                invalid.push(conflict_evidence(&event));
            }
            None => {
                first_index_by_sender_and_id.insert(key, deduped.len());
            }
        }
        deduped.push(event);
    }

    // The inbound dedupe index and the outbound carriers deliberately span
    // every Event Message kind on this link: an Allowance Event can conflict
    // with a non-Allowance Event, and vice versa.
    for event in &deduped {
        let event_id = event.event_id().as_str();
        let dedupe = history.dedupe_records.get(event_id);
        let (same_sender_conflict, cross_sender_conflict) = match event.source.kind() {
            EventSourceKind::Inbound => (
                dedupe.is_some_and(|record| !record.conflicting_stream_item_ids.is_empty()),
                carriers.event_ids.contains(event_id),
            ),
            EventSourceKind::Outbound => (
                carriers.conflicted_event_ids.contains(event_id),
                dedupe.is_some(),
            ),
        };
        if same_sender_conflict || cross_sender_conflict {
            tainted_keys.insert((event.source.kind(), event_id.to_owned()));
            invalid.push(conflict_evidence(event));
        }
    }

    for event in &mut deduped {
        event.tainted_event_id =
            tainted_keys.contains(&(event.source.kind(), event.event_id().as_str().to_owned()));
    }
    (deduped, invalid)
}

fn derive_allowance(
    history: &AllowanceLinkHistory,
    allowance_id: String,
    mut events: Vec<StoredAllowanceEvent<'_>>,
    invalid_evidence: Vec<InvalidEvidence<'_>>,
    link_event_ids: &HashSet<&str>,
) -> Option<AllowanceRecord> {
    // Order by sending direction and then by that sender's FIFO position. No
    // timestamp or UUID order is invented across the two directions.
    events.sort_by_key(|stored| (source_rank(stored.source), stored.source.order()));
    let proposals = events
        .iter()
        .filter_map(|stored| match &stored.event {
            AllowanceEvent::Proposal(proposal) => Some((stored, proposal)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let (stored_proposal, proposal) = *proposals.first()?;

    let mut record = AllowanceRecord::new(
        history.counterparty.clone(),
        history.counterparty_receiver_path.clone(),
        allowance_id,
    );
    for stored in &events {
        touch_record(&mut record, stored.source);
    }
    for evidence in &invalid_evidence {
        touch_record(&mut record, evidence.source);
        mark_invalid(&mut record, evidence.reason);
        record
            .conflict_event_ids
            .extend(evidence.conflict_event_id.clone());
    }

    if proposals.len() > 1 {
        record.state = AllowanceLifecycleState::Conflicted;
        mark_invalid(
            &mut record,
            "multiple distinct proposals reused one Allowance ID",
        );
        record.conflict_event_ids.extend(
            proposals
                .iter()
                .map(|(stored, _)| stored.event_id().as_str().to_owned()),
        );
        finish_record(&mut record, history.link_recovery_required);
        return Some(record);
    }

    apply_proposal(&mut record, stored_proposal, proposal);
    if stored_proposal.tainted_event_id {
        mark_invalid(&mut record, "Proposal Event ID is conflicted");
        record
            .conflict_event_ids
            .push(stored_proposal.event_id().as_str().to_owned());
    }

    let proposal_source = stored_proposal.source.kind();
    if let Some(stored) = controlling_response(
        &mut record,
        proposal,
        proposal_source,
        &events,
        link_event_ids,
    ) {
        match &stored.event {
            AllowanceEvent::Acceptance(acceptance) => {
                record.state = AllowanceLifecycleState::Accepted;
                record.acceptance_event_id = Some(acceptance.event_id().as_str().to_owned());
                record.acceptance_outbound_status = stored.source.outbound_status();
            }
            AllowanceEvent::Rejection(rejection) => {
                record.state = AllowanceLifecycleState::Rejected;
                record.rejection_event_id = Some(rejection.event_id().as_str().to_owned());
                record.rejection_outbound_status = stored.source.outbound_status();
            }
            AllowanceEvent::Proposal(_) | AllowanceEvent::End(_) => {}
        }
    }

    // Every End is validated so invalid evidence is recorded; the first valid
    // End in the deterministic order above is retained.
    let mut retained_end = None;
    for stored in &events {
        let AllowanceEvent::End(end) = &stored.event else {
            continue;
        };
        let valid = validate_end(
            &mut record,
            proposal,
            proposal_source,
            stored,
            end,
            link_event_ids,
        );
        if valid && retained_end.is_none() {
            retained_end = Some((stored, end));
        }
    }
    if let Some((stored, end)) = retained_end {
        record.state = AllowanceLifecycleState::Ended;
        record.end_event_id = Some(end.event_id().as_str().to_owned());
        record.end_outbound_status = stored.source.outbound_status();
    }

    finish_record(&mut record, history.link_recovery_required);
    Some(record)
}

fn controlling_response<'e, 'a>(
    record: &mut AllowanceRecord,
    proposal: &AllowanceProposal,
    proposal_source: EventSourceKind,
    events: &'e [StoredAllowanceEvent<'a>],
    link_event_ids: &HashSet<&str>,
) -> Option<&'e StoredAllowanceEvent<'a>> {
    let expected_source = opposite_source(proposal_source);
    let mut responses = events
        .iter()
        .filter_map(|stored| {
            let proposal_event_id = match &stored.event {
                AllowanceEvent::Acceptance(event) => event.proposal_event_id(),
                AllowanceEvent::Rejection(event) => event.proposal_event_id(),
                AllowanceEvent::Proposal(_) | AllowanceEvent::End(_) => return None,
            };
            Some((stored, proposal_event_id))
        })
        .collect::<Vec<_>>();
    responses.sort_by_key(|(stored, _)| stored.source.order());
    let mut controlling = None;

    for (stored, proposal_event_id) in responses {
        if proposal_event_id != proposal.event_id() {
            mark_invalid_or_unresolved(
                record,
                proposal_event_id.as_str(),
                link_event_ids,
                "Allowance response references the wrong Proposal Event ID",
            );
            continue;
        }
        if stored.source.kind() != expected_source {
            mark_invalid(record, "Allowance response came from the proposal sender");
            continue;
        }
        if stored.tainted_event_id {
            mark_invalid(record, "Allowance response Event ID is conflicted");
            record
                .conflict_event_ids
                .push(stored.event_id().as_str().to_owned());
            continue;
        }
        if controlling.is_some() {
            mark_invalid(record, "multiple Allowance responses followed one proposal");
            continue;
        }
        controlling = Some(stored);
    }
    controlling
}

fn validate_end(
    record: &mut AllowanceRecord,
    proposal: &AllowanceProposal,
    proposal_source: EventSourceKind,
    stored: &StoredAllowanceEvent<'_>,
    end: &AllowanceEnd,
    link_event_ids: &HashSet<&str>,
) -> bool {
    if end.proposal_event_id() != proposal.event_id() {
        mark_invalid_or_unresolved(
            record,
            end.proposal_event_id().as_str(),
            link_event_ids,
            "Allowance End references the wrong Proposal Event ID",
        );
        return false;
    }
    if stored.tainted_event_id {
        mark_invalid(record, "Allowance End Event ID is conflicted");
        record
            .conflict_event_ids
            .push(stored.event_id().as_str().to_owned());
        return false;
    }
    let Some(acceptance_event_id) = end.acceptance_event_id() else {
        if stored.source.kind() == proposal_source {
            return true;
        }
        mark_invalid(
            record,
            "Allowance proposal withdrawal came from the proposal recipient",
        );
        return false;
    };
    if record.acceptance_event_id.as_deref() == Some(acceptance_event_id.as_str()) {
        return true;
    }
    mark_invalid_or_unresolved(
        record,
        acceptance_event_id.as_str(),
        link_event_ids,
        "Allowance End references the wrong Acceptance Event ID",
    );
    false
}

fn apply_proposal(
    record: &mut AllowanceRecord,
    stored: &StoredAllowanceEvent<'_>,
    proposal: &AllowanceProposal,
) {
    record.local_role = Some(match stored.source.kind() {
        EventSourceKind::Inbound => proposal.recipient_role().into(),
        EventSourceKind::Outbound => proposal.proposer_role().into(),
    });
    record.proposal_event_id = Some(proposal.event_id().as_str().to_owned());
    record.terms = Some(AllowanceTermsRecord::from(proposal.terms()));
    match stored.source {
        EventSource::Inbound(item) => record.proposal_stream_item_id = Some(item.stream_item_id),
        EventSource::Outbound(message) => {
            record.proposal_outbound_message_id = Some(message.outbound_message_id);
            record.proposal_outbound_status = Some(message.status.clone());
        }
    }
}

fn touch_record(record: &mut AllowanceRecord, source: EventSource<'_>) {
    record.last_event_at = record.last_event_at.max(Some(source.recorded_at()));
    match source {
        EventSource::Inbound(item) => {
            record.last_stream_item_id = record.last_stream_item_id.max(Some(item.stream_item_id));
        }
        EventSource::Outbound(message) => {
            if record
                .last_outbound_message_id
                .is_none_or(|current| message.outbound_message_id >= current)
            {
                record.last_outbound_message_id = Some(message.outbound_message_id);
                record.last_outbound_status = Some(message.status.clone());
            }
            if message.status == OutboundPrivateMessageStatus::RecoveryRequired {
                record.history_status = AllowanceHistoryStatus::RecoveryRequired;
            }
        }
    }
}

fn mark_invalid(record: &mut AllowanceRecord, reason: &'static str) {
    if record.history_status != AllowanceHistoryStatus::RecoveryRequired {
        record.history_status = AllowanceHistoryStatus::Invalid;
    }
    if record.invalid_reason.is_none() {
        record.invalid_reason = Some(reason.to_owned());
    }
}

fn mark_unresolved(record: &mut AllowanceRecord, event_id: &str) {
    if record.history_status == AllowanceHistoryStatus::Consistent {
        record.history_status = AllowanceHistoryStatus::UnresolvedReferences;
    }
    record.pending_causal_event_ids.push(event_id.to_owned());
}

fn mark_invalid_or_unresolved(
    record: &mut AllowanceRecord,
    event_id: &str,
    link_event_ids: &HashSet<&str>,
    invalid_reason: &'static str,
) {
    if link_event_ids.contains(event_id) {
        mark_invalid(record, invalid_reason);
    } else {
        mark_unresolved(record, event_id);
    }
}

fn finish_record(record: &mut AllowanceRecord, link_recovery_required: bool) {
    record.pending_causal_event_ids.sort();
    record.pending_causal_event_ids.dedup();
    record.conflict_event_ids.sort();
    record.conflict_event_ids.dedup();
    if link_recovery_required {
        record.history_status = AllowanceHistoryStatus::RecoveryRequired;
    }
}

fn conflict_evidence<'a>(event: &StoredAllowanceEvent<'a>) -> InvalidEvidence<'a> {
    InvalidEvidence {
        allowance_id: event.allowance_id().as_str().to_owned(),
        source: event.source,
        reason: "Event ID reused by conflicting Event Messages",
        conflict_event_id: Some(event.event_id().as_str().to_owned()),
    }
}

fn opposite_source(source: EventSourceKind) -> EventSourceKind {
    match source {
        EventSourceKind::Inbound => EventSourceKind::Outbound,
        EventSourceKind::Outbound => EventSourceKind::Inbound,
    }
}

fn source_rank(source: EventSource<'_>) -> u8 {
    match source {
        EventSource::Inbound(_) => 0,
        EventSource::Outbound(_) => 1,
    }
}

/// Return a canonical Allowance ID from a JSON carrier when one is present.
pub(super) fn canonical_allowance_id(raw_json: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(raw_json).ok()?;
    let value = value.get("allowance_id")?.as_str()?;
    AllowanceId::new(value)
        .ok()
        .map(|id| id.as_str().to_owned())
}

/// Presentation order: newest local record time first, then stable tiebreaks.
pub(crate) fn sort_allowances_newest_first(records: &mut [AllowanceRecord]) {
    records.sort_by(|left, right| {
        right
            .last_event_at
            .cmp(&left.last_event_at)
            .then_with(|| right.last_stream_item_id.cmp(&left.last_stream_item_id))
            .then_with(|| {
                right
                    .last_outbound_message_id
                    .cmp(&left.last_outbound_message_id)
            })
            .then_with(|| left.counterparty.as_str().cmp(right.counterparty.as_str()))
            .then_with(|| left.allowance_id.cmp(&right.allowance_id))
    });
}
