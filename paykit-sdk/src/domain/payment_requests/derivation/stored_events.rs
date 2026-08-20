use super::{
    reducer::{apply_stored_event, mark_invalid_stored, proposal_expired, record_for},
    *,
};

#[derive(Clone)]
pub(super) enum StoredPaymentRequestEvent {
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
    pub(super) fn record_time(&self) -> DateTime<Utc> {
        match self {
            Self::Received { item, .. } => item.received_at,
            Self::Outbound { message, .. } => message.created_at,
        }
    }

    fn phase_order(&self) -> u8 {
        match self.event() {
            PaymentRequestEvent::Request(_) => 0,
            PaymentRequestEvent::Acceptance(_)
            | PaymentRequestEvent::Rejection(_)
            | PaymentRequestEvent::Cancellation(_)
            | PaymentRequestEvent::Proof(_) => 1,
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

    pub(super) fn event(&self) -> &PaymentRequestEvent {
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

    pub(super) fn app_id(&self) -> Option<paykit_lib::PaykitAppId> {
        match self {
            Self::Received { item, .. } => item
                .parsed_app_id
                .as_ref()
                .and_then(|app_id| paykit_lib::PaykitAppId::new(app_id).ok()),
            Self::Outbound { message, .. } => Some(message.app_id.clone()),
        }
    }
}

pub(crate) fn derive_payment_request_records_from_parts(
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
            app_id: app_id_from_raw_json(&message.raw_json),
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
    if a.source_rank() == b.source_rank() {
        return a
            .source_order()
            .cmp(&b.source_order())
            .then_with(|| a.record_time().cmp(&b.record_time()));
    }

    let phase_order = a.phase_order().cmp(&b.phase_order());
    if phase_order != Ordering::Equal {
        return phase_order;
    }

    a.record_time()
        .cmp(&b.record_time())
        .then_with(|| a.source_rank().cmp(&b.source_rank()))
        .then_with(|| a.source_order().cmp(&b.source_order()))
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

pub(super) fn payment_request_message_from_item(
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
        app_id: item.parsed_app_id.clone(),
        raw_json: item.raw_json.clone(),
    })
}

fn app_id_from_raw_json(raw_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw_json)
        .ok()?
        .get("app_id")?
        .as_str()
        .map(str::to_owned)
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
