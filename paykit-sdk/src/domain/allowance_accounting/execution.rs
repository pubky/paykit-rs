use super::*;

pub(super) fn wallet_checks(terms: &PaymentRequestTerms, checks: &PaymentExecutionChecks) -> bool {
    checks.endpoint_current
        && checks.local_enabled
        && checks.recurrence_eligible
        && terms
            .accepted_payment_endpoint_identifiers()
            .contains(&checks.payment_endpoint_identifier)
        && same_amount(terms.amount(), &checks.actual_amount)
}

fn same_amount(left: &paykit_lib::PaymentAmount, right: &paykit_lib::PaymentAmount) -> bool {
    left.asset() == right.asset()
        && paykit_lib::compare_decimal_amounts(left.value(), right.value())
            .is_ok_and(std::cmp::Ordering::is_eq)
}

fn occurrence_matches(
    key: &PaymentOccurrenceKey,
    terms: &PaymentRequestTerms,
    time: DateTime<Utc>,
) -> bool {
    match (&key.billing_period, terms.recurrence()) {
        (None, None) => true,
        (Some(period), Some(recurrence)) => {
            validation::utc_time(recurrence.starts_at())
                .is_ok_and(|start| period.starts_at >= start)
                && recurrence.ends_at().as_ref().is_none_or(|end| {
                    validation::utc_time(end).is_ok_and(|end| period.ends_at <= end)
                })
                && period.starts_at <= time
        }
        _ => false,
    }
}

pub(crate) fn reserve(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input: PaymentOccurrence,
    expected_revision: Option<u64>,
    checks: PaymentExecutionChecks,
    mode: PaymentExecutionMode,
) -> Result<PaymentAttemptDecision> {
    validation::valid_time(checks.trusted_time)?;
    let key = key(tx, local, &input)?;
    let Some(mut state) = tx
        .allowance_accounting_state()
        .filter(|s| !s.requires_reconciliation)
    else {
        return Ok(blocked(AllowanceAccountingBlock::ReconciliationRequired));
    };
    validate_accounting(&state)?;
    let decision = reserve_checked(tx, &mut state, key, expected_revision, &checks, mode);
    save(tx, state)?;
    Ok(decision)
}

