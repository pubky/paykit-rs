//! Frozen classification-matrix fixture replay.
//!
//! The fixture pair under `fixtures/` pins the CURRENT four-boundary decision
//! for every private message shape: intake classification via real
//! `persist_private_stream_batch`, outbound validation, backup stream-item
//! validation, and the derived Event ID dedupe / Receipt Access indexes.
//! Later commits that change classifier output (for example parse-error
//! redaction) must re-freeze `classification_matrix_expected.json`
//! deliberately; the change-detector test prints the full actual decision set
//! on mismatch so the file can be regenerated from real output, never by hand.

use chrono::{TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    classify_private_application_message, enforce_receipt_access_receiver_scope,
    persist_private_stream_batch, private_application_message_from_raw, private_message_header,
};
use crate::storage::InMemoryStorage;
use crate::{PaykitReceiverPath, PubkyPublicKey};
use paykit_lib::PrivateApplicationMessage;

pub(crate) const CLASSIFICATION_MATRIX_EXPECTED_JSON: &str =
    include_str!("fixtures/classification_matrix_expected.json");

/// Frozen decision set of the PREVIOUS classifier generation: the pre-redaction
/// classifier whose parse errors embedded serde detail and interpolated values.
/// It is kept byte-identical as historical input: normalization proofs replay
/// its `parse_error` strings as stale persisted state, and the envelope
/// generation work uses it as the generation-1 fixture. The change detector
/// compares against `CLASSIFICATION_MATRIX_EXPECTED_JSON` only; this file must
/// never be re-frozen.
pub(crate) const CLASSIFICATION_MATRIX_EXPECTED_LEGACY_JSON: &str =
    include_str!("fixtures/classification_matrix_expected_legacy.json");

const CLASSIFICATION_MATRIX_MESSAGES_JSON: &str =
    include_str!("fixtures/classification_matrix_messages.json");

/// One frozen input message: a label plus the raw private message payload.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct ClassificationFixtureMessage {
    pub(crate) label: String,
    pub(crate) raw_json: String,
}

pub(crate) fn classification_fixture_messages() -> Vec<ClassificationFixtureMessage> {
    serde_json::from_str(CLASSIFICATION_MATRIX_MESSAGES_JSON)
        .expect("classification fixture messages must parse")
}

pub(crate) fn classification_fixture_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

// Every production decode derives the envelope header from the decrypted
// body; the replay reuses the SDK reconstruction of that derivation so header
// changes there also move the fixture, never a private copy of the logic.
pub(crate) fn fixture_private_message(raw_json: &str) -> PrivateApplicationMessage {
    let (parsed_version, parsed_kind, _) =
        private_message_header(raw_json).expect("header derivation is infallible today");
    private_application_message_from_raw(raw_json.to_owned(), parsed_version, parsed_kind)
}

/// Replay every fixture message through real intake, outbound validation, and
/// backup validation, and return the complete decision set as JSON.
pub(crate) async fn replay_classification_matrix() -> Value {
    let messages = classification_fixture_messages();
    let storage = InMemoryStorage::new();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let received_at = Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap();

    let report = persist_private_stream_batch(
        &storage,
        counterparty,
        classification_fixture_receiver_path(),
        messages
            .iter()
            .map(|message| fixture_private_message(&message.raw_json))
            .collect(),
        None,
        received_at,
    )
    .await
    .expect("fixture intake batch must persist");
    let state = storage.snapshot().expect("fixture snapshot must load");
    assert_eq!(state.private_stream_items.len(), messages.len());

    let mut decisions = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let item = &state.private_stream_items[index];
        assert_eq!(item.stream_item_id, index as u64);
        assert_eq!(item.raw_json, message.raw_json);

        // Post-scope event header, from the same classifier intake used.
        let mut classification =
            classify_private_application_message(&fixture_private_message(&message.raw_json));
        enforce_receipt_access_receiver_scope(
            &mut classification,
            &classification_fixture_receiver_path(),
        );

        let dedupe = state.event_dedup_records.values().find_map(|record| {
            if record.first_stream_item_id == item.stream_item_id {
                Some("first")
            } else if record
                .duplicate_stream_item_ids
                .contains(&item.stream_item_id)
            {
                Some("duplicate")
            } else if record
                .conflicting_stream_item_ids
                .contains(&item.stream_item_id)
            {
                Some("conflict")
            } else {
                None
            }
        });
        let receipt_access_indexed = state
            .receipt_access_records
            .values()
            .any(|record| record.stream_item_id == item.stream_item_id);

        let outbound = match crate::domain::outbound_private::validate_outbound_private_message(
            &message.raw_json,
        ) {
            Ok(kind) => json!({ "ok": kind }),
            Err(err) => json!({ "error": err.to_string() }),
        };
        let backup = match crate::backup::validate_private_stream_items(std::slice::from_ref(item))
        {
            Ok(()) => json!({ "ok": true }),
            Err(err) => json!({ "error": err.to_string() }),
        };

        decisions.push(json!({
            "label": message.label,
            "intake": {
                "parsed_version": item.parsed_version,
                "parsed_kind": item.parsed_kind,
                "known_paykit_kind": item.known_paykit_kind,
                "parse_status": item.parse_status,
                "parse_error": item.parse_error,
                "event_id": classification.event.as_ref().map(|event| event.event_id.clone()),
                "event_kind": classification.event.as_ref().map(|event| event.event_kind.clone()),
                "dedupe": dedupe,
                "receipt_access_indexed": receipt_access_indexed,
            },
            "outbound": outbound,
            "backup": backup,
        }));
    }

    let mut event_dedup_records: Vec<_> = state.event_dedup_records.values().collect();
    event_dedup_records.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    let event_dedup_records: Vec<Value> = event_dedup_records
        .into_iter()
        .map(|record| {
            json!({
                "event_id": record.event_id,
                "event_kind": record.event_kind,
                "payload_hash": record.payload_hash,
                "first_stream_item_id": record.first_stream_item_id,
                "duplicate_stream_item_ids": record.duplicate_stream_item_ids,
                "conflicting_stream_item_ids": record.conflicting_stream_item_ids,
            })
        })
        .collect();

    let mut receipt_access_records: Vec<_> = state.receipt_access_records.values().collect();
    receipt_access_records.sort_by(|a, b| a.event_id.cmp(&b.event_id));
    let receipt_access_records: Vec<Value> = receipt_access_records
        .into_iter()
        .map(|record| {
            json!({
                "event_id": record.event_id,
                "receipt_id": record.receipt_id,
                "payment_reference": record.payment_reference,
                "payment_request_id": record.payment_request_id,
                "billing_period": record.billing_period,
                "location": record.location,
                "key": record.key,
                "stream_item_id": record.stream_item_id,
                "receive_batch_id": record.receive_batch_id,
                "retrieval_status": record.retrieval_status,
            })
        })
        .collect();

    let event_conflicts: Vec<Value> = report
        .event_conflicts
        .iter()
        .map(|conflict| {
            json!({
                "event_id": conflict.event_id,
                "first_stream_item_id": conflict.first_stream_item_id,
                "conflicting_stream_item_id": conflict.conflicting_stream_item_id,
            })
        })
        .collect();

    json!({
        "messages": decisions,
        "intake_event_conflicts": event_conflicts,
        "event_dedup_records": event_dedup_records,
        "receipt_access_records": receipt_access_records,
    })
}
