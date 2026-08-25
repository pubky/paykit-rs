//! Classification normalization for persisted private stream items.
//!
//! Security assumptions:
//! - `raw_json` plus the immutable per-item source context (counterparty,
//!   receiver path, stream and batch ids, receive time) is the durable source
//!   of truth. Every derived classification field and every Event ID dedupe or
//!   Receipt Access index entry is rebuildable from it with the current
//!   classifier.
//! - Normalization never rewrites `raw_json` or the immutable source context;
//!   it only rewrites derived classification fields and derived indexes, and
//!   only through the narrowly scoped [`StorageTransaction`] mutators.
//! - Receipt Access retrieval state (status, timestamps, last error) is local
//!   and cannot be rebuilt from raw data. It is preserved only while the same
//!   source event remains authoritative for the record; otherwise it is reset
//!   to pending so stale local state can never claim authority over re-derived
//!   access data.
//! - Cached decrypted Receipt records are not rebuildable either (the Receipt
//!   lives on the issuer's remote storage). A cached Receipt is dropped when
//!   normalization removes or rewrites the Receipt Access record it depends
//!   on and the Receipt no longer matches it: the plaintext was obtained
//!   under an authority the current classifier no longer derives, so failing
//!   closed means discarding it rather than exporting un-restorable state.
//!   When the surviving access record still matches, retrieval can fetch the
//!   Receipt again. Inconsistencies that predate normalization are left in
//!   place so restore validation still rejects them.

use std::collections::HashMap;

use crate::{
    domain::receipts::{receipt_record_matches_access, ReceiptAccessRecord, ReceiptRecord},
    storage::{
        EventDedupRecord, PrivateStreamItemClassificationUpdate, PrivateStreamItemRecord,
        StorageTransaction,
    },
    PaykitReceiverPath, PubkyPublicKey, Result,
};

use super::{
    classify_private_application_message, enforce_receipt_access_receiver_scope, payload_hash,
    private_application_message_from_raw, private_message_header,
    PrivateStreamMessageClassification,
};

pub(crate) type EventStorageKey = (PubkyPublicKey, PaykitReceiverPath, String);
pub(crate) type ReceiptStorageKey = (PubkyPublicKey, PaykitReceiverPath, String);

/// Counts-only summary of one private stream normalization pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PrivateStreamNormalizationReport {
    /// Stream items whose derived classification fields were rewritten.
    pub(crate) items_reclassified: usize,
    /// Event dedupe records written because they were missing or stale.
    pub(crate) event_dedup_records_rewritten: usize,
    /// Event dedupe records removed because no stream item carries the event.
    pub(crate) event_dedup_records_removed: usize,
    /// Receipt Access records written because they were missing or stale.
    pub(crate) receipt_access_records_rewritten: usize,
    /// Receipt Access records removed because no stream item carries the event.
    pub(crate) receipt_access_records_removed: usize,
    /// Receipt Access records whose local retrieval state was reset because
    /// the previously indexed source event is no longer authoritative.
    pub(crate) receipt_retrieval_states_reset: usize,
    /// Cached Receipt records dropped because normalization removed or
    /// rewrote the Receipt Access record they were decrypted under.
    pub(crate) receipt_records_removed: usize,
}

/// Expected derived state computed from raw stream items alone.
pub(crate) struct PrivateStreamNormalizationOutcome {
    /// Classification updates for items whose derived fields are stale.
    pub(crate) item_updates: Vec<PrivateStreamItemClassificationUpdate>,
    /// Complete expected Event dedupe index.
    pub(crate) expected_event_dedup_records: HashMap<EventStorageKey, EventDedupRecord>,
    /// Complete expected Receipt Access index.
    pub(crate) expected_receipt_access_records: HashMap<EventStorageKey, ReceiptAccessRecord>,
    /// Cached Receipt records orphaned by access-index reconciliation.
    pub(crate) removed_receipt_record_keys: Vec<ReceiptStorageKey>,
    /// Counts describing the difference from the given stored state.
    pub(crate) report: PrivateStreamNormalizationReport,
}

