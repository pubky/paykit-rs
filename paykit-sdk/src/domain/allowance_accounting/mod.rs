//! Durable wallet payment admission. This module never executes or settles payment.

use crate::storage::StorageTransaction;
use crate::{PaykitReceiverPath, PaykitSdkError, Result};
use chrono::{DateTime, Utc};
use paykit_lib::{AllowanceId, AllowanceTerms, PaymentRequestTerms};

mod execution;
mod history;
mod selection;
mod types;
mod validation;
pub(crate) use execution::{begin, reserve};
pub(crate) use history::{
    invalidate_accounting, merge_restored_accounting, reconcile, report_outcome,
};
pub(crate) use selection::{candidates, reassociate, select, set_disposition};
pub use types::*;
pub(crate) use validation::validate_accounting;

fn policy(message: &str) -> PaykitSdkError {
    PaykitSdkError::Policy {
        context: message.to_owned(),
        source: None,
    }
}

fn protocol(message: &str) -> PaykitSdkError {
    PaykitSdkError::Protocol {
        context: message.to_owned(),
        source: None,
    }
}

fn new_id() -> String {
    paykit_lib::EventId::new_v4().as_str().to_owned()
}

fn scope(
    tx: &dyn StorageTransaction,
    local_path: &PaykitReceiverPath,
    input: &PaymentRequestScope,
) -> Result<PaymentAccountingScope> {
    let identity = tx
        .load_identity_state()
        .and_then(|state| state.local_pubky_public_key)
        .ok_or_else(|| policy("Payment accounting requires an initialized payer identity"))?;
    Ok(PaymentAccountingScope {
        local_public_key: identity,
        local_receiver_path: local_path.clone(),
        counterparty: input.counterparty.clone(),
        counterparty_receiver_path: input.counterparty_receiver_path.clone(),
        payment_request_id: input.payment_request_id.as_str().to_owned(),
    })
}

fn key(
    tx: &dyn StorageTransaction,
    local_path: &PaykitReceiverPath,
    input: &PaymentOccurrence,
) -> Result<PaymentOccurrenceKey> {
    let billing_period = input
        .billing_period
        .as_ref()
        .map(|period| {
            let start = validation::utc_time(period.starts_at())?;
            let end = validation::utc_time(period.ends_at())?;
            if end <= start {
                return Err(policy("Billing Period must end after its start"));
            }
            Ok(AccountingBillingPeriod {
                starts_at: start,
                ends_at: end,
            })
        })
        .transpose()?;
    Ok(PaymentOccurrenceKey {
        request: scope(tx, local_path, &input.request)?,
        billing_period,
    })
}

fn ready(state: &AllowanceAccountingState) -> Result<()> {
    if state.requires_reconciliation {
        Err(policy(
            "Payment accounting requires complete wallet reconciliation",
        ))
    } else {
        Ok(())
    }
}

fn load(tx: &dyn StorageTransaction) -> Result<AllowanceAccountingState> {
    let state = tx
        .allowance_accounting_state()
        .ok_or_else(|| policy("Payment accounting requires complete wallet reconciliation"))?;
    validate_accounting(&state)?;
    Ok(state)
}

fn save(tx: &mut dyn StorageTransaction, mut state: AllowanceAccountingState) -> Result<()> {
    state.revision = state
        .revision
        .checked_add(1)
        .ok_or_else(|| protocol("Payment accounting revision exhausted"))?;
    validate_accounting(&state)?;
    tx.save_allowance_accounting_state(state);
    Ok(())
}

fn request(
    tx: &dyn StorageTransaction,
    scope: &PaymentAccountingScope,
    time: DateTime<Utc>,
) -> Result<crate::PaymentRequestRecord> {
    use crate::domain::payment_requests::payment_request_records_in_transaction;
    payment_request_records_in_transaction(
        tx,
        &scope.counterparty,
        &scope.counterparty_receiver_path,
        time,
    )?
    .into_iter()
    .find(|record| record.payment_request_id == scope.payment_request_id)
    .ok_or_else(|| policy("Payment Request history is unavailable"))
}

fn request_terms(record: &crate::PaymentRequestRecord) -> Result<PaymentRequestTerms> {
    crate::domain::payment_requests::request_from_record(record)
        .map(|request| request.request().clone())
        .ok_or_else(|| policy("Payment Request terms are unavailable"))
}

fn valid_request(
    tx: &dyn StorageTransaction,
    scope: &PaymentAccountingScope,
    record: &crate::PaymentRequestRecord,
    proposed: bool,
) -> bool {
    use crate::{LinkedPeerState, PaymentRequestLifecycleState as State, PaymentRequestLocalRole};
    record.local_role == Some(PaymentRequestLocalRole::Payer)
        && record.invalid_reason.is_none()
        && ((matches!(record.state, State::Accepted | State::ActiveRecurring)
            || (record.state == State::ProofSubmitted && record.accepted_event_id.is_some()))
            || (proposed && record.state == State::Proposed))
        && tx
            .linked_peer(&scope.counterparty, &scope.counterparty_receiver_path)
            .is_some_and(|peer| peer.state == LinkedPeerState::Linked)
}

