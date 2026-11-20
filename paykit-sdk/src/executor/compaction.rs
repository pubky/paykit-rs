//! The signed ledger writes that moved completed Allowance payments into totals.

use std::collections::{HashMap, HashSet};

use paykit_lib::allowances::{check_successor, EntryOutcome, Ledger, LedgerEntry};
use paykit_lib::records::bind_ledger;
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{App, KeptOp, RecordTarget};
use pubky_switch_core::chain::TransitionProof;
use pubky_switch_core::ops::{Condition, OpKind, SignedOp};
use serde::{Deserialize, Serialize};

use super::checkpoint::AcceptedLedger;
use super::evidence::AppliedRecord;
use super::restore::{ledger_change, LedgerChange};
use crate::state::key;
use crate::Result;

/// The compacting application, its signed predecessor and the claim opened in that batch.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LedgerCompaction {
    pub(super) before: KeptOp,
    pub(super) after: KeptOp,
    pub(super) opening: KeptOp,
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
        let Some((opening, _)) = verify(&self.opening) else {
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
            && matches!(&opening.body().kind, OpKind::Claim(c) if c.kind == pubky_switch_core::claims::ClaimType::Attempt)
            && opening.body().batch == after.body().batch
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

impl CompactionRead<'_> {
    /// Verify the full successor in the same grant and generation, then identify the
    /// paid entries it removed. The aggregate amount alone cannot identify a payment.
    fn retired(&self, app: &App, receipt: &LedgerCompaction) -> Option<Vec<LedgerEntry>> {
        let before = AppliedRecord::retained(app, self.target, &receipt.before)?;
        let after = AppliedRecord::retained(app, self.target, &receipt.after)?;
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
        if before.revision.generation != self.current.revision.generation
            || after.revision.generation != self.current.revision.generation
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
        let Some(LedgerChange::Reservation(entry)) = ledger_change(&held, &newer, terms).ok()?
        else {
            return None;
        };
        if !after.opens_in_window(terms, entry).ok()?
            || !after.signed_opening(app, &receipt.opening, entry, &self.grant)
        {
            return None;
        }
        Some(retired_paid(held, &newer))
    }
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
        receipts: &[LedgerCompaction],
    ) -> Option<HashMap<AttemptId, LedgerEntry>> {
        let mut retired = HashMap::new();
        for receipt in receipts {
            let entries = self.retired(app, receipt)?;
            for entry in entries {
                if retired.insert(entry.attempt_id, entry).is_some() {
                    return None;
                }
            }
        }
        let total = retired
            .values()
            .try_fold(0u128, |total, e| total.checked_add(e.value.scaled()));
        let spent = self.ledger.spent.as_ref().map_or(0, |spent| spent.scaled());
        if u64::try_from(retired.len()).ok() != Some(self.ledger.count) || total != Some(spent) {
            return None;
        }
        Some(retired)
    }
}