fn reserve_checked(
    tx: &dyn StorageTransaction,
    state: &mut AllowanceAccountingState,
    key: PaymentOccurrenceKey,
    expected_revision: Option<u64>,
    checks: &PaymentExecutionChecks,
    mode: PaymentExecutionMode,
) -> PaymentAttemptDecision {
    let selected = association(state, &key).cloned();
    let previous = if mode == PaymentExecutionMode::Automatic {
        selected
            .as_ref()
            .map(|a| watermark(state, &key.request, &a.allowance_id, checks.trusted_time))
    } else {
        None
    };
    if state
        .history
        .occurrences
        .iter()
        .any(|o| o.key == key && occupied(o))
    {
        return blocked(AllowanceAccountingBlock::PaymentAlreadyRecorded);
    }
    let Ok(record) = request(tx, &key.request, checks.trusted_time) else {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    };
    if !valid_request(tx, &key.request, &record, false) {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    }
    let Ok(terms) = request_terms(&record) else {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    };
    if !wallet_checks(&terms, checks) || !occurrence_matches(&key, &terms, checks.trusted_time) {
        return blocked(AllowanceAccountingBlock::WalletChecksFailed);
    }
    if mode == PaymentExecutionMode::Automatic {
        if state.history.occurrences.iter().any(|o| {
            o.key.request == key.request
                && (o.key == key || o.key.billing_period.is_none())
                && o.disposition == PaymentDisposition::ManualOnly
        }) {
            return blocked(AllowanceAccountingBlock::ManualOnly);
        }
        let Some(selected) = &selected else {
            return blocked(AllowanceAccountingBlock::NoSelection);
        };
        if Some(selected.revision) != expected_revision {
            return blocked(AllowanceAccountingBlock::StaleRevision);
        }
        let Ok((allowance, allowance_terms)) =
            allowance_terms(tx, &key.request, &selected.allowance_id)
        else {
            return blocked(AllowanceAccountingBlock::InvalidLifecycle);
        };
        if !valid_allowance(&allowance) {
            return blocked(AllowanceAccountingBlock::InvalidLifecycle);
        }
        let usage = state
            .history
            .occurrences
            .iter()
            .filter(|o| {
                o.key.request.local_public_key == key.request.local_public_key
                    && o.key.request.local_receiver_path == key.request.local_receiver_path
                    && o.key.request.counterparty == key.request.counterparty
                    && o.key.request.counterparty_receiver_path
                        == key.request.counterparty_receiver_path
            })
            .flat_map(|o| &o.attempts)
            .filter(|a| {
                a.mode == PaymentExecutionMode::Automatic
                    && a.allowance_id.as_deref() == Some(selected.allowance_id.as_str())
                    && a.status != PaymentExecutionStatus::Failed
            })
            .map(|a| {
                validation::amount(&a.amount).and_then(|amount| {
                    paykit_lib::AllowanceUsageEntry::new(amount, a.admitted_at)
                        .map_err(|_| protocol("Invalid accounting usage"))
                })
            })
            .collect::<Result<Vec<_>>>();
        let Ok(usage) = usage else {
            return blocked(AllowanceAccountingBlock::ReconciliationRequired);
        };
        match paykit_lib::evaluate_allowance(&paykit_lib::AllowanceEvaluationInput {
            terms: &allowance_terms,
            request: &terms,
            trusted_time: checks.trusted_time,
            watermark: previous.expect("Automatic selection established its watermark above"),
            usage: &usage,
        }) {
            Err(error) => return blocked(shared(error)),
            Ok(evaluation)
                if !evaluation
                    .eligible_payment_endpoint_identifiers
                    .contains(&checks.payment_endpoint_identifier) =>
            {
                return blocked(AllowanceAccountingBlock::WalletChecksFailed)
            }
            Ok(_) => {}
        }
    }
    let automatic = mode == PaymentExecutionMode::Automatic;
    let attempt = PaymentAttemptRecord {
        attempt_id: new_id(),
        mode,
        allowance_id: if automatic {
            selected.as_ref().map(|a| a.allowance_id.clone())
        } else {
            None
        },
        association_revision: if automatic {
            selected.as_ref().map(|a| a.revision)
        } else {
            None
        },
        amount: terms.amount().into(),
        admitted_at: checks.trusted_time,
        status: PaymentExecutionStatus::Prepared,
        epoch: state.epoch.clone(),
    };
    if let Some(existing) = state.history.occurrences.iter_mut().find(|o| o.key == key) {
        if automatic {
            existing.allowance_id = attempt.allowance_id.clone();
            existing.association_revision = attempt.association_revision;
            existing.disposition = PaymentDisposition::Automatic;
        }
        if !automatic {
            existing.disposition = PaymentDisposition::ManualOnly;
        }
        existing.attempts.push(attempt.clone());
    } else {
        state.history.occurrences.push(PaymentOccurrenceRecord {
            key,
            disposition: if automatic {
                PaymentDisposition::Automatic
            } else {
                PaymentDisposition::ManualOnly
            },
            allowance_id: attempt.allowance_id.clone(),
            association_revision: attempt.association_revision,
            attempts: vec![attempt.clone()],
        });
    }
    PaymentAttemptDecision::Ready { attempt }
}

