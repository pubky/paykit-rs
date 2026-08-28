use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_allowance_event_message, AllowanceAcceptance, AllowanceEvent, AllowanceId,
    AllowanceProposal, AllowanceRejection, EventId, PrivateApplicationMessage, PrivateMessageKind,
};
use serde_json::Value as JsonValue;

use crate::{
    domain::{
        linked_peers::LinkedPeerState,
        outbound_private::OutboundPrivateMessageStatus,
        private_stream::{canonical_event_id, is_event_message_kind, PrivateStreamParseStatus},
    },
    storage::{OutboundPrivateMessageRecord, PrivateStreamItemRecord, StorageState},
    PaykitReceiverPath, PubkyPublicKey,
};

use super::{
    AllowanceHistoryStatus, AllowanceLifecycleState, AllowanceRecord, AllowanceTermsRecord,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum EventSourceKind {
    Inbound,
    Outbound,
}

#[derive(Clone)]
enum EventSource {
    Inbound(PrivateStreamItemRecord),
    Outbound(OutboundPrivateMessageRecord),
}

impl EventSource {
    fn kind(&self) -> EventSourceKind {
        match self {
            Self::Inbound(_) => EventSourceKind::Inbound,
            Self::Outbound(_) => EventSourceKind::Outbound,
        }
    }

    fn order(&self) -> u64 {
        match self {
            Self::Inbound(item) => item.stream_item_id,
            Self::Outbound(message) => message.outbound_message_id,
        }
    }

    fn recorded_at(&self) -> DateTime<Utc> {
        match self {
            Self::Inbound(item) => item.received_at,
            Self::Outbound(message) => message.created_at,
        }
    }

    fn raw_json(&self) -> &str {
        match self {
            Self::Inbound(item) => &item.raw_json,
            Self::Outbound(message) => &message.raw_json,
        }
    }
}

#[derive(Clone)]
struct StoredAllowanceEvent {
    source: EventSource,
    event: AllowanceEvent,
    tainted_event_id: bool,
}

impl StoredAllowanceEvent {
    fn event_id(&self) -> &EventId {
        self.event.event_id()
    }

    fn allowance_id(&self) -> &AllowanceId {
        self.event.allowance_id()
    }
}

#[derive(Clone)]
struct InvalidEvidence {
    allowance_id: String,
    source: EventSource,
    reason: &'static str,
    conflict_event_id: Option<String>,
}

enum ControllingResponse<'a> {
    Acceptance(&'a StoredAllowanceEvent, &'a AllowanceAcceptance),
    Rejection(&'a StoredAllowanceEvent, &'a AllowanceRejection),
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
pub(crate) fn allowance_records_from_state(
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Vec<AllowanceRecord> {
    let inbound = state
        .private_stream_items
        .iter()
        .filter(|item| {
            &item.counterparty == counterparty
                && &item.counterparty_receiver_path == counterparty_receiver_path
        })
        .cloned()
        .collect::<Vec<_>>();
    let outbound = state
        .outbound_private_messages
        .iter()
        .filter(|message| {
            &message.counterparty == counterparty
                && &message.counterparty_receiver_path == counterparty_receiver_path
                && !matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Invalid
                        | OutboundPrivateMessageStatus::Superseded
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    let (events, mut invalid_evidence) = collect_allowance_events(state, inbound, outbound);
    let link_event_ids = link_event_ids(state, counterparty, counterparty_receiver_path);
    let allowance_ids_with_proposals = events
        .iter()
        .filter_map(|stored| match stored.event {
            AllowanceEvent::Proposal(_) => Some(stored.allowance_id().as_str().to_owned()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    invalid_evidence
        .retain(|evidence| allowance_ids_with_proposals.contains(evidence.allowance_id.as_str()));

    let mut events_by_allowance = HashMap::<String, Vec<StoredAllowanceEvent>>::new();
    for event in events {
        events_by_allowance
            .entry(event.allowance_id().as_str().to_owned())
            .or_default()
            .push(event);
    }
    let mut invalid_by_allowance = HashMap::<String, Vec<InvalidEvidence>>::new();
    for evidence in invalid_evidence {
        invalid_by_allowance
            .entry(evidence.allowance_id.clone())
            .or_default()
            .push(evidence);
    }

    let link_recovery_required = state
        .linked_peers
        .get(&(counterparty.clone(), counterparty_receiver_path.clone()))
        .is_some_and(|peer| peer.state == LinkedPeerState::RecoveryRequired);
    let mut records = Vec::new();
    for (allowance_id, events) in events_by_allowance {
        let evidence = invalid_by_allowance
            .remove(&allowance_id)
            .unwrap_or_default();
        if let Some(record) = derive_allowance_record(
            counterparty,
            counterparty_receiver_path,
            allowance_id,
            events,
            evidence,
            &link_event_ids,
            link_recovery_required,
        ) {
            records.push(record);
        }
    }
    sort_allowances_newest_first(&mut records);
    records
}

/// Derive one Allowance by exact link scope and Allowance ID.
pub(crate) fn allowance_record_from_state(
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: &AllowanceId,
) -> Option<AllowanceRecord> {
    allowance_records_from_state(state, counterparty, counterparty_receiver_path)
        .into_iter()
        .find(|record| record.allowance_id == allowance_id.as_str())
}

fn collect_allowance_events(
    state: &StorageState,
    inbound: Vec<PrivateStreamItemRecord>,
    outbound: Vec<OutboundPrivateMessageRecord>,
) -> (Vec<StoredAllowanceEvent>, Vec<InvalidEvidence>) {
    let mut events = Vec::new();
    let mut invalid = Vec::new();

    for item in inbound {
        let source = EventSource::Inbound(item.clone());
        if item
            .known_paykit_kind
            .as_deref()
            .is_some_and(is_allowance_kind)
        {
            collect_recognized_allowance_message(source, &mut events, &mut invalid);
        } else if item.parse_status == PrivateStreamParseStatus::UnknownKind {
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

    for message in outbound {
        if is_allowance_kind(&message.kind) {
            collect_recognized_allowance_message(
                EventSource::Outbound(message),
                &mut events,
                &mut invalid,
            );
        }
    }

    dedupe_and_taint_events(state, events, invalid)
}

fn collect_recognized_allowance_message(
    source: EventSource,
    events: &mut Vec<StoredAllowanceEvent>,
    invalid: &mut Vec<InvalidEvidence>,
) {
    let message = PrivateApplicationMessage {
        version: parsed_version(&source),
        kind: parsed_kind(&source),
        raw_json: source.raw_json().to_owned(),
    };
    let Some(parsed) = parse_allowance_event_message(&message) else {
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

fn dedupe_and_taint_events(
    state: &StorageState,
    events: Vec<StoredAllowanceEvent>,
    mut invalid: Vec<InvalidEvidence>,
) -> (Vec<StoredAllowanceEvent>, Vec<InvalidEvidence>) {
    let mut first_by_sender_and_id =
        HashMap::<(EventSourceKind, String), StoredAllowanceEvent>::new();
    let mut payloads_by_sender_and_id =
        HashMap::<(EventSourceKind, String), HashSet<String>>::new();
    let mut tainted_keys = HashSet::<(EventSourceKind, String)>::new();
    let mut deduped = Vec::new();

    for event in events {
        let key = (event.source.kind(), event.event_id().as_str().to_owned());
        let payload = event.source.raw_json().to_owned();
        if !payloads_by_sender_and_id
            .entry(key.clone())
            .or_default()
            .insert(payload)
        {
            continue;
        }
        if let Some(first) = first_by_sender_and_id.get(&key) {
            tainted_keys.insert(key.clone());
            invalid.push(conflict_evidence(first));
            invalid.push(conflict_evidence(&event));
            deduped.push(event);
        } else {
            first_by_sender_and_id.insert(key, event.clone());
            deduped.push(event);
        }
    }

    let outbound_payloads = outbound_event_payloads(state);

    for event in &deduped {
        let event_id = event.event_id().as_str();
        let key = (event.source.kind(), event_id.to_owned());
        let scope_key = (
            event_counterparty(event),
            event_receiver_path(event),
            event_id.to_owned(),
        );
        let sender_conflict = match event.source.kind() {
            EventSourceKind::Inbound => state
                .event_dedup_records
                .iter()
                .find(|((key, path, id), _)| {
                    key == &event_counterparty(event)
                        && path == &event_receiver_path(event)
                        && id == event_id
                })
                .is_some_and(|(_, record)| !record.conflicting_stream_item_ids.is_empty()),
            EventSourceKind::Outbound => outbound_payloads
                .get(&scope_key)
                .is_some_and(|payloads| payloads.len() > 1),
        };
        let cross_sender_conflict = match event.source.kind() {
            EventSourceKind::Inbound => outbound_payloads.contains_key(&scope_key),
            EventSourceKind::Outbound => state.event_dedup_records.contains_key(&scope_key),
        };
        if sender_conflict || cross_sender_conflict {
            tainted_keys.insert(key);
            invalid.push(conflict_evidence(event));
        }
    }

    // An outbound Allowance Event can conflict with an inbound non-Allowance
    // Event, and vice versa. The generic carrier sets above deliberately span
    // every Event Message kind on this link.
    for event in &mut deduped {
        let key = (event.source.kind(), event.event_id().as_str().to_owned());
        event.tainted_event_id = tainted_keys.contains(&key);
    }
    (deduped, invalid)
}

fn outbound_event_payloads(
    state: &StorageState,
) -> HashMap<(PubkyPublicKey, PaykitReceiverPath, String), HashSet<String>> {
    let mut payloads =
        HashMap::<(PubkyPublicKey, PaykitReceiverPath, String), HashSet<String>>::new();
    for message in &state.outbound_private_messages {
        if matches!(
            message.status,
            OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::Superseded
        ) || !is_event_message_kind(&message.kind)
        {
            continue;
        }
        if let Some(event_id) = canonical_event_id(&message.raw_json) {
            payloads
                .entry((
                    message.counterparty.clone(),
                    message.counterparty_receiver_path.clone(),
                    event_id,
                ))
                .or_default()
                .insert(message.raw_json.clone());
        }
    }
    payloads
}

fn link_event_ids(
    state: &StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> HashSet<String> {
    let mut event_ids = state
        .event_dedup_records
        .keys()
        .filter(|(key, path, _)| key == counterparty && path == counterparty_receiver_path)
        .map(|(_, _, event_id)| event_id.clone())
        .collect::<HashSet<_>>();
    for message in &state.outbound_private_messages {
        if &message.counterparty != counterparty
            || &message.counterparty_receiver_path != counterparty_receiver_path
            || !is_event_message_kind(&message.kind)
            || matches!(
                message.status,
                OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::Superseded
            )
        {
            continue;
        }
        if let Some(event_id) = canonical_event_id(&message.raw_json) {
            event_ids.insert(event_id);
        }
    }
    event_ids
}

fn derive_allowance_record(
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    allowance_id: String,
    mut events: Vec<StoredAllowanceEvent>,
    invalid_evidence: Vec<InvalidEvidence>,
    link_event_ids: &HashSet<String>,
    link_recovery_required: bool,
) -> Option<AllowanceRecord> {
    events.sort_by(|left, right| {
        source_rank(&left.source)
            .cmp(&source_rank(&right.source))
            .then_with(|| left.source.order().cmp(&right.source.order()))
    });
    let proposals = events
        .iter()
        .filter(|event| matches!(event.event, AllowanceEvent::Proposal(_)))
        .collect::<Vec<_>>();
    if proposals.is_empty() {
        return None;
    }

    let mut record = AllowanceRecord::new(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        allowance_id,
    );
    for event in &events {
        touch_record(&mut record, &event.source);
    }
    for evidence in &invalid_evidence {
        touch_record(&mut record, &evidence.source);
        mark_invalid(&mut record, evidence.reason);
        if let Some(event_id) = &evidence.conflict_event_id {
            record.conflict_event_ids.push(event_id.clone());
        }
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
                .map(|proposal| proposal.event_id().as_str().to_owned()),
        );
        finish_record(&mut record, link_recovery_required);
        return Some(record);
    }

    let stored_proposal = proposals[0];
    let AllowanceEvent::Proposal(proposal) = &stored_proposal.event else {
        unreachable!("proposal filter preserves event variant")
    };
    apply_proposal(&mut record, stored_proposal, proposal);
    if stored_proposal.tainted_event_id {
        mark_invalid(&mut record, "Proposal Event ID is conflicted");
        record
            .conflict_event_ids
            .push(stored_proposal.event_id().as_str().to_owned());
    }

    let controlling_response = derive_controlling_response(
        &mut record,
        proposal,
        stored_proposal.source.kind(),
        &events,
        link_event_ids,
    );
    match controlling_response.as_ref() {
        Some(ControllingResponse::Acceptance(stored, acceptance)) => {
            record.state = AllowanceLifecycleState::Accepted;
            apply_acceptance(&mut record, stored, acceptance);
        }
        Some(ControllingResponse::Rejection(stored, rejection)) => {
            record.state = AllowanceLifecycleState::Rejected;
            apply_rejection(&mut record, stored, rejection);
        }
        None => {}
    }

    let valid_ends = events
        .iter()
        .filter_map(|stored| match &stored.event {
            AllowanceEvent::End(event) => Some((stored, event)),
            _ => None,
        })
        .filter(|(stored, event)| {
            validate_end(
                &mut record,
                proposal,
                stored_proposal.source.kind(),
                controlling_response.as_ref(),
                stored,
                event,
                link_event_ids,
            )
        })
        .collect::<Vec<_>>();
    // `events` is already ordered by source and then by that sender's FIFO
    // position. Selecting from that order is deterministic without inventing
    // a timestamp or UUID order across the two directions.
    if let Some((stored, event)) = valid_ends.first() {
        record.state = AllowanceLifecycleState::Ended;
        apply_end(&mut record, stored, event);
    }

    finish_record(&mut record, link_recovery_required);
    Some(record)
}

fn derive_controlling_response<'a>(
    record: &mut AllowanceRecord,
    proposal: &AllowanceProposal,
    proposal_source: EventSourceKind,
    events: &'a [StoredAllowanceEvent],
    link_event_ids: &HashSet<String>,
) -> Option<ControllingResponse<'a>> {
    let expected_source = opposite_source(proposal_source);
    let mut responses = events
        .iter()
        .filter(|stored| {
            matches!(
                stored.event,
                AllowanceEvent::Acceptance(_) | AllowanceEvent::Rejection(_)
            )
        })
        .collect::<Vec<_>>();
    responses.sort_by_key(|stored| stored.source.order());
    let mut controlling = None;

    for stored in responses {
        let proposal_event_id = match &stored.event {
            AllowanceEvent::Acceptance(event) => event.proposal_event_id(),
            AllowanceEvent::Rejection(event) => event.proposal_event_id(),
            _ => unreachable!("response filter preserves event variant"),
        };
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
        controlling = Some(match &stored.event {
            AllowanceEvent::Acceptance(event) => ControllingResponse::Acceptance(stored, event),
            AllowanceEvent::Rejection(event) => ControllingResponse::Rejection(stored, event),
            _ => unreachable!("response filter preserves event variant"),
        });
    }
    controlling
}

fn validate_end(
    record: &mut AllowanceRecord,
    proposal: &AllowanceProposal,
    proposal_source: EventSourceKind,
    controlling_response: Option<&ControllingResponse<'_>>,
    stored: &StoredAllowanceEvent,
    event: &paykit_lib::AllowanceEnd,
    link_event_ids: &HashSet<String>,
) -> bool {
    if event.proposal_event_id() != proposal.event_id() {
        mark_invalid_or_unresolved(
            record,
            event.proposal_event_id().as_str(),
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
    let Some(acceptance_event_id) = event.acceptance_event_id() else {
        if stored.source.kind() == proposal_source {
            return true;
        }
        mark_invalid(
            record,
            "Allowance proposal withdrawal came from the proposal recipient",
        );
        return false;
    };
    match controlling_response {
        Some(ControllingResponse::Acceptance(_, acceptance))
            if acceptance.event_id() == acceptance_event_id =>
        {
            true
        }
        _ => {
            mark_invalid_or_unresolved(
                record,
                acceptance_event_id.as_str(),
                link_event_ids,
                "Allowance End references the wrong Acceptance Event ID",
            );
            false
        }
    }
}

fn apply_proposal(
    record: &mut AllowanceRecord,
    stored: &StoredAllowanceEvent,
    proposal: &AllowanceProposal,
) {
    record.local_role = Some(match stored.source.kind() {
        EventSourceKind::Inbound => proposal.recipient_role().into(),
        EventSourceKind::Outbound => proposal.proposer_role().into(),
    });
    record.proposal_event_id = Some(proposal.event_id().as_str().to_owned());
    record.terms = Some(AllowanceTermsRecord::from(proposal.terms()));
    match &stored.source {
        EventSource::Inbound(item) => record.proposal_stream_item_id = Some(item.stream_item_id),
        EventSource::Outbound(message) => {
            record.proposal_outbound_message_id = Some(message.outbound_message_id);
            record.proposal_outbound_status = Some(message.status.clone());
        }
    }
}

fn apply_acceptance(
    record: &mut AllowanceRecord,
    stored: &StoredAllowanceEvent,
    acceptance: &AllowanceAcceptance,
) {
    record.acceptance_event_id = Some(acceptance.event_id().as_str().to_owned());
    match &stored.source {
        EventSource::Inbound(item) => record.acceptance_stream_item_id = Some(item.stream_item_id),
        EventSource::Outbound(message) => {
            record.acceptance_outbound_message_id = Some(message.outbound_message_id);
            record.acceptance_outbound_status = Some(message.status.clone());
        }
    }
}

fn apply_rejection(
    record: &mut AllowanceRecord,
    stored: &StoredAllowanceEvent,
    rejection: &AllowanceRejection,
) {
    record.rejection_event_id = Some(rejection.event_id().as_str().to_owned());
    match &stored.source {
        EventSource::Inbound(item) => record.rejection_stream_item_id = Some(item.stream_item_id),
        EventSource::Outbound(message) => {
            record.rejection_outbound_message_id = Some(message.outbound_message_id);
            record.rejection_outbound_status = Some(message.status.clone());
        }
    }
}

fn apply_end(
    record: &mut AllowanceRecord,
    stored: &StoredAllowanceEvent,
    event: &paykit_lib::AllowanceEnd,
) {
    record.end_event_id = Some(event.event_id().as_str().to_owned());
    match &stored.source {
        EventSource::Inbound(item) => record.end_stream_item_id = Some(item.stream_item_id),
        EventSource::Outbound(message) => {
            record.end_outbound_message_id = Some(message.outbound_message_id);
            record.end_outbound_status = Some(message.status.clone());
        }
    }
}

fn touch_record(record: &mut AllowanceRecord, source: &EventSource) {
    record.last_event_at = Some(
        record
            .last_event_at
            .map_or(source.recorded_at(), |current| {
                current.max(source.recorded_at())
            }),
    );
    match source {
        EventSource::Inbound(item) => {
            record.last_stream_item_id = Some(
                record
                    .last_stream_item_id
                    .map_or(item.stream_item_id, |current| {
                        current.max(item.stream_item_id)
                    }),
            );
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
    link_event_ids: &HashSet<String>,
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

fn conflict_evidence(event: &StoredAllowanceEvent) -> InvalidEvidence {
    InvalidEvidence {
        allowance_id: event.allowance_id().as_str().to_owned(),
        source: event.source.clone(),
        reason: "Event ID reused by conflicting Event Messages",
        conflict_event_id: Some(event.event_id().as_str().to_owned()),
    }
}

fn parsed_version(source: &EventSource) -> Option<u8> {
    match source {
        EventSource::Inbound(item) => item
            .parsed_version
            .and_then(|version| u8::try_from(version).ok()),
        EventSource::Outbound(_) => Some(1),
    }
}

fn parsed_kind(source: &EventSource) -> Option<String> {
    match source {
        EventSource::Inbound(item) => item.parsed_kind.clone(),
        EventSource::Outbound(message) => Some(message.kind.clone()),
    }
}

fn event_counterparty(event: &StoredAllowanceEvent) -> PubkyPublicKey {
    match &event.source {
        EventSource::Inbound(item) => item.counterparty.clone(),
        EventSource::Outbound(message) => message.counterparty.clone(),
    }
}

fn event_receiver_path(event: &StoredAllowanceEvent) -> PaykitReceiverPath {
    match &event.source {
        EventSource::Inbound(item) => item.counterparty_receiver_path.clone(),
        EventSource::Outbound(message) => message.counterparty_receiver_path.clone(),
    }
}

fn opposite_source(source: EventSourceKind) -> EventSourceKind {
    match source {
        EventSourceKind::Inbound => EventSourceKind::Outbound,
        EventSourceKind::Outbound => EventSourceKind::Inbound,
    }
}

fn source_rank(source: &EventSource) -> u8 {
    match source {
        EventSource::Inbound(_) => 0,
        EventSource::Outbound(_) => 1,
    }
}

fn canonical_allowance_id(raw_json: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(raw_json).ok()?;
    let value = value.get("allowance_id")?.as_str()?;
    AllowanceId::new(value)
        .ok()
        .map(|id| id.as_str().to_owned())
}

fn is_allowance_kind(kind: &str) -> bool {
    matches!(
        PrivateMessageKind::parse(kind),
        Some(
            PrivateMessageKind::AllowanceProposal
                | PrivateMessageKind::AllowanceAcceptance
                | PrivateMessageKind::AllowanceRejection
                | PrivateMessageKind::AllowanceEnd
        )
    )
}

fn sort_allowances_newest_first(records: &mut [AllowanceRecord]) {
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
            .then_with(|| left.allowance_id.cmp(&right.allowance_id))
    });
}
