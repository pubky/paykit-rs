//! Verified outcomes shown to the executor of an Allowance (Paykit spec 8.3).

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{record_outcomes, AttemptEvidence, ClaimState, EntryOutcome, Ledger};
use paykit_lib::records::{ActivityRecord, RecordBinding};
use paykit_lib::{Asset, AttemptId, Body, Decimal};
use pubky_dispatch_client::app::{OpBatch, RecordRead, UserConfirmed};
use pubky_dispatch_core::ops::Condition;

use super::evidence::AppliedRecord;
use super::restore::LedgerRead;
use super::ReservationClaim;
use crate::activity::ActivityTarget;
use crate::{Paykit, Refusal, Result};

/// An unresolved reservation whose failure was reported by an app other than its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedByOtherApp {
    /// The attempt on the Allowance's ledger.
    pub attempt_id: AttemptId,
    /// The reserved transfer amount in the Allowance's asset.
    pub value: Decimal,
}

#[derive(Default)]
pub(super) struct LedgerFacts {
    pub(super) signed: HashMap<AttemptId, AttemptEvidence>,
    release_records: HashMap<AttemptId, Condition>,
    other_failures: Vec<ReservedByOtherApp>,
}

impl LedgerFacts {
    /// Whether an attempt another app reported failed still holds room the user can give
    /// back ([`Paykit::release_allowance_attempt`]).
    /// The amount, scaled, the user could give back by releasing every attempt another app
    /// reported failed that is releasable by the evidence the release itself requires:
    /// never paid, ended before handoff on this installation, failed by another app, and
    /// with the record write the release is conditioned on. `None` when there is none.
    pub(super) fn releasable(&self) -> Option<u128> {
        self.other_failures
            .iter()
            .filter(|failure| {
                self.release_records.contains_key(&failure.attempt_id)
                    && self.signed.get(&failure.attempt_id).is_some_and(|fact| {
                        !fact.paid
                            && fact.never_dispatched
                            && fact.claim == Some(ClaimState::Failed { by_owner: false })
                    })
            })
            .try_fold(None, |freed: Option<u128>, failure| {
                Some(Some(
                    freed.unwrap_or(0).checked_add(failure.value.scaled())?,
                ))
            })
            .flatten()
    }
}

