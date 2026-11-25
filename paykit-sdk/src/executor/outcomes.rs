//! Verified outcomes shown to the executor of an Allowance (Paykit spec 8.3).

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{record_outcomes, AttemptEvidence, ClaimState, EntryOutcome, Ledger};
use paykit_lib::records::{ActivityRecord, RecordBinding};
use paykit_lib::{AttemptId, Body, Decimal};
use pubky_dispatch_client::app::{OpBatch, RecordRead};
use pubky_dispatch_core::ops::Condition;

use super::evidence::AppliedRecord;
use super::restore::LedgerRead;
use super::ReservationClaim;
use crate::activity::ActivityTarget;
use crate::{Paykit, Result};

impl Paykit {
    async fn submit_outcomes(
        &mut self,
        record: &[u8; 32],
        checked: super::restore::ReconciledLedger,
    ) -> Result<()> {
        let Condition::RecordCurrent { key: ledger, .. } = checked.current else {
            return Err(crate::PaykitError::corrupt());
        };
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        let prepared = checked.stage(self, batch, Vec::new(), true).await?;
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
                    record_outcomes(&checked.terms, &checked.ledger, &checked.facts)?
                {
                    checked.ledger = next;
                    Box::pin(self.submit_outcomes(&record, checked)).await?;
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

    /// The obligation of the activity's own request or occurrence with the ledger's
    /// counterparty, whether or not the record holds a proof yet.
    fn activity_obligation(
        &self,
        activity: &ActivityRecord,
        ledger: &Ledger,
    ) -> Result<Option<[u8; 32]>> {
        if activity.counterparty != ledger.counterparty {
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
        Ok(Some(self.obligation(
            &activity.counterparty,
            &request,
            period.map_or(0, |p: paykit_lib::periods::PeriodRef| p.start_secs()),
        )))
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

    /// The attempts among `attempts` a payer proof shows paid: first those whose proof the
    /// current payer record holds in a dated write, which may be copied into the ledger, then
    /// those only this app's copy of the record shows, which can't be dated now. Either is
    /// the payer's own signed proof of the attempt under the Allowance, so both count it
    /// spent (Paykit wire 5.5, Counting); an attempt is never released for a proof that
    /// couldn't be read again.
    async fn dated_allowance_payments(
        &mut self,
        ledger: &Ledger,
        attempts: &HashMap<AttemptId, ([u8; 32], &Decimal)>,
        handed_off: &HashMap<AttemptId, &crate::rail::AllowanceHandoff>,
        before: Option<&AppliedRecord>,
    ) -> Result<(Vec<AttemptId>, Vec<AttemptId>)> {
        let mut paid_attempts = Vec::new();
        let mut undated = Vec::new();
        let held_by = |found: Option<(AttemptId, [u8; 32])>| {
            found.filter(|(id, obligation)| {
                attempts.get(id).is_some_and(|(key, _)| key == obligation)
            })
        };
        for activity in self.payer_records().await? {
            // Every record of an obligation the ledger still counts is read again, also one
            // whose copy here holds no proof yet: the proof may have reached the record since
            // (Paykit spec 8.3, `failed` releases).
            let Some(obligation) = self.activity_obligation(&activity, ledger)? else {
                continue;
            };
            if !attempts.values().any(|(key, _)| key == &obligation) {
                continue;
            }
            if let Some((id, _)) = held_by(self.allowance_proof(&activity, ledger)?) {
                undated.push(id);
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
            let Some((id, _)) = held_by(self.allowance_proof(&current, ledger)?) else {
                continue;
            };
            undated.push(id);
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
                // A proof the method's history shows paid must be that payment's. A history
                // showing the attempt failed, pending or not at all doesn't refute the payer's
                // own signed proof, which counts the attempt spent (Paykit spec 8.3, `failed`
                // releases).
                let matches_method = match handed_off.get(&id).map(|d| &d.outcome) {
                    Some(crate::rail::RailOutcome::Paid(method)) => matches!(proof.body(),
                        Body::PaymentProof(paid) if paid.proof == method.proof),
                    Some(crate::rail::RailOutcome::Failed | crate::rail::RailOutcome::Pending)
                    | None => true,
                };
                if current_write && dated && matches_method {
                    paid_attempts.push(id);
                }
            }
        }
        let mut once = HashSet::new();
        undated.retain(|id| !paid_attempts.contains(id) && once.insert(*id));
        Ok((paid_attempts, undated))
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
                // A `done` this read shows is a payment seen made, kept as any other, so a
                // count that relies on an earlier read can't miss it (Paykit spec 8.3).
                if read.view.attempt(&id.to_bytes()).is_some_and(|shown| {
                    shown.outcome == pubky_dispatch_core::claims::Outcome::Done
                }) {
                    self.saw_paid(&id, obligation, crate::journal::PaidSource::ClaimShowsDone)
                        .await?;
                }
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
            // A failure copied from the claim may be any own app's (Paykit spec 8.3, `failed`
            // releases): either writer's verified claim confirms it.
            let states: &[ClaimState] = match outcome {
                EntryOutcome::Done => &[ClaimState::Done],
                EntryOutcome::Failed => &[
                    ClaimState::Failed { by_owner: true },
                    ClaimState::Failed { by_owner: false },
                ],
            };
            if let Some(&state) = states.iter().find(|&&state| {
                results.confirms(
                    id,
                    ReservationClaim { obligation, state },
                    opened,
                    Some(current),
                )
            }) {
                facts.insert(id, AttemptEvidence::from(state));
                payments.remove(&id);
            }
        }
        // A signed `done` held without the opening it follows explains a copied `done` too,
        // as the count takes it (Paykit spec 8.3, `failed` releases).
        let unopened: Vec<_> = payments
            .iter()
            .map(|(id, (obligation, _))| (*id, *obligation))
            .collect();
        for (id, obligation) in unopened {
            let done = ReservationClaim {
                obligation,
                state: ClaimState::Done,
            };
            if results.confirms(id, done, 0, Some(current)) {
                facts.insert(id, AttemptEvidence::from(ClaimState::Done));
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
            .0
            {
                facts.entry(id).or_default().paid = true;
            }
        }
        let Some(expected) = record_outcomes(&proposed.terms, before, &facts)? else {
            return Ok(false);
        };
        Ok(expected.to_json()? == current.value)
    }

    /// The verified evidence for each attempt the ledger counts (Paykit spec 8.3): its
    /// authenticated claim, and whether a dated payer proof bound to the Allowance shows it
    /// paid; with the attempts only this app's copy of a payer record shows paid, which
    /// count spent but aren't copied into the ledger. A failure counts only from a verified
    /// claim or a recorded outcome, never from this installation's own journal.
    pub(super) async fn allowance_outcomes(
        &mut self,
        ledger: &Ledger,
        claims: &HashMap<AttemptId, ReservationClaim>,
        handed_off: &HashMap<AttemptId, &crate::rail::AllowanceHandoff>,
    ) -> Result<(HashMap<AttemptId, AttemptEvidence>, Vec<AttemptId>)> {
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
            return Ok((facts, Vec::new()));
        }
        let (dated, undated) =
            Box::pin(self.dated_allowance_payments(ledger, &attempts, handed_off, None)).await?;
        for id in dated {
            facts.entry(id).or_default().paid = true;
        }
        Ok((facts, undated))
    }
}
