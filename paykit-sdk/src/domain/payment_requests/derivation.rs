use super::*;

/// Derive received Payment Request lifecycle records for one counterparty.
///
/// Records are derived from the persisted inbound private stream and returned
/// newest-first by the last applied stream item. Malformed recognized Payment
/// Request events without a valid `payment_request_id` remain available in the
/// raw private stream log but cannot be attached to a request-scoped record.
pub(crate) async fn received_payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    let (items, dedupe_records, outbound_carriers) = storage
        .transaction(|tx| {
            let items = tx.private_stream_items(counterparty, counterparty_receiver_path);
            let outbound = tx.outbound_private_messages(counterparty, counterparty_receiver_path);
            let outbound_carriers = outbound_event_carriers(&outbound);
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
                if let Some(record) = tx.event_dedup_record(
                    counterparty,
                    counterparty_receiver_path,
                    event_id.as_str(),
                ) {
                    dedupe_records.insert(event_id.as_str().to_owned(), record);
                }
            }
            Ok((items, dedupe_records, outbound_carriers))
        })
        .await?;
    derive_received_payment_request_records(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        items,
        dedupe_records,
        outbound_carriers,
        now,
    )
}

/// Derive local Payment Request records for one counterparty.
///
/// Records merge received Payment Request events from the private stream with
/// local outbound Payment Request events from the outbound private-message log.
/// Returned records are newest-first by local record time.
pub(crate) async fn payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            payment_request_records_in(tx, counterparty, counterparty_receiver_path, now)
        })
        .await
}

