//! Check the ordinary direct successor of the backed-up executor ledger (Paykit spec 8.3).

use std::collections::{hash_map::Entry, HashMap};

use paykit_lib::allowances::{
    check_successor, usage, AllowanceTerms, CompactedFailed, Ledger, LedgerEntry, Usage,
};
use paykit_lib::records::{bind_ledger, ActivityRecord, RecordBinding, StoredMessage};
use paykit_lib::{AllowanceId, Asset, Body, EndpointAmount, PaymentAmount};
use pubky_switch_client::app::{KeptOp, KeptReason, RecordRead};
use pubky_switch_core::ops::Condition;
use pubky_switch_core::shared::ApplicationRef;

use super::approval::ApprovedOpening;
use super::checkpoint::{ledger_target, ExecutorCheckpoint};
use super::compaction::{CompactionRead, LedgerCompaction};
use super::evidence::AppliedRecord;
use super::ExecutorHistory;
use super::{reconciled_usage, wallet_inventory, Accounting};
use crate::allowances::allowance_target;
use crate::state::key;
use crate::{Paykit, Result};

/// The SDK keeps the authenticated application it accepted. The spending batch must still
/// condition on both current records and atomically open the new attempt and reservation.
pub(super) struct ReconciledLedger {
    pub(super) ledger: Ledger,
    pub(super) usage: Usage,
    pub(super) evidence: KeptOp,
    pub(super) grant: Condition,
    pub(super) current: Condition,
    pub(super) terms: AllowanceTerms,
    pub(super) at: u64,
    pub(super) compaction: Option<LedgerCompaction>,
    pub(super) at_initial_ledger: bool,
}

fn compacted_transfer_matches(
    obligation: [u8; 32],
    amount: &PaymentAmount,
    retired: Option<&LedgerEntry>,
    failed: Option<&CompactedFailed>,
    grant_asset: &Asset,
) -> bool {
    let saved = match (retired, failed) {
        (Some(entry), None) => (entry.obligation, &entry.asset, &entry.value),
        (None, Some(entry)) => (entry.obligation, grant_asset, &entry.value),
        _ => return false,
    };
    saved == (obligation, &amount.asset, &amount.value)
}

/// A settled tombstone can outlive its signed opening. It corroborates a Done entry
/// already accepted under this grant, never a new reservation (Paykit spec 8.3).
/// The caller must also verify the dated payer proof and the method's exact transfer.
fn settled_after_accepted_payment(
    read: &pubky_switch_client::app::ClaimRead,
    previous: Option<&LedgerEntry>,
    accepted_at: u64,
    entry: &LedgerEntry,
    executor: [u8; 16],
) -> bool {
    use paykit_lib::allowances::EntryOutcome;
    use pubky_switch_core::claims::{ClaimStatus, Outcome};

    let claim = &read.view;
    claim.tombstone
        && read.entries.is_empty()
        && claim.status == ClaimStatus::Settled
        && !claim.rebuilt
        && !claim.divergent
        && claim.claim_key == entry.obligation
        && entry.outcome == Some(EntryOutcome::Done)
        && previous == Some(entry)
        && claim
            .attempt(&entry.attempt_id.to_bytes())
            .is_some_and(|attempt| {
                attempt.owner == executor
                    && attempt.outcome == Outcome::Done
                    && attempt.resolved_by == Some(executor)
                    && !attempt.duplicate
                    && !attempt.possibly_duplicate
                    && entry.at <= attempt.opened_at
                    && attempt.opened_at <= accepted_at
            })
}

