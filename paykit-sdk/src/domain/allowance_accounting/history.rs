use super::*;
use std::collections::{hash_map::Entry, HashMap, HashSet};

/// Private-state loss invalidates every unissued handoff without releasing usage.
pub(crate) fn invalidate_accounting(state: &mut AllowanceAccountingState) {
    state.revision = state.revision.saturating_add(1);
    state.epoch = new_id();
    state.requires_reconciliation = true;
    for occurrence in &mut state.history.occurrences {
        for attempt in &mut occurrence.attempts {
            if attempt.status == PaymentExecutionStatus::Prepared {
                attempt.status = PaymentExecutionStatus::Unknown;
            }
        }
    }
}

pub(crate) fn merge_restored_accounting(
    current: Option<AllowanceAccountingState>,
    restored: Option<AllowanceAccountingState>,
) -> Result<Option<AllowanceAccountingState>> {
    let mut state = match (current, restored) {
        (None, None) => return Ok(None),
        (Some(state), None) | (None, Some(state)) => {
            validate_accounting(&state)?;
            state
        }
        (Some(mut current), Some(restored)) => {
            validate_accounting(&current)?;
            validate_accounting(&restored)?;
            merge(&mut current.history, restored.history)?;
            current.revision = current.revision.max(restored.revision);
            current
        }
    };
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| protocol("Accounting revision exhausted"))?;
    invalidate_accounting(&mut state);
    validate_accounting(&state)?;
    Ok(Some(state))
}

pub(crate) fn reconcile(
    tx: &mut dyn StorageTransaction,
    input: AllowanceAccountingReconciliation,
) -> Result<AllowanceAccountingState> {
    validation::valid_time(input.trusted_time)?;
    let current = tx.allowance_accounting_state();
    if current.as_ref().map(|state| state.revision) != input.expected_revision {
        return Err(policy("Accounting reconciliation revision is stale"));
    }
    let supplied = input.history;
    let identity = tx
        .load_identity_state()
        .and_then(|i| i.public_key)
        .ok_or_else(|| policy("Reconciliation requires an initialized identity"))?;
    if supplied
        .associations
        .iter()
        .any(|a| a.request.local_public_key != identity)
        || supplied
            .occurrences
            .iter()
            .any(|o| o.key.request.local_public_key != identity)
        || supplied
            .watermarks
            .iter()
            .any(|w| w.local_public_key != identity)
    {
        return Err(policy(
            "Reconciled accounting belongs to another payer scope",
        ));
    }
    let mut state = current.unwrap_or_else(|| AllowanceAccountingState {
        revision: 0,
        epoch: new_id(),
        requires_reconciliation: true,
        history: AllowanceAccountingHistory::default(),
    });
    validate_accounting(&state)?;
    validation::payer_scope(&state, &identity)?;
    // Supplied rows are validated on their own, so duplicates within them still
    // fail, but against the retained association histories and with a watermark
    // at `trusted_time` for every referenced scope. A wallet rebuilding a lost
    // payment therefore need not copy retained rows or invent watermark times.
    // The derived watermarks are safe because the check below rejects any
    // retained or supplied evidence later than `trusted_time`, and every
    // watermark ends at `trusted_time` anyway.
    let mut context = AllowanceAccountingState {
        revision: 0,
        epoch: new_id(),
        requires_reconciliation: true,
        history: supplied.clone(),
    };
    merge_associations(
        &mut context.history.associations,
        state.history.associations.clone(),
    )?;
    raise_watermarks(&mut context.history, input.trusted_time);
    merge(&mut state.history, supplied)?;
    validation::payer_scope(&state, &identity)?;
    if state
        .history
        .watermarks
        .iter()
        .any(|w| w.evaluated_at > input.trusted_time)
        || state
            .history
            .associations
            .iter()
            .flat_map(|a| &a.revisions)
            .any(|r| r.authorized_at > input.trusted_time)
        || state
            .history
            .occurrences
            .iter()
            .flat_map(|o| &o.attempts)
            .any(|a| a.admitted_at > input.trusted_time)
    {
        return Err(policy("Reconciliation time precedes retained evidence"));
    }
    validate_accounting(&context)?;
    // New epochs deliberately cannot resume an old Prepared token. The wallet
    // must explicitly attest a terminal outcome or retain its reservation.
    invalidate_accounting(&mut state);
    for outcome in input.outcomes {
        apply_outcome(&mut state, outcome)?;
    }
    raise_watermarks(&mut state.history, input.trusted_time);
    state.requires_reconciliation = false;
    save(tx, state)?;
    crate::domain::payment_requests::release_resolved_payment_execution_claims(
        tx,
        input.trusted_time,
    )?;
    load(tx)
}