/// Compute the expected derived classification state for stored stream items.
///
/// Pure function of the given records: items are reclassified from `raw_json`
/// only, in ascending stream item order, byte-equivalent to intake
/// classification and Event ID dedupe folding. Existing records are used only
/// to decide whether Receipt Access retrieval state may be preserved, which
/// cached Receipt records the reconciled access index orphans, and to fill in
/// the report counts.
pub(crate) fn compute_private_stream_normalization(
    items: &[PrivateStreamItemRecord],
    event_dedup_records: &HashMap<EventStorageKey, EventDedupRecord>,
    receipt_access_records: &HashMap<EventStorageKey, ReceiptAccessRecord>,
    receipt_records: &HashMap<ReceiptStorageKey, ReceiptRecord>,
) -> PrivateStreamNormalizationOutcome {
    let mut ordered: Vec<&PrivateStreamItemRecord> = items.iter().collect();
    ordered.sort_by_key(|item| item.stream_item_id);

    let mut report = PrivateStreamNormalizationReport::default();
    let mut item_updates = Vec::new();
    let mut expected_event_dedup_records: HashMap<EventStorageKey, EventDedupRecord> =
        HashMap::new();
    let mut expected_receipt_access_records: HashMap<EventStorageKey, ReceiptAccessRecord> =
        HashMap::new();

    for item in ordered {
        // Infallible today: header parse failures classify as no header.
        let (parsed_version, parsed_kind, known_kind) =
            private_message_header(&item.raw_json).unwrap_or((None, None, None));
        let mut classification =
            classify_private_application_message(&private_application_message_from_raw(
                item.raw_json.clone(),
                parsed_version,
                parsed_kind.clone(),
            ));
        enforce_receipt_access_receiver_scope(
            &mut classification,
            &item.counterparty_receiver_path,
        );
        let PrivateStreamMessageClassification {
            status,
            parse_error,
            event,
            receipt_access,
        } = classification;
        let known_paykit_kind = known_kind.map(|kind| kind.as_str().to_owned());

        if item.parsed_version != parsed_version
            || item.parsed_kind != parsed_kind
            || item.known_paykit_kind != known_paykit_kind
            || item.parse_status != status
            || item.parse_error != parse_error
        {
            item_updates.push(PrivateStreamItemClassificationUpdate {
                stream_item_id: item.stream_item_id,
                parsed_version,
                parsed_kind,
                known_paykit_kind,
                parse_status: status,
                parse_error,
            });
        }

        let Some(event) = event else {
            continue;
        };
        let key = (
            item.counterparty.clone(),
            item.counterparty_receiver_path.clone(),
            event.event_id.clone(),
        );
        let item_hash = payload_hash(&item.raw_json);
        if let Some(record) = expected_event_dedup_records.get_mut(&key) {
            if record.payload_hash == item_hash {
                record.duplicate_stream_item_ids.push(item.stream_item_id);
            } else {
                record.conflicting_stream_item_ids.push(item.stream_item_id);
            }
            continue;
        }
        expected_event_dedup_records.insert(
            key.clone(),
            EventDedupRecord {
                counterparty: item.counterparty.clone(),
                counterparty_receiver_path: item.counterparty_receiver_path.clone(),
                event_id: event.event_id,
                event_kind: event.event_kind,
                payload_hash: item_hash,
                first_stream_item_id: item.stream_item_id,
                duplicate_stream_item_ids: Vec::new(),
                conflicting_stream_item_ids: Vec::new(),
            },
        );
        if let Some(access) = receipt_access.as_ref() {
            let rebuilt = ReceiptAccessRecord::from_access(
                item.counterparty.clone(),
                item.counterparty_receiver_path.clone(),
                item.stream_item_id,
                item.receive_batch_id,
                item.received_at,
                access,
            );
            let expected = match receipt_access_records.get(&key) {
                Some(existing) if receipt_access_authority_unchanged(existing, &rebuilt) => {
                    existing.clone()
                }
                Some(_) => {
                    report.receipt_retrieval_states_reset += 1;
                    rebuilt
                }
                None => rebuilt,
            };
            expected_receipt_access_records.insert(key, expected);
        }
    }

    // A cached Receipt stays only while it matches a surviving access record.
    // Dropping is limited to receipts orphaned by this reconciliation;
    // divergence the access index never explains is left for validation.
    let mut removed_receipt_record_keys = Vec::new();
    for (key, record) in receipt_records {
        let access_key = (
            record.issuer.clone(),
            record.issuer_receiver_path.clone(),
            record.receipt_access_event_id.clone(),
        );
        let expected = expected_receipt_access_records.get(&access_key);
        if let Some(access) = expected {
            if receipt_record_matches_access(record, access) {
                continue;
            }
        }
        if expected != receipt_access_records.get(&access_key) {
            removed_receipt_record_keys.push(key.clone());
        }
    }
    report.receipt_records_removed = removed_receipt_record_keys.len();

    report.items_reclassified = item_updates.len();
    report.event_dedup_records_rewritten = expected_event_dedup_records
        .iter()
        .filter(|(key, expected)| event_dedup_records.get(*key) != Some(expected))
        .count();
    report.event_dedup_records_removed = event_dedup_records
        .keys()
        .filter(|key| !expected_event_dedup_records.contains_key(*key))
        .count();
    report.receipt_access_records_rewritten = expected_receipt_access_records
        .iter()
        .filter(|(key, expected)| receipt_access_records.get(*key) != Some(expected))
        .count();
    report.receipt_access_records_removed = receipt_access_records
        .keys()
        .filter(|key| !expected_receipt_access_records.contains_key(*key))
        .count();

    PrivateStreamNormalizationOutcome {
        item_updates,
        expected_event_dedup_records,
        expected_receipt_access_records,
        removed_receipt_record_keys,
        report,
    }
}

