//! The signed ledger writes that moved completed Allowance payments into totals.

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{
    check_successor, record_outcomes, AttemptEvidence, ClaimState, CompactedFailed, EntryOutcome,
    Ledger, LedgerEntry,
};
use paykit_lib::records::bind_ledger;
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{App, KeptOp, RecordTarget};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::ops::{Condition, OpKind, SignedOp};
use serde::{Deserialize, Serialize};

use super::checkpoint::AcceptedLedger;
use super::evidence::{AppliedRecord, Purpose};
use super::restore::{ledger_change, LedgerChange};
use crate::state::key;
use crate::Result;

/// The compacting application, its signed predecessor and the claim opened in that batch.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerCompaction {
    pub(super) before: KeptOp,
    pub(super) after: KeptOp,
    #[serde(deserialize_with = "Option::deserialize")]
    pub(super) opening: Option<KeptOp>,
}

impl LedgerCompaction {
    pub(crate) fn bound(
        &self,
        ledger: &[u8; 32],
        application: &[u8; 32],
        identity: &[u8; 32],
        executor: [u8; 16],
    ) -> bool {
        let verify = |kept: &KeptOp| {
            let cert = paykit_lib::scopes::verify_certificate(&kept.cert, identity).ok()?;
            let op = SignedOp::verify_with_cert(&kept.op, &cert, identity).ok()?;
            let proof = TransitionProof::peek(kept.proof.as_ref()?).ok()?;
            proof.check_binds(&op).ok()?;
            (cert.app_id() == executor && cert.holds_scope("paykit_allowance_execute"))
                .then_some((op, proof))
        };
        let Some((before, from)) = verify(&self.before) else {
            return false;
        };
        let Some((after, _)) = verify(&self.after) else {
            return false;
        };

        let Ok(revision) = from.record_revision() else {
            return false;
        };
        let prior = Condition::RecordCurrent {
            key: *ledger,
            revision,
            deleted: false,
        };
        self.after.op.doc_hash().ok().as_ref() == Some(application)
            && matches!(&before.body().kind, OpKind::RecordPut(w) if &w.key == ledger)
            && matches!(&after.body().kind, OpKind::RecordPut(w) if &w.key == ledger)
            && after.body().conditions.contains(&prior)
            && self.opening.as_ref().is_none_or(|kept| verify(kept).is_some_and(|(opening, _)| {
                matches!(&opening.body().kind, OpKind::Claim(c) if c.kind == pubky_switch_core::claims::ClaimType::Attempt)
                    && opening.body().batch == after.body().batch
            }))
    }

    pub(super) fn key(&self, ledger: &[u8; 32]) -> Result<String> {
        let hash = self.after.op.doc_hash()?;
        Ok(format!(
            "{}{}",
            key::ledger_compactions(ledger),
            pubky_switch_core::enc::b64(&hash)
        ))
    }
}

pub(super) struct CompactionRead<'a> {
    pub(super) target: &'a RecordTarget,
    pub(super) key: [u8; 32],
    pub(super) accepted: &'a AcceptedLedger,
    pub(super) grant: Condition,
    pub(super) current: &'a AppliedRecord,
    pub(super) ledger: &'a Ledger,
}

pub(super) enum Retired {
    Paid(LedgerEntry),
    PreviouslyFailed(CompactedFailed),
}

impl Retired {
    fn member(&self) -> (AttemptId, &paykit_lib::Decimal) {
        match self {
            Self::Paid(e) => (e.attempt_id, &e.value),
            Self::PreviouslyFailed(e) => (e.attempt_id, &e.value),
        }
    }
}

