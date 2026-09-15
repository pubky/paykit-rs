use super::*;

pub(crate) fn candidates(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input: PaymentRequestScope,
    time: DateTime<Utc>,
) -> Result<Vec<AllowanceCandidate>> {
    validation::valid_time(time)?;
    let scope = scope(tx, local, &input)?;
    let request = request(tx, &scope, time)?;
    let terms = request_terms(&request)?;
    let records = crate::domain::allowances::allowance_records_in_transaction(
        tx,
        &scope.counterparty,
        &scope.counterparty_receiver_path,
    );
    let mut state = tx.allowance_accounting_state();
    if let Some(state) = &state {
        validate_accounting(state)?;
    }
    let mut results = Vec::new();
    for record in records {
        let id = record.allowance_id;
        let mut result = AllowanceCandidate {
            allowance_id: id.clone(),
            eligible_payment_endpoint_identifiers: Vec::new(),
            blocked: None,
        };
        result.blocked = if let Some(state) = state.as_mut().filter(|s| !s.requires_reconciliation)
        {
            let previous = watermark(state, &scope, &id, time);
            if !valid_request(tx, &scope, &request, true) {
                Some(AllowanceAccountingBlock::InvalidLifecycle)
            } else {
                match static_check(tx, &scope, &id, &terms, time, previous) {
                    Ok(endpoints) => {
                        result.eligible_payment_endpoint_identifiers =
                            endpoints.iter().map(|e| e.as_str().to_owned()).collect();
                        None
                    }
                    Err(reason) => Some(reason),
                }
            }
        } else {
            Some(AllowanceAccountingBlock::ReconciliationRequired)
        };
        results.push(result);
    }
    if let Some(state) = state {
        save(tx, state)?;
    }
    Ok(results)
}

pub(super) fn static_check(
    tx: &dyn StorageTransaction,
    scope: &PaymentAccountingScope,
    id: &str,
    request: &PaymentRequestTerms,
    time: DateTime<Utc>,
    previous: DateTime<Utc>,
) -> std::result::Result<Vec<paykit_lib::PaymentEndpointIdentifier>, AllowanceAccountingBlock> {
    let (record, terms) =
        allowance_terms(tx, scope, id).map_err(|_| AllowanceAccountingBlock::InvalidLifecycle)?;
    if !valid_allowance(&record) {
        return Err(AllowanceAccountingBlock::InvalidLifecycle);
    }
    paykit_lib::check_allowance_time(&terms, time, previous).map_err(shared)?;
    paykit_lib::match_allowance_request(&terms, request).map_err(shared)
}

pub(crate) fn select(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input_scope: PaymentRequestScope,
    input: AllowanceSelectionInput,
    acceptance: Option<PaymentExecutionChecks>,
) -> Result<std::result::Result<AllowanceAssociationRecord, AllowanceAccountingBlock>> {
    validation::valid_time(input.trusted_time)?;
    let scope = scope(tx, local, &input_scope)?;
    let mut state = load(tx)?;
    ready(&state)?;
    let previous = watermark(
        &mut state,
        &scope,
        input.allowance_id.as_str(),
        input.trusted_time,
    );
    let result = select_checked(
        tx,
        &scope,
        &mut state,
        &input,
        previous,
        acceptance.as_ref(),
    );
    if result.is_ok() && acceptance.is_some() {
        let event = paykit_lib::PaymentRequestAcceptance::new(
            paykit_lib::EventId::new_v4(),
            input_scope.payment_request_id,
        );
        let raw = paykit_lib::serialize_payment_request_event(
            &paykit_lib::PaymentRequestEvent::Acceptance(event),
        )
        .map_err(|_| protocol("Could not encode Payment Request Acceptance"))?;
        let kind = crate::domain::outbound_private::validate_outbound_private_message(&raw)?;
        tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
            scope.counterparty.clone(),
            scope.counterparty_receiver_path.clone(),
            kind,
            raw,
            input.trusted_time,
        ));
    }
    save(tx, state)?;
    Ok(result)
}