/// Once the original journal expires, the seller's signed receipt still identifies
/// the completed occurrence. Accept its original fixed or plain amount only when every
/// allowed endpoint precision produces the exact transfer in the wallet's history.
fn receipted_period_amount(
    activity: &ActivityRecord,
    request: &StoredMessage,
    proof: &StoredMessage,
    transfer: &PaymentAmount,
) -> bool {
    let Body::PaymentRequest(request_body) = request.body() else {
        return false;
    };
    let Body::PaymentProof(paid) = proof.body() else {
        return false;
    };
    let Some(receipt) = crate::receipts::displayed_receipt(activity, Some(request_body)) else {
        return false;
    };
    let Some(period) = paid.billing_period else {
        return false;
    };
    if receipt.message().refs != [proof.hash(), request.hash()] {
        return false;
    }
    let request_hash = request.hash();
    let Ok(later_destination) = crate::periods::needs_period_destination(request_body, period)
    else {
        return false;
    };
    match (later_destination, proof.message().refs.as_slice()) {
        (false, [original]) if *original == request_hash => (),
        (true, [original, detail]) if *original == request_hash => {
            let Some(selected) = activity.period_details.as_ref() else {
                return false;
            };
            let Body::PaymentPeriodDetails(details) = selected.body() else {
                return false;
            };
            if selected.hash() != *detail
                || selected.message().refs != [request_hash]
                || selected.sender() != request.sender()
                || selected.check_payee_sender(request_body).is_err()
                || details.check_against(request_body).is_err()
                || details.billing_period != period
                || details.rates.is_some()
                || paid.list_version.is_some()
                || selected.cert().app_id() != paid.payment_app_id
                || !crate::periods::destination_app_permitted(request_body, paid.payment_app_id)
                || request_body
                    .accepted_payment_endpoint_identifiers
                    .as_ref()
                    .is_some_and(|ids| !ids.contains(&paid.payment_endpoint_identifier))
                || !details.payment_endpoints.as_ref().is_some_and(|endpoints| {
                    endpoints.contains_key(&paid.payment_endpoint_identifier)
                })
            {
                return false;
            }
        }
        _ => return false,
    }
    // A destination-only detail leaves the signed request's fixed or plain price intact.
    let rates = crate::conversion::fixed_rates(request_body.conversion.as_ref());
    let price = |digits| {
        matches!(paykit_lib::amount_for_endpoint(&request_body.amount, rates,
        &paid.payment_endpoint_identifier, digits), Ok(EndpointAmount::ByRate{amount,..}
        | EndpointAmount::RequestedAsset(amount)) if &amount == transfer)
    };
    price(0) && price(paykit_lib::amount::MAX_FRACTION_DIGITS as u32)
}

/// How this executor established the application it is about to incorporate.
#[derive(Clone, Copy)]
pub(super) enum LedgerRead {
    Inventory,
    Outcome,
    Applied(ApplicationRef),
}

#[derive(Clone, Copy)]
pub(super) enum LedgerChange<'a> {
    Reservation(&'a LedgerEntry),
    Outcome(&'a LedgerEntry),
}

/// Rebuild the complete executor write before accepting its accompanying claim.
pub(super) fn ledger_change<'a>(
    held: &Ledger,
    current: &'a Ledger,
    terms: &AllowanceTerms,
) -> Result<Option<LedgerChange<'a>>> {
    if let Some(entry) = current.entries.last().filter(|entry| {
        entry.outcome.is_none()
            && !held
                .entries
                .iter()
                .any(|e| e.attempt_id == entry.attempt_id)
            && !held.failed.iter().any(|e| e.attempt_id == entry.attempt_id)
    }) {
        let expected = super::ledger::reserved_ledger(terms, held.clone(), entry.clone())?;
        // A removed entry takes its unknown members with it on the wire (Paykit wire 5).
        return Ok(
            (expected.to_json()? == current.to_json()?).then_some(LedgerChange::Reservation(entry))
        );
    }
    let mut expected = held.clone();
    for (previous, entry) in expected.entries.iter_mut().zip(&current.entries) {
        if previous.outcome != entry.outcome {
            previous.outcome = entry.outcome;
            return Ok((expected == *current).then_some(LedgerChange::Outcome(entry)));
        }
    }
    Ok(None)
}

impl Paykit {
    /// Reconcile this app's accepted Allowance ledger following a restore (Paykit spec
    /// 8.3). Returns the verified usage, counting outstanding reservations in full. When
    /// the wallet lacks complete account history or the current signed grant, ledger and
    /// claim cannot be reconciled with its checkpoint, automatic use remains held.
    ///
    /// # Errors
    ///
    /// The stored or signed evidence cannot be read, the client raises a rollback alarm,
    /// or the service or storage operation fails. A successful reconciliation durably
    /// accepts the complete current ledger before reporting its usage.
    pub async fn reconcile_allowance(&mut self, record: &[u8; 32]) -> Result<Option<Usage>> {
        Ok(Box::pin(self.executor_base(record, LedgerRead::Inventory))
            .await?
            .map(|checked| checked.usage))
    }

    pub(super) async fn executor_base(
        &mut self,
        record: &[u8; 32],
        source_kind: LedgerRead,
    ) -> Result<Option<ReconciledLedger>> {
        let local = key::executor_checkpoint(record);
        let Some(history) = self.local_get::<ExecutorHistory>(&local).await? else {
            return Ok(None);
        };
        let Some(opening) = Box::pin(self.approved_opening(record)).await? else {
            return Ok(None);
        };
        let Some(source) = history.selected(&self.app, record, &opening) else {
            return Ok(None);
        };
        let mut checkpoint = Box::pin(self.finish_executor_grant(record, source, &opening)).await?;
        let Some(checked) =
            Box::pin(self.restore_executor_ledger(record, &checkpoint, &opening, source_kind))
                .await?
        else {
            return Ok(None);
        };
        if history.0.len() > 1
            || (!checked.at_initial_ledger && checkpoint.later.as_ref() != Some(&checked.evidence))
        {
            Box::pin(self.keep_executor_checkpoint(record, &mut checkpoint, &checked)).await?;
        }
        Ok(Some(checked))
    }