impl CompactionRead<'_> {
    /// Verify the full successor in the same grant, then identify the paid entries it
    /// removed. The aggregate amount alone cannot identify a payment.
    fn retired(
        &self,
        app: &App,
        receipt: &LedgerCompaction,
        purpose: Purpose,
    ) -> Option<Vec<Retired>> {
        // The predecessor came from the accepted checkpoint even for a newly observed successor.
        let before = AppliedRecord::established(app, self.target, &receipt.before)?;
        let after = AppliedRecord::retained_for(app, self.target, &receipt.after, purpose)?;
        let terms = match self.accepted.record.proposal.body() {
            Body::AllowanceProposal(p) => &p.terms,
            _ => return None,
        };
        let bind = |write: &AppliedRecord| {
            bind_ledger(
                &write.value,
                app.naming_key(),
                &self.key,
                &self.accepted.record,
            )
            .ok()
        };
        let held = bind(&before)?;
        let newer = bind(&after)?;
        // An orderly generation change continues the sequence (Switch spec 12.5).
        if before.revision.generation > after.revision.generation
            || after.revision.generation > self.current.revision.generation
            || before.revision.seq <= self.accepted.grant.revision.seq
            || after.revision.seq <= before.revision.seq
            || after.revision.seq > self.current.revision.seq
            || (after.revision.seq == self.current.revision.seq
                && after.revision != self.current.revision)
            || before.at_ms > after.at_ms
            || after.at_ms > self.current.at_ms
            || !after.follows(&before)
            || [&before, &after].iter().any(|w| {
                w.cert.app_id() != app.app_id()
                    || !w.cert.holds_scope("paykit_allowance_execute")
                    || !w.op.body().conditions.contains(&self.grant)
            })
            || check_successor(terms, &newer, after.at_ms, self.ledger, self.current.at_ms).is_err()
            || newer.count <= held.count
        {
            return None;
        }
        if let Some(opening) = &receipt.opening {
            let Some(LedgerChange::Reservation(entry)) =
                ledger_change(&held, &newer, terms).ok()?
            else {
                return None;
            };
            if !after.opens_in_window(terms, entry).ok()?
                || !after.signed_opening(app, opening, entry, &self.grant, purpose)
            {
                return None;
            }
            return Some(
                retired_paid(held, &newer)
                    .into_iter()
                    .map(Retired::Paid)
                    .collect(),
            );
        }
        copied_failures(terms, &held, &newer)
    }
}

/// A copied outcome increases the totals only by the compacted failures it settles.
fn copied_failures(
    terms: &paykit_lib::allowances::AllowanceTerms,
    held: &Ledger,
    newer: &Ledger,
) -> Option<Vec<Retired>> {
    let kept: HashSet<_> = newer.failed.iter().map(|e| e.attempt_id).collect();
    let mut facts = HashMap::new();
    let mut retired = Vec::new();
    for entry in &held.failed {
        if !kept.contains(&entry.attempt_id) {
            facts.insert(entry.attempt_id, AttemptEvidence::from(ClaimState::Done));
            retired.push(Retired::PreviouslyFailed(entry.clone()));
        }
    }
    for entry in &newer.entries {
        if let Some(outcome) = entry.outcome {
            facts.insert(
                entry.attempt_id,
                AttemptEvidence::from(match outcome {
                    EntryOutcome::Done => ClaimState::Done,
                    EntryOutcome::Failed => ClaimState::Failed { by_owner: true },
                }),
            );
        }
    }
    let expected = record_outcomes(terms, held, &facts).ok()??;
    (expected.to_json().ok()? == newer.to_json().ok()?).then_some(retired)
}

/// The exact Done entries retired by a verified compacting write.
pub(super) fn retired_paid(held: Ledger, newer: &Ledger) -> Vec<LedgerEntry> {
    let retained: HashSet<_> = newer.entries.iter().map(|e| e.attempt_id).collect();
    held.entries
        .into_iter()
        .filter(|e| e.outcome == Some(EntryOutcome::Done) && !retained.contains(&e.attempt_id))
        .collect()
}

