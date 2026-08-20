use super::{stored_events::StoredPaymentRequestEvent, *};

pub(super) fn record_for<'a>(
    records: &'a mut HashMap<String, PaymentRequestRecord>,
    counterparty: &PubkyPublicKey,
    payment_request_id: String,
) -> &'a mut PaymentRequestRecord {
    records
        .entry(payment_request_id.clone())
        .or_insert_with(|| PaymentRequestRecord::new(counterparty.clone(), payment_request_id))
}

pub(super) fn apply_event(
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
            let cancellation_app_id = item
                .parsed_app_id
                .as_ref()
                .and_then(|app_id| paykit_lib::PaykitAppId::new(app_id).ok());
            if cancellation_app_id.as_ref() != record.proposal_app_id.as_ref() {
                record.mark_invalid(
                    item,
                    "Payment Request cancellation came from a different payee application",
                );
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
            record.canceled_event_id = Some(cancellation.event_id.as_str().to_owned());
            record.state = PaymentRequestLifecycleState::Canceled;
            record.touch(item);
        }
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
    record.proposal_event_id = Some(request.event_id.as_str().to_owned());
    record.proposal_app_id = item
        .parsed_app_id
        .as_ref()
        .and_then(|app_id| paykit_lib::PaykitAppId::new(app_id).ok());
    record.terms = Some(PaymentRequestTermsRecord::from(&request.request));
    record.touch(item);
}

pub(super) fn apply_stored_event(
    record: &mut PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
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
            if is_competing_payer_app(record, stored) {
                touch_stored_audit(record, stored);
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
            if !matches!(record.state, PaymentRequestLifecycleState::Proposed) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request acceptance arrived after transition",
                );
                return;
            }
            record.accepted_event_id = Some(acceptance.event_id.as_str().to_owned());
            record.accepted_outbound_status = outbound_status(stored);
            record.payer_app_id = stored.app_id();
            record.state = if record
                .terms
                .as_ref()
                .and_then(|terms| terms.recurrence.as_ref())
                .is_some()
            {
                PaymentRequestLifecycleState::ActiveRecurring
            } else {
                PaymentRequestLifecycleState::Accepted
            };
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
            if is_competing_payer_app(record, stored) {
                touch_stored_audit(record, stored);
                return;
            }
            if !has_terms(
                record,
                stored,
                "Payment Request rejection arrived before proposal",
            ) {
                return;
            }
            if !matches!(record.state, PaymentRequestLifecycleState::Proposed) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request rejection arrived after transition",
                );
                return;
            }
            record.rejected_event_id = Some(rejection.event_id.as_str().to_owned());
            record.rejected_outbound_status = outbound_status(stored);
            record.payer_app_id = stored.app_id();
            record.state = PaymentRequestLifecycleState::Rejected;
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
            let payer_action = payer_action_source_allowed(record, stored);
            let cancellation_app_id = stored.app_id();
            let allowed = if payer_action {
                record
                    .payer_app_id
                    .as_ref()
                    .is_none_or(|payer_app_id| Some(payer_app_id) == cancellation_app_id.as_ref())
            } else {
                record.proposal_app_id.as_ref() == cancellation_app_id.as_ref()
            };
            if !allowed {
                if payer_action && is_competing_payer_app(record, stored) {
                    touch_stored_audit(record, stored);
                    return;
                }
                mark_invalid_stored(
                    record,
                    stored,
                    if payer_action {
                        "Payment Request cancellation came from a different payer application"
                    } else {
                        "Payment Request cancellation came from a different payee application"
                    },
                );
                return;
            }
            if matches!(
                record.state,
                PaymentRequestLifecycleState::Rejected
                    | PaymentRequestLifecycleState::Canceled
                    | PaymentRequestLifecycleState::RecoveryRequired
                    | PaymentRequestLifecycleState::InvalidConflict
            ) {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Request cancellation arrived after terminal state",
                );
                return;
            }
            if payer_action && record.payer_app_id.is_none() {
                record.payer_app_id = cancellation_app_id;
            }
            record.canceled_event_id = Some(cancellation.event_id.as_str().to_owned());
            record.canceled_outbound_status = outbound_status(stored);
            record.state = PaymentRequestLifecycleState::Canceled;
            touch_stored(record, stored);
        }
        PaymentRequestEvent::Proof(proof) => {
            if !payer_action_source_allowed(record, stored) {
                mark_invalid_stored(record, stored, "Payment Proof came from the wrong side");
                return;
            }
            if is_competing_payer_app(record, stored) {
                touch_stored_audit(record, stored);
                return;
            }
            if !matches!(
                record.state,
                PaymentRequestLifecycleState::Accepted
                    | PaymentRequestLifecycleState::ActiveRecurring
            ) {
                mark_invalid_stored(record, stored, "Payment Proof arrived before acceptance");
                return;
            }
            if record.payer_app_id.as_ref() != stored.app_id().as_ref() {
                mark_invalid_stored(
                    record,
                    stored,
                    "Payment Proof came from a different payer application",
                );
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
            if let Err(err) = proof.validate_for_request(&request) {
                mark_invalid_stored(record, stored, err.to_string());
                return;
            }
            record.payment_proofs.push(PaymentProofRecord {
                event_id: proof.event_id.as_str().to_owned(),
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
                payment_reference: proof.payment_reference.as_str().to_owned(),
                billing_period: proof.billing_period.as_ref().map(BillingPeriodRecord::from),
                payment_app_id: proof.payment_app_id.clone(),
                payment_endpoint_identifier: proof.payment_endpoint_identifier.as_str().to_owned(),
                proof: proof.proof.clone(),
                recorded_at: stored.record_time(),
            });
            record.state = if record
                .terms
                .as_ref()
                .and_then(|terms| terms.recurrence.as_ref())
                .is_some()
            {
                PaymentRequestLifecycleState::ActiveRecurring
            } else {
                PaymentRequestLifecycleState::ProofSubmitted
            };
            touch_stored(record, stored);
        }
    }
}

