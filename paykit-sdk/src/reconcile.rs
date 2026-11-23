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
//! can't be written, stays doubtful, a reported failure too, since a payment that completed
//! after all couldn't be reported for it. What nothing can tell stays doubtful, "may be paid", for
//! the user: [`Paykit::resolve_attempt`] writes the user's choice (`done` basis
//! `reconcile`, `unknown`, or `failed`). [`Paykit::end_reconcile`] completes Paykit's part;
//! the app stays in reconcile mode until its other protocols complete theirs.
//!
//! After a provider loss, a reconcile also lists each one-time request, and each Billing
//! Period of a subscription that may have been attempted, of the payer's records still in
//! doubt from before the loss ([`Doubtful::before_loss`], Paykit spec 7.3, "After a
//! provider loss"), with no attempt when the new service holds none. A SETTLED claim
//! resolves the doubt. A Billing Period of an earlier subscription stays held while its
//! payment history is incomplete. For a one-time request the user can confirm the doubt
//! or state it wasn't paid before that loss ([`Paykit::confirm_not_paid_before_loss`]).
//!
//! The payer pass, the reading of the payer's obligations above, needs `paykit_pay`: without it the
//! switch answers no claim read. After a restore, no new attempt is made and nothing is
//! dispatched until a reconcile under `paykit_pay` has run it, even once reconcile mode has ended,
//! since only it finds the attempts made after the backup ([`ReconcileReport::payer_skipped`]).
//!
//! Only the payee's yes counts: a receipt settles an attempt; nothing from the payee shows an
//! attempt wasn't paid (Paykit spec 7.3).

use paykit_lib::bodies::RequestDestination;
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::periods::{BillingPeriod, PeriodRef};
use paykit_lib::records::{ActivityRecord, ActivitySide, DestinationSource, RecordBinding};
use paykit_lib::{AttemptId, Body};
use pubky_switch_client::app::{
    ClaimOp, ObligationDeadline, OpBatch, RecordContent, UserConfirmed,
};
use pubky_switch_core::claims::{Basis, ClaimStatus, Outcome};
use pubky_switch_core::ops::{Condition, OpKind};

use crate::activity::ActivityTarget;
use crate::destination::Carrier;
use crate::error::{PaykitError, Refusal, Result};
use crate::hold::RestoreHold;
use crate::journal::{Destination, JournalEntry, KeptAmount, KeptPeriod, Stage};
use crate::pay::{own_deadline, PayOutcome, Written};
use crate::paykit::Paykit;
use crate::rail::{RailOutcome, RailPaid};
use crate::records::KeptMessage;
use crate::requests::{cancelled, rejected, request_of};
use crate::state::key;

/// How the user resolves a doubtful attempt (Paykit spec 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct Doubtful {
    /// The claim key.
    pub obligation: [u8; 32],
    /// The attempts in doubt: `unknown`, or open and orphaned or unresolved here. Empty for
    /// an obligation in doubt only after a provider loss ([`Doubtful::before_loss`]): the
    /// attempts the old service took don't reach the new one.
    pub attempts: Vec<AttemptId>,
    /// The payee, when the app's records name the obligation.
    pub payee: Option<[u8; 32]>,
    /// The request, when there is one.
    pub request_hash: Option<[u8; 32]>,
    /// The obligation is in doubt after a provider loss (Paykit spec 7.3, "After a provider
    /// loss"): a one-time request payable before the loss, not answered here, whose claim at
    /// the new service isn't SETTLED, so another own app, or this one, may have paid it at
    /// the old service. It is listed as [`crate::RequestStatus::MayBePaid`] shows the
    /// request, until its claim shows it SETTLED or the user states it wasn't paid before
    /// the loss. The host shows the user that it may already be paid; the user pays it by
    /// hand over the doubt ([`crate::PayOptions::with_may_be_paid_confirmed`], with the
    /// doubt [`Paykit::pay_request`] refuses it with) or states it wasn't paid
    /// ([`Paykit::confirm_not_paid_before_loss`], with this loss). A newer loss lists it
    /// again, under that loss. A Billing Period of an earlier subscription stays held while
    /// its payment history is incomplete. `None` for an obligation no provider loss leaves in doubt.
    pub before_loss: Option<crate::LossBoundary>,
}