impl Paykit {
    /// Reservations whose failure another app reported (Paykit spec 8.3). Display the
    /// amount and exact attempt when asking the user to release a reservation.
    ///
    /// # Errors
    ///
    /// The current executor grant, ledger or complete wallet history cannot be verified,
    /// the executor scope is missing, or the underlying read fails.
    pub async fn reserved_by_other_apps(
        &mut self,
        record: &[u8; 32],
    ) -> Result<Vec<ReservedByOtherApp>> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        if !self.holds("paykit_allowance_execute") {
            return Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into());
        }
        let checked = Box::pin(self.executor_base(record, LedgerRead::Inventory))
            .await?
            .ok_or_else(super::preparation::no_executor_base)?;
        Ok(checked.facts.other_failures)
    }

    /// Confirm the release of the displayed attempt (Paykit spec 8.3). The current
    /// claim must still show another app's failure. This executor must have durably
    /// ended the attempt before handoff on the original installation.
    ///
    /// # Errors
    ///
    /// Returns a refusal if the supporting evidence is missing or the attempt paid.
    /// The client's kept error carries the pending batch ID if application is unknown.
    pub async fn release_allowance_attempt(
        &mut self,
        record: &[u8; 32],
        attempt_id: AttemptId,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        // A delivery operation for the whole call (Paykit spec 8.1).
        let _delivery = self.delivery_operation();
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        if !self.holds("paykit_allowance_execute") {
            return Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into());
        }
        let mut checked = Box::pin(self.executor_base(record, LedgerRead::Inventory))
            .await?
            .ok_or_else(super::preparation::no_executor_base)?;
        let refused = || {
            Refusal::Usage("release requires another app's failed claim and this wallet's evidence that the attempt ended before handoff".into())
        };
        let entry = checked
            .ledger
            .entries
            .iter_mut()
            .find(|entry| entry.attempt_id == attempt_id && entry.outcome.is_none())
            .ok_or_else(refused)?;
        let Some(fact) = checked.facts.signed.get(&attempt_id) else {
            return Err(refused().into());
        };
        if fact.paid
            || !fact.never_dispatched
            || fact.claim != Some(ClaimState::Failed { by_owner: false })
        {
            return Err(refused().into());
        }
        let Some(read) = self.app.read_claim(&entry.obligation).await? else {
            return Err(refused().into());
        };
        let Some(shown) = read.view.attempt(&attempt_id.to_bytes()) else {
            return Err(refused().into());
        };
        if read.view.claim_key != entry.obligation
            || read.view.divergent
            || read.view.rebuilt
            || shown.duplicate
            || shown.possibly_duplicate
            || shown.owner != self.app_id()
            || shown.outcome != pubky_dispatch_core::claims::Outcome::Failed
            || shown.resolved_by.is_none_or(|by| by == shown.owner)
        {
            return Err(refused().into());
        }
        let payer = checked
            .facts
            .release_records
            .remove(&attempt_id)
            .ok_or_else(refused)?;
        entry.outcome = Some(EntryOutcome::Failed);
        let claim = Condition::ClaimStep {
            claim_key: entry.obligation,
            step: read.view.step,
        };
        Box::pin(self.submit_outcomes(record, checked, vec![claim, payer])).await
    }

    async fn submit_outcomes(
        &mut self,
        record: &[u8; 32],
        checked: super::restore::ReconciledLedger,
        conditions: Vec<Condition>,
    ) -> Result<()> {
        let Condition::RecordCurrent { key: ledger, .. } = checked.current else {
            return Err(crate::PaykitError::corrupt());
        };
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        let prepared = checked.stage(self, batch, conditions, true).await?;
        let applied = self.submit(prepared).await?;
        let name = pubky_dispatch_core::enc::b64(&ledger);
        let revision = applied
            .proofs
            .iter()
            .find(|p| p.body().object == name)
            .and_then(|p| p.record_revision().ok())
            .ok_or_else(super::preparation::written_unverified)?;
        Box::pin(self.executor_base(record, LedgerRead::Applied(revision)))
            .await?
            .ok_or_else(super::preparation::written_unverified)?;
        Ok(())
    }

    /// Copy outcomes from authenticated claims and payer records after receive and resume.
    pub(crate) async fn record_ledger_outcomes(&mut self) -> Result<()> {
        if !self.holds("paykit_allowance_execute") {
            return Ok(());
        }
        for (_, history) in self
            .local_scan::<super::ExecutorHistory>(crate::state::key::EXECUTOR_CHECKPOINTS)
            .await?
        {
            let first = history.0.first().ok_or_else(crate::PaykitError::corrupt)?;
            let id = paykit_lib::AllowanceId::parse(&first.allowance_id)?;
            let record = self.app.record_key(&crate::allowances::allowance_target(
                &first.counterparty,
                &id,
            ))?;
            let target = super::checkpoint::ledger_target(&first.counterparty, &id);
            // The read has already updated the local copy. A complete ledger with only
            // paid entries has nothing to copy. This check never authorizes a write.
            if self
                .app
                .shared_record(&self.app.record_key(&target)?)
                .await?
                .and_then(|r| Ledger::from_json(r.content.value()?).ok())
                .is_some_and(|l| {
                    l.failed.is_empty()
                        && l.entries
                            .iter()
                            .all(|e| e.outcome == Some(EntryOutcome::Done))
                })
                && self
                    .local_scan::<crate::state::LedgerBatchIds>(&crate::state::key::ledger_batches(
                        &record,
                    ))
                    .await?
                    .is_empty()
            {
                continue;
            }
            let result = async {
                let Some(mut checked) =
                    Box::pin(self.executor_from_history(&record, LedgerRead::Inventory, history))
                        .await?
                else {
                    return Ok(());
                };
                if let Some(next) =
                    record_outcomes(&checked.terms, &checked.ledger, &checked.facts.signed)?
                {
                    checked.ledger = next;
                    Box::pin(self.submit_outcomes(&record, checked, Vec::new())).await?;
                }
                Ok(())
            }
            .await;
            let result: Result<()> = result;
            if let Err(error) = result {
                crate::pay::unless_stop(error)?;
            }
        }
        Ok(())
    }

    /// Bind each signed payer proof to its activity's own request or occurrence and
    /// the original reservation. A payment method result alone cannot be copied.
    fn allowance_proof(
        &self,
        activity: &ActivityRecord,
        ledger: &Ledger,
    ) -> Result<Option<(AttemptId, [u8; 32])>> {
        let Some(proof) = activity.proof.as_ref() else {
            return Ok(None);
        };
        let Body::PaymentProof(paid) = proof.body() else {
            return Ok(None);
        };
        if activity.counterparty != ledger.counterparty
            || proof.sender() != self.identity()
            || paid.allowance_id.as_ref() != Some(&ledger.allowance_id)
        {
            return Ok(None);
        }
        let (request, period) = match activity.binding()? {
            RecordBinding::Request { request_hash } => (request_hash, None),
            RecordBinding::Period {
                request_hash,
                billing_period,
            } => (request_hash, Some(billing_period)),
            RecordBinding::Unrequested => return Ok(None),
        };
        if paid.billing_period != period || proof.message().refs.first() != Some(&request) {
            return Ok(None);
        }
        Ok(Some((
            paid.attempt_id,
            self.obligation(
                &activity.counterparty,
                &request,
                period.map_or(0, |p| p.start_secs()),
            ),
        )))
    }

    async fn dated_allowance_payments(
        &mut self,
        ledger: &Ledger,
        attempts: &HashMap<AttemptId, ([u8; 32], &Decimal)>,
        handed_off: &HashMap<AttemptId, &crate::rail::AllowanceHandoff>,
        before: Option<&AppliedRecord>,
    ) -> Result<Vec<AttemptId>> {
        let mut paid_attempts = Vec::new();
        for activity in self.payer_records().await? {
            let Some((id, obligation)) = self.allowance_proof(&activity, ledger)? else {
                continue;
            };
            if attempts.get(&id).is_none_or(|(key, _)| key != &obligation) {
                continue;
            }
            let target = ActivityTarget::payer(
                activity.natural_key_with(&self.identity(), &|endpoint, proof| {
                    self.rail.settlement_id(endpoint, proof)
                })?,
            );
            let record = target.record_target();
            let RecordRead::Fresh(fresh) = self.app.read_record_fresh(&record).await? else {
                continue;
            };
            let Some(current) = self.read_activity(&target, Some(&fresh)) else {
                continue;
            };
            if self.allowance_proof(&current, ledger)? != Some((id, obligation)) {
                continue;
            }
            if let Some(proof) = &current.proof {
                let held = self.app.shared_evidence().await?;
                let dated = held.ops.iter().any(|op| {
                    AppliedRecord::retained(&self.app, &record, op).is_some_and(|write| {
                        write.dates_payment_proof(&self.app, proof)
                            // The sequence continues across an orderly generation change.
                            && before.is_none_or(|limit| {
                                write.revision.generation <= limit.revision.generation
                                    && write.revision.seq < limit.revision.seq
                                    && write.at_ms <= limit.at_ms
                            })
                    })
                });
                let current_write = held
                    .ops
                    .iter()
                    .any(|op| AppliedRecord::current(&self.app, &record, &fresh, op).is_some());
                let matches_method = match handed_off.get(&id).map(|d| &d.outcome) {
                    Some(crate::rail::RailOutcome::Paid(method)) => matches!(proof.body(),
                        Body::PaymentProof(paid) if paid.proof == method.proof),
                    _ => true,
                };
                if current_write && dated && matches_method {
                    paid_attempts.push(id);
                }
            }
        }
        Ok(paid_attempts)
    }

    /// Reconstruct one copied successor from an already accepted predecessor. A
    /// restored wallet may lack the live wallet's preparation. Only independently
    /// proved outcomes already present before the signed successor can explain it.
    pub(super) async fn copied_successor(
        &mut self,
        accepted: &super::checkpoint::AcceptedLedger,
        signers: &super::checkpoint::ReservationSigners,
        current: &AppliedRecord,
        ledger: &Ledger,
        grant: &Condition,
    ) -> Result<bool> {
        let before = &accepted.ledger;
        let Body::AllowanceProposal(proposed) = accepted.record.proposal.body() else {
            return Ok(false);
        };
        if before.entries.len() != ledger.entries.len() {
            return Ok(false);
        }
        let retained: HashSet<_> = ledger.failed.iter().map(|e| e.attempt_id).collect();
        let changes = before
            .entries
            .iter()
            .zip(&ledger.entries)
            .filter_map(|(old, new)| {
                (old.outcome != new.outcome).then_some((
                    old.attempt_id,
                    old.obligation,
                    old.at,
                    &old.value,
                    new.outcome?,
                ))
            })
            .chain(
                before
                    .failed
                    .iter()
                    .filter(|e| !retained.contains(&e.attempt_id))
                    .map(|e| (e.attempt_id, e.obligation, 0, &e.value, EntryOutcome::Done)),
            );
        let mut facts = HashMap::new();
        let mut payments = HashMap::new();
        let mut claims = Vec::new();
        for (id, obligation, at, value, outcome) in changes {
            if outcome == EntryOutcome::Done {
                payments.insert(id, (obligation, value));
            }
            if let Some(read) = self.app.read_claim(&obligation).await? {
                claims.push((id, obligation, at, outcome, read));
            }
        }
        let evidence = self.app.shared_evidence().await?;
        let results = super::evidence::ClaimResults::new(
            &self.app,
            &evidence.ops,
            signers,
            accepted,
            current,
        );
        for (id, obligation, at, outcome, read) in claims {
            let Some(opened) = evidence.ops.iter().find_map(|op| {
                let original = signers.original(id, op)?;
                let (_, seq) = accepted.application.opening(
                    &self.app,
                    &original,
                    &obligation,
                    id,
                    &read,
                    Some((grant, at)),
                )?;
                Some(seq)
            }) else {
                continue;
            };
            let state = match outcome {
                EntryOutcome::Done => ClaimState::Done,
                EntryOutcome::Failed => ClaimState::Failed { by_owner: true },
            };
            if results.confirms(
                id,
                ReservationClaim { obligation, state },
                opened,
                Some(current),
            ) {
                facts.insert(id, AttemptEvidence::from(state));
                payments.remove(&id);
            }
        }
        if !payments.is_empty() {
            for id in Box::pin(self.dated_allowance_payments(
                before,
                &payments,
                &HashMap::new(),
                Some(current),
            ))
            .await?
            {
                facts.entry(id).or_default().paid = true;
            }
        }
        let Some(expected) = record_outcomes(&proposed.terms, before, &facts)? else {
            return Ok(false);
        };
        Ok(expected.to_json()? == current.value)
    }

    pub(super) async fn allowance_outcomes(
        &mut self,
        ledger: &Ledger,
        asset: &Asset,
        grant: &Condition,
        claims: &HashMap<AttemptId, ReservationClaim>,
        handed_off: &HashMap<AttemptId, &crate::rail::AllowanceHandoff>,
    ) -> Result<LedgerFacts> {
        let mut observed = LedgerFacts::default();
        let mut facts: HashMap<_, _> = claims
            .iter()
            .map(|(&id, claim)| (id, AttemptEvidence::from(claim.state)))
            .collect();
        let attempts: HashMap<_, _> = ledger
            .entries
            .iter()
            .filter(|e| {
                e.outcome != Some(EntryOutcome::Done)
                    || handed_off.contains_key(&e.attempt_id) && !claims.contains_key(&e.attempt_id)
            })
            .map(|e| (e.attempt_id, (e.obligation, &e.value)))
            .chain(
                ledger
                    .failed
                    .iter()
                    .map(|e| (e.attempt_id, (e.obligation, &e.value))),
            )
            .collect();
        if attempts.is_empty() {
            observed.signed = facts;
            return Ok(observed);
        }
        for id in
            Box::pin(self.dated_allowance_payments(ledger, &attempts, handed_off, None)).await?
        {
            facts.entry(id).or_default().paid = true;
        }
        let Condition::RecordCurrent { revision, .. } = grant else {
            return Err(crate::PaykitError::corrupt());
        };
        for (id, (obligation, value)) in attempts {
            if handed_off.contains_key(&id)
                || facts
                    .get(&id)
                    .is_some_and(|fact| fact.paid || fact.claim == Some(ClaimState::Done))
            {
                continue;
            }
            let Some(entry) = self.journal_entry_on(&id, &obligation).await? else {
                continue;
            };
            if !entry.never_dispatched
                || entry.restored
                || entry.payee != ledger.counterparty
                || !entry.allowance.as_ref().is_some_and(|a| {
                    a.id == ledger.allowance_id.as_str() && a.revision() == *revision
                })
                || entry
                    .paid_amount()?
                    .is_none_or(|amount| &amount.asset != asset || &amount.value != value)
            {
                continue;
            }
            let target = ActivityTarget::payer(entry.natural_key.clone());
            let record = target.record_target();
            let RecordRead::Fresh(fresh) = self.app.read_record_fresh(&record).await? else {
                continue;
            };
            if self.pause_of(&fresh).await?.blocks_automatic() {
                continue;
            }
            let Some(activity) = self.read_activity(&target, Some(&fresh)) else {
                continue;
            };
            if self.allowance_proof(&activity, ledger)? == Some((id, obligation)) {
                continue;
            }
            if let Some(write) = self
                .app
                .shared_evidence()
                .await?
                .ops
                .iter()
                .find_map(|op| AppliedRecord::current(&self.app, &record, &fresh, op))
            {
                observed.release_records.insert(
                    id,
                    Condition::RecordCurrent {
                        key: fresh.key,
                        revision: write.revision,
                        deleted: false,
                    },
                );
                facts.entry(id).or_default().never_dispatched = true;
            }
        }
        observed.other_failures = ledger
            .entries
            .iter()
            .filter(|entry| {
                let id = &entry.attempt_id;
                entry.outcome.is_none()
                    && facts.get(id).is_some_and(|fact| {
                        !fact.paid && fact.claim == Some(ClaimState::Failed { by_owner: false })
                    })
                    && !handed_off
                        .get(id)
                        .is_some_and(|d| matches!(d.outcome, crate::rail::RailOutcome::Paid(_)))
            })
            .map(|entry| ReservedByOtherApp {
                attempt_id: entry.attempt_id,
                value: entry.value.clone(),
            })
            .collect();
        observed.signed = facts;
        Ok(observed)
    }
}