/// Sets every watermark to `time`, adding one for each scope that an
/// association revision or automatic attempt references without a watermark.
fn raise_watermarks(history: &mut AllowanceAccountingHistory, time: DateTime<Utc>) {
    for watermark in &mut history.watermarks {
        watermark.evaluated_at = time;
    }
    let mut scopes: HashSet<_> = history
        .watermarks
        .iter()
        .map(validation::watermark_key)
        .collect();
    let selected = history.associations.iter().flat_map(|a| {
        a.revisions
            .iter()
            .map(move |r| (&a.request, r.allowance_id.as_str()))
    });
    let admitted = history.occurrences.iter().flat_map(|o| {
        o.attempts
            .iter()
            .filter(|a| a.mode == PaymentExecutionMode::Automatic)
            .filter_map(move |a| Some((&o.key.request, a.allowance_id.as_deref()?)))
    });
    let missing: Vec<_> = selected
        .chain(admitted)
        .filter(|(scope, id)| scopes.insert(validation::scope_key(scope, id)))
        .map(|(scope, id)| AllowanceWatermarkRecord {
            local_public_key: scope.local_public_key.clone(),
            counterparty: scope.counterparty.clone(),
            allowance_id: id.to_owned(),
            evaluated_at: time,
        })
        .collect();
    history.watermarks.extend(missing);
}

pub(crate) fn report_outcome(
    tx: &mut dyn StorageTransaction,
    input: PaymentOutcomeReport,
) -> Result<PaymentAttemptRecord> {
    let mut state = load(tx)?;
    let result = apply_outcome(&mut state, input)?;
    save(tx, state)?;
    crate::domain::payment_requests::release_resolved_payment_execution_claims(
        tx,
        result.admitted_at,
    )?;
    Ok(result)
}

fn apply_outcome(
    state: &mut AllowanceAccountingState,
    input: PaymentOutcomeReport,
) -> Result<PaymentAttemptRecord> {
    validation::uuid(&input.attempt_id)?;
    let attempt = state
        .history
        .occurrences
        .iter_mut()
        .flat_map(|o| &mut o.attempts)
        .find(|a| a.attempt_id == input.attempt_id)
        .ok_or_else(|| policy("Unknown payment attempt"))?;
    let status = match input.outcome {
        PaymentOutcome::Succeeded => PaymentExecutionStatus::Succeeded,
        PaymentOutcome::Failed => PaymentExecutionStatus::Failed,
        PaymentOutcome::Unknown => PaymentExecutionStatus::Unknown,
    };
    if matches!(
        attempt.status,
        PaymentExecutionStatus::Succeeded | PaymentExecutionStatus::Failed
    ) && attempt.status != status
    {
        return Err(policy("Terminal payment outcome cannot be rewritten"));
    }
    if attempt.status == PaymentExecutionStatus::Prepared
        && status == PaymentExecutionStatus::Succeeded
    {
        return Err(policy("Unissued payment cannot be reported as settled"));
    }
    attempt.status = status;
    Ok(attempt.clone())
}