    async fn keep_executor_checkpoint(
        &mut self,
        record: &[u8; 32],
        checkpoint: &mut ExecutorCheckpoint,
        checked: &ReconciledLedger,
    ) -> Result<()> {
        let mut tx = crate::state::Tx::default();
        if let Some(receipt) = &checked.compaction {
            let Condition::RecordCurrent { key, .. } = checked.current else {
                return Err(crate::PaykitError::corrupt());
            };
            tx.put(&receipt.key(&key)?, receipt)?;
        }
        if !checked.at_initial_ledger {
            checkpoint.later = Some(checked.evidence.clone());
        }
        tx.put(
            &key::executor_checkpoint(record),
            &ExecutorHistory(vec![checkpoint.clone()]),
        )?;
        self.local_commit(tx).await
    }

    async fn restore_executor_ledger(
        &mut self,
        permanent_key: &[u8; 32],
        checkpoint: &ExecutorCheckpoint,
        approval: &ApprovedOpening,
        source: LedgerRead,
    ) -> Result<Option<ReconciledLedger>> {
        let Some(accepted) = checkpoint.accepted(&self.app, permanent_key, approval) else {
            return Ok(None);
        };
        let id = AllowanceId::parse(&checkpoint.allowance_id)?;
        let allowance = allowance_target(&checkpoint.counterparty, &id);
        let ledger_target = ledger_target(&checkpoint.counterparty, &id);
        let ledger_key = self.app.record_key(&ledger_target)?;
        let RecordRead::Fresh(current_grant) =
            Box::pin(self.app.read_record_fresh(&allowance)).await?
        else {
            return Ok(None);
        };
        let current_ledger = match Box::pin(self.app.read_record_fresh(&ledger_target)).await? {
            RecordRead::Fresh(value) => value,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                ..
            } => {
                let alarm = self
                    .app
                    .raise_rollback(
                        crate::protocol::BACKUP_NAME,
                        "the current service is missing the executor's accepted permanent ledger",
                    )
                    .await?;
                return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
            }
            _ => return Ok(None),
        };
        let originals = self.app.shared_evidence().await?;
        let Some(grant) = originals
            .ops
            .iter()
            .find_map(|op| AppliedRecord::current(&self.app, &allowance, &current_grant, op))
        else {
            return Ok(None);
        };
        // An intervening grant, handoff, end or other Allowance change requires its own
        // authorization and accounting checks. This path covers the same grant throughout.
        if grant.revision != accepted.grant.revision {
            return Ok(None);
        }
        let Some((current, evidence)) = originals.ops.iter().find_map(|op| {
            Some((
                AppliedRecord::current(&self.app, &ledger_target, &current_ledger, op)?,
                op,
            ))
        }) else {
            return Ok(None);
        };
        let mut claims = HashMap::new();
        let Ok(ledger) = bind_ledger(
            &current.value,
            self.app.naming_key(),
            &ledger_key,
            &accepted.record,
        ) else {
            return Ok(None);
        };
        let Body::AllowanceProposal(proposed) = accepted.record.proposal.body() else {
            return Ok(None);
        };
        let exact_grant = Condition::RecordCurrent {
            key: *permanent_key,
            revision: grant.revision,
            deleted: false,
        };
        let mut compaction = None;
        let mut reads = HashMap::new();
        if current.revision != accepted.application.revision {
            if current.expected < accepted.application.version() {
                let alarm = self
                    .app
                    .raise_rollback(
                        crate::protocol::BACKUP_NAME,
                        "the ledger write is based before the executor's accepted version",
                    )
                    .await?;
                return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
            }
            if !current.follows(&accepted.application)
                || !current.cert.holds_scope("paykit_allowance_execute")
                || current.op.body().app_id != Some(self.app_id())
                || !current.op.body().conditions.contains(&exact_grant)
                || current.at_ms < accepted.application.at_ms
                || current.at_ms < grant.at_ms
                || check_successor(
                    &proposed.terms,
                    &accepted.ledger,
                    accepted.application.at_ms,
                    &ledger,
                    current.at_ms,
                )
                .is_err()
            {
                return Ok(None);
            }
            let Some(change) = ledger_change(&accepted.ledger, &ledger, &proposed.terms)? else {
                return Ok(None);
            };
            let entry = match change {
                LedgerChange::Reservation(entry) => {
                    if !current.opens_in_window(&proposed.terms, entry)? {
                        return Ok(None);
                    }
                    entry
                }
                LedgerChange::Outcome(entry) => entry,
            };
            let Some(claim) = self.app.read_claim(&entry.obligation).await? else {
                return Ok(None);
            };
            let with_claim = self.app.shared_evidence().await?;
            if matches!(change, LedgerChange::Reservation(_)) {
                let Some((opening, original)) = with_claim
                    .ops
                    .iter()
                    .find_map(|op| Some((current.reservation(&self.app, op, entry, &claim)?, op)))
                else {
                    return Ok(None);
                };
                if ledger.count > accepted.ledger.count {
                    compaction = Some(LedgerCompaction {
                        before: checkpoint
                            .later
                            .as_ref()
                            .unwrap_or(&approval.ledger_write)
                            .clone(),
                        after: evidence.clone(),
                        opening: original.clone(),
                    });
                }
                claims.insert(entry.attempt_id, opening);
            } else if !with_claim.ops.iter().any(|op| {
                current
                    .applied_outcome(&self.app, op, entry, &claim)
                    .is_some()
            }) {
                return Ok(None);
            }
            reads.insert(entry.obligation, Some(claim));
        }
        let Some(at) = self.service_time() else {
            return Ok(None);
        };
        let needs_inventory = match source {
            LedgerRead::Inventory => true,
            LedgerRead::Outcome => {
                checkpoint.restored || current.revision != accepted.application.revision
            }
            LedgerRead::Applied(revision) if revision == current.revision => false,
            LedgerRead::Applied(_) => return Ok(None),
        };
        let result = |usage, ledger, grant, compaction| ReconciledLedger {
            compaction,
            at_initial_ledger: current.revision == approval.ledger.revision,
            at,
            terms: proposed.terms.clone(),
            ledger,
            usage,
            evidence: evidence.clone(),
            grant,
            current: Condition::RecordCurrent {
                key: ledger_key,
                revision: current.revision,
                deleted: false,
            },
        };
        if !needs_inventory {
            let Ok(counted) = usage(&proposed.terms, &ledger, &HashMap::new(), at) else {
                return Ok(None);
            };
            return Ok(Some(result(counted, ledger, exact_grant, compaction)));
        }
        let Some(inventory) = wallet_inventory(&*self.rail, self.identity()).await else {
            return Ok(None);
        };
        let mut compactions: Vec<_> = if ledger.count == 0 {
            Vec::new()
        } else {
            self.local_scan::<LedgerCompaction>(&key::ledger_compactions(&ledger_key))
                .await?
                .into_iter()
                .map(|(_, row)| row)
                .collect()
        };
        if let Some(receipt) = &compaction {
            if !compactions.contains(receipt) {
                compactions.push(receipt.clone());
            }
        }
        // An open claim reserves its entry even before dispatch (Paykit spec 8.3).
        // Terminal entries only need a claim read when attributing a wallet attempt.
        let dispatched: HashMap<_, _> = inventory.iter().map(|d| (d.attempt_id, d)).collect();
        let mut settled_payments = Vec::new();
        let entries: Vec<_> = ledger
            .entries
            .iter()
            .filter(|entry| {
                !claims.contains_key(&entry.attempt_id)
                    && (entry.outcome.is_none() || dispatched.contains_key(&entry.attempt_id))
            })
            .collect();
        let failed: Vec<_> = ledger
            .failed
            .iter()
            .filter(|entry| dispatched.contains_key(&entry.attempt_id))
            .collect();
        for obligation in entries
            .iter()
            .map(|e| e.obligation)
            .chain(failed.iter().map(|e| e.obligation))
        {
            if let Entry::Vacant(slot) = reads.entry(obligation) {
                slot.insert(self.app.read_claim(&obligation).await?);
            }
        }
        if !entries.is_empty() || !failed.is_empty() {
            let ops = self.app.shared_evidence().await?;
            let accepted_entries: HashMap<_, _> = accepted
                .ledger
                .entries
                .iter()
                .map(|e| (e.attempt_id, e))
                .collect();
            let openings = || ops.ops.iter().chain(compactions.iter().map(|c| &c.opening));
            for entry in entries {
                let Some(read) = reads.get(&entry.obligation).and_then(Option::as_ref) else {
                    continue;
                };
                if let Some(opening) = openings().find_map(|op| {
                    accepted
                        .application
                        .accepted_opening(&self.app, op, entry, read, &exact_grant)
                }) {
                    claims.insert(entry.attempt_id, opening);
                } else if let Some(payment) = dispatched.get(&entry.attempt_id).filter(|payment| {
                    matches!(payment.outcome, crate::rail::RailOutcome::Paid(_))
                        && settled_after_accepted_payment(
                            read,
                            accepted_entries.get(&entry.attempt_id).copied(),
                            accepted.application.at_ms,
                            entry,
                            self.app_id(),
                        )
                }) {
                    claims.insert(
                        entry.attempt_id,
                        super::ReservationClaim {
                            obligation: entry.obligation,
                            state: paykit_lib::allowances::ClaimState::Done,
                        },
                    );
                    settled_payments.push((*payment).clone());
                } else if read.view.rebuilt
                    || read.view.divergent
                    || read.view.attempt(&entry.attempt_id.to_bytes()).is_some()
                {
                    // An uncertain view or an attempt without its bound opening doesn't
                    // establish that this claim has disappeared.
                    return Ok(None);
                }
            }
            for entry in failed {
                let Some(read) = reads.get(&entry.obligation).and_then(Option::as_ref) else {
                    continue;
                };
                if let Some(opening) = openings()
                    .find_map(|op| current.failed_opening(&self.app, op, entry, read, &exact_grant))
                {
                    claims.insert(entry.attempt_id, opening);
                }
            }
        }
        let Some(Accounting {
            counted,
            unclassified,
        }) = reconciled_usage(inventory, &proposed.terms, &ledger, &claims, at)
        else {
            return Ok(None);
        };
        let mut retired = if ledger.count == 0 {
            HashMap::new()
        } else {
            let read = CompactionRead {
                target: &ledger_target,
                key: ledger_key,
                accepted: &accepted,
                grant: exact_grant,
                current: &current,
                ledger: &ledger,
            };
            let Some(retired) = read.payments(&self.app, &compactions) else {
                return Ok(None);
            };
            retired
        };
        if !unclassified.is_empty() || !settled_payments.is_empty() {
            let records = self.payer_records().await?;
            for dispatch in unclassified.into_iter().chain(settled_payments) {
                let crate::rail::RailOutcome::Paid(method) = &dispatch.outcome else {
                    return Ok(None);
                };
                let mut related = records.iter().filter_map(|activity| {
                    let proof = activity.proof.as_ref()?;
                    let Body::PaymentProof(paid) = proof.body() else {
                        return None;
                    };
                    (paid.attempt_id == dispatch.attempt_id).then_some((activity, proof, paid))
                });
                let Some((activity, proof, paid)) = related.next() else {
                    return Ok(None);
                };
                if related.next().is_some()
                    || proof.sender() != self.identity()
                    || paid.proof != method.proof
                {
                    return Ok(None);
                }
                let natural = activity
                    .natural_key_with(&self.identity(), &|proof| self.rail.settlement_id(proof))?;
                let target = crate::activity::ActivityTarget::payer(natural).record_target();
                let _ = self.app.read_record_fresh(&target).await?;
                let dated = self.app.shared_evidence().await?.ops.iter().any(|op| {
                    AppliedRecord::retained(&self.app, &target, op)
                        .is_some_and(|write| write.dates_payment_proof(&self.app, proof))
                });
                if !dated {
                    return Ok(None);
                }
                let binding = activity.binding()?;
                let request = match binding {
                    RecordBinding::Request { .. } => {
                        crate::periods::request_for_payment(activity, activity, &binding)
                    }
                    RecordBinding::Period { .. } => records.iter().find_map(|parent| {
                        crate::periods::request_for_payment(parent, activity, &binding)
                    }),
                    RecordBinding::Unrequested => None,
                };
                let signed_amount = match (binding, request) {
                    (RecordBinding::Unrequested, _) => paid.amount.as_ref(),
                    (RecordBinding::Request { .. }, Some((_, body)))
                        if body.conversion.is_none() =>
                    {
                        Some(&body.amount)
                    }
                    (RecordBinding::Period { .. }, Some((_, body)))
                        if paid.payment_endpoint_identifier.asset_segment()
                            == Some(body.amount.asset.as_str())
                            && !crate::conversion::fixed_rates(body.conversion.as_ref())
                                .is_some_and(|rates| {
                                    !matches!(
                                        paykit_lib::rate_for_endpoint(
                                            rates,
                                            &paid.payment_endpoint_identifier
                                        ),
                                        Ok(None)
                                    )
                                }) =>
                    {
                        // Period rates only price other assets (Paykit wire 4.9).
                        Some(&body.amount)
                    }
                    _ => None,
                };
                let obligation = request.map(|(request, _)| {
                    self.obligation(
                        &activity.counterparty,
                        &request.hash(),
                        paid.billing_period.map_or(0, |p| p.start_secs()),
                    )
                });
                let this_allowance = activity.counterparty == checkpoint.counterparty
                    && paid.allowance_id.as_ref() == Some(&id);
                if let Some(entry) = ledger
                    .entries
                    .iter()
                    .find(|e| e.attempt_id == dispatch.attempt_id)
                {
                    // Only a paid tombstone reaches this branch. Its accepted reservation
                    // pins the amount already matched to the method, including rounding.
                    if !this_allowance
                        || proof.cert().app_id() != self.app_id()
                        || obligation != Some(entry.obligation)
                        || request.is_none_or(|(original, _)| {
                            proof.message().refs.first() != Some(&original.hash())
                        })
                        || signed_amount.is_none_or(|amount| amount.asset != entry.asset)
                    {
                        return Ok(None);
                    }
                    continue;
                }
                let failed = ledger
                    .failed
                    .iter()
                    .find(|entry| entry.attempt_id == dispatch.attempt_id);
                let compacted = retired.get(&dispatch.attempt_id);
                if compacted.is_some() || failed.is_some() {
                    if !this_allowance
                        || signed_amount
                            .is_none_or(|nominal| nominal.asset != dispatch.amount.asset)
                        || !obligation.is_some_and(|obligation| {
                            compacted_transfer_matches(
                                obligation,
                                &dispatch.amount,
                                compacted,
                                failed,
                                &proposed.terms.asset,
                            )
                        })
                    {
                        return Ok(None);
                    }
                    retired.remove(&dispatch.attempt_id);
                    continue;
                }
                if signed_amount != Some(&dispatch.amount) {
                    let Some(obligation) = obligation.filter(|_| !this_allowance) else {
                        return Ok(None);
                    };
                    let entry = self
                        .journal_entry_on(&dispatch.attempt_id, &obligation)
                        .await?;
                    let amount_proven = match entry {
                        Some(entry) if entry.transfer_bound.is_some() => {
                            entry.paid_amount()?.as_ref() == Some(&dispatch.amount)
                                && self.journal_matches_paid_proof(&entry, proof)?
                        }
                        _ => request.is_some_and(|(original, _)| {
                            receipted_period_amount(activity, original, proof, &dispatch.amount)
                        }),
                    };
                    if !amount_proven {
                        return Ok(None);
                    }
                }
                if !this_allowance {
                    continue;
                }
                if let Some(obligation) = obligation {
                    // The opening after the accepted ledger excludes lawful compaction
                    // before that checkpoint. A direct reservation successor only
                    // compacts existing final entries by the ordinary ledger rule.
                    if !reads.contains_key(&obligation) {
                        let _ = self.app.read_claim(&obligation).await?;
                    }
                    if self.app.shared_evidence().await?.ops.iter().any(|op| {
                        accepted.application.opening_after(
                            &self.app,
                            op,
                            &exact_grant,
                            &obligation,
                            dispatch.attempt_id,
                        )
                    }) {
                        let alarm = self.app.raise_rollback(
                            crate::protocol::BACKUP_NAME,
                            "the service ledger omits a paid Allowance attempt opened after the accepted write",
                        ).await?;
                        return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
                    }
                }
                return Ok(None);
            }
        }
        if !retired.is_empty() {
            return Ok(None);
        }
        Ok(Some(result(counted, ledger, exact_grant, compaction)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_lib::allowances::EntryOutcome;
    use paykit_lib::{Asset, AttemptId, Decimal};

    #[test]
    fn test_journal_less_period_receipt_uses_the_original_requests_selling_scope() {
        check_period_receipt_selling_scope(false);
    }

    #[test]
    fn test_journal_less_fixed_rate_period_with_a_signed_destination_checks_receipt_scope() {
        check_period_receipt_selling_scope(true);
    }

    fn check_period_receipt_selling_scope(later_destination: bool) {
        use crate::testkit::{object, Party, NOW};
        use paykit_lib::bodies::PaymentReceipt;
        use paykit_lib::periods::{AnchoredRule, PeriodRef, Recurrence, Unit};
        use paykit_lib::records::ActivitySide;
        use serde_json::json;

        let payer = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new(
            "bob",
            "subscriptions",
            &[
                "paykit_request:subscriptions",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let schedule =
            Recurrence::new(AnchoredRule::new(NOW, 1, Unit::Hour).unwrap(), NOW, None).unwrap();
        let period = PeriodRef::from(schedule.periods(2).unwrap()[usize::from(later_destination)]);
        let mut subscription = object(json!({
            "amount": { "asset": "usd", "value": "1" },
            "payment_reference": "subscription-1", "proposal_expires_at": null,
            "label": "subscriptions", "recurrence": schedule.to_json(),
            "conversion": { "type": "fixed", "rates": [{ "asset": "btc", "value": "2" }] }
        }));
        if later_destination {
            subscription.insert("per_period_endpoints".into(), json!(true));
            subscription.insert(
                "required_app_id".into(),
                json!(pubky_switch_core::enc::zbase32_encode(&shop.app_id())),
            );
            subscription.insert(
                "accepted_payment_endpoint_identifiers".into(),
                json!(["btc-lightning-bolt11"]),
            );
        }
        let request = shop.message(&payer, "paykit.payment_request", vec![], subscription);
        let selected = later_destination.then(|| {
            let details = object(json!({
                "billing_period": period.to_json(),
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" },
                "expires_at": period.ends_at
            }));
            shop.message(
                &payer,
                "paykit.payment_period_details",
                vec![request.hash()],
                details,
            )
        });
        let mut references = vec![request.hash()];
        if let Some(detail) = &selected {
            references.push(detail.hash());
        }
        let attempt = AttemptId::from_bytes([3; 16]);
        let proof = payer.message(
            &shop,
            "paykit.payment_proof",
            references,
            object(json!({
                "attempt_id": attempt.to_text(), "billing_period": period.to_json(),
                "payment_endpoint_identifier": "btc-lightning-bolt11",
                "payment_app_id": pubky_switch_core::enc::zbase32_encode(&shop.app_id()),
                "label": "subscriptions", "proof": {}
            })),
        );
        let transfer = PaymentAmount {
            asset: Asset::new("btc").unwrap(),
            value: Decimal::new("2").unwrap(),
        };
        let body = PaymentReceipt {
            attempt_id: attempt,
            billing_period: Some(period),
            metadata: None,
            payment_reference: None,
        }
        .to_body(2)
        .unwrap();
        let mut parent = ActivityRecord::new(ActivitySide::Payer, shop.id());
        parent.request = Some(request.clone());
        let mut activity = ActivityRecord::new(ActivitySide::Payer, shop.id());
        activity.request_hash = Some(request.hash());
        activity.billing_period = Some(period);
        activity.proof = Some(proof.clone());
        activity.period_details = selected;
        for (scope, allowed) in [
            ("paykit_request:subscriptions", true),
            ("paykit_request", true),
            ("paykit_request:shipping", false),
        ] {
            let counter = Party::new(
                "bob",
                scope,
                &[scope, "paykit_settlements", "paykit_contacts"],
            );
            activity.receipt = Some(counter.message(
                &payer,
                "paykit.payment_receipt",
                vec![proof.hash(), request.hash()],
                body.clone(),
            ));
            assert_eq!(
                crate::receipts::receipt_binding(&activity, Some(&parent))
                    == crate::receipts::ReceiptBinding::Verified,
                allowed,
                "{scope}"
            );
            assert_eq!(
                receipted_period_amount(&activity, &request, &proof, &transfer),
                allowed,
                "{scope}"
            );
        }
    }

    #[test]
    fn test_settled_claim_needs_an_exact_previously_accepted_payment() {
        use pubky_switch_client::app::ClaimRead;
        use pubky_switch_core::claims::{AttemptView, ClaimStatus, ClaimView, Outcome};

        let executor = [7; 16];
        let entry = LedgerEntry {
            obligation: [2; 32],
            attempt_id: AttemptId::from_bytes([4; 16]),
            asset: Asset::new("btc").unwrap(),
            value: Decimal::new("70").unwrap(),
            at: 1000,
            outcome: Some(EntryOutcome::Done),
        };
        let mut read = ClaimRead {
            entries: Vec::new(),
            view: ClaimView {
                claim_key: entry.obligation,
                status: ClaimStatus::Settled,
                owner: None,
                attempt_id: None,
                step: 2,
                attempts: vec![AttemptView {
                    attempt_id: entry.attempt_id.to_bytes(),
                    owner: executor,
                    outcome: Outcome::Done,
                    opened_at: 1002,
                    orphan_after: 60,
                    orphaned: false,
                    possibly_duplicate: false,
                    duplicate: false,
                    resolved_by: Some(executor),
                }],
                tombstone: true,
                deadline: None,
                divergent: false,
                rebuilt: false,
                next_possibly_duplicate: false,
            },
        };
        let matches = |read: &ClaimRead, previous: Option<&LedgerEntry>, current: &LedgerEntry| {
            settled_after_accepted_payment(read, previous, 1004, current, executor)
        };
        assert!(matches(&read, Some(&entry), &entry));
        assert!(!matches(&read, None, &entry));
        let mut different = entry.clone();
        different.value = Decimal::new("30").unwrap();
        assert!(!matches(&read, Some(&entry), &different));
        different = entry.clone();
        different.outcome = None;
        assert!(!matches(&read, Some(&different), &different));
        read.view.attempts[0].duplicate = true;
        assert!(!matches(&read, Some(&entry), &entry));
        read.view.attempts[0].duplicate = false;
        read.view.attempts[0].owner = [8; 16];
        assert!(!matches(&read, Some(&entry), &entry));
        read.view.attempts[0].owner = executor;
        read.view.rebuilt = true;
        assert!(!matches(&read, Some(&entry), &entry));
    }

    #[test]
    fn test_compacted_reservation_uses_its_original_transfer_amount() {
        let asset = Asset::new("btc").unwrap();
        let failed = CompactedFailed {
            obligation: [6; 32],
            attempt_id: AttemptId::from_bytes([5; 16]),
            value: Decimal::new("0.00000002").unwrap(),
        };
        let transferred = PaymentAmount {
            asset: asset.clone(),
            value: failed.value.clone(),
        };
        let nominal = PaymentAmount {
            asset: asset.clone(),
            value: Decimal::new("0.000000015").unwrap(),
        };
        let settled = LedgerEntry {
            obligation: failed.obligation,
            attempt_id: failed.attempt_id,
            value: transferred.value.clone(),
            asset: asset.clone(),
            at: 1_000,
            outcome: Some(EntryOutcome::Done),
        };
        assert!(compacted_transfer_matches(
            settled.obligation,
            &transferred,
            Some(&settled),
            None,
            &asset,
        ));
        assert!(!compacted_transfer_matches(
            settled.obligation,
            &nominal,
            Some(&settled),
            None,
            &asset,
        ));
        assert!(compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            [7; 32],
            &transferred,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &nominal,
            None,
            Some(&failed),
            &asset
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            Some(&failed),
            &Asset::new("usd").unwrap()
        ));
        assert!(!compacted_transfer_matches(
            failed.obligation,
            &transferred,
            None,
            None,
            &asset
        ));
    }

    #[test]
    fn test_applied_reservation_compacts_old_entries_into_the_exact_totals() {
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset":"btc","lifetime_amount_limit":"10000","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let mut held = Ledger::empty(
            [7; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [8; 32],
        );
        let entry = |n: u128, at, outcome| LedgerEntry {
            obligation: [6; 32],
            attempt_id: AttemptId::from_bytes(n.to_be_bytes()),
            value: Decimal::new("1").unwrap(),
            asset: Asset::new("btc").unwrap(),
            at,
            outcome,
        };
        // These completed within one window, before any entry could move into totals.
        while held.to_json().unwrap().len() < 197_000 {
            for _ in 0..50 {
                let n = held.entries.len() as u128;
                let outcome = if n == 0 {
                    EntryOutcome::Failed
                } else {
                    EntryOutcome::Done
                };
                held.entries.push(entry(n, 1_000, Some(outcome)));
            }
        }
        assert!(held.to_json().unwrap().len() < 256 * 1024 - 4);
        let next = entry(u128::MAX, 100_000_000, None);
        let mut applied =
            super::super::ledger::reserved_ledger(&terms, held.clone(), next.clone()).unwrap();
        assert_eq!(applied.entries, vec![next.clone()]);
        assert_eq!(applied.count, (held.entries.len() - 1) as u64);
        assert_eq!(applied.failed.len(), 1);
        let removed = super::super::compaction::retired_paid(held.clone(), &applied);
        assert_eq!(removed, held.entries[1..]);
        assert_eq!(removed.len() as u64, applied.count);
        assert_eq!(
            removed.iter().map(|e| e.value.scaled()).sum::<u128>(),
            applied.spent.as_ref().unwrap().scaled()
        );
        let failed = &held.entries[0];
        let belongs = super::super::spend::accounts_for_attempt;
        assert!(belongs(
            &held,
            failed.obligation,
            failed.attempt_id.to_bytes()
        ));
        assert!(belongs(
            &applied,
            failed.obligation,
            failed.attempt_id.to_bytes()
        ));
        assert!(!belongs(&applied, [9; 32], failed.attempt_id.to_bytes()));
        assert!(
            matches!(ledger_change(&held, &applied, &terms).unwrap(), Some(LedgerChange::Reservation(e)) if e == &next)
        );
        applied.count += 1;
        assert!(ledger_change(&held, &applied, &terms).unwrap().is_none());
        applied.count -= 1;
        applied.failed.clear();
        assert!(ledger_change(&held, &applied, &terms).unwrap().is_none());
    }
}
