//! Reconciliation (Paykit spec 7.3; Switch spec 10.2, 12.4).
//!
//! After a restore, or with continuity unverified, the app is in reconcile mode and makes no
//! new attempt. [`Paykit::reconcile`] reads the shared records again, finishes the payments
//! its journal holds, and resolves every open attempt of its own on the payer's obligations
//! from the payment method's history: a dispatch the history shows paid is written `done`
//! (basis `owner`), one it shows failed `failed`. What nothing can tell stays doubtful, "may
//! be paid", for the user: [`Paykit::resolve_attempt`] writes the user's choice (`done` basis
//! `reconcile`, `unknown`, or `failed`). [`Paykit::end_reconcile`] ends the mode.
//!
//! Only the payee's yes counts: a receipt settles an attempt; nothing from the payee shows an
//! attempt wasn't paid (Paykit spec 7.3).

use paykit_lib::collections::Collection;
use paykit_lib::derive::payment_obligation;
use paykit_lib::records::{ActivityRecord, ActivitySide, RecordBinding};
use paykit_lib::AttemptId;
use pubky_switch_client::app::{ClaimOp, ObligationDeadline, OpBatch, UserConfirmed};
use pubky_switch_core::claims::{Basis, Outcome};

use crate::error::{Refusal, Result};
use crate::journal::{JournalEntry, Stage};
use crate::pay::{own_deadline, PayOutcome};
use crate::paykit::Paykit;
use crate::rail::RailOutcome;
use crate::state::{self, key};

/// How the user resolves a doubtful attempt (Paykit spec 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// `done`, basis `reconcile`: the user confirms it was paid; the claim is SETTLED.
    Done,
    /// `unknown`: the claim is FREE, and its next attempt is flagged `possibly_duplicate`.
    Unknown,
    /// `failed`: definitively not paid.
    Failed,
}

/// An obligation that may already be paid, shown to the user (Paykit spec 7.1, 7.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doubtful {
    /// The claim key.
    pub obligation: [u8; 32],
    /// The attempts in doubt: `unknown`, or open and orphaned or unresolved here.
    pub attempts: Vec<AttemptId>,
    /// The payee, when the app's records name the obligation.
    pub payee: Option<[u8; 32]>,
    /// The request, when there is one.
    pub request_hash: Option<[u8; 32]>,
}

/// What a reconciliation did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The outcomes of the payments it finished.
    pub resolved: Vec<PayOutcome>,
    /// What stays doubtful, for the user.
    pub doubtful: Vec<Doubtful>,
}

/// An obligation of the payer's records.
struct Obligation {
    claim_key: [u8; 32],
    payee: [u8; 32],
    request_hash: Option<[u8; 32]>,
}

impl Paykit {
    fn obligations(&self, records: &[ActivityRecord]) -> Vec<Obligation> {
        let mut out = Vec::new();
        for record in records {
            match record.binding() {
                Ok(RecordBinding::Request { request_hash }) => out.push(Obligation {
                    claim_key: self.obligation(&record.counterparty, &request_hash, 0),
                    payee: record.counterparty,
                    request_hash: Some(request_hash),
                }),
                Ok(RecordBinding::Unrequested) => {
                    if let Some(attempt) = record.attempt_id {
                        out.push(Obligation {
                            claim_key: payment_obligation(&attempt),
                            payee: record.counterparty,
                            request_hash: None,
                        });
                    }
                }
                _ => {}
            }
        }
        out
    }