pub(crate) fn payment_request_records_in(
    tx: &dyn crate::storage::StorageTransaction,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>> {
    let items = tx.private_stream_items(counterparty, counterparty_receiver_path);
    let outbound = tx.outbound_private_messages(counterparty, counterparty_receiver_path);
    let outbound_carriers = outbound_event_carriers(&outbound);
    let received_event_ids = items
        .iter()
        .filter_map(payment_request_message_from_item)
        .filter_map(|message| parse_payment_request_event_message(&message))
        .filter_map(|parsed| parsed.event_id().map(|id| id.as_str().to_owned()))
        .collect::<HashSet<_>>();
    let mut dedupe_records = HashMap::new();
    for event_id in outbound_carriers
        .event_ids
        .iter()
        .chain(&received_event_ids)
    {
        if let Some(record) =
            tx.event_dedup_record(counterparty, counterparty_receiver_path, event_id)
        {
            dedupe_records.insert(event_id.clone(), record);
        }
    }
    derive_payment_request_records(
        counterparty.clone(),
        counterparty_receiver_path.clone(),
        items,
        outbound,
        dedupe_records,
        outbound_carriers,
        now,
    )
}

fn derive_received_payment_request_records(
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    mut items: Vec<PrivateStreamItemRecord>,
    dedupe_records: HashMap<String, EventDedupRecord>,
    outbound_carriers: OutboundEventCarriers,
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
                        record_for(
                            &mut records,
                            &counterparty,
                            &counterparty_receiver_path,
                            payment_request_id,
                        )
                        .mark_invalid(&item, "Event ID reused with different payload");
                    }
                    continue;
                }
            }
            if outbound_carriers.event_ids.contains(event_id) {
                if let Some(payment_request_id) = payment_request_id {
                    record_for(
                        &mut records,
                        &counterparty,
                        &counterparty_receiver_path,
                        payment_request_id,
                    )
                    .mark_invalid(&item, "Event ID reused by another Event Message carrier");
                }
                continue;
            }
        }

        let Some(payment_request_id) = payment_request_id else {
            continue;
        };
        let record = record_for(
            &mut records,
            &counterparty,
            &counterparty_receiver_path,
            payment_request_id,
        );
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

    fn phase_order(&self) -> u8 {
        match self.event() {
            PaymentRequestEvent::Request(_) => 0,
            PaymentRequestEvent::Acceptance(_)
            | PaymentRequestEvent::Rejection(_)
            | PaymentRequestEvent::Cancellation(_)
            | PaymentRequestEvent::ConversionQuote(_)
            | PaymentRequestEvent::Proof(_) => 1,
        }
    }

    fn request_key(&self) -> &str {
        self.event().payment_request_id().as_str()
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
    counterparty_receiver_path: PaykitReceiverPath,
    mut items: Vec<PrivateStreamItemRecord>,
    outbound: Vec<OutboundPrivateMessageRecord>,
    dedupe_records: HashMap<String, EventDedupRecord>,
    outbound_carriers: OutboundEventCarriers,
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
                        record_for(
                            &mut records,
                            &counterparty,
                            &counterparty_receiver_path,
                            payment_request_id,
                        )
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
            if outbound_carriers.event_ids.contains(event_id) {
                if let Some(payment_request_id) = payment_request_id {
                    tainted_event_seeds.push(TaintedEventSeed {
                        event_id: event_id.to_owned(),
                        payment_request_id: payment_request_id.clone(),
                        payload_hash: payload_hash(&item.raw_json),
                        source_rank: 0,
                    });
                    pre_invalid_request_ids.insert(payment_request_id.clone());
                    record_for(
                        &mut records,
                        &counterparty,
                        &counterparty_receiver_path,
                        payment_request_id,
                    )
                    .mark_invalid(&item, "Event ID reused by another Event Message carrier");
                }
                continue;
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
            record_for(
                &mut records,
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            )
            .mark_invalid(
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
        if dedupe_records.contains_key(event.event_id().as_str())
            || outbound_carriers
                .conflicted_event_ids
                .contains(event.event_id().as_str())
        {
            let payment_request_id = stored.payment_request_id();
            pre_invalid_request_ids.insert(payment_request_id.clone());
            let record = record_for(
                &mut records,
                &counterparty,
                &counterparty_receiver_path,
                payment_request_id,
            );
            mark_invalid_stored(
                record,
                &stored,
                "Event ID reused by another Event Message carrier",
            );
            continue;
        }
        events.push(stored);
    }

    events = order_stored_events(events);
    let tainted_events = events
        .iter()
        .filter(|event| pre_invalid_request_ids.contains(&event.payment_request_id()))
        .cloned()
        .collect::<Vec<_>>();
    events.retain(|event| !pre_invalid_request_ids.contains(&event.payment_request_id()));
    events = dedupe_stored_events(
        &counterparty,
        &counterparty_receiver_path,
        &mut records,
        events,
        tainted_events,
        tainted_event_seeds,
    );
    let by_event_id: HashMap<_, _> = events
        .iter()
        .map(|event| (event.event().event_id().as_str(), event))
        .collect();
    let mut recovery_required = HashSet::new();
    for event in &events {
        if outbound_status(event) == Some(OutboundPrivateMessageStatus::RecoveryRequired)
            && !matches!(event.event(), PaymentRequestEvent::ConversionQuote(_))
        {
            recovery_required.insert(event.request_key());
        }
        let record = record_for(
            &mut records,
            &counterparty,
            &counterparty_receiver_path,
            event.payment_request_id(),
        );
        apply_stored_event(record, event, &by_event_id);
    }

    for record in records.values_mut() {
        if record.state != PaymentRequestLifecycleState::InvalidConflict {
            if let Err(err) = validate_record_conversion(record, &by_event_id) {
                record.state = PaymentRequestLifecycleState::InvalidConflict;
                record.invalid_reason = Some(err.to_string());
            } else if recovery_required.contains(record.payment_request_id.as_str()) {
                // Delivery recovery blocks new commands, not evidence already in the log.
                record.state = PaymentRequestLifecycleState::RecoveryRequired;
            }
        }
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

// Preserve each sender's order; lifecycle priority only chooses between stream heads.
// Using both rules inside a sort comparator would not define a transitive order.
fn order_stored_events(events: Vec<StoredPaymentRequestEvent>) -> Vec<StoredPaymentRequestEvent> {
    let (mut received, mut outbound): (Vec<_>, Vec<_>) = events
        .into_iter()
        .partition(|event| matches!(event, StoredPaymentRequestEvent::Received { .. }));
    for stream in [&mut received, &mut outbound] {
        stream.sort_by(|a, b| {
            a.request_key()
                .cmp(b.request_key())
                .then_with(|| a.source_order().cmp(&b.source_order()))
        });
    }
    let mut events = Vec::with_capacity(received.len() + outbound.len());
    let mut received = received.into_iter().peekable();
    let mut outbound = outbound.into_iter().peekable();
    while let (Some(a), Some(b)) = (received.peek(), outbound.peek()) {
        let order = a
            .request_key()
            .cmp(b.request_key())
            .then_with(|| a.phase_order().cmp(&b.phase_order()))
            .then_with(|| a.record_time().cmp(&b.record_time()));
        let stream = if order != Ordering::Greater {
            &mut received
        } else {
            &mut outbound
        };
        events.extend(stream.next());
    }
    events.extend(received);
    events.extend(outbound);
    events
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
    counterparty_receiver_path: &PaykitReceiverPath,
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
                let first_record = record_for(
                    records,
                    counterparty,
                    counterparty_receiver_path,
                    first.payment_request_id.clone(),
                );
                mark_invalid_stored(
                    first_record,
                    first_event,
                    "Event ID reused with different payload",
                );
                conflicted_request_ids.insert(first.payment_request_id.clone());
            }
            let current_record = record_for(
                records,
                counterparty,
                counterparty_receiver_path,
                event.payment_request_id(),
            );
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
    counterparty_receiver_path: &PaykitReceiverPath,
    payment_request_id: String,
) -> &'a mut PaymentRequestRecord {
    records
        .entry(payment_request_id.clone())
        .or_insert_with(|| {
            PaymentRequestRecord::new(
                counterparty.clone(),
                counterparty_receiver_path.clone(),
                payment_request_id,
            )
        })
}

fn payment_request_message_from_item(
    item: &PrivateStreamItemRecord,
) -> Option<PrivateApplicationMessage> {
    let kind = item.known_paykit_kind.as_deref()?;
    if !is_payment_request_kind(Some(kind)) {
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
            record.canceled_event_id = Some(cancellation.event_id().as_str().to_owned());
            record.state = PaymentRequestLifecycleState::Canceled;
            record.touch(item);
        }
        // Quote eligibility depends on outbound acceptance and is available only in the full view.
        PaymentRequestEvent::ConversionQuote(_) => record.touch(item),
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
    record.proposal_event_id = Some(request.event_id().as_str().to_owned());
    record.terms = Some(PaymentRequestTermsRecord::from(request.request()));
    record.touch(item);
}

fn apply_stored_event(
    record: &mut PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
    events: &HashMap<&str, &StoredPaymentRequestEvent>,
) {
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
            // Only the first Acceptance may cross a payee Cancellation; any
            // later Acceptance falls through to the transition check below.
            let crosses_payee_cancellation = record.state == PaymentRequestLifecycleState::Canceled
                && cancellation_was_sent_by_payee(record)
                && record.accepted_event_id.is_none()
                && record.rejected_event_id.is_none();
            if !matches!(record.state, PaymentRequestLifecycleState::Proposed)
                && !crosses_payee_cancellation
            {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request acceptance arrived after transition",
                );
                return;
            }
            record.accepted_event_id = Some(acceptance.event_id().as_str().to_owned());
            record.accepted_outbound_status = outbound_status(stored);
            if !crosses_payee_cancellation {
                record.state = accepted_state(record);
            }
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
            // Opposite-direction FIFO histories cannot order this payer
            // decision against the payee's Cancellation. Retain the first
            // response only when the payer had not already transitioned.
            let crosses_payee_cancellation = record.state == PaymentRequestLifecycleState::Canceled
                && cancellation_was_sent_by_payee(record)
                && record.accepted_event_id.is_none()
                && record.rejected_event_id.is_none();
            if record.state != PaymentRequestLifecycleState::Proposed && !crosses_payee_cancellation
            {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request rejection arrived after transition",
                );
                return;
            }
            record.rejected_event_id = Some(rejection.event_id().as_str().to_owned());
            record.rejected_outbound_status = outbound_status(stored);
            if !crosses_payee_cancellation {
                record.state = PaymentRequestLifecycleState::Rejected;
            }
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
            let crosses_payer_rejection = record.state == PaymentRequestLifecycleState::Rejected
                && !payer_action_source_allowed(record, stored);
            if matches!(
                record.state,
                PaymentRequestLifecycleState::Rejected
                    | PaymentRequestLifecycleState::Canceled
                    | PaymentRequestLifecycleState::RecoveryRequired
                    | PaymentRequestLifecycleState::InvalidConflict
            ) && !crosses_payer_rejection
            {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request cancellation arrived after terminal state",
                );
                return;
            }
            record.canceled_event_id = Some(cancellation.event_id().as_str().to_owned());
            record.canceled_outbound_status = outbound_status(stored);
            record.state = PaymentRequestLifecycleState::Canceled;
            touch_stored(record, stored);
        }
        PaymentRequestEvent::ConversionQuote(quote) => {
            if record.local_role.is_none() || payer_action_source_allowed(record, stored) {
                mark_invalid_stored(record, stored, "conversion quote came from the wrong side");
                return;
            }
            if record.canceled_event_id.is_some() && cancellation_was_sent_by_payee(record) {
                mark_invalid_stored(
                    record,
                    stored,
                    "conversion quote followed the payee's cancellation",
                );
                return;
            }
            // Cross-stream acceptance and proof references are checked after the complete fold.
            record
                .conversion_quotes
                .push(quote_record(quote, outbound_status(stored)));
            touch_stored(record, stored);
        }
        PaymentRequestEvent::Proof(proof) => {
            if !payer_action_source_allowed(record, stored) {
                mark_invalid_stored(record, stored, "Payment Proof came from the wrong side");
                return;
            }
            let follows_cancellation = record.state == PaymentRequestLifecycleState::Canceled;
            if record.rejected_event_id.is_some() || !proof_follows_acceptance(record) {
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
            let quote = proof
                .conversion_quote_id()
                .and_then(|id| events.get(id.as_str()))
                .and_then(|event| match event.event() {
                    PaymentRequestEvent::ConversionQuote(quote) => Some(quote),
                    _ => None,
                });
            if let Err(err) = proof.validate_conversion_quote(&request, quote) {
                mark_invalid_stored(record, stored, err.to_string());
                return;
            }
            record.payment_proofs.push(PaymentProofRecord {
                conversion_quote_id: proof.conversion_quote_id().map(|id| id.as_str().to_owned()),
                event_id: proof.event_id().as_str().to_owned(),
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
                payment_reference: proof.payment_reference().as_str().to_owned(),
                billing_period: proof
                    .billing_period()
                    .as_ref()
                    .map(BillingPeriodRecord::from),
                payment_endpoint_identifier: proof
                    .payment_endpoint_identifier()
                    .as_str()
                    .to_owned(),
                allowance_id: proof.allowance_id().map(|id| id.as_str().to_owned()),
                proof: proof.proof().clone(),
                recorded_at: stored.record_time(),
            });
            record.state = if follows_cancellation {
                PaymentRequestLifecycleState::Canceled
            } else if request.request().recurrence().is_some() {
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
    record.proposal_event_id = Some(request.event_id().as_str().to_owned());
    record.terms = Some(PaymentRequestTermsRecord::from(request.request()));
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

fn cancellation_was_sent_by_payee(record: &PaymentRequestRecord) -> bool {
    matches!(
        (record.local_role, &record.canceled_outbound_status),
        (Some(PaymentRequestLocalRole::Payer), None)
            | (Some(PaymentRequestLocalRole::Payee), Some(_))
    )
}

/// Lifecycle states in which a Payment Proof may follow.
///
/// A canceled request stays eligible only while its record retains a valid
/// Acceptance, so evidence of payment that crossed the cancellation is kept.
/// The same requirement allows corrective proofs after a one-time request has
/// reached Proof Submitted; another proof is evidence, not another payment.
pub(crate) fn payment_proof_allowed_states(
    record: &PaymentRequestRecord,
) -> &'static [PaymentRequestLifecycleState] {
    if record.accepted_event_id.is_some() {
        &[
            PaymentRequestLifecycleState::Accepted,
            PaymentRequestLifecycleState::ActiveRecurring,
            PaymentRequestLifecycleState::Canceled,
            PaymentRequestLifecycleState::ProofSubmitted,
        ]
    } else {
        &[
            PaymentRequestLifecycleState::Accepted,
            PaymentRequestLifecycleState::ActiveRecurring,
        ]
    }
}

fn proof_follows_acceptance(record: &PaymentRequestRecord) -> bool {
    payment_proof_allowed_states(record).contains(&record.state)
}

fn accepted_state(record: &PaymentRequestRecord) -> PaymentRequestLifecycleState {
    if record
        .terms
        .as_ref()
        .and_then(|terms| terms.recurrence.as_ref())
        .is_some()
    {
        PaymentRequestLifecycleState::ActiveRecurring
    } else {
        PaymentRequestLifecycleState::Accepted
    }
}

fn touch_stored(record: &mut PaymentRequestRecord, stored: &StoredPaymentRequestEvent) {
    match stored {
        StoredPaymentRequestEvent::Received { item, .. } => record.touch(item),
        StoredPaymentRequestEvent::Outbound { message, .. } => {
            record.touch_outbound(message);
        }
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
        Some(
            paykit_lib::Recurrence::try_from(paykit_lib::RecurrenceConfig {
                every: recurrence.every,
                unit: parse_recurrence_unit(&recurrence.unit)?,
                starts_at: recurrence.starts_at.clone(),
                anchor: recurrence.anchor.clone(),
                ends_at: recurrence.ends_at.clone(),
            })
            .ok()?,
        )
    } else {
        None
    };
    Some(PaymentRequest::new(
        paykit_lib::EventId::new(proposal_event_id).ok()?,
        paykit_lib::PaymentRequestId::new(record.payment_request_id.clone()).ok()?,
        paykit_lib::PaymentRequestTerms::builder(
            paykit_lib::PaymentAmount::new(terms.amount.value.clone(), terms.amount.asset.clone())
                .ok()?,
            paykit_lib::PaymentReference::new(terms.payment_reference.clone()).ok()?,
            terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .map(|identifier| paykit_lib::PaymentEndpointIdentifier::new(identifier).ok())
                .collect::<Option<Vec<_>>>()?,
        )
        .proposal_expires_at(terms.proposal_expires_at.clone())
        .recurrence(recurrence)
        .conversion(terms.conversion.clone())
        .payment_deadline(terms.payment_deadline.clone())
        .metadata(terms.metadata.clone())
        .build()
        .ok()?,
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

pub(super) fn recurrence_unit_to_str(unit: paykit_lib::RecurrenceUnit) -> &'static str {
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

// Quote references can point across independently delivered FIFO streams. Validate
// them without moving later payee events ahead of already received payment evidence.
fn validate_record_conversion(
    record: &PaymentRequestRecord,
    events: &HashMap<&str, &StoredPaymentRequestEvent>,
) -> paykit_lib::Result<()> {
    if record.conversion_quotes.is_empty()
        && record
            .payment_proofs
            .iter()
            .all(|proof| proof.conversion_quote_id.is_none())
    {
        return Ok(());
    }
    let request = request_from_record(record).ok_or_else(|| {
        paykit_lib::PaykitError::Validation("conversion requires Payment Request terms".into())
    })?;
    let acceptance = record
        .accepted_event_id
        .as_deref()
        .and_then(|id| events.get(id));
    for quote in &record.conversion_quotes {
        if acceptance.is_none() || record.rejected_event_id.is_some() {
            return Err(paykit_lib::PaykitError::Validation(
                "conversion quote arrived before acceptance".into(),
            ));
        }
        let stored = events[quote.event_id.as_str()];
        if let PaymentRequestEvent::ConversionQuote(quote) = stored.event() {
            quote.validate_for_request(&request)?;
        }
    }
    Ok(())
}

fn quote_record(
    quote: &PaymentConversionQuote,
    outbound_status: Option<OutboundPrivateMessageStatus>,
) -> PaymentConversionQuoteRecord {
    PaymentConversionQuoteRecord {
        event_id: quote.event_id().as_str().to_owned(),
        billing_period: BillingPeriodRecord::from(quote.billing_period()),
        rates: quote.rates().to_vec(),
        valid_from: quote.valid_from().to_owned(),
        expires_at: quote.expires_at().to_owned(),
        outbound_status,
    }
}

pub(crate) fn validate_proof_conversion(
    record: &PaymentRequestRecord,
    proof: &PaymentProof,
    request: &PaymentRequest,
) -> paykit_lib::Result<()> {
    let quote = proof
        .conversion_quote_id()
        .and_then(|id| {
            record
                .conversion_quotes
                .iter()
                .find(|quote| quote.event_id == id.as_str())
        })
        .map(|quote| {
            PaymentConversionQuote::new(
                EventId::new(&quote.event_id)?,
                request.payment_request_id().clone(),
                paykit_lib::BillingPeriod::try_from(&quote.billing_period)?,
                quote.rates.clone(),
                quote.valid_from.clone(),
                quote.expires_at.clone(),
            )
        })
        .transpose()?;
    proof.validate_conversion_quote(request, quote.as_ref())
}