fn merge(
    destination: &mut AllowanceAccountingHistory,
    recovered: AllowanceAccountingHistory,
) -> Result<()> {
    merge_associations(&mut destination.associations, recovered.associations)?;
    for incoming in recovered.occurrences {
        if let Some(existing) = destination
            .occurrences
            .iter_mut()
            .find(|r| r.key == incoming.key)
        {
            // Neither a stale snapshot nor wallet import may erase manual-only.
            if incoming.disposition == PaymentDisposition::ManualOnly {
                existing.disposition = PaymentDisposition::ManualOnly;
            }
            for attempt in incoming.attempts {
                if let Some(known) = existing
                    .attempts
                    .iter_mut()
                    .find(|a| a.attempt_id == attempt.attempt_id)
                {
                    let mut comparable = attempt.clone();
                    comparable.status = known.status.clone();
                    if *known != comparable {
                        return Err(protocol("Conflicting payment attempt evidence"));
                    }
                    known.status = merge_status(&known.status, &attempt.status)?;
                } else {
                    existing.attempts.push(attempt);
                }
            }
            // Retained execution evidence supersedes temporary deferral, but
            // never clears a sticky manual-only decision or a failed-only retry.
            if occupied(existing)
                && matches!(existing.disposition, PaymentDisposition::Deferred { .. })
            {
                existing.disposition = PaymentDisposition::Automatic;
            }
        } else {
            destination.occurrences.push(incoming);
        }
    }
    // Watermarks are identity-wide and a linked counterparty can add rows, so
    // recovered rows find their match by scope instead of by scanning. Callers
    // validate `destination` first, so its scopes are unique.
    let watermark_key = |w: &AllowanceWatermarkRecord| {
        (
            w.local_public_key.clone(),
            w.counterparty.clone(),
            w.allowance_id.clone(),
        )
    };
    let mut positions: HashMap<_, _> = destination
        .watermarks
        .iter()
        .enumerate()
        .map(|(position, watermark)| (watermark_key(watermark), position))
        .collect();
    for incoming in recovered.watermarks {
        match positions.entry(watermark_key(&incoming)) {
            Entry::Occupied(position) => {
                let existing = &mut destination.watermarks[*position.get()];
                existing.evaluated_at = existing.evaluated_at.max(incoming.evaluated_at);
            }
            Entry::Vacant(position) => {
                position.insert(destination.watermarks.len());
                destination.watermarks.push(incoming);
            }
        }
    }
    Ok(())
}

/// Keeps the longer of two association histories for a request when one
/// extends the other. Reconciliation merges the whole retained ledger into the
/// supplied rows, so requests are matched by key instead of by scanning.
fn merge_associations(
    destination: &mut Vec<AllowanceAssociationRecord>,
    recovered: Vec<AllowanceAssociationRecord>,
) -> Result<()> {
    let request_key = |r: &AllowanceAssociationRecord| {
        (
            r.request.local_public_key.clone(),
            r.request.counterparty.clone(),
            r.request.payment_request_id.clone(),
        )
    };
    let mut positions: HashMap<_, _> = destination
        .iter()
        .enumerate()
        .map(|(position, record)| (request_key(record), position))
        .collect();
    for incoming in recovered {
        match positions.entry(request_key(&incoming)) {
            Entry::Occupied(position) => {
                let existing = &mut destination[*position.get()];
                let common = existing.revisions.len().min(incoming.revisions.len());
                if existing.revisions[..common] != incoming.revisions[..common] {
                    return Err(protocol("Conflicting association history"));
                }
                if incoming.revisions.len() > existing.revisions.len() {
                    existing.revisions = incoming.revisions;
                }
            }
            Entry::Vacant(position) => {
                position.insert(destination.len());
                destination.push(incoming);
            }
        }
    }
    Ok(())
}

fn merge_status(
    known: &PaymentExecutionStatus,
    recovered: &PaymentExecutionStatus,
) -> Result<PaymentExecutionStatus> {
    use PaymentExecutionStatus::*;
    match (known, recovered) {
        (Succeeded, Failed) | (Failed, Succeeded) => {
            Err(protocol("Conflicting terminal payment outcomes"))
        }
        (Succeeded, _) => Ok(Succeeded),
        (_, Succeeded) => Ok(known.clone()),
        (Failed, _) => Ok(Failed),
        (_, Failed) => Ok(known.clone()),
        (Unknown, _) | (_, Unknown) => Ok(Unknown),
        (Submitted, _) | (_, Submitted) => Ok(Submitted),
        _ => Ok(Prepared),
    }
}