fn is_competing_payer_app(
    record: &PaymentRequestRecord,
    stored: &StoredPaymentRequestEvent,
) -> bool {
    let Some(payer_app_id) = record.payer_app_id.as_ref() else {
        return false;
    };
    stored
        .app_id()
        .as_ref()
        .is_some_and(|app_id| app_id != payer_app_id)
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
    record.proposal_event_id = Some(request.event_id.as_str().to_owned());
    record.proposal_app_id = stored.app_id();
    record.terms = Some(PaymentRequestTermsRecord::from(&request.request));
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

fn touch_stored(record: &mut PaymentRequestRecord, stored: &StoredPaymentRequestEvent) {
    match stored {
        StoredPaymentRequestEvent::Received { item, .. } => record.touch(item),
        StoredPaymentRequestEvent::Outbound { message, .. } => {
            record.touch_outbound(message);
            if message.status == OutboundPrivateMessageStatus::RecoveryRequired
                && record.state != PaymentRequestLifecycleState::InvalidConflict
            {
                record.state = PaymentRequestLifecycleState::RecoveryRequired;
            }
        }
    }
}

fn touch_stored_audit(record: &mut PaymentRequestRecord, stored: &StoredPaymentRequestEvent) {
    match stored {
        StoredPaymentRequestEvent::Received { item, .. } => record.touch(item),
        StoredPaymentRequestEvent::Outbound { message, .. } => record.touch_outbound(message),
    }
}

fn outbound_status(stored: &StoredPaymentRequestEvent) -> Option<OutboundPrivateMessageStatus> {
    match stored {
        StoredPaymentRequestEvent::Outbound { message, .. } => Some(message.status.clone()),
        StoredPaymentRequestEvent::Received { .. } => None,
    }
}

pub(super) fn mark_invalid_stored(
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
        Some(paykit_lib::Recurrence {
            every: recurrence.every,
            unit: parse_recurrence_unit(&recurrence.unit)?,
            starts_at: recurrence.starts_at.clone(),
            anchor: recurrence.anchor.clone(),
            ends_at: recurrence.ends_at.clone(),
        })
    } else {
        None
    };
    Some(PaymentRequest::new(
        paykit_lib::EventId::new(proposal_event_id).ok()?,
        paykit_lib::PaymentRequestId::new(record.payment_request_id.clone()).ok()?,
        paykit_lib::PaymentRequestTerms {
            amount: paykit_lib::PaymentAmount::new(
                terms.amount.value.clone(),
                terms.amount.asset.clone(),
            )
            .ok()?,
            payment_reference: paykit_lib::PaymentReference::new(terms.payment_reference.clone())
                .ok()?,
            proposal_expires_at: terms.proposal_expires_at.clone(),
            recurrence,
            accepted_payment_endpoint_identifiers: terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .map(|identifier| paykit_lib::PaymentEndpointIdentifier::new(identifier).ok())
                .collect::<Option<Vec<_>>>()?,
            required_app_id: terms.required_app_id.clone(),
            metadata: terms.metadata.clone(),
        },
    ))
}

pub(super) fn proposal_expired(record: &PaymentRequestRecord, now: DateTime<Utc>) -> bool {
    record
        .terms
        .as_ref()
        .and_then(|terms| terms.proposal_expires_at.as_ref())
        .and_then(|expires_at| DateTime::parse_from_rfc3339(expires_at).ok())
        .map(|expires_at| now >= expires_at.with_timezone(&Utc))
        .unwrap_or(false)
}

pub(crate) fn recurrence_unit_to_str(unit: paykit_lib::RecurrenceUnit) -> &'static str {
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
