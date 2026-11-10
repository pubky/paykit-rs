//! Reconciliation (Paykit spec 7.3; Switch spec 10.2, 12.4).
//!
//! After a restore, or with continuity unverified, the app is in reconcile mode and makes no
//! new attempt. [`Paykit::reconcile`] reads the shared records again, finishes the payments
//! its journal holds, and resolves every open attempt of its own on the payer's obligations
//! from the payment method's history: a dispatch the history shows paid is written `done`
//! (basis `owner`) with the payer's activity record and the proof, then the proof is sent,
//! as any payment's outcome (Paykit spec 7.2 steps 3 and 4); one it shows failed `failed`.
//! An attempt the journal doesn't hold, made after the backup a restore came from, has its
//! entry rebuilt from the records; one whose destination they can't tell, and so whose proof
//! can't be written, stays doubtful. What nothing can tell stays doubtful, "may be paid", for
//! the user: [`Paykit::resolve_attempt`] writes the user's choice (`done` basis
//! `reconcile`, `unknown`, or `failed`). [`Paykit::end_reconcile`] ends the mode.
//!
//! Only the payee's yes counts: a receipt settles an attempt; nothing from the payee shows an
//! attempt wasn't paid (Paykit spec 7.3).

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::{natural_key, payment_obligation};
use paykit_lib::records::{ActivityRecord, ActivitySide, DestinationSource, RecordBinding};
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{ClaimOp, ObligationDeadline, OpBatch, UserConfirmed};
use pubky_switch_core::claims::{Basis, Outcome};

use crate::destination::Carrier;
use crate::error::{Refusal, Result};
use crate::journal::{Destination, JournalEntry, KeptAmount, Stage};
use crate::pay::{own_deadline, PayOutcome};
use crate::paykit::Paykit;
use crate::rail::RailOutcome;
use crate::requests::request_of;
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
    record: ActivityRecord,
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
                    record: record.clone(),
                }),
                Ok(RecordBinding::Unrequested) => {
                    if let Some(attempt) = record.attempt_id {
                        out.push(Obligation {
                            claim_key: payment_obligation(&attempt),
                            payee: record.counterparty,
                            request_hash: None,
                            record: record.clone(),
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
                    // An own attempt the journal doesn't hold, such as one made after the
                    // backup a restore came from: the method's history decides, and the
                    // outcome goes the normal way, `done` with the activity record and the
                    // proof in one batch, then the proof to the payee (Paykit spec 7.2 steps 3
                    // and 4). A payment whose destination the records can't tell stays
                    // doubtful: its proof can't be written.
                    match self.rail.payment(id).await? {
                        Some(RailOutcome::Paid { proof, paid_at_ms }) => {
                            if let Some(mut entry) = self.rebuilt_entry(&obligation, id).await? {
                                entry.rail_proof = Some(proof);
                                entry.paid_at_ms = paid_at_ms;
                                entry.stage = Stage::RailPaid;
                                report.resolved.push(self.report(&mut entry).await?);
                                continue;
                            }
                        }
                        Some(RailOutcome::Failed) => {
                            match self.rebuilt_entry(&obligation, id).await? {
                                Some(mut entry) => {
                                    // Reported failed by the method, so asked again while its
                                    // history keeps the dispatch (Paykit spec 7.2).
                                    entry.failed_by_method = true;
                                    entry.stage = Stage::RailFailed;
                                    report.resolved.push(self.report(&mut entry).await?);
                                }
                                None => self.resolve_own_failed(&obligation.claim_key, id).await?,
                            }
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

    /// The journal entry of an own attempt on `obligation` the journal doesn't hold, rebuilt
    /// from the payer's request record and the payee's lists, enough for its outcome and its
    /// proof (Paykit spec 7.2, Paykit wire 4.10): the request's amount, label and deadline,
    /// and the destination paid. `None` when the records can't tell which destination was
    /// paid: a request whose own endpoints leave more than one usable, a list endpoint not
    /// marked used by this attempt at the version held, or a payment without a request,
    /// whose record doesn't keep the amount its proof states.
    async fn rebuilt_entry(
        &self,
        obligation: &Obligation,
        attempt: AttemptId,
    ) -> Result<Option<JournalEntry>> {
        let (Some(request_hash), Some((stored, body))) =
            (obligation.request_hash, request_of(&obligation.record))
        else {
            return Ok(None);
        };
        if body.is_recurring() {
            return Ok(None);
        }
        let (destination, carrier) = match &body.destination {
            RequestDestination::Endpoints(endpoints) => {
                let mut usable = endpoints.iter().filter(|(id, _)| {
                    body.accepted_payment_endpoint_identifiers
                        .as_ref()
                        .is_none_or(|ids| ids.contains(id))
                });
                let (Some((id, payload)), None) = (usable.next(), usable.next()) else {
                    return Ok(None);
                };
                let destination = Destination::new(
                    stored.message().from.app_id,
                    id,
                    payload,
                    DestinationSource::Request,
                    None,
                    false,
                );
                (destination, Carrier::Message(Box::new(stored.clone())))
            }
            RequestDestination::RequiredApp(_) | RequestDestination::Any => {
                let mut found = None;
                for list in self.lists_of(&obligation.payee).await? {
                    let Some(mark) = list.used.iter().find(|u| u.attempt_id == attempt) else {
                        continue;
                    };
                    let Body::PrivatePaymentList(body) = list.list.body() else {
                        continue;
                    };
                    let Some(endpoint) = body
                        .payment_endpoints
                        .get(&mark.endpoint)
                        .filter(|_| body.version == mark.version)
                    else {
                        continue;
                    };
                    found = Some((
                        Destination::new(
                            list.app_id,
                            &mark.endpoint,
                            &endpoint.payload,
                            DestinationSource::List,
                            Some(mark.version),
                            true,
                        ),
                        Carrier::Message(Box::new(list.list.clone())),
                    ));
                }
                let Some(found) = found else {
                    return Ok(None);
                };
                found
            }
        };
        let mut entry = JournalEntry {
            attempt_id: attempt.to_text(),
            obligation: obligation.claim_key,
            payee: obligation.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request_hash)),
            natural_key: natural_key::request_record(&obligation.payee, &request_hash),
            amount: KeptAmount::of(&body.amount),
            destination,
            deadline: body.attempt_deadline(None)?,
            stage: Stage::Paying,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            // The acceptance went with the payment from the device that made it, or the proof
            // stands for it: none is signed again.
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: true,
            journaled_at_ms: self.now(),
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
        };
        entry.keep_carrier(&carrier);
        Ok(Some(entry))
    }

    /// The owner reports its attempt `failed` (Switch spec 10.2 row 5), when no journal entry
    /// can be rebuilt for it: a failure needs no proof.
    async fn resolve_own_failed(&mut self, claim_key: &[u8; 32], attempt: AttemptId) -> Result<()> {
        let Some(read) = self.app.read_claim(claim_key).await? else {
            return Ok(());
        };
        let op = ClaimOp::failed(*claim_key, read.view.step, attempt.to_bytes());
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
