use super::*;

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
    local: &PaykitReceiverPath,
    input: AllowanceAccountingReconciliation,
) -> Result<AllowanceAccountingState> {
    validation::valid_time(input.trusted_time)?;
    let current = tx.allowance_accounting_state();
    if current.as_ref().map(|state| state.revision) != input.expected_revision {
        return Err(policy("Accounting reconciliation revision is stale"));
    }
    let supplied = AllowanceAccountingState {
        revision: 0,
        epoch: new_id(),
        requires_reconciliation: true,
        history: input.history,
    };
    validate_accounting(&supplied)?;
    let identity = tx
        .load_identity_state()
        .and_then(|i| i.local_pubky_public_key)
        .ok_or_else(|| policy("Reconciliation requires an initialized identity"))?;
    if supplied
        .history
        .associations
        .iter()
        .any(|a| a.request.local_public_key != identity || a.request.local_receiver_path != *local)
        || supplied.history.occurrences.iter().any(|o| {
            o.key.request.local_public_key != identity
                || o.key.request.local_receiver_path != *local
        })
        || supplied
            .history
            .watermarks
            .iter()
            .any(|w| w.local_public_key != identity || w.local_receiver_path != *local)
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
    validation::payer_scope(&state, &identity, local)?;
    merge(&mut state.history, supplied.history)?;
    validation::payer_scope(&state, &identity, local)?;
    if state
        .history
        .watermarks
        .iter()
        .any(|w| w.evaluated_at > input.trusted_time)
        || state
            .history
            .occurrences
            .iter()
            .flat_map(|o| &o.attempts)
            .any(|a| a.admitted_at > input.trusted_time)
    {
        return Err(policy("Reconciliation time precedes retained evidence"));
    }
    // New epochs deliberately cannot resume an old Prepared token. The wallet
    // must explicitly attest a terminal outcome or retain its reservation.
    invalidate_accounting(&mut state);
    for outcome in input.outcomes {
        apply_outcome(&mut state, outcome)?;
    }
    for watermark in &mut state.history.watermarks {
        watermark.evaluated_at = input.trusted_time;
    }
    state.requires_reconciliation = false;
    save(tx, state)?;
    load(tx)
}

pub(crate) fn report_outcome(
    tx: &mut dyn StorageTransaction,
    input: PaymentOutcomeReport,
) -> Result<PaymentAttemptRecord> {
    let mut state = load(tx)?;
    let result = apply_outcome(&mut state, input)?;
    save(tx, state)?;
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
    for incoming in recovered.associations {
        if let Some(existing) = destination
            .associations
            .iter_mut()
            .find(|r| r.request == incoming.request)
        {
            let common = existing.revisions.len().min(incoming.revisions.len());
            if existing.revisions[..common] != incoming.revisions[..common] {
                return Err(protocol("Conflicting association history"));
            }
            if incoming.revisions.len() > existing.revisions.len() {
                existing.revisions = incoming.revisions;
            }
        } else {
            destination.associations.push(incoming);
        }
    }
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
        } else {
            destination.occurrences.push(incoming);
        }
    }
    for incoming in recovered.watermarks {
        if let Some(existing) = destination.watermarks.iter_mut().find(|w| {
            w.local_public_key == incoming.local_public_key
                && w.local_receiver_path == incoming.local_receiver_path
                && w.counterparty == incoming.counterparty
                && w.counterparty_receiver_path == incoming.counterparty_receiver_path
                && w.allowance_id == incoming.allowance_id
        }) {
            existing.evaluated_at = existing.evaluated_at.max(incoming.evaluated_at);
        } else {
            destination.watermarks.push(incoming);
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
