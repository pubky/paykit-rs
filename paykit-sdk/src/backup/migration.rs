//! Compatibility migrations applied before strict backup validation.

use std::collections::{HashMap, HashSet};

use paykit_lib::{PaykitReceiverPath, PrivateApplicationMessage, PrivateMessageKind};

use crate::{
    domain::{
        private_stream::{
            classify_private_application_message, enforce_receipt_access_receiver_scope,
            payload_hash, PrivateStreamMessageClassification, PrivateStreamParseStatus,
        },
        receipts::{ReceiptAccessRecord, ReceiptRecord},
    },
    storage::{EventDedupRecord, PrivateStreamItemRecord},
    PubkyPublicKey,
};

type EventKey = (PubkyPublicKey, PaykitReceiverPath, String);
type ReceiptKey = (PubkyPublicKey, PaykitReceiverPath, String);

pub(super) fn migrate_legacy_allowance_stream_state(
    stream_items: &mut [PrivateStreamItemRecord],
    event_dedup_records: &mut HashMap<EventKey, EventDedupRecord>,
    receipt_access_records: &mut HashMap<EventKey, ReceiptAccessRecord>,
    receipt_records: &mut HashMap<ReceiptKey, ReceiptRecord>,
) {
    let affected_event_keys = migrate_legacy_allowance_items(stream_items);
    for key in sorted_event_keys(affected_event_keys) {
        rebuild_event_indexes(
            &key,
            stream_items,
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

fn legacy_allowance_classification(
    item: &PrivateStreamItemRecord,
) -> Option<(PrivateMessageKind, PrivateStreamMessageClassification)> {
    if item.known_paykit_kind.is_some()
        || item.parse_status != PrivateStreamParseStatus::UnknownKind
        || item.parse_error.is_some()
    {
        return None;
    }
    let message = private_application_message(&item.raw_json)?;
    if message.version.is_none()
        || item.parsed_version != message.version.map(u32::from)
        || item.parsed_kind.as_deref() != message.kind.as_deref()
    {
        return None;
    }
    let kind = message.known_kind()?;
    if !is_allowance_kind(kind) {
        return None;
    }
    Some((kind, classify_private_application_message(&message)))
}

fn private_application_message(raw_json: &str) -> Option<PrivateApplicationMessage> {
    let value = serde_json::from_str::<serde_json::Value>(raw_json).ok()?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u8::try_from(version).ok());
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Some(PrivateApplicationMessage {
        version,
        kind,
        raw_json: raw_json.to_owned(),
    })
}

fn is_allowance_kind(kind: PrivateMessageKind) -> bool {
    matches!(
        kind,
        PrivateMessageKind::AllowanceProposal
            | PrivateMessageKind::AllowanceAcceptance
            | PrivateMessageKind::AllowanceRejection
            | PrivateMessageKind::AllowanceEnd
    )
}

fn sorted_event_keys(keys: HashSet<EventKey>) -> Vec<EventKey> {
    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_by(|left, right| {
        left.0
            .as_str()
            .cmp(right.0.as_str())
            .then(left.1.as_str().cmp(right.1.as_str()))
            .then(left.2.cmp(&right.2))
    });
    keys
}

fn rebuild_event_indexes(
    key: &EventKey,
    stream_items: &[PrivateStreamItemRecord],
    event_dedup_records: &mut HashMap<EventKey, EventDedupRecord>,
    receipt_access_records: &mut HashMap<EventKey, ReceiptAccessRecord>,
    receipt_records: &mut HashMap<ReceiptKey, ReceiptRecord>,
) {
    let previous_first_stream_item_id = event_dedup_records
        .get(key)
        .map(|record| record.first_stream_item_id);
    let Some(record) = rebuilt_event_dedup_record(key, stream_items) else {
        return;
    };
    let first_stream_item_id = record.first_stream_item_id;
    event_dedup_records.insert(key.clone(), record);
    if previous_first_stream_item_id != Some(first_stream_item_id) {
        reconcile_receipt_indexes(
            key,
            first_stream_item_id,
            stream_items,
            receipt_access_records,
            receipt_records,
        );
    }
}

fn rebuilt_event_dedup_record(
    key: &EventKey,
    stream_items: &[PrivateStreamItemRecord],
) -> Option<EventDedupRecord> {
    let mut matching_events = stream_items
        .iter()
        .filter_map(|item| event_for_key(item, key).map(|event| (item, event)))
        .collect::<Vec<_>>();
    matching_events.sort_by_key(|(item, _)| item.stream_item_id);
    let (first_item, first_event) = matching_events.first()?;
    let first_payload_hash = payload_hash(&first_item.raw_json);
    let mut duplicates = Vec::new();
    let mut conflicts = Vec::new();
    for (item, event) in matching_events.iter().skip(1) {
        if event.event_kind == first_event.event_kind
            && payload_hash(&item.raw_json) == first_payload_hash
        {
            duplicates.push(item.stream_item_id);
        } else {
            conflicts.push(item.stream_item_id);
        }
    }
    Some(EventDedupRecord {
        counterparty: key.0.clone(),
        counterparty_receiver_path: key.1.clone(),
        event_id: key.2.clone(),
        event_kind: first_event.event_kind.clone(),
        payload_hash: first_payload_hash,
        first_stream_item_id: first_item.stream_item_id,
        duplicate_stream_item_ids: duplicates,
        conflicting_stream_item_ids: conflicts,
    })
}

fn event_for_key(
    item: &PrivateStreamItemRecord,
    key: &EventKey,
) -> Option<crate::domain::private_stream::PrivateStreamEventHeader> {
    if item.counterparty != key.0 || item.counterparty_receiver_path != key.1 {
        return None;
    }
    let classification = classify_stream_item(item)?;
    classification.event.filter(|event| event.event_id == key.2)
}

fn classify_stream_item(
    item: &PrivateStreamItemRecord,
) -> Option<PrivateStreamMessageClassification> {
    let message = private_application_message(&item.raw_json)?;
    let mut classification = classify_private_application_message(&message);
    enforce_receipt_access_receiver_scope(&mut classification, &item.counterparty_receiver_path);
    Some(classification)
}

fn reconcile_receipt_indexes(
    key: &EventKey,
    first_stream_item_id: u64,
    stream_items: &[PrivateStreamItemRecord],
    receipt_access_records: &mut HashMap<EventKey, ReceiptAccessRecord>,
    receipt_records: &mut HashMap<ReceiptKey, ReceiptRecord>,
) {
    let previous_access = receipt_access_records.remove(key);
    let new_access = first_receipt_access(key, first_stream_item_id, stream_items);
    let descriptor_unchanged = previous_access
        .as_ref()
        .zip(new_access.as_ref())
        .is_some_and(|(previous, current)| receipt_access_descriptor_matches(previous, current));
    if !descriptor_unchanged {
        remove_cached_receipts(key, receipt_records);
    }
    if let Some(mut access) = new_access {
        if let Some(previous) = previous_access.filter(|_| descriptor_unchanged) {
            preserve_receipt_retrieval_state(&mut access, &previous);
        }
        receipt_access_records.insert(key.clone(), access);
    }
}

fn first_receipt_access(
    key: &EventKey,
    first_stream_item_id: u64,
    stream_items: &[PrivateStreamItemRecord],
) -> Option<ReceiptAccessRecord> {
    let item = stream_items
        .iter()
        .find(|item| item.stream_item_id == first_stream_item_id)?;
    let classification = classify_stream_item(item)?;
    let access = classification.receipt_access?;
    Some(ReceiptAccessRecord::from_access(
        key.0.clone(),
        key.1.clone(),
        item.stream_item_id,
        item.receive_batch_id,
        item.received_at,
        &access,
    ))
}

fn receipt_access_descriptor_matches(
    previous: &ReceiptAccessRecord,
    current: &ReceiptAccessRecord,
) -> bool {
    previous.event_id == current.event_id
        && previous.receipt_id == current.receipt_id
        && previous.payment_reference == current.payment_reference
        && previous.payment_request_id == current.payment_request_id
        && previous.billing_period == current.billing_period
        && previous.location == current.location
        && previous.key == current.key
}

fn preserve_receipt_retrieval_state(
    current: &mut ReceiptAccessRecord,
    previous: &ReceiptAccessRecord,
) {
    current.retrieval_status = previous.retrieval_status;
    current.retrieval_attempted_at = previous.retrieval_attempted_at;
    current.retrieved_at = previous.retrieved_at;
    current.last_retrieval_error = previous.last_retrieval_error.clone();
}

fn remove_cached_receipts(
    key: &EventKey,
    receipt_records: &mut HashMap<ReceiptKey, ReceiptRecord>,
) {
    receipt_records.retain(|_, record| {
        record.issuer != key.0
            || record.issuer_receiver_path != key.1
            || record.receipt_access_event_id != key.2
    });
}
