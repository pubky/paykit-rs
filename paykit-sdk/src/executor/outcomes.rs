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
    /// `first` holds the claim reads the restore already made, by obligation; this reads
    /// each changed attempt's claim again, and [`successor_from_reads`] decides from both
    /// reads. A payment either read shows made is kept, whatever the answer.
    pub(super) async fn copied_successor(
        &mut self,
        accepted: &super::checkpoint::AcceptedLedger,
        signers: &super::checkpoint::ReservationSigners,
        current: &AppliedRecord,
        ledger: &Ledger,
        grant: &Condition,
        first: &HashMap<[u8; 32], Option<pubky_dispatch_client::app::ClaimRead>>,
    ) -> Result<bool> {
        let before = &accepted.ledger;
        let Body::AllowanceProposal(proposed) = accepted.record.proposal.body() else {
            return Ok(false);
        };
        let Some(copied) = copied_attempts(before, ledger) else {
            return Ok(false);
        };
        let mut second = HashMap::new();
        for (attempt, _) in &copied {
            if let Some(read) = self.app.read_claim(&attempt.obligation).await? {
                second.insert(attempt.obligation, read);
            }
        }
        let payments: HashMap<_, _> = copied
            .iter()
            .filter(|(attempt, _)| attempt.outcome == EntryOutcome::Done)
            .map(|(attempt, value)| (attempt.id, (attempt.obligation, *value)))
            .collect();
        let dated: HashSet<_> = if payments.is_empty() {
            HashSet::new()
        } else {
            Box::pin(self.dated_allowance_payments(
                before,
                &payments,
                &HashMap::new(),
                Some(current),
            ))
            .await?
            .0
            .into_iter()
            .collect()
        };
        let evidence = self.app.shared_evidence().await?;
        let results = super::evidence::ClaimResults::new(
            &self.app,
            &evidence.ops,
            signers,
            accepted,
            current,
        );
        let copied: Vec<_> = copied.into_iter().map(|(attempt, _)| attempt).collect();
        let decided = successor_from_reads(
            &SuccessorReads {
                terms: &proposed.terms,
                before,
                successor: &current.value,
                copied: &copied,
                first,
                second: &second,
                dated: &dated,
            },
            |attempt, read| {
                evidence.ops.iter().find_map(|op| {
                    let original = signers.original(attempt.id, op)?;
                    let (_, seq) = accepted.application.opening(
                        &self.app,
                        &original,
                        &attempt.obligation,
                        attempt.id,
                        read,
                        Some((grant, attempt.at)),
                    )?;
                    Some(seq)
                })
            },
            |id, claim, opened| results.confirms(id, claim, opened, Some(current)),
        )?;
        for (id, obligation) in decided.sightings {
            self.saw_paid(&id, obligation, crate::journal::PaidSource::ClaimShowsDone)
                .await?;
        }
        Ok(decided.explained)
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

/// One attempt a copied successor changed: the outcome it copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Copied {
    pub(super) id: AttemptId,
    pub(super) obligation: [u8; 32],
    /// When the attempt was reserved; 0 for a compacted failure.
    pub(super) at: u64,
    pub(super) outcome: EntryOutcome,
}

/// The attempts `ledger` changes from `before`, with their values: entries whose outcome
/// it sets, and compacted failures it moves into the paid totals. `None` where the two
/// don't list the same entries.
fn copied_attempts<'a>(before: &'a Ledger, ledger: &Ledger) -> Option<Vec<(Copied, &'a Decimal)>> {
    if before.entries.len() != ledger.entries.len() {
        return None;
    }
    let retained: HashSet<_> = ledger.failed.iter().map(|e| e.attempt_id).collect();
    let entries = before
        .entries
        .iter()
        .zip(&ledger.entries)
        .filter_map(|(old, new)| {
            let outcome = new.outcome?;
            (old.outcome != Some(outcome)).then_some((
                Copied {
                    id: old.attempt_id,
                    obligation: old.obligation,
                    at: old.at,
                    outcome,
                },
                &old.value,
            ))
        });
    let promoted = before
        .failed
        .iter()
        .filter(|e| !retained.contains(&e.attempt_id))
        .map(|e| {
            (
                Copied {
                    id: e.attempt_id,
                    obligation: e.obligation,
                    at: 0,
                    outcome: EntryOutcome::Done,
                },
                &e.value,
            )
        });
    Some(entries.chain(promoted).collect())
}

/// What a copied successor is decided on.
pub(super) struct SuccessorReads<'a> {
    pub(super) terms: &'a paykit_lib::allowances::AllowanceTerms,
    /// The accepted ledger the successor follows.
    pub(super) before: &'a Ledger,
    /// The successor's value as written.
    pub(super) successor: &'a [u8],
    pub(super) copied: &'a [Copied],
    /// The claim reads made before the successor was checked, by obligation.
    pub(super) first: &'a HashMap<[u8; 32], Option<pubky_dispatch_client::app::ClaimRead>>,
    /// The claim reads made to check it, by obligation; a claim that couldn't be read is
    /// absent.
    pub(super) second: &'a HashMap<[u8; 32], pubky_dispatch_client::app::ClaimRead>,
    /// The copied payments a dated payer proof shows paid.
    pub(super) dated: &'a HashSet<AttemptId>,
}

