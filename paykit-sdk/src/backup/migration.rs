//! Compatibility migrations applied before strict backup validation.

use std::collections::{HashMap, HashSet};

use paykit_lib::{PaykitReceiverPath, PrivateMessageKind};

use crate::{
    domain::{
        private_stream::{
            classify_private_application_message, enforce_receipt_access_receiver_scope,
            is_allowance_kind, payload_hash, PrivateStreamEventHeader,
            PrivateStreamMessageClassification, PrivateStreamParseStatus,
        },
        receipts::{ReceiptAccessRecord, ReceiptRecord},
    },
    storage::{EventDedupRecord, PrivateStreamItemRecord},
    PubkyPublicKey,
};

use super::validation::{private_application_message_from_raw, private_message_header};

type EventKey = (PubkyPublicKey, PaykitReceiverPath, String);

/// One classified inbound carrier of an Event ID.
struct EventCarrier<'a> {
    item: &'a PrivateStreamItemRecord,
    header: PrivateStreamEventHeader,
}

/// Recognize Allowance items that an older SDK stored as unknown kinds, then
/// rebuild the Event indexes for every Event ID those items carry.
///
/// `stream_items` must already be sorted by `stream_item_id`, so the first
/// carrier of each Event ID is the authoritative one.
pub(super) fn migrate_legacy_allowance_stream_state(
    stream_items: &mut [PrivateStreamItemRecord],
    event_dedup_records: &mut HashMap<EventKey, EventDedupRecord>,
    receipt_access_records: &mut HashMap<EventKey, ReceiptAccessRecord>,
    receipt_records: &mut HashMap<EventKey, ReceiptRecord>,
) {
    let affected_event_keys = migrate_legacy_allowance_items(stream_items);
    if affected_event_keys.is_empty() {
        return;
    }
    let affected_links = affected_event_keys
        .iter()
        .map(|(counterparty, receiver_path, _)| (counterparty, receiver_path))
        .collect::<HashSet<_>>();

    // Classify each item on an affected link exactly once.
    let mut carriers_by_key = HashMap::<EventKey, Vec<EventCarrier<'_>>>::new();
    for item in stream_items.iter() {
        if !affected_links.contains(&(&item.counterparty, &item.counterparty_receiver_path)) {
            continue;
        }
        let Some(classification) = classify_stream_item(item) else {
            continue;
        };
        let Some(header) = classification.event else {
            continue;
        };
        let key = (
            item.counterparty.clone(),
            item.counterparty_receiver_path.clone(),
            header.event_id.clone(),
        );
        if !affected_event_keys.contains(&key) {
            continue;
        }
        carriers_by_key
            .entry(key)
            .or_default()
            .push(EventCarrier { item, header });
    }

    for (key, carriers) in carriers_by_key {
        rebuild_event_indexes(
            &key,
            &carriers,
            event_dedup_records,
            receipt_access_records,
            receipt_records,
        );
    }
}

fn migrate_legacy_allowance_items(
    stream_items: &mut [PrivateStreamItemRecord],
) -> HashSet<EventKey> {
    let mut affected_event_keys = HashSet::new();
    for item in stream_items {
        let Some((kind, classification)) = legacy_allowance_classification(item) else {
            continue;
        };
        item.known_paykit_kind = Some(kind.as_str().to_owned());
        item.parse_status = classification.status;
        item.parse_error = classification.parse_error;
        if let Some(event) = classification.event {
            affected_event_keys.insert((
                item.counterparty.clone(),
                item.counterparty_receiver_path.clone(),
                event.event_id,
            ));
        }
    }
    affected_event_keys
}

/// Recognize an item stored as an unknown kind whose exact header the current
/// classifier now knows as an Allowance kind.
fn legacy_allowance_classification(
    item: &PrivateStreamItemRecord,
) -> Option<(PrivateMessageKind, PrivateStreamMessageClassification)> {
    if item.known_paykit_kind.is_some()
        || item.parse_status != PrivateStreamParseStatus::UnknownKind
        || item.parse_error.is_some()
    {
        return None;
    }
    let (parsed_version, parsed_kind, known_kind) = private_message_header(&item.raw_json).ok()?;
    if parsed_version.is_none()
        || item.parsed_version != parsed_version
        || item.parsed_kind != parsed_kind
    {
        return None;
    }
    let kind = known_kind.filter(|kind| is_allowance_kind(kind.as_str()))?;
    let classification = classify_private_application_message(
        &private_application_message_from_raw(item.raw_json.clone(), parsed_version, parsed_kind),
    );
    Some((kind, classification))
}

/// Classify one stored item from its raw JSON header rather than its stored
/// metadata, which may be stale for legacy items.
fn classify_stream_item(
    item: &PrivateStreamItemRecord,
) -> Option<PrivateStreamMessageClassification> {
    let (parsed_version, parsed_kind, _) = private_message_header(&item.raw_json).ok()?;
    let mut classification = classify_private_application_message(
        &private_application_message_from_raw(item.raw_json.clone(), parsed_version, parsed_kind),
    );
    enforce_receipt_access_receiver_scope(&mut classification, &item.counterparty_receiver_path);
    Some(classification)
}

fn rebuild_event_indexes(
    key: &EventKey,
    carriers: &[EventCarrier<'_>],
    event_dedup_records: &mut HashMap<EventKey, EventDedupRecord>,
    receipt_access_records: &mut HashMap<EventKey, ReceiptAccessRecord>,
    receipt_records: &mut HashMap<EventKey, ReceiptRecord>,
) {
    let Some(first) = carriers.first() else {
        return;
    };
    let previous_first_stream_item_id = event_dedup_records
        .get(key)
        .map(|record| record.first_stream_item_id);
    let first_payload_hash = payload_hash(&first.item.raw_json);
    let mut duplicates = Vec::new();
    let mut conflicts = Vec::new();
    for carrier in &carriers[1..] {
        if payload_hash(&carrier.item.raw_json) == first_payload_hash {
            duplicates.push(carrier.item.stream_item_id);
        } else {
            conflicts.push(carrier.item.stream_item_id);
        }
    }
    event_dedup_records.insert(
        key.clone(),
        EventDedupRecord {
            counterparty: key.0.clone(),
            counterparty_receiver_path: key.1.clone(),
            event_id: key.2.clone(),
            event_kind: first.header.event_kind.clone(),
            payload_hash: first_payload_hash,
            first_stream_item_id: first.item.stream_item_id,
            duplicate_stream_item_ids: duplicates,
            conflicting_stream_item_ids: conflicts,
        },
    );
    if previous_first_stream_item_id != Some(first.item.stream_item_id) {
        // A changed first carrier is always a migrated Allowance item, so any
        // Receipt Access index for this Event ID is stale; strict validation
        // decides the rest.
        receipt_access_records.remove(key);
        receipt_records.retain(|_, record| {
            record.issuer != key.0
                || record.issuer_receiver_path != key.1
                || record.receipt_access_event_id != key.2
        });
    }
}