/// What a reconciliation did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReconcileReport {
    /// The outcomes of the payments it finished.
    pub resolved: Vec<PayOutcome>,
    /// What stays doubtful, for the user: the attempts nothing here could resolve, and the
    /// obligations a provider loss leaves in doubt ([`Doubtful::before_loss`]).
    pub doubtful: Vec<Doubtful>,
    /// The payer pass was skipped, the certificate lacking `paykit_pay`: the payer's obligations
    /// weren't read, so `doubtful` holds only the client's own doubtful claims, and none a
    /// provider loss leaves in doubt. After a restore, no new attempt is made until a
    /// reconcile under `paykit_pay` runs the pass, also after [`Paykit::end_reconcile`].
    pub payer_skipped: bool,
}

/// An obligation of the payer's records.
struct Obligation {
    claim_key: [u8; 32],
    payee: [u8; 32],
    request_hash: Option<[u8; 32]>,
    /// The request record, or a payment without a request's record.
    record: ActivityRecord,
    /// The Billing Period, for a recurring request.
    period: Option<BillingPeriod>,
    /// The natural key of the record its outcome writes: the request's, the period's, or
    /// the payment's.
    natural_key: Vec<u8>,
}

impl Paykit {
    /// The obligations of the payer's `records` (Paykit spec 7.3): each one-time request's,
    /// each payment without a request's, and, for a recurring request, every Billing Period
    /// its request record lists in `attempted_periods`, where every attempt on a period is
    /// listed from its own batch on until its claim has no open or `unknown` attempt, and
    /// every period of it a period record names (Paykit spec 8.2).
    ///
    /// # Errors
    ///
    /// A corrupt store for a listed start whose recurrence can't be computed.
    fn obligations(&self, records: &[ActivityRecord]) -> Result<Vec<Obligation>> {
        let mut out = Vec::new();
        let mut periods: Vec<([u8; 32], [u8; 32], u64)> = Vec::new();
        for record in records {
            match record.binding() {
                Ok(RecordBinding::Request { request_hash }) => {
                    let payee = record.counterparty;
                    if request_of(record).is_some_and(|(_, body)| body.is_recurring()) {
                        periods.extend(
                            record
                                .attempted_periods
                                .iter()
                                .map(|start| (payee, request_hash, *start)),
                        );
                        continue;
                    }
                    out.push(Obligation {
                        claim_key: self.obligation(&payee, &request_hash, 0),
                        payee,
                        request_hash: Some(request_hash),
                        record: record.clone(),
                        period: None,
                        natural_key: natural_key::request_record(&payee, &request_hash),
                    });
                }
                Ok(RecordBinding::Period {
                    request_hash,
                    billing_period,
                }) => periods.push((
                    record.counterparty,
                    request_hash,
                    billing_period.start_secs(),
                )),
                Ok(RecordBinding::Unrequested) => {
                    if let Some(attempt) = record.attempt_id {
                        out.push(Obligation {
                            claim_key: self.payment_obligation(&attempt),
                            payee: record.counterparty,
                            request_hash: None,
                            record: record.clone(),
                            period: None,
                            natural_key: natural_key::payer_payment_record(
                                &self.identity(),
                                &attempt,
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
        periods.sort_unstable();
        periods.dedup();
        for (payee, request_hash, start) in periods {
            // The period of a request the app holds, by its recurrence; a start that names
            // no Billing Period of it is no period's (Paykit wire 5.3).
            let Some((record, body)) = records.iter().find_map(|record| {
                let (stored, body) = request_of(record)?;
                (stored.hash() == request_hash && record.counterparty == payee)
                    .then_some((record, body))
            }) else {
                continue;
            };
            let period = match body
                .recurrence
                .as_ref()
                .map(|r| r.by_start_secs(start))
                .transpose()?
            {
                Some(Some(period)) => period,
                _ => continue,
            };
            out.push(Obligation {
                claim_key: self.obligation(&payee, &request_hash, start),
                payee,
                request_hash: Some(request_hash),
                record: record.clone(),
                period: Some(period),
                natural_key: natural_key::period_record(&payee, &request_hash, start),
            });
        }
        Ok(out)
    }

    /// The provider loss that leaves `obligation`, a one-time request's or a Billing
    /// Period's, in doubt (Paykit spec 7.3), its claim at the new service not SETTLED, which
    /// the caller judges first: as the request's status reads
    /// [`crate::RequestStatus::MayBePaid`] for the loss, the request record showing it neither
    /// rejected nor cancelled, and the record of the occurrence not showing it paid. A Billing
    /// Period may have been paid early, before the signed subscription's service was lost.
    async fn request_loss_doubt(
        &self,
        obligation: &Obligation,
    ) -> Result<Option<crate::LossBoundary>> {
        let record = &obligation.record;
        let Some((stored, body)) = request_of(record) else {
            return Ok(None);
        };
        if obligation.request_hash.is_none()
            || body.is_recurring() != obligation.period.is_some()
            || rejected(record)
            || cancelled(record)
        {
            return Ok(None);
        }
        let paid = match obligation.period {
            None => record.shows_paid(),
            Some(_) => self
                .held_activity(&ActivityTarget::payer(obligation.natural_key.clone()))
                .await?
                .is_some_and(|period| period.shows_paid()),
        };
        if paid {
            return Ok(None);
        }
        self.loss_doubt(
            &obligation.claim_key,
            Some(stored),
            obligation.period.map(PeriodRef::from),
        )
        .await
    }

    pub(crate) async fn payer_records(&self) -> Result<Vec<ActivityRecord>> {
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
                &|endpoint, proof| self.rail.settlement_id(endpoint, proof),
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
    /// Returns [`crate::Refusal::NotNow`] while more collection pages remain or a wider
    /// certificate's scope keys are missing. Call again to continue the listing. The payer
    /// pass needs the keys to read every obligation. [`Paykit::resume_payments`] can still
    /// finish payments already dispatched. Client and rail errors propagate to the caller.
    pub async fn reconcile(&mut self) -> Result<ReconcileReport> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.reconcile_inner()).await
    }

    async fn reconcile_inner(&mut self) -> Result<ReconcileReport> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.list_records("Call reconcile again").await?;
        Box::pin(self.repair_settlement_buckets()).await?;
        // After a restore, the counterparties of the open requests the records now show are
        // asked to re-send from the backup's cursor time, once (`hold.rs`, Paykit spec 7.2);
        // what isn't sent now goes at the next receive.
        self.queue_restore_asks().await?;
        let _ = self.send_asks(&mut None).await;
        let payer_pass = self.holds("paykit_pay");
        let mut report = ReconcileReport {
            resolved: self.resume_payments().await?,
            doubtful: Vec::new(),
            payer_skipped: !payer_pass,
        };
        let journaled: Vec<[u8; 16]> = self
            .journal()
            .await?
            .iter()
            .filter(|e| e.open())
            .filter_map(|e| e.attempt().ok().map(AttemptId::to_bytes))
            .collect();
        // Without `paykit_pay` the app reads no claim (the service refuses it without
        // `claims.read`): the payer records' obligations wait for an app holding it, and
        // the client's own doubtful claims below are still reported.
        let records = if payer_pass {
            self.payer_records().await?
        } else {
            Vec::new()
        };
        let me = self.app_id();
        for obligation in self.obligations(&records)? {
            let mut read = self.app.read_claim(&obligation.claim_key).await?;
            let attempts = read
                .as_ref()
                .map(|read| read.view.attempts.clone())
                .unwrap_or_default();
            let resolved_before = report.resolved.len();
            let mut doubtful = Vec::new();
            for attempt in &attempts {
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
                        Some(RailOutcome::Paid(RailPaid {
                            proof, paid_at_ms, ..
                        })) => {
                            if let Some(mut entry) = self.rebuilt_entry(&obligation, id).await? {
                                entry.rail_proof = Some(proof);
                                entry.paid_at_ms = paid_at_ms;
                                // Journaled first: a report whose write fails leaves it for a
                                // resume, as [`PayOutcome::PaidOutcomeUnwritten`] says.
                                self.set_stage(&mut entry, Stage::RailPaid).await?;
                                report.resolved.push(self.report(&mut entry).await?);
                                continue;
                            }
                        }
                        Some(RailOutcome::Failed) => {
                            // Reported failed by the method, so asked again while its history
                            // keeps the dispatch, and a payment that completed after all sends
                            // its proof (Paykit spec 7.2): only with an entry that can do it.
                            // Without one, writing `failed` would free the claim for a second
                            // payment while a late success could never be reported: the
                            // attempt stays doubtful, for a receipt or the user.
                            if let Some(mut entry) = self.rebuilt_entry(&obligation, id).await? {
                                entry.failed_by_method = true;
                                // Journaled first: a report whose write fails leaves it for a
                                // resume, as [`PayOutcome::PaidOutcomeUnwritten`] says.
                                self.set_stage(&mut entry, Stage::RailFailed).await?;
                                report.resolved.push(self.report(&mut entry).await?);
                                continue;
                            }
                        }
                        _ => {}
                    }
                }
                doubtful.push(id);
            }
            // An outcome written here may have settled the claim: it is read again for the
            // doubt a provider loss leaves.
            if report.resolved.len() > resolved_before {
                read = self.app.read_claim(&obligation.claim_key).await?;
            }
            let settled = read
                .as_ref()
                .is_some_and(|read| read.view.status == ClaimStatus::Settled);
            let before_loss = if settled {
                None
            } else {
                self.request_loss_doubt(&obligation).await?
            };
            if !doubtful.is_empty() || before_loss.is_some() {
                report.doubtful.push(Doubtful {
                    obligation: obligation.claim_key,
                    attempts: doubtful,
                    payee: Some(obligation.payee),
                    request_hash: obligation.request_hash,
                    before_loss,
                });
            }
        }
        for (claim_key, doubt) in self.app.doubtful_claims().await? {
            let mut attempts: Vec<_> = doubt
                .unknown
                .iter()
                .chain(doubt.orphaned.iter())
                .map(|a| AttemptId::from_bytes(*a))
                .collect();
            attempts.sort_unstable();
            attempts.dedup();
            if let Some(row) = report
                .doubtful
                .iter_mut()
                .find(|d| d.obligation == claim_key)
            {
                for attempt in attempts {
                    if !row.attempts.contains(&attempt) {
                        row.attempts.push(attempt);
                    }
                }
                continue;
            }
            report.doubtful.push(Doubtful {
                obligation: claim_key,
                attempts,
                payee: None,
                request_hash: None,
                before_loss: None,
            });
        }
        // Every own attempt on the payer's obligations was read: new attempts may follow
        // once reconcile mode ends (`Paykit::payer_pass_owed`).
        if payer_pass {
            self.payer_pass_ran().await?;
        }
        Ok(report)
    }

    /// The journal entry of an own attempt on `obligation` the journal doesn't hold, rebuilt
    /// from the payer's request record and the payee's lists, enough for its outcome and its
    /// proof (Paykit spec 7.2, Paykit wire 4.10): the request's amount, label and deadline,
    /// the Billing Period's for a period, and the destination paid. `None` when the records
    /// can't tell which destination was paid: a request whose own endpoints leave more than
    /// one usable, a list endpoint not marked used by this attempt at the version held, a
    /// later Billing Period of a request with `per_period_endpoints`, paid to its period
    /// details' destination, or a payment without a request, whose record doesn't keep the
    /// amount its proof states.
    async fn rebuilt_entry(
        &mut self,
        obligation: &Obligation,
        attempt: AttemptId,
    ) -> Result<Option<JournalEntry>> {
        let (Some(request_hash), Some((stored, body))) =
            (obligation.request_hash, request_of(&obligation.record))
        else {
            return Ok(None);
        };
        let target = ActivityTarget::payer(natural_key::request_record(
            &obligation.payee,
            &request_hash,
        ));
        if body.is_recurring() != obligation.period.is_some()
            || self
                .activity_base_changed(&self.activity_key(&target)?)
                .await?
        {
            return Ok(None);
        }
        // The request's destination serves a period unless the request has
        // `per_period_endpoints` and it isn't the first (Paykit spec 8.2).
        if let Some(period) = obligation.period {
            let first = body
                .recurrence
                .as_ref()
                .ok_or_else(PaykitError::corrupt)?
                .first()?;
            if body.per_period_endpoints && period != first {
                return Ok(None);
            }
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
                let required = match &body.destination {
                    RequestDestination::RequiredApp(app) => Some(*app),
                    _ => None,
                };
                // Every single-use mark that names the attempt, in a list the request allows:
                // of its required app, if any, at the version held, for an endpoint the list
                // carries and the request accepts (Paykit wire 4.5, 5.4). Kept once each, by
                // app, endpoint and version; the destination is known only when one is left.
                let mut found: std::collections::BTreeMap<_, (Destination, Carrier)> =
                    std::collections::BTreeMap::new();
                for list in self.lists_of(&obligation.payee).await? {
                    if required.is_some_and(|app| app != list.app_id) {
                        continue;
                    }
                    let Body::PrivatePaymentList(listed) = list.list.body() else {
                        continue;
                    };
                    for mark in list.used.iter().filter(|u| u.attempt_id == attempt) {
                        let accepted = body
                            .accepted_payment_endpoint_identifiers
                            .as_ref()
                            .is_none_or(|ids| ids.contains(&mark.endpoint));
                        let Some(endpoint) = listed
                            .payment_endpoints
                            .get(&mark.endpoint)
                            .filter(|e| accepted && e.single_use && listed.version == mark.version)
                        else {
                            continue;
                        };
                        found.insert(
                            (list.app_id, mark.endpoint.as_str().to_owned(), mark.version),
                            (
                                Destination::new(
                                    list.app_id,
                                    &mark.endpoint,
                                    &endpoint.payload,
                                    DestinationSource::List,
                                    Some(mark.version),
                                    true,
                                ),
                                Carrier::Message(Box::new(list.list.clone())),
                            ),
                        );
                    }
                }
                if found.len() != 1 {
                    return Ok(None);
                }
                let Some((_, found)) = found.pop_first() else {
                    return Ok(None);
                };
                found
            }
        };
        // The rail's history establishes an outcome but cannot identify the signed rate
        // details the user selected. A cross-asset period proof needs their exact hash.
        if matches!(
            body.conversion,
            Some(paykit_lib::bodies::Conversion::PerPeriod)
        ) && destination.endpoint_id()?.asset_segment() != Some(body.amount.asset.as_str())
        {
            return Ok(None);
        }
        let allowance = if let Some(id) = &obligation.record.allowance_id {
            let Some(under) = Box::pin(self.restored_payment_allowance(
                &obligation.payee,
                id,
                &obligation.claim_key,
                attempt,
            ))
            .await?
            else {
                return Ok(None);
            };
            Some(under)
        } else {
            None
        };
        let mut entry = JournalEntry {
            // The claim alone cannot establish that the user started this payment.
            user_started: false,
            allowance,
            attempt_id: attempt.to_text(),
            obligation: obligation.claim_key,
            payee: obligation.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request_hash)),
            natural_key: obligation.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            payment_amount: KeptAmount::of(&body.amount),
            destination,
            deadline: body.attempt_deadline(obligation.period.as_ref())?,
            stage: Stage::Paying,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            // The acceptance went with the payment from the device that made it, or the proof
            // stands for it: none is signed again.
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            acceptance_cert: None,
            acceptance_released: false,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            offline: None,
            restored: true,
            journaled_at_ms: self.now(),
            allowance_expires_at_ms: None,
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            never_dispatched: false,
            user_paid: false,
            request: Some(KeptMessage::of(stored)),
            record_held: false,
            transfer_bound: None,
            price: None,
            failed_on_claim: false,
            billing_period: obligation
                .period
                .map(|period| KeptPeriod::of(&PeriodRef::from(period))),
        };
        entry.keep_carrier(&carrier);
        Ok(Some(entry))
    }