// "Same source event remains authoritative": the record still points at the
// rebuilt first carrier and every access-derived field byte-equals
// re-derivation from that carrier's raw payload.
fn receipt_access_authority_unchanged(
    existing: &ReceiptAccessRecord,
    rebuilt: &ReceiptAccessRecord,
) -> bool {
    existing.stream_item_id == rebuilt.stream_item_id
        && existing.receipt_id == rebuilt.receipt_id
        && existing.payment_reference == rebuilt.payment_reference
        && existing.payment_request_id == rebuilt.payment_request_id
        && existing.billing_period == rebuilt.billing_period
        && existing.location == rebuilt.location
        && existing.key == rebuilt.key
}

/// Normalize stored derived classification state inside one transaction.
///
/// Rewrites only derived data that differs from re-derivation, through the
/// narrowly scoped classification mutators, and fails closed with
/// [`crate::PaykitSdkError::Storage`] when an item update cannot be applied.
pub(crate) fn normalize_private_stream_classifications(
    tx: &mut dyn StorageTransaction,
) -> Result<PrivateStreamNormalizationReport> {
    let state = tx.export_storage_state();
    let PrivateStreamNormalizationOutcome {
        item_updates,
        expected_event_dedup_records,
        expected_receipt_access_records,
        removed_receipt_record_keys,
        report,
    } = compute_private_stream_normalization(
        &state.private_stream_items,
        &state.event_dedup_records,
        &state.receipt_access_records,
        &state.receipt_records,
    );

    for update in item_updates {
        tx.update_private_stream_item_classification(update)?;
    }
    for (key, expected) in &expected_event_dedup_records {
        if state.event_dedup_records.get(key) != Some(expected) {
            tx.save_event_dedup_record(expected.clone());
        }
    }
    for key in state.event_dedup_records.keys() {
        if !expected_event_dedup_records.contains_key(key) {
            tx.remove_event_dedup_record(&key.0, &key.1, &key.2);
        }
    }
    for (key, expected) in &expected_receipt_access_records {
        if state.receipt_access_records.get(key) != Some(expected) {
            tx.save_receipt_access_record(expected.clone());
        }
    }
    for key in state.receipt_access_records.keys() {
        if !expected_receipt_access_records.contains_key(key) {
            tx.remove_receipt_access_record(&key.0, &key.1, &key.2);
        }
    }
    for key in &removed_receipt_record_keys {
        tx.remove_receipt_record(&key.0, &key.1, &key.2);
    }

    Ok(report)
}
