use super::{
    reducer::{apply_event, proposal_expired, record_for},
    stored_events::{derive_payment_request_records_from_parts, payment_request_message_from_item},
    *,
};

/// Derive received Payment Request lifecycle records for one counterparty.
///
/// Records are derived from the persisted inbound private stream and returned
/// newest-first by the last applied stream item. Malformed recognized Payment
/// Request events without a valid `payment_request_id` remain available in the
/// raw private stream log but cannot be attached to a request-scoped record.
pub(crate) async fn received_payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    let (items, dedupe_records, execution_claims) = storage
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
            let execution_claims = tx
                .export_storage_state()
                .payment_request_execution_claims
                .into_iter()
                .filter(|((claim_counterparty, _), _)| claim_counterparty == counterparty)
                .collect::<HashMap<_, _>>();
            Ok((items, dedupe_records, execution_claims))
        })
        .await?;
    let mut records =
        derive_received_payment_request_records(counterparty.clone(), items, dedupe_records, now)?;
    apply_execution_claims(&mut records, &execution_claims);
    Ok(records)
}

/// Derive local Payment Request records for one counterparty.
///
/// Records merge received Payment Request events from the private stream with
/// local outbound Payment Request events from the outbound private-message log.
/// Returned records are newest-first by local record time.
pub(crate) async fn payment_request_records<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| payment_request_records_from_transaction(tx, counterparty, now))
        .await
}

pub(crate) fn payment_request_records_from_transaction(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<Vec<PaymentRequestRecord>> {
    let items = tx.private_stream_items(counterparty);
    let outbound = tx.outbound_private_messages(counterparty);
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
    let mut records = derive_payment_request_records_from_parts(
        counterparty.clone(),
        items,
        outbound,
        dedupe_records,
        now,
    )?;
    apply_execution_claims(
        &mut records,
        &tx.export_storage_state().payment_request_execution_claims,
    );
    Ok(records)
}

fn apply_execution_claims(
    records: &mut [PaymentRequestRecord],
    claims: &HashMap<(PubkyPublicKey, String), PaymentRequestExecutionClaim>,
) {
    for record in records {
        record.execution_claim_app_id = claims
            .get(&(
                record.counterparty.clone(),
                record.payment_request_id.clone(),
            ))
            .map(|claim| claim.app_id.clone());
    }
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
                .last_stream_item_id
                .or(record.proposal_stream_item_id),
        )
    });
    Ok(records)
}