fn select_checked(
    tx: &dyn StorageTransaction,
    scope: &PaymentAccountingScope,
    state: &mut AllowanceAccountingState,
    input: &AllowanceSelectionInput,
    previous: DateTime<Utc>,
    acceptance: Option<&PaymentExecutionChecks>,
) -> std::result::Result<AllowanceAssociationRecord, AllowanceAccountingBlock> {
    let request = request(tx, scope, input.trusted_time)
        .map_err(|_| AllowanceAccountingBlock::InvalidLifecycle)?;
    if !valid_request(tx, scope, &request, true)
        || (acceptance.is_some() && request.state != crate::PaymentRequestLifecycleState::Proposed)
    {
        return Err(AllowanceAccountingBlock::InvalidLifecycle);
    }
    let terms = request_terms(&request).map_err(|_| AllowanceAccountingBlock::InvalidLifecycle)?;
    let endpoints = static_check(
        tx,
        scope,
        input.allowance_id.as_str(),
        &terms,
        input.trusted_time,
        previous,
    )?;
    if let Some(checks) = acceptance {
        if checks.trusted_time != input.trusted_time
            || !execution::wallet_checks(&terms, checks)
            || !endpoints.contains(&checks.payment_endpoint_identifier)
        {
            return Err(AllowanceAccountingBlock::WalletChecksFailed);
        }
    }
    if state.history.occurrences.iter().any(|o| {
        o.key.request == *scope
            && o.key.billing_period.is_none()
            && o.disposition == PaymentDisposition::ManualOnly
    }) {
        return Err(AllowanceAccountingBlock::ManualOnly);
    }
    if let Some(existing) = state
        .history
        .associations
        .iter()
        .find(|a| a.request == *scope)
    {
        let latest = existing
            .revisions
            .last()
            .ok_or(AllowanceAccountingBlock::NoSelection)?;
        if Some(latest.revision) != input.expected_revision
            || latest.allowance_id != input.allowance_id.as_str()
        {
            return Err(AllowanceAccountingBlock::StaleRevision);
        }
        return Ok(existing.clone());
    }
    if input.expected_revision.is_some() {
        return Err(AllowanceAccountingBlock::StaleRevision);
    }
    let result = AllowanceAssociationRecord {
        request: scope.clone(),
        revisions: vec![AllowanceAssociationRevision {
            revision: 1,
            allowance_id: input.allowance_id.as_str().to_owned(),
            effective_from: None,
            authorization_id: None,
            authorized_at: input.trusted_time,
        }],
    };
    state.history.associations.push(result.clone());
    Ok(result)
}

pub(crate) fn reassociate(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input_scope: PaymentRequestScope,
    input: AllowanceReassociationInput,
) -> Result<std::result::Result<AllowanceAssociationRecord, AllowanceAccountingBlock>> {
    validation::valid_time(input.trusted_time)?;
    validation::valid_time(input.effective_from)?;
    validation::uuid(&input.authorization_id)?;
    if input.effective_from < input.trusted_time {
        return Err(policy(
            "Reassociation requires an explicitly authorized future boundary",
        ));
    }
    let scope = scope(tx, local, &input_scope)?;
    let mut state = load(tx)?;
    ready(&state)?;
    let previous = watermark(
        &mut state,
        &scope,
        input.allowance_id.as_str(),
        input.trusted_time,
    );
    let result = (|| {
        let request = request(tx, &scope, input.trusted_time)
            .map_err(|_| AllowanceAccountingBlock::InvalidLifecycle)?;
        if !valid_request(tx, &scope, &request, false)
            || request.state != crate::PaymentRequestLifecycleState::ActiveRecurring
        {
            return Err(AllowanceAccountingBlock::InvalidLifecycle);
        }
        let terms =
            request_terms(&request).map_err(|_| AllowanceAccountingBlock::InvalidLifecycle)?;
        static_check(
            tx,
            &scope,
            input.allowance_id.as_str(),
            &terms,
            input.trusted_time,
            previous,
        )?;
        let association = state
            .history
            .associations
            .iter_mut()
            .find(|a| a.request == scope)
            .ok_or(AllowanceAccountingBlock::NoSelection)?;
        let current = association
            .revisions
            .last()
            .ok_or(AllowanceAccountingBlock::NoSelection)?;
        if current.revision != input.expected_revision
            || current
                .effective_from
                .is_some_and(|from| from >= input.effective_from)
            || input.trusted_time < current.authorized_at
        {
            return Err(AllowanceAccountingBlock::StaleRevision);
        }
        let revision = current
            .revision
            .checked_add(1)
            .ok_or(AllowanceAccountingBlock::StaleRevision)?;
        association.revisions.push(AllowanceAssociationRevision {
            revision,
            allowance_id: input.allowance_id.as_str().to_owned(),
            effective_from: Some(input.effective_from),
            authorization_id: Some(input.authorization_id),
            authorized_at: input.trusted_time,
        });
        // Prepared attempts retain their reservation and old revision; begin
        // compares the effective revision again, so this cannot grant two spends.
        Ok(association.clone())
    })();
    save(tx, state)?;
    Ok(result)
}

pub(crate) fn set_disposition(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input: PaymentOccurrence,
    disposition: PaymentDisposition,
) -> Result<PaymentOccurrenceRecord> {
    validation::disposition(&disposition)?;
    let key = key(tx, local, &input)?;
    let mut state = load(tx)?;
    if let Some(existing) = state.history.occurrences.iter_mut().find(|o| o.key == key) {
        if occupied(existing) {
            return Err(policy("Unresolved or successful execution cannot become a deferred or manual-only decision"));
        }
        if existing.disposition == PaymentDisposition::ManualOnly
            && disposition != PaymentDisposition::ManualOnly
        {
            return Err(policy(
                "Manual-only disposition cannot be cleared by background retry",
            ));
        }
        existing.disposition = disposition;
    } else {
        let selected = association(&state, &key).cloned();
        state.history.occurrences.push(PaymentOccurrenceRecord {
            key: key.clone(),
            disposition,
            allowance_id: selected.as_ref().map(|a| a.allowance_id.clone()),
            association_revision: selected.map(|a| a.revision),
            attempts: Vec::new(),
        });
    }
    let result = state
        .history
        .occurrences
        .iter()
        .find(|o| o.key == key)
        .ok_or_else(|| protocol("Payment disposition was not persisted"))?
        .clone();
    save(tx, state)?;
    Ok(result)
}