fn allowance_terms(
    tx: &dyn StorageTransaction,
    scope: &PaymentAccountingScope,
    id: &str,
) -> Result<(crate::AllowanceRecord, AllowanceTerms)> {
    use crate::domain::allowances::allowance_records_in_transaction;
    let record = allowance_records_in_transaction(
        tx,
        &scope.counterparty,
        &scope.counterparty_receiver_path,
    )
    .into_iter()
    .find(|record| record.allowance_id == id)
    .ok_or_else(|| policy("Allowance history is unavailable"))?;
    let raw = if let Some(item_id) = record.proposal_stream_item_id {
        tx.private_stream_items(&scope.counterparty, &scope.counterparty_receiver_path)
            .into_iter()
            .find(|item| item.stream_item_id == item_id)
            .map(|item| item.raw_json)
    } else {
        tx.outbound_private_messages(&scope.counterparty, &scope.counterparty_receiver_path)
            .into_iter()
            .find(|item| Some(item.outbound_message_id) == record.proposal_outbound_message_id)
            .map(|item| item.raw_json)
    }
    .ok_or_else(|| policy("Allowance proposal history is unavailable"))?;
    let message = paykit_lib::PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.allowance_proposal".into()),
        raw_json: raw,
    };
    let parsed = paykit_lib::parse_allowance_event_message(&message)
        .ok_or_else(|| policy("Allowance proposal is invalid"))?;
    let Some(paykit_lib::AllowanceEvent::Proposal(proposal)) = parsed.parsed_event() else {
        return Err(policy("Allowance proposal is invalid"));
    };
    Ok((record, proposal.terms().clone()))
}

fn valid_allowance(record: &crate::AllowanceRecord) -> bool {
    record.local_role == Some(crate::AllowanceLocalRole::Allower)
        && record.state == crate::AllowanceLifecycleState::Accepted
        && record.history_status == crate::AllowanceHistoryStatus::Consistent
}

fn watermark(
    state: &mut AllowanceAccountingState,
    scope: &PaymentAccountingScope,
    id: &str,
    time: DateTime<Utc>,
) -> DateTime<Utc> {
    if let Some(record) = state
        .history
        .watermarks
        .iter_mut()
        .find(|record| watermark_scope(record, scope, id))
    {
        let previous = record.evaluated_at;
        record.evaluated_at = previous.max(time);
        previous
    } else {
        state.history.watermarks.push(AllowanceWatermarkRecord {
            local_public_key: scope.local_public_key.clone(),
            local_receiver_path: scope.local_receiver_path.clone(),
            counterparty: scope.counterparty.clone(),
            counterparty_receiver_path: scope.counterparty_receiver_path.clone(),
            allowance_id: id.to_owned(),
            evaluated_at: time,
        });
        time
    }
}

fn watermark_scope(
    record: &AllowanceWatermarkRecord,
    scope: &PaymentAccountingScope,
    id: &str,
) -> bool {
    record.local_public_key == scope.local_public_key
        && record.local_receiver_path == scope.local_receiver_path
        && record.counterparty == scope.counterparty
        && record.counterparty_receiver_path == scope.counterparty_receiver_path
        && record.allowance_id == id
}

fn association<'a>(
    state: &'a AllowanceAccountingState,
    key: &PaymentOccurrenceKey,
) -> Option<&'a AllowanceAssociationRevision> {
    state
        .history
        .associations
        .iter()
        .find(|record| record.request == key.request)?
        .revisions
        .iter()
        .rev()
        .find(|revision| {
            revision.effective_from.is_none_or(|boundary| {
                key.billing_period
                    .as_ref()
                    .is_some_and(|period| period.starts_at >= boundary)
            })
        })
}

fn blocked(reason: AllowanceAccountingBlock) -> PaymentAttemptDecision {
    PaymentAttemptDecision::Blocked { reason }
}
fn shared(error: paykit_lib::AllowanceEvaluationBlock) -> AllowanceAccountingBlock {
    AllowanceAccountingBlock::SharedRule {
        code: error.code().into(),
    }
}
fn occupied(record: &PaymentOccurrenceRecord) -> bool {
    record
        .attempts
        .iter()
        .any(|attempt| attempt.status != PaymentExecutionStatus::Failed)
}

/// Serialize an ordinary manual response with every payment admission path.
pub(crate) fn manual_response(
    tx: &mut dyn StorageTransaction,
    local: &PaykitReceiverPath,
    input: PaymentRequestScope,
) -> Result<()> {
    let scope = scope(tx, local, &input)?;
    let mut state = tx
        .allowance_accounting_state()
        .unwrap_or_else(|| AllowanceAccountingState {
            revision: 0,
            epoch: new_id(),
            requires_reconciliation: true,
            history: AllowanceAccountingHistory::default(),
        });
    validate_accounting(&state)?;
    // Prepared is known not to have received a handoff; revocation is atomic
    // with the response. Submitted/Unknown evidence is never released here.
    for occurrence in state
        .history
        .occurrences
        .iter_mut()
        .filter(|o| o.key.request == scope)
    {
        for attempt in &mut occurrence.attempts {
            if attempt.status == PaymentExecutionStatus::Prepared {
                attempt.status = PaymentExecutionStatus::Failed;
            }
        }
        if !occupied(occurrence) {
            occurrence.disposition = PaymentDisposition::ManualOnly;
        }
    }
    if !state
        .history
        .occurrences
        .iter()
        .any(|o| o.key.request == scope && o.key.billing_period.is_none())
    {
        state.history.occurrences.push(PaymentOccurrenceRecord {
            key: PaymentOccurrenceKey {
                request: scope,
                billing_period: None,
            },
            disposition: PaymentDisposition::ManualOnly,
            allowance_id: None,
            association_revision: None,
            attempts: Vec::new(),
        });
    }
    save(tx, state)
}

#[cfg(test)]
mod tests;