    /// Write the user's resolution of a doubtful attempt (Paykit spec 7.3): `done` with basis
    /// `reconcile` or `unknown` for an orphaned attempt, `failed` for an `unknown` one or an
    /// orphaned one whose owner isn't current. An attempt of this app's own is reported as
    /// its owner reports it: `done` with basis `owner`, or `failed` (Switch spec 10.2 rows 2
    /// and 5). The user has seen "may already be paid". The attempt's journal entry, if this
    /// app holds one, is finished in the same batch.
    ///
    /// `done`, whatever its basis, never goes without the payer's activity record's update in
    /// its batch (Paykit spec 7.2 step 3), as at a payment's outcome:
    ///
    /// - an own attempt the method reported paid (`RailPaid`) is written as its outcome is,
    ///   with the acceptance and the proof placed and the proof queued;
    /// - otherwise the record gets the journaled acceptance, if any, and `status` `paid`,
    ///   built on the version held; an absent or deleted record is rebuilt from the journal
    ///   entry, and one that doesn't read, with no copy this app trusts, holds;
    /// - for a Billing Period the shared record must carry the signed proof of this
    ///   attempt. The claim expires, and a period's status does not establish payment;
    /// - a record this app can't find or rebuild holds too ([`Refusal::RecordHeld`]).
    ///
    /// `unknown` and `failed` move no money toward the payee and need no record update. Neither
    /// is taken for an own attempt whose payment the method reported made.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a claim without the attempt, unless it is this app's own
    /// restored dispatch still open, which no claim shows, as after a provider loss: the
    /// user's `done` or `failed` then finishes its journal entry, `done` with the record's
    /// update, and writes no claim entry (Paykit spec 7.2, 8.4), while `unknown` is refused;
    /// [`Refusal::AlreadyPaid`] for
    /// `failed` or `unknown` on an own attempt the method reported paid; [`Refusal::RecordHeld`] for a
    /// `done` whose record can't be updated, with nothing written; the switch's refusal, such
    /// as `409 conflict`, for a resolution its rows don't allow now.
    pub async fn resolve_attempt(
        &mut self,
        obligation: &[u8; 32],
        attempt: AttemptId,
        resolution: Resolution,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.resolve_attempt_inner(obligation, attempt, resolution, _confirmed)).await
    }

    async fn resolve_attempt_inner(
        &mut self,
        obligation: &[u8; 32],
        attempt: AttemptId,
        resolution: Resolution,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        // The journal entry of an own attempt is finished with the resolution, in its batch:
        // the user decided, so it neither blocks another payment as one in progress nor is
        // resumed, and never dispatched again (Paykit spec 7.2, "No second execution").
        // Read bound to its key and to `obligation`: an entry of another attempt, or of this
        // attempt on another claim, is a corrupt store, and nothing is written.
        let mut entry = self.journal_entry_on(&attempt, obligation).await?;
        let read = self.app.read_claim(obligation).await?;
        let (basis, op) = match read
            .as_ref()
            .and_then(|read| Some((read.view.step, read.view.attempt(&attempt.to_bytes())?)))
        {
            Some((step, held)) => {
                let basis = if held.owner == self.app_id() {
                    Basis::Owner
                } else {
                    Basis::Reconcile
                };
                let op = match resolution {
                    Resolution::Done => {
                        ClaimOp::done(*obligation, step, attempt.to_bytes(), basis, None)?
                    }
                    Resolution::Unknown => ClaimOp::unknown(*obligation, step, attempt.to_bytes()),
                    Resolution::Failed => ClaimOp::failed(*obligation, step, attempt.to_bytes()),
                };
                (basis, Some(op))
            }
            // No claim shows the attempt, as after a provider loss or once the claim is gone
            // past its window: an own restored dispatch still open stays "potentially paid",
            // and only a receipt or the user can resolve it (Paykit spec 7.2, 8.4). The user's
            // `done` or `failed` finishes its journal entry, `done` with the record's update,
            // and writes no claim entry, there being none to write on.
            None if resolution != Resolution::Unknown
                && entry.as_ref().is_some_and(|e| {
                    e.restored
                        && e.open()
                        && (e.may_have_dispatched() || e.stage == Stage::RailPaid)
                }) =>
            {
                (Basis::Owner, None)
            }
            // A restored dispatch the method reported paid is never marked `unknown`: it
            // holds its proof (Paykit spec 7.2, "Failed means definitively not paid").
            None if entry
                .as_ref()
                .is_some_and(|e| e.restored && e.open() && e.rail_proof.is_some()) =>
            {
                return Err(Refusal::AlreadyPaid.into())
            }
            None => return Err(Refusal::NotFound.into()),
        };
        // An own payment the method reported made is never `failed` or `unknown`: either
        // would free the claim for a second payment, and its proof would never go (Paykit
        // spec 7.2, "Failed means definitively not paid"). It is resolved `done`, or by
        // [`Paykit::repair_activity_record`] once its record can be written.
        if resolution != Resolution::Done
            && basis == Basis::Owner
            && entry.as_ref().is_some_and(|e| e.rail_proof.is_some())
        {
            return Err(Refusal::AlreadyPaid.into());
        }
        if resolution == Resolution::Done {
            // An own payment the method reported paid: its outcome, with the proof.
            if let Some(mut paid) = entry
                .clone()
                .filter(|e| basis == Basis::Owner && e.stage == Stage::RailPaid)
            {
                return match self.write_outcome(&mut paid, true, false).await? {
                    Written::Done => Ok(()),
                    Written::Held => Err(Refusal::RecordHeld(
                        "the payment waits for an activity record it can update".into(),
                    )
                    .into()),
                    Written::Divergent(record) => {
                        Err(Box::pin(self.divergent_refusal(record)).await?)
                    }
                };
            }
        }
        // The user's `done` on a payment this app may have dispatched
        // ([`JournalEntry::may_have_dispatched`]) says it was made: the user's word stands
        // where the method's history would, so an acceptance the entry still owes goes
        // first, and the record places it (Paykit spec 7.2 step 2). One that can't be queued
        // now refuses the call with nothing written; the user tries again. The entry can't
        // wait in `AcceptanceOwed`: that stage holds a proof. An entry known unsubmitted, or
        // one the method reported failed, sends none.
        // The record `done` goes with is found first, and a value there only waiting for its
        // scope key refuses the call before anything is released: the acceptance goes only
        // with a `done` that can be written now.
        let done_target = match resolution {
            Resolution::Done => Some(
                self.done_target(obligation, &attempt, entry.as_ref())
                    .await?,
            ),
            _ => None,
        };
        if resolution == Resolution::Done && basis == Basis::Owner {
            if let Some(owed) = entry.as_mut().filter(|e| e.may_have_dispatched()) {
                if owed.owes_acceptance()? {
                    self.release_acceptance(owed, None).await?;
                }
            }
        }
        let deadline = entry
            .as_ref()
            .map_or(ObligationDeadline::NotKnown, |e| own_deadline(e.deadline));
        let record_write = match &done_target {
            Some(target) => Some(self.done_record(target, &attempt, entry.as_ref()).await?),
            None => None,
        };
        let on_claim = op.is_some();
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        if let Some(op) = op {
            batch = batch
                .op(op.kind())
                .obligation_deadline(*obligation, deadline);
        }
        if let Some(write) = record_write {
            batch = batch.entry(write);
        }
        // An Allowance ledger's outcome goes only beside the claim entry it records: a
        // ledger successor with no claim entry in its batch can't be verified again (Paykit
        // spec 8.3), as at a payment's outcome. With no claim, the reservation stays counted.
        if let Some(own) = entry
            .as_ref()
            .filter(|_| on_claim && basis == Basis::Owner && resolution != Resolution::Unknown)
        {
            batch =
                Box::pin(self.executor_outcome(own, resolution == Resolution::Done, batch)).await?;
        }
        // With no claim entry, the journal keeps the user's `done` on the entry itself: no
        // payment of its obligation goes again while it is journaled (Paykit spec 8.4).
        if !on_claim && resolution == Resolution::Done {
            if let Some(entry) = entry.as_mut() {
                entry.user_paid = true;
            }
        }
        // With no claim entry, record or ledger write to make, only the journal changes:
        // the entry finishes locally.
        if batch.is_empty() {
            if let Some(entry) = entry.as_mut().filter(|e| e.open()) {
                self.set_stage(entry, Stage::Finished).await?;
            }
            return Ok(());
        }
        if let Some(entry) = entry.as_mut().filter(|e| e.open()) {
            if resolution == Resolution::Failed {
                entry.end_before_the_call();
            }
            entry.stage = Stage::Finished;
            batch = batch.local(self.journal_write(entry)?);
        }
        let applied = self.submit(batch).await?;
        if let Some(own) = entry
            .as_ref()
            .filter(|_| on_claim && basis == Basis::Owner && resolution != Resolution::Unknown)
        {
            Box::pin(self.keep_executor_reply(own, &applied.proofs)).await?;
        }
        Ok(())
    }

    /// The payer's activity record a user's `done` without a proof updates
    /// ([`Paykit::done_record`]): the record of `obligation`, from the journal entry, the
    /// attempt of a payment without a request, or the request records this app holds.
    ///
    /// # Errors
    ///
    /// [`Refusal::RecordHeld`] when no record can be found; [`Refusal::NotNow`] naming the
    /// scope keys while the record's value is sealed under a key the app doesn't hold yet:
    /// it opens once the key comes, and is never written over meanwhile.
    async fn done_target(
        &mut self,
        obligation: &[u8; 32],
        attempt: &AttemptId,
        entry: Option<&JournalEntry>,
    ) -> Result<ActivityTarget> {
        let own = self.identity();
        let held =
            |why: &str| -> crate::error::PaykitError { Refusal::RecordHeld(why.into()).into() };
        let natural = match entry {
            Some(entry) => Some(entry.natural_key.clone()),
            None if *obligation == self.payment_obligation(attempt) => {
                Some(natural_key::payer_payment_record(&own, attempt))
            }
            None => {
                let records = self.payer_records().await?;
                self.obligations(&records)?
                    .into_iter()
                    .find(|o| o.claim_key == *obligation)
                    .and_then(|o| o.request_hash.map(|_| o.natural_key))
            }
        };
        let Some(natural) = natural else {
            return Err(held("no payer record names the obligation"));
        };
        let target = ActivityTarget::payer(natural);
        let key = self.activity_key(&target)?;
        if self
            .activity_write_base(&key)
            .await?
            .is_some_and(|r| crate::hold::awaits_key(&r.content))
        {
            return Err(crate::hold::value_awaits_key());
        }
        // A user's `done` by hand on a record paused after a view showed it divergent waits
        // for the user's confirmation of that record first (Paykit spec 6).
        Box::pin(self.refuse_while_paused(&[key])).await?;
        Ok(target)
    }

    /// The payer's activity record's update for a user's `done` without a proof (Paykit spec
    /// 7.2 step 3, as [`Paykit::resolve_attempt`] says) at `target` ([`Paykit::done_target`]):
    /// with the journaled acceptance placed and `status` `paid`, built on the version held, so
    /// `done` and the update are applied together or not at all. An absent or deleted record
    /// is rebuilt from `entry`; one that doesn't read, with no copy this app trusts, or one
    /// that can't be rebuilt, holds. A Billing Period also needs its signed proof:
    /// the period's status and its manual-only mark cannot show payment after claim expiry.
    async fn done_record(
        &mut self,
        target: &ActivityTarget,
        attempt: &AttemptId,
        entry: Option<&JournalEntry>,
    ) -> Result<(Vec<Condition>, OpKind)> {
        let held =
            |why: &str| -> crate::error::PaykitError { Refusal::RecordHeld(why.into()).into() };
        let key = self.activity_key(target)?;
        let current = self.activity_write_base(&key).await?;
        let read = match current.as_ref().map(|r| &r.content) {
            Some(RecordContent::Deleted)
                if entry.is_some_and(|entry| entry.billing_period.is_some()) =>
            {
                return Err(crate::requests::payment_history_lost().into())
            }
            None | Some(RecordContent::Deleted) => None,
            Some(_) => match self.read_activity(target, current.as_ref()) {
                Some(record) => Some(record),
                None => return Err(held("the record holds a value that doesn't read")),
            },
        };
        let mut record = match read {
            Some(record) => record,
            None => match entry.map(|e| self.rebuilt_record(e)).transpose()? {
                Some(record) => record,
                None => return Err(held("the record is gone, with nothing to rebuild it from")),
            },
        };
        if let Some(entry) = entry {
            self.place_acceptance(&mut record, entry)?;
        }
        let mut conditions = Vec::new();
        if let binding @ RecordBinding::Period { request_hash, .. } = record.binding()? {
            let parent = ActivityTarget::payer(natural_key::request_record(
                &record.counterparty,
                &request_hash,
            ));
            let parent_key = self.activity_key(&parent)?;
            let base = self.activity_write_base(&parent_key).await?;
            let request = self.read_activity(&parent, base.as_ref());
            let proof = record.proof.as_ref().map(|stored| stored.body());
            if !matches!(proof, Some(Body::PaymentProof(proof)) if &proof.attempt_id == attempt)
                || !request.as_ref().is_some_and(|request| {
                    crate::periods::request_for_payment(request, &record, &binding).is_some()
                })
            {
                return Err(held("the Billing Period needs this attempt's proof and its signed request to record payment"));
            }
            conditions.push(self.app.record_condition(&parent_key).await?);
        }
        record.status = Some("paid".into());
        let value = record.to_json()?;
        Ok(self
            .app
            .record_op(&target.record_target(), Some(&value), conditions)
            .await?)
    }

    /// Paykit's reconciliation finished (Switch spec 12.4 step 2.4). The app stays in
    /// reconcile mode until every registered protocol has finished. The host completes
    /// its other protocols through
    /// [`App::end_reconcile_for`](pubky_switch_client::app::App::end_reconcile_for).
    /// New attempts wait until then.
    ///
    /// After a restore, the re-send requests to open requests' counterparties are queued
    /// first if [`Paykit::reconcile`] didn't queue them (Paykit spec 7.2): a host that
    /// finishes without reconciling still asks once. Payments still wait for a reconcile
    /// under `paykit_pay` that runs the payer pass, if none has yet; Paykit's payee side
    /// doesn't require that payer pass.
    ///
    /// # Errors
    ///
    /// The client's errors, such as the restore question still open; nothing ends while the
    /// asks can't be queued.
    pub async fn end_reconcile(&mut self, confirmed: UserConfirmed) -> Result<()> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        if self.floor_write_is_kept().await? {
            return Err(crate::floor::unreadable_floor());
        }
        Box::pin(self.restore_verdict_not_known_holds()).await?;
        let asks_owed = self
            .local_get::<RestoreHold>(key::RESTORE_HOLD)
            .await?
            .is_some_and(|hold| hold.asks_due());
        if asks_owed {
            self.list_records("Call end_reconcile again").await?;
            self.queue_restore_asks().await?;
            // Queued: what isn't sent now goes at the next receive.
            let _ = self.send_asks(&mut None).await;
        }
        self.app
            .end_reconcile_for(crate::protocol::BACKUP_NAME, confirmed)
            .await?;
        Ok(())
    }
}