pub(crate) fn begin(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    attempt_id: String,
    checks: PaymentExecutionChecks,
) -> Result<PaymentAttemptDecision> {
    validation::uuid(&attempt_id)?;
    validation::valid_time(checks.trusted_time)?;
    let Some(mut state) = tx
        .allowance_accounting_state()
        .filter(|s| !s.requires_reconciliation)
    else {
        return Ok(blocked(AllowanceAccountingBlock::ReconciliationRequired));
    };
    validate_accounting(&state)?;
    let (key, attempt) = state
        .history
        .occurrences
        .iter()
        .find_map(|o| {
            o.attempts
                .iter()
                .find(|a| a.attempt_id == attempt_id)
                .map(|a| (o.key.clone(), a.clone()))
        })
        .ok_or_else(|| policy("Unknown payment attempt"))?;
    let current_identity = tx
        .load_identity_state()
        .and_then(|i| i.local_pubky_public_key);
    let result = if current_identity.as_ref() != Some(&key.request.local_public_key)
        || *local != key.request.local_receiver_path
    {
        blocked(AllowanceAccountingBlock::InvalidLifecycle)
    } else {
        begin_checked(tx, &mut state, &key, &attempt, &checks)
    };
    save(tx, state)?;
    Ok(result)
}

fn begin_checked(
    tx: &dyn StorageTransaction,
    state: &mut AllowanceAccountingState,
    key: &PaymentOccurrenceKey,
    attempt: &PaymentAttemptRecord,
    checks: &PaymentExecutionChecks,
) -> PaymentAttemptDecision {
    let previous = attempt
        .allowance_id
        .as_ref()
        .map(|id| watermark(state, &key.request, id, checks.trusted_time));
    if attempt.status != PaymentExecutionStatus::Prepared || attempt.epoch != state.epoch {
        return blocked(AllowanceAccountingBlock::PaymentAlreadyRecorded);
    }
    let Ok(record) = request(tx, &key.request, checks.trusted_time) else {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    };
    if !valid_request(tx, &key.request, &record, false) {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    }
    let Ok(terms) = request_terms(&record) else {
        return blocked(AllowanceAccountingBlock::InvalidLifecycle);
    };
    if checks.trusted_time < attempt.admitted_at
        || !wallet_checks(&terms, checks)
        || !occurrence_matches(key, &terms, checks.trusted_time)
    {
        return blocked(AllowanceAccountingBlock::WalletChecksFailed);
    }
    if attempt.mode == PaymentExecutionMode::Automatic {
        if state.history.occurrences.iter().any(|o| {
            o.key.request == key.request
                && (o.key == *key || o.key.billing_period.is_none())
                && o.disposition == PaymentDisposition::ManualOnly
        }) {
            return blocked(AllowanceAccountingBlock::ManualOnly);
        }
        let Some(selected) = association(state, key) else {
            return blocked(AllowanceAccountingBlock::NoSelection);
        };
        if Some(selected.revision) != attempt.association_revision
            || Some(selected.allowance_id.as_str()) != attempt.allowance_id.as_deref()
        {
            return blocked(AllowanceAccountingBlock::StaleRevision);
        }
        match selection::static_check(
            tx,
            &key.request,
            &selected.allowance_id,
            &terms,
            checks.trusted_time,
            previous.expect("Automatic selection established its watermark above"),
        ) {
            Err(reason) => return blocked(reason),
            Ok(endpoints) if !endpoints.contains(&checks.payment_endpoint_identifier) => {
                return blocked(AllowanceAccountingBlock::WalletChecksFailed)
            }
            Ok(_) => {}
        }
    }
    let stored = state
        .history
        .occurrences
        .iter_mut()
        .flat_map(|o| &mut o.attempts)
        .find(|a| a.attempt_id == attempt.attempt_id)
        .expect("Attempt was located in this same exclusively owned ledger above");
    stored.status = PaymentExecutionStatus::Submitted;
    PaymentAttemptDecision::Ready {
        attempt: stored.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_accounting_decimal_equality_uses_validated_exact_comparison() {
        for (left, right) in [(".5", "0.50"), ("001.0", "1"), ("0.", ".0")] {
            assert!(same_amount(
                &paykit_lib::PaymentAmount::new(left, "btc").unwrap(),
                &paykit_lib::PaymentAmount::new(right, "btc").unwrap()
            ));
        }
        assert!(!same_amount(
            &paykit_lib::PaymentAmount::new("1", "btc").unwrap(),
            &paykit_lib::PaymentAmount::new("1.01", "btc").unwrap()
        ));
    }
}
