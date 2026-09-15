use super::super::*;

pub(in crate::backup) fn validate_private_stream_items(
    records: &[PrivateStreamItemRecord],
) -> Result<()> {
    for record in records {
        let (parsed_version, parsed_kind, parsed_app_id, known_kind) =
            private_message_header(&record.raw_json);
        let classification =
            classify_private_application_message(&private_application_message_from_raw(
                record.raw_json.clone(),
                parsed_version,
                parsed_kind.clone(),
            ));
        if record.parsed_version != parsed_version {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parsed version metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parsed_kind.as_deref() != parsed_kind.as_deref() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parsed kind metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parsed_app_id.as_deref() != parsed_app_id.as_deref() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale App ID metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.known_paykit_kind.as_deref() != known_kind.map(PrivateMessageKind::as_str) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale known kind metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_status != classification.status {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parse status metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_error.as_deref() != classification.parse_error.as_deref() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parse error metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_status == PrivateStreamParseStatus::Valid {
            let Some(kind) = known_kind else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "private stream item {} is marked valid without a recognized Paykit kind",
                        record.stream_item_id
                    ),
                    source: None,
                });
            };
            validate_valid_private_stream_body(record, kind)?;
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_event_dedup_records(
    records: &HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        validate_event_dedup_membership(record)?;
        let Some(first) = stream_by_id.get(&record.first_stream_item_id) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' references missing first stream item {}",
                    record.event_id, record.first_stream_item_id
                ),
                source: None,
            });
        };
        if first.counterparty != record.counterparty {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' counterparty does not match first stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        if payload_hash(&first.raw_json) != record.payload_hash {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' payload hash does not match first stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        validate_event_dedup_stream_item(record, first, EventDedupeItemKind::First)?;
        for stream_item_id in &record.duplicate_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' references missing stream item {}",
                        record.event_id, stream_item_id
                    ),
                    source: None,
                });
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Duplicate)?;
        }
        for stream_item_id in &record.conflicting_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' references missing stream item {}",
                        record.event_id, stream_item_id
                    ),
                    source: None,
                });
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Conflict)?;
        }
    }
    Ok(())
}

fn validate_event_dedup_membership(record: &EventDedupRecord) -> Result<()> {
    let mut seen = HashSet::new();
    seen.insert(record.first_stream_item_id);
    for stream_item_id in record
        .duplicate_stream_item_ids
        .iter()
        .chain(record.conflicting_stream_item_ids.iter())
    {
        if !seen.insert(*stream_item_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' references stream item {} more than once",
                    record.event_id, stream_item_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EventDedupeItemKind {
    First,
    Duplicate,
    Conflict,
}

fn validate_event_dedup_stream_item(
    record: &EventDedupRecord,
    item: &PrivateStreamItemRecord,
    item_kind: EventDedupeItemKind,
) -> Result<()> {
    if item.counterparty != record.counterparty {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' counterparty does not match stream item {}",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    }

    let classification =
        classify_private_application_message(&private_application_message_from_raw(
            item.raw_json.clone(),
            item.parsed_version,
            item.parsed_kind.clone(),
        ));
    let Some(event) = classification.event else {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' references non-event stream item {}",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    };
    if event.event_id != record.event_id {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' does not match stream item {} event header",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    }

    let item_hash = payload_hash(&item.raw_json);
    match item_kind {
        EventDedupeItemKind::First | EventDedupeItemKind::Duplicate => {
            if event.event_kind != record.event_kind {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                    "Event dedupe record '{}' same-payload stream item {} has different event kind",
                    record.event_id, item.stream_item_id
                ),
                    source: None,
                });
            }
            if item_hash != record.payload_hash {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "Event dedupe record '{}' same-payload stream item {} has different payload hash",
                    record.event_id, item.stream_item_id
                ), source: None });
            }
        }
        EventDedupeItemKind::Conflict => {
            if item_hash == record.payload_hash {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' conflict stream item {} has same payload hash",
                        record.event_id, item.stream_item_id
                    ),
                    source: None,
                });
            }
        }
    }

    Ok(())
}
pub(in crate::backup) fn validate_required_private_stream_indexes(
    stream_items: &[PrivateStreamItemRecord],
    event_dedup_records: &HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    receipt_access_records: &HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
) -> Result<()> {
    for item in stream_items {
        let classification =
            classify_private_application_message(&private_application_message_from_raw(
                item.raw_json.clone(),
                item.parsed_version,
                item.parsed_kind.clone(),
            ));
        let Some(event) = classification.event else {
            continue;
        };
        let key = (item.counterparty.clone(), event.event_id.clone());
        let Some(dedupe) = event_dedup_records.get(&key) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} is missing required Event dedupe record '{}'",
                    item.stream_item_id, event.event_id
                ),
                source: None,
            });
        };
        if !event_dedup_record_contains_stream_event(dedupe, item, &event.event_kind) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' does not include private stream item {}",
                    event.event_id, item.stream_item_id
                ),
                source: None,
            });
        }
        if classification.receipt_access.is_some()
            && dedupe.first_stream_item_id == item.stream_item_id
            && !receipt_access_records.contains_key(&key)
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access event '{}' is missing required Receipt Access record",
                    event.event_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