    async fn payer_records(&self) -> Result<Vec<ActivityRecord>> {
        let own = self.identity();
        let mut out = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(value) = held.content.value() else {
                continue;
            };
            if let Some(record) = crate::records::bind_activity(
                value,
                &own,
                ActivitySide::Payer,
                None,
                &held.key,
                |t| self.app.record_key(t).ok(),
            ) {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Reconcile (Paykit spec 7.3, Switch spec 12.4): read the records again, finish the
    /// journal's payments, resolve the own open attempts the payment method's history can,
    /// and report what stays doubtful. The app stays in reconcile mode until
    /// [`Paykit::end_reconcile`].
    ///
    /// # Errors
    ///
    /// The client's errors; the rail's errors.
    pub async fn reconcile(&mut self) -> Result<ReconcileReport> {
        self.app.list_collections().await?;
        let mut report = ReconcileReport {
            resolved: self.resume_payments().await?,
            doubtful: Vec::new(),
        };
        let journaled: Vec<[u8; 16]> = self
            .journal()
            .await?
            .iter()
            .filter(|e| e.open())
            .filter_map(|e| e.attempt().ok().map(AttemptId::to_bytes))
            .collect();
        let records = self.payer_records().await?;
        let me = self.app_id();
        for obligation in self.obligations(&records) {
            let Some(read) = self.app.read_claim(&obligation.claim_key).await? else {
                continue;
            };
            let mut doubtful = Vec::new();
            for attempt in &read.view.attempts {
                let open = matches!(attempt.outcome, Outcome::Open | Outcome::Unknown);
                if !open || journaled.contains(&attempt.attempt_id) {
                    continue;
                }
                let id = AttemptId::from_bytes(attempt.attempt_id);
                if attempt.owner == me {
                    // An own attempt the journal doesn't hold: the method's history decides.
                    match self.rail.payment(id).await? {
                        Some(RailOutcome::Paid { .. }) => {
                            self.resolve_own(&obligation.claim_key, id, true).await?;
                            continue;
                        }
                        Some(RailOutcome::Failed) => {
                            self.resolve_own(&obligation.claim_key, id, false).await?;
                            continue;
                        }
                        _ => {}
                    }
                }
                doubtful.push(id);
            }
            if !doubtful.is_empty() {
                report.doubtful.push(Doubtful {
                    obligation: obligation.claim_key,
                    attempts: doubtful,
                    payee: Some(obligation.payee),
                    request_hash: obligation.request_hash,
                });
            }
        }
        for (claim_key, doubt) in self.app.doubtful_claims().await? {
            if report.doubtful.iter().any(|d| d.obligation == claim_key) {
                continue;
            }
            let attempts = doubt
                .unknown
                .iter()
                .chain(doubt.orphaned.iter())
                .map(|a| AttemptId::from_bytes(*a))
                .collect();
            report.doubtful.push(Doubtful {
                obligation: claim_key,
                attempts,
                payee: None,
                request_hash: None,
            });
        }
        Ok(report)
    }

    /// The owner reports its attempt: `done` (basis `owner`) or `failed` (Switch spec 10.2
    /// rows 2 and 5).
    async fn resolve_own(
        &mut self,
        claim_key: &[u8; 32],
        attempt: AttemptId,
        paid: bool,
    ) -> Result<()> {
        let Some(read) = self.app.read_claim(claim_key).await? else {
            return Ok(());
        };
        let op = if paid {
            ClaimOp::done(
                *claim_key,
                read.view.step,
                attempt.to_bytes(),
                Basis::Owner,
                None,
            )?
        } else {
            ClaimOp::failed(*claim_key, read.view.step, attempt.to_bytes())
        };
        self.app
            .write_claim(op, Vec::new(), ObligationDeadline::NotKnown)
            .await?;
        Ok(())
    }

    /// Write the user's resolution of a doubtful attempt (Paykit spec 7.3): `done` with basis
    /// `reconcile` or `unknown` for an orphaned attempt, `failed` for an `unknown` one or an
    /// orphaned one whose owner isn't current. An attempt of this app's own is reported as
    /// its owner reports it: `done` with basis `owner`, or `failed` (Switch spec 10.2 rows 2
    /// and 5). The user has seen "may already be paid". The attempt's journal entry, if this
    /// app holds one, is finished in the same batch.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a claim without the attempt; the switch's refusal, such as
    /// `409 conflict`, for a resolution its rows don't allow now.
    pub async fn resolve_attempt(
        &mut self,
        obligation: &[u8; 32],
        attempt: AttemptId,
        resolution: Resolution,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        let read = self
            .app
            .read_claim(obligation)
            .await?
            .ok_or(Refusal::NotFound)?;
        let Some(held) = read.view.attempt(&attempt.to_bytes()) else {
            return Err(Refusal::NotFound.into());
        };
        let basis = if held.owner == self.app_id() {
            Basis::Owner
        } else {
            Basis::Reconcile
        };
        let step = read.view.step;
        let op = match resolution {
            Resolution::Done => ClaimOp::done(*obligation, step, attempt.to_bytes(), basis, None)?,
            Resolution::Unknown => ClaimOp::unknown(*obligation, step, attempt.to_bytes()),
            Resolution::Failed => ClaimOp::failed(*obligation, step, attempt.to_bytes()),
        };
        // The journal entry of an own attempt is finished with the resolution, in its batch:
        // the user decided, so it neither blocks another payment as one in progress nor is
        // resumed, and never dispatched again (Paykit spec 7.2, "No second execution").
        let journal_key = key::journal(&attempt);
        let entry = self.local_get::<JournalEntry>(&journal_key).await?;
        let deadline = entry
            .as_ref()
            .map_or(ObligationDeadline::NotKnown, |e| own_deadline(e.deadline));
        let mut batch = OpBatch::new()
            .op(op.kind())
            .obligation_deadline(*obligation, deadline);
        if let Some(mut entry) = entry.filter(JournalEntry::open) {
            entry.stage = Stage::Finished;
            batch = batch.local(state::local_put(&journal_key, &entry)?);
        }
        self.app.submit_batch(batch).await?;
        Ok(())
    }

    /// Reconciliation finished (Switch spec 12.4 step 2.4): reconcile mode ends, and new
    /// attempts may be made again.
    ///
    /// # Errors
    ///
    /// The client's errors, such as the restore question still open.
    pub async fn end_reconcile(&mut self, confirmed: UserConfirmed) -> Result<()> {
        self.app.end_reconcile(confirmed).await?;
        Ok(())
    }
}