/// What a copied successor's reads decide.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SuccessorDecision {
    /// The successor is the copy the proved outcomes give.
    pub(super) explained: bool,
    /// The copied attempts a read shows `done`, by attempt and obligation: payments seen
    /// made, kept whatever the answer.
    pub(super) sightings: Vec<(AttemptId, [u8; 32])>,
}

/// Decide a copied successor from both reads of each claim it changes (Paykit spec 8.3).
/// The second read verifies the copy: `opened` finds an attempt's opening in it, and
/// `confirms` checks a claim state against the signed outcomes from that opening. Either
/// read showing `done` is a sighting, so a later read that shows the payment made is never
/// discarded for an earlier one that showed it failed, nor the other way round (`failed`
/// releases).
pub(super) fn successor_from_reads(
    reads: &SuccessorReads<'_>,
    opened: impl Fn(&Copied, &pubky_dispatch_client::app::ClaimRead) -> Option<u64>,
    confirms: impl Fn(AttemptId, ReservationClaim, u64) -> bool,
) -> Result<SuccessorDecision> {
    use pubky_dispatch_core::claims::Outcome;
    let mut facts = HashMap::new();
    let mut payments: HashSet<_> = reads
        .copied
        .iter()
        .filter(|attempt| attempt.outcome == EntryOutcome::Done)
        .map(|attempt| attempt.id)
        .collect();
    for attempt in reads.copied {
        let Some(read) = reads.second.get(&attempt.obligation) else {
            continue;
        };
        let Some(opened) = opened(attempt, read) else {
            continue;
        };
        // A failure copied from the claim may be any own app's (Paykit spec 8.3, `failed`
        // releases): either writer's verified claim confirms it.
        let states: &[ClaimState] = match attempt.outcome {
            EntryOutcome::Done => &[ClaimState::Done],
            EntryOutcome::Failed => &[
                ClaimState::Failed { by_owner: true },
                ClaimState::Failed { by_owner: false },
            ],
        };
        let obligation = attempt.obligation;
        if let Some(&state) = states
            .iter()
            .find(|&&state| confirms(attempt.id, ReservationClaim { obligation, state }, opened))
        {
            facts.insert(attempt.id, AttemptEvidence::from(state));
            payments.remove(&attempt.id);
        }
    }
    // A signed `done` held without the opening it follows explains a copied `done` too,
    // as the count takes it (Paykit spec 8.3, `failed` releases).
    for attempt in reads.copied {
        if !payments.contains(&attempt.id) {
            continue;
        }
        let done = ReservationClaim {
            obligation: attempt.obligation,
            state: ClaimState::Done,
        };
        if confirms(attempt.id, done, 0) {
            facts.insert(attempt.id, AttemptEvidence::from(ClaimState::Done));
            payments.remove(&attempt.id);
        }
    }
    for id in payments {
        if reads.dated.contains(&id) {
            facts.entry(id).or_default().paid = true;
        }
    }
    let explained = match record_outcomes(reads.terms, reads.before, &facts)? {
        Some(expected) => expected.to_json()? == reads.successor,
        None => false,
    };
    let mut sightings = Vec::new();
    for attempt in reads.copied {
        let first = reads
            .first
            .get(&attempt.obligation)
            .and_then(Option::as_ref);
        let second = reads.second.get(&attempt.obligation);
        let shows_done = first.into_iter().chain(second).any(|read| {
            match read
                .view
                .attempt(&attempt.id.to_bytes())
                .map(|shown| shown.outcome)
            {
                Some(Outcome::Done) => true,
                Some(Outcome::Open | Outcome::Unknown | Outcome::Failed) | None => false,
            }
        });
        if shows_done && !sightings.contains(&(attempt.id, attempt.obligation)) {
            sightings.push((attempt.id, attempt.obligation));
        }
    }
    Ok(SuccessorDecision {
        explained,
        sightings,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use paykit_lib::allowances::{AllowanceTerms, ClaimState, EntryOutcome, Ledger, LedgerEntry};
    use paykit_lib::{AllowanceId, AttemptId, Decimal};
    use pubky_dispatch_client::app::ClaimRead;
    use pubky_dispatch_core::claims::{AttemptView, ClaimStatus, ClaimView, Outcome};

    use super::{successor_from_reads, Copied, SuccessorDecision, SuccessorReads};

    const ATTEMPT: [u8; 16] = [4; 16];
    const OBLIGATION: [u8; 32] = [6; 32];

    fn read(outcome: Outcome) -> ClaimRead {
        ClaimRead {
            entries: Vec::new(),
            view: ClaimView {
                claim_key: [2; 32],
                status: ClaimStatus::Settled,
                owner: None,
                attempt_id: None,
                step: 2,
                attempts: vec![AttemptView {
                    attempt_id: ATTEMPT,
                    owner: [7; 16],
                    outcome,
                    opened_at: 1002,
                    orphan_after: 60,
                    orphaned: false,
                    possibly_duplicate: false,
                    duplicate: false,
                    resolved_by: Some([9; 16]),
                }],
                tombstone: true,
                deadline: None,
                divergent: false,
                rebuilt: false,
                next_possibly_duplicate: false,
            },
        }
    }

    /// The decision on a successor that copies the attempt's `failed`, where the signed
    /// failure verifies, given the restore's first read and the check's second read; with
    /// what the count of the copied ledger then gives.
    fn decide(
        first: Option<Outcome>,
        second: Option<Outcome>,
        signed_failure: bool,
    ) -> (SuccessorDecision, String) {
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset":"btc","lifetime_amount_limit":"100","period_limits":[],
            "per_payment_amount":null,"active_from":null,"expires_at":null,
            "allowed_payment_endpoint_identifiers":null
        }))
        .unwrap();
        let id = AttemptId::from_bytes(ATTEMPT);
        let mut before = Ledger::empty(
            [7; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [8; 32],
        );
        before.entries.push(LedgerEntry {
            obligation: OBLIGATION,
            attempt_id: id,
            value: Decimal::new("70").unwrap(),
            asset: terms.asset.clone(),
            at: 1_000,
            outcome: None,
        });
        let mut successor = before.clone();
        successor.entries[0].outcome = Some(EntryOutcome::Failed);
        let copied = [Copied {
            id,
            obligation: OBLIGATION,
            at: 1_000,
            outcome: EntryOutcome::Failed,
        }];
        let first = HashMap::from([(OBLIGATION, first.map(read))]);
        let second: HashMap<_, _> = second.map(|s| (OBLIGATION, read(s))).into_iter().collect();
        let decided = successor_from_reads(
            &SuccessorReads {
                terms: &terms,
                before: &before,
                successor: &successor.to_json().unwrap(),
                copied: &copied,
                first: &first,
                second: &second,
                dated: &HashSet::new(),
            },
            |_, _| Some(1),
            // Only the signed failure, where held, verifies; nothing signed shows `done`.
            |_, claim, _| {
                signed_failure && matches!(claim.state, ClaimState::Failed { by_owner: false })
            },
        )
        .unwrap();
        // The count of the copied ledger, the claim gone, with the sightings kept.
        let sighted: HashSet<_> = decided.sightings.iter().map(|(id, _)| *id).collect();
        let counted = super::super::reconciled_usage(
            Vec::new(),
            &terms,
            &successor,
            &HashMap::new(),
            &HashMap::new(),
            &mut HashMap::new(),
            HashMap::new(),
            &HashSet::new(),
            &sighted,
            1_000_000,
        )
        .unwrap()
        .counted;
        (decided, counted.lifetime().as_str().to_owned())
    }

    fn kept() -> Vec<(AttemptId, [u8; 32])> {
        vec![(AttemptId::from_bytes(ATTEMPT), OBLIGATION)]
    }

    #[test]
    fn test_outcomes_successor_keeps_a_done_the_second_read_shows_after_a_failed_first() {
        let (decided, lifetime) = decide(Some(Outcome::Failed), Some(Outcome::Done), true);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: true,
                sightings: kept()
            }
        );
        assert_eq!(lifetime, "70", "a payment a read showed made stays counted");
    }

    #[test]
    fn test_outcomes_successor_keeps_a_done_the_first_read_shows_before_a_failed_second() {
        let (decided, lifetime) = decide(Some(Outcome::Done), Some(Outcome::Failed), true);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: true,
                sightings: kept()
            }
        );
        assert_eq!(lifetime, "70");
    }

    #[test]
    fn test_outcomes_successor_failed_in_both_reads_releases() {
        let (decided, lifetime) = decide(Some(Outcome::Failed), Some(Outcome::Failed), true);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: true,
                sightings: Vec::new()
            }
        );
        assert_eq!(
            lifetime, "0",
            "a recorded failure nothing shows paid releases"
        );
    }

    #[test]
    fn test_outcomes_successor_unexplained_when_the_second_read_cant_be_made() {
        let (decided, lifetime) = decide(Some(Outcome::Failed), None, true);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: false,
                sightings: Vec::new()
            }
        );
        assert_eq!(lifetime, "0");
        // The first read's `done` is still kept when the second can't be made.
        let (decided, lifetime) = decide(Some(Outcome::Done), None, true);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: false,
                sightings: kept()
            }
        );
        assert_eq!(lifetime, "70");
    }

    #[test]
    fn test_outcomes_successor_unexplained_when_the_failure_doesnt_verify() {
        let (decided, lifetime) = decide(Some(Outcome::Failed), Some(Outcome::Failed), false);
        assert_eq!(
            decided,
            SuccessorDecision {
                explained: false,
                sightings: Vec::new()
            }
        );
        assert_eq!(lifetime, "0");
    }
}