fn event_dedup_record_contains_stream_event(
    record: &EventDedupRecord,
    item: &PrivateStreamItemRecord,
    event_kind: &str,
) -> bool {
    let item_hash = payload_hash(&item.raw_json);
    if record.first_stream_item_id == item.stream_item_id {
        return event_kind == record.event_kind && item_hash == record.payload_hash;
    }
    if record
        .duplicate_stream_item_ids
        .contains(&item.stream_item_id)
    {
        return event_kind == record.event_kind && item_hash == record.payload_hash;
    }
    if record
        .conflicting_stream_item_ids
        .contains(&item.stream_item_id)
    {
        return item_hash != record.payload_hash;
    }
    false
}
pub(in crate::backup) type PrivateMessageHeader = (
    Option<u32>,
    Option<String>,
    Option<String>,
    Option<PrivateMessageKind>,
);

pub(in crate::backup) fn private_message_header(raw_json: &str) -> PrivateMessageHeader {
    let value = match serde_json::from_str::<serde_json::Value>(raw_json) {
        Ok(value) => value,
        Err(_) => return (None, None, None, None),
    };
    let parsed_version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u8::try_from(version).ok())
        .map(u32::from);
    let parsed_kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let parsed_app_id = value
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let known_kind = parsed_kind.as_deref().and_then(PrivateMessageKind::parse);
    (parsed_version, parsed_kind, parsed_app_id, known_kind)
}

fn validate_valid_private_stream_body(
    record: &PrivateStreamItemRecord,
    kind: PrivateMessageKind,
) -> Result<()> {
    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            paykit_lib::parse_private_payment_list_json(&record.raw_json)?;
        }
        PrivateMessageKind::ReceiptAccess => {
            let event = paykit_lib::parse_receipt_access_event_message(
                &private_application_message(record, kind),
            )
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} Receipt Access payload does not match its kind",
                    record.stream_item_id
                ),
                source: None,
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol {
                    context: error.to_owned(),
                    source: None,
                });
            }
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            let event = paykit_lib::parse_payment_request_event_message(
                &private_application_message(record, kind),
            )
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} Payment Request payload does not match its kind",
                    record.stream_item_id
                ),
                source: None,
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol {
                    context: error.to_owned(),
                    source: None,
                });
            }
        }
    }
    Ok(())
}

pub(in crate::backup) fn private_application_message(
    record: &PrivateStreamItemRecord,
    kind: PrivateMessageKind,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: record
            .parsed_version
            .and_then(|version| u8::try_from(version).ok()),
        kind: Some(kind.as_str().to_owned()),
        app_id: record.parsed_app_id.clone(),
        raw_json: record.raw_json.clone(),
    }
}

pub(in crate::backup) fn private_application_message_from_raw(
    raw_json: String,
    parsed_version: Option<u32>,
    parsed_kind: Option<String>,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: parsed_version.and_then(|version| u8::try_from(version).ok()),
        kind: parsed_kind,
        app_id: app_id_from_raw_json(&raw_json),
        raw_json,
    }
}

pub(in crate::backup) fn app_id_from_raw_json(raw_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_json)
        .ok()?
        .get("app_id")?
        .as_str()
        .map(str::to_owned)
}