impl CompactionRead<'_> {
    pub(super) fn payments(
        &self,
        app: &App,
        receipts: &[(LedgerCompaction, Purpose)],
    ) -> Option<HashMap<AttemptId, Retired>> {
        let mut retired = HashMap::new();
        for (receipt, purpose) in receipts {
            let entries = self.retired(app, receipt, *purpose)?;
            for entry in entries {
                if retired.insert(entry.member().0, entry).is_some() {
                    return None;
                }
            }
        }
        let total = retired
            .values()
            .try_fold(0u128, |total, e| total.checked_add(e.member().1.scaled()));
        let spent = self.ledger.spent.as_ref().map_or(0, |spent| spent.scaled());
        if u64::try_from(retired.len()).ok() != Some(self.ledger.count) || total != Some(spent) {
            return None;
        }
        Some(retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{reconciled_usage, ReservationClaim};
    use crate::rail::{AllowanceDispatch, RailOutcome};
    use paykit_lib::allowances::{compact, AllowanceTerms};
    use paykit_lib::{AllowanceId, Decimal, PaymentAmount};

    #[test]
    fn test_paid_claim_or_dated_proof_moves_a_compacted_failure_once_with_lagging_history() {
        let terms = AllowanceTerms::from_json(&serde_json::json!({
            "asset": "btc", "lifetime_amount_limit": "100", "period_limits": [],
            "per_payment_amount": null, "active_from": null, "expires_at": null,
            "allowed_payment_endpoint_identifiers": null
        }))
        .unwrap();
        let id = AttemptId::from_bytes([4; 16]);
        let mut ledger = Ledger::empty(
            [1; 32],
            AllowanceId::new("4b3fb324-091b-47da-a7ad-46b3a07d0409").unwrap(),
            [3; 32],
        );
        let transfer = PaymentAmount {
            asset: terms.asset.clone(),
            value: Decimal::new("7").unwrap(),
        };
        let obligation = [5; 32];
        ledger.entries.push(LedgerEntry {
            obligation,
            attempt_id: id,
            at: 1_000,
            asset: transfer.asset.clone(),
            value: transfer.value.clone(),
            outcome: Some(EntryOutcome::Failed),
        });
        let now = 70_000;
        let held = compact(&terms, &ledger, now).unwrap().unwrap();
        assert!(held.entries.is_empty());
        assert_eq!(held.failed[0].attempt_id, id);
        let dispatch = AllowanceDispatch {
            attempt_id: id,
            amount: transfer.clone(),
            outcome: RailOutcome::Failed,
        };
        let read = |ledger: &Ledger,
                    claims: &HashMap<_, _>,
                    facts: &HashMap<_, _>,
                    retired: &mut HashMap<_, _>,
                    row| {
            reconciled_usage(
                vec![row],
                &terms,
                ledger,
                claims,
                &HashMap::new(),
                retired,
                facts.clone(),
                &HashSet::new(),
                &HashSet::new(),
                now,
            )
        };
        for by_claim in [true, false] {
            let mut claims = HashMap::new();
            let evidence = if by_claim {
                claims.insert(
                    id,
                    ReservationClaim {
                        obligation,
                        state: ClaimState::Done,
                    },
                );
                AttemptEvidence::from(ClaimState::Done)
            } else {
                AttemptEvidence {
                    paid: true,
                    ..Default::default()
                }
            };
            let facts = HashMap::from([(id, evidence)]);
            let mut empty = HashMap::new();
            let before = read(&held, &claims, &facts, &mut empty, dispatch.clone()).unwrap();
            assert_eq!(before.counted.lifetime().as_str(), "7");
            assert!(before.unclassified.is_empty());
            let paid = record_outcomes(&terms, &held, &facts).unwrap().unwrap();
            assert_eq!(paid.count, 1);
            assert_eq!(paid.spent.as_ref().unwrap(), &transfer.value);
            assert!(paid.failed.is_empty());
            let mut retired: HashMap<_, _> = copied_failures(&terms, &held, &paid)
                .unwrap()
                .into_iter()
                .map(|e| (e.member().0, e))
                .collect();
            let mut different = dispatch.clone();
            different.amount.value = Decimal::new("6").unwrap();
            assert!(read(&paid, &claims, &facts, &mut retired, different.clone()).is_none());
            assert!(read(&held, &claims, &facts, &mut empty, different).is_none());
            let other = HashMap::from([(
                id,
                ReservationClaim {
                    obligation: [6; 32],
                    state: ClaimState::Done,
                },
            )]);
            assert!(read(&held, &other, &facts, &mut empty, dispatch.clone()).is_none());
            let repeated = read(
                &paid,
                &HashMap::new(),
                &HashMap::new(),
                &mut retired,
                dispatch.clone(),
            )
            .unwrap();
            assert_eq!(repeated.counted.lifetime().as_str(), "7");
            assert!(repeated.unclassified.is_empty() && retired.is_empty());
            assert!(record_outcomes(&terms, &paid, &facts).unwrap().is_none());
            // The claim may be the only signed evidence from the wallet even
            // when the method later agrees. The promoted total stays unchanged.
            let mut retired: HashMap<_, _> = copied_failures(&terms, &held, &paid)
                .unwrap()
                .into_iter()
                .map(|e| (e.member().0, e))
                .collect();
            let mut settled = dispatch.clone();
            settled.outcome = RailOutcome::Paid(crate::rail::RailPaid::new(Default::default()));
            let agreed = read(
                &paid,
                &HashMap::new(),
                &HashMap::new(),
                &mut retired,
                settled,
            )
            .unwrap();
            assert_eq!(agreed.counted.lifetime().as_str(), "7");
            assert!(agreed.unclassified.is_empty() && retired.is_empty());
            let before_compaction = record_outcomes(&terms, &ledger, &facts).unwrap().unwrap();
            let compacted = compact(&terms, &before_compaction, now).unwrap().unwrap();
            let mut oldest: HashMap<_, _> = retired_paid(before_compaction, &compacted)
                .into_iter()
                .map(|e| (e.attempt_id, Retired::Paid(e)))
                .collect();
            let read_back = read(
                &compacted,
                &HashMap::new(),
                &HashMap::new(),
                &mut oldest,
                dispatch.clone(),
            )
            .unwrap();
            assert_eq!(read_back.counted.lifetime().as_str(), "7");
            assert!(read_back.unclassified.is_empty() && oldest.is_empty());
        }
    }
}
