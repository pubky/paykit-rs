//! Recurring requests' Billing Periods (Paykit spec 8.2; Paykit wire 4.4, 4.9, 5.3): the
//! payee's period details, the period records of both sides, and the views of a
//! subscription's periods.
//!
//! - **Period details** are sent by a payee app that holds the request's record, once the
//!   payer has accepted the subscription, as v0.1's quotes were: the app writes them into its
//!   `payee_activity` period record in the batch that queues them, conditioned on the version
//!   it read, so its other own apps can credit a proof that names them, and it sends none
//!   once the record holds 8 for the period ([`Paykit::send_period_details`]).
//! - **A period record** keeps every details of its period, none replacing another; the
//!   proof of the period and its receipt go there too, never into the request record
//!   (Paykit wire 5.3 "Binding").
//! - **Views:** a request's view lists the period records the app holds for it
//!   ([`PeriodView`]), each with its details, latest first.
//! - **Paying a period** ([`Paykit::pay_period`]) follows the paying order of every payment
//!   (Paykit spec 7.2), on the period's own obligation. Accepting a subscription pays
//!   nothing by itself: the host calls it for each period, on its user's confirmation or by
//!   its own policy, which is an automatic payment for every rule that names one (Paykit spec
//!   8.2, "Paying periods"). The attempt carries `server_time_in(s, null)`, `s` the period's
//!   start, or the time the user confirmed paying it early; an attempt on a period the
//!   request record doesn't list in `attempted_periods` writes the record in its batch,
//!   adding the period and dropping the finished ones, each conditioned on its claim's step
//!   (Paykit wire 5.3, "Attempted periods"). The proof goes into the period record, names the
//!   period and the details it was paid by.

use std::collections::HashMap;

use paykit_lib::bodies::{period_details_order, PaymentRequest, PeriodDetails};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::BillingPeriod;
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, DestinationSource, RecordBinding, StoredMessage,
    MAX_ATTEMPTED_PERIODS, MAX_PERIOD_DETAILS,
};
use paykit_lib::{AttemptId, Body, PaymentEndpointIdentifier, Rate};
use pubky_switch_client::app::Initiator;
use pubky_switch_core::claims::{ClaimStatus, ClaimView, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind};

use crate::activity::{ActivityTarget, Role};
use crate::destination::Carrier;
use crate::error::{Refusal, Result};
use crate::journal::{Destination, JournalEntry, KeptAmount, KeptPeriod, Stage};
use crate::pay::{
    unsupported, BlockedPayment, Choice, Chosen, PayOptions, PayOutcome, PaymentMode, Plan,
};
use crate::paykit::Paykit;
use crate::receipts::{received_receipt, ReceivedReceipt};
use crate::records::conflict;
use crate::records::KeptMessage;
use crate::requests::RequestRef;
use crate::requests::{cancelled, rejected, request_of};

/// How many times a payee's write of period details is built again after its record changed
/// meanwhile.
const WRITE_TRIES: usize = 3;

/// Period details as the views show them (Paykit wire 4.9): no endpoint payload.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeriodDetailsView {
    /// The message ID.
    pub msg_id: String,
    /// The payee app that sent them, which owns their endpoints.
    pub app_id: [u8; 16],
    /// Their signed `created_at`; their rates apply from it, cut to whole seconds.
    pub created_at_ms: u64,
    /// Until when they are valid.
    pub expires_at_ms: u64,
    /// The identifiers of the period's destination, empty for details with rates only.
    pub endpoint_ids: Vec<PaymentEndpointIdentifier>,
    /// The period's conversion rates, empty for details with a destination only.
    pub rates: Vec<Rate>,
}

/// One Billing Period of a recurring request, from the period record the app holds (Paykit
/// wire 5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PeriodView {
    /// The record key of the period record.
    pub record: [u8; 32],
    /// The Billing Period.
    pub billing_period: PeriodRef,
    /// Every period details the record holds, latest first (Paykit wire 4.9): the latest is
    /// the payer's default, and each stays valid until its own expiry.
    pub details: Vec<PeriodDetailsView>,
    /// The record holds the proof of the period's payment.
    pub paid: bool,
    /// The payee's receipt of the period, if the record holds one.
    pub receipt: Option<ReceivedReceipt>,
}

/// The view of a period record, `record` held at `key`, with the request it belongs to;
/// `None` for a record of another kind.
fn period_view(key: [u8; 32], record: &ActivityRecord) -> Option<([u8; 32], PeriodView)> {
    let Ok(RecordBinding::Period {
        request_hash,
        billing_period,
    }) = record.binding()
    else {
        return None;
    };
    let mut held = record.details();
    held.sort_by(|a, b| period_details_order(b.message(), a.message()));
    let details = held
        .into_iter()
        .filter_map(|stored| {
            let Body::PaymentPeriodDetails(body) = stored.body() else {
                return None;
            };
            Some(PeriodDetailsView {
                msg_id: stored.message().msg_id.clone(),
                app_id: stored.message().from.app_id,
                created_at_ms: stored.message().created_at_ms,
                expires_at_ms: body.expires_at,
                endpoint_ids: body
                    .payment_endpoints
                    .iter()
                    .flat_map(|e| e.keys().cloned())
                    .collect(),
                rates: body.rates.clone().unwrap_or_default(),
            })
        })
        .collect();
    Some((
        request_hash,
        PeriodView {
            record: key,
            billing_period,
            details,
            paid: record.shows_paid(),
            receipt: received_receipt(key, record),
        },
    ))
}

/// The Billing Period a proof names, when `request` is recurring and the period is one of its
/// own (Paykit wire 4.4): `Ok(None)` for a proof that names none.
///
/// # Errors
///
/// A period that isn't one of the request's, or one named for a one-time request.
pub(crate) fn named_period(
    request: &PaymentRequest,
    stored: &StoredMessage,
) -> std::result::Result<Option<PeriodRef>, paykit_lib::Error> {
    match stored.body().billing_period() {
        None if request.is_recurring() && matches!(stored.body(), Body::PaymentProof(_)) => {
            Err(paykit_lib::Error::Invalid(
                "Paykit wire 4.10: a proof of a recurring request names its Billing Period".into(),
            ))
        }
        None => Ok(None),
        Some(named) => Ok(Some(request.named_period(named)?.into())),
    }
}

/// `attempted_periods` with `start` added (Paykit spec 8.2, "Attempted periods"): the listed
/// periods in `finished`, whose claims have no open attempt, dropped, the list ascending,
/// each start once. `None` when 16 would stay listed besides it: no attempt on another
/// period then. A listed `start` needs no write; the caller doesn't ask.
pub(crate) fn attempted_with(listed: &[u64], start: u64, finished: &[u64]) -> Option<Vec<u64>> {
    let mut list: Vec<u64> = listed
        .iter()
        .copied()
        .filter(|period| !finished.contains(period))
        .collect();
    if list.len() >= MAX_ATTEMPTED_PERIODS {
        return None;
    }
    list.push(start);
    list.sort_unstable();
    list.dedup();
    Some(list)
}

/// The step of a listed period's claim, `view`, when it has no open attempt, at which a
/// write that drops the period from `attempted_periods`, or a payer's cancellation, is
/// conditioned (Paykit wire 5.3): 0 for no claim. `None` while an attempt is open, or, on a
/// claim not settled, an attempt's outcome is `unknown`, which may still be paid: the period
/// stays listed.
pub(crate) fn finished_step(view: Option<&ClaimView>) -> Option<u64> {
    match view {
        None => Some(0),
        Some(view) => {
            let unknown = view.attempts.iter().any(|a| a.outcome == Outcome::Unknown);
            match view.status {
                ClaimStatus::Attempting => None,
                ClaimStatus::Free if unknown => None,
                _ => Some(view.step),
            }
        }
    }
}

/// A payment the app makes by its own policy, without its user's confirmation, carries none
/// of the user's confirmations (Paykit spec 8.2, "Paying periods"): it is an automatic
/// payment for every rule that names one, so nothing the user would confirm lets it pass.
fn refuse_confirmations(options: &PayOptions) -> Result<()> {
    let confirmed = options.may_be_paid_confirmed.is_some()
        || options.transfer_confirmed.is_some()
        || options.unconfirmed_identity_confirmed.is_some()
        || options.possibly_cancelled_confirmed.is_some()
        || options.blocked_confirmed.is_some()
        || options.early_payment_confirmed.is_some()
        || matches!(options.mode, PaymentMode::Public(_));
    if confirmed {
        return Err(Refusal::Usage(
            "Paykit spec 8.2: a period paid without the user's confirmation carries none of \
             the user's confirmations"
                .into(),
        )
        .into());
    }
    Ok(())
}

/// The natural key of the period record of `request_hash`'s Billing Period `period`, the
/// request `payee`'s (Paykit wire 5.3).
pub(crate) fn period_key(payee: &[u8; 32], request_hash: &[u8; 32], period: &PeriodRef) -> Vec<u8> {
    natural_key::period_record(payee, request_hash, period.start_secs())
}

impl Paykit {
    /// The period records of `collection` this app holds that bind, by the request they
    /// belong to, each list in period order.
    pub(crate) async fn periods_of(
        &self,
        collection: Collection,
    ) -> Result<HashMap<[u8; 32], Vec<PeriodView>>> {
        let own = self.identity();
        let side = match collection {
            Collection::PayeeActivity => ActivitySide::Payee,
            _ => ActivitySide::Payer,
        };
        let mut out: HashMap<[u8; 32], Vec<PeriodView>> = HashMap::new();
        for held in self.app.records_of(collection.name()).await? {
            let (Some(value), Some(label)) =
                (held.content.value(), collection.policy_label(&held.policy))
            else {
                continue;
            };
            let Some(record) = crate::records::bind_activity(
                value,
                &own,
                side,
                label.as_ref(),
                &held.key,
                |t| self.app.record_key(t).ok(),
                &|p| self.rail.settlement_id(p),
            ) else {
                continue;
            };
            if let Some((request, view)) = period_view(held.key, &record) {
                out.entry(request).or_default().push(view);
            }
        }
        for views in out.values_mut() {
            views.sort_by_key(|v| v.billing_period.starts_at);
        }
        Ok(out)
    }

    /// The payee's request record a record at `target`, `bound`, belongs to: itself for a
    /// request record, the one of its request, under the same label, for a period record.
    pub(crate) async fn payee_request_record(
        &self,
        target: &ActivityTarget,
        bound: &ActivityRecord,
    ) -> Result<Option<ActivityRecord>> {
        if bound.request.is_some() {
            return Ok(Some(bound.clone()));
        }
        let Ok(RecordBinding::Period { request_hash, .. }) = bound.binding() else {
            return Ok(None);
        };
        let request = ActivityTarget::payee(
            natural_key::request_record(&self.identity(), &request_hash),
            target.label().as_ref(),
        );
        self.held_activity(&request).await
    }

    /// Pay one Billing Period, `period`, of the subscription `request` (Paykit spec 7.2,
    /// 8.2): the paying order of every payment, on the period's obligation, its proof naming
    /// the period and the period details it was paid by. Accepting a subscription pays
    /// nothing: the host calls this for each period. `by` says who decided it:
    ///
    /// - [`Initiator::User`]: the user confirmed paying this period, as [`Paykit::pay_request`]
    ///   pays a one-time request, with the same confirmations in `options`.
    /// - [`Initiator::Automatic`]: the app pays it by its own policy, without asking its user,
    ///   which spec 8.2 leaves to the app as v0.1 did. It is an automatic payment for every
    ///   rule that names one: the deny list read fresh before the attempt and again before
    ///   the dispatch, the clock bound, no doubtful claim, no request held after a restore,
    ///   no destination whose transfer isn't the amount requested, and no payment priced by
    ///   rates. `options` carries no user confirmation then. Its proof carries no
    ///   `allowance_id`: no Allowance is involved.
    ///
    /// The destination, by default: the latest unexpired period details the payer's period
    /// record holds that carry one (`options.period_details` names another the record
    /// holds), else the request's own, which serve only the first period when the request
    /// has `per_period_endpoints`. A period that hasn't started on the latest service time
    /// verified is paid only by the user, after confirming paying it early
    /// ([`PayOptions::with_early_payment_confirmed`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`], and: [`Refusal::Usage`] for a one-time request, a period
    /// that isn't one of the request's, or a payment by policy that carries a user's
    /// confirmation; [`Refusal::NotSubscribed`] before the subscription is accepted;
    /// [`Refusal::AlreadyPaid`] for a period whose record holds its proof;
    /// [`Refusal::PeriodNotStarted`]; [`Refusal::AttemptedPeriodsFull`];
    /// [`Refusal::NoDestination`] for a later period of a request with `per_period_endpoints`
    /// and no period details with its destination yet; [`Refusal::NotNow`] while the period
    /// record's value waits for its scope key. A payment by policy refuses what a user
    /// could confirm: [`Refusal::Blocked`], [`Refusal::HeldAfterRestore`],
    /// [`Refusal::MayBePaid`] and [`Refusal::Transfer`] can't be passed then.
    pub async fn pay_period(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        by: Initiator,
        options: PayOptions,
    ) -> Result<PayOutcome> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.pay_period_inner(request, period, by, options)).await
    }

    async fn pay_period_inner(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        by: Initiator,
        options: PayOptions,
    ) -> Result<PayOutcome> {
        let automatic = by == Initiator::Automatic;
        if automatic {
            refuse_confirmations(&options)?;
        }
        self.refresh_own()?;
        self.records_known().await?;
        self.require_pay()?;
        // For a payment by policy, the clock bound too (Paykit spec 7.2).
        self.app
            .may_attempt(automatic)
            .map_err(|refusal| Refusal::NotNow(refusal.to_string()))?;
        self.refuse_while_payer_pass_owed().await?;
        let (target, record) = self.payer_request(request).await?;
        let (stored, body) = request_of(&record).ok_or(Refusal::NotFound)?;
        let (stored, body) = (stored.clone(), body.clone());
        if !body.is_recurring() {
            return Err(Refusal::Usage(
                "Paykit spec 8.1: a one-time request is paid with pay_request".into(),
            )
            .into());
        }
        let billing = body.named_period(period).map_err(|_| {
            Refusal::Usage("Paykit wire 4.4: not a Billing Period of the request".into())
        })?;
        let period = PeriodRef::from(billing);
        if rejected(&record) {
            return Err(Refusal::Rejected.into());
        }
        // A cancellation either side recorded, or a payee's the app keeps, stops every
        // period (Paykit spec 8.2, "Ending").
        if cancelled(&record) || self.kept_stop(&target).await? {
            return Err(Refusal::Cancelled.into());
        }
        if record.response.is_none() {
            return Err(Refusal::NotSubscribed.into());
        }
        let period_target =
            ActivityTarget::payer(period_key(&request.payee, &request.request_hash, &period));
        if let Some(held) = self
            .app
            .shared_record(&self.activity_key(&period_target)?)
            .await?
        {
            if crate::hold::awaits_key(&held.content) {
                return Err(crate::hold::value_awaits_key());
            }
        }
        let held_period = self.held_activity(&period_target).await?;
        // The record shows it paid (Paykit spec 7.2).
        if held_period.as_ref().is_some_and(ActivityRecord::shows_paid) {
            return Err(Refusal::AlreadyPaid.into());
        }
        let shown = BlockedPayment {
            payee: request.payee,
            request: Some(*request),
            period: Some(period),
            amount: body.amount.clone(),
        };
        let end_hold = if automatic {
            // No automatic payment while the request is held, nor to a blocked payee on the
            // deny list read now; a read that fails counts as blocked (Paykit spec 7.2, 8.1).
            if self.held(&self.activity_key(&target)?, &record).await? {
                return Err(Refusal::HeldAfterRestore.into());
            }
            if self.denied(&request.payee).await? {
                return Err(Refusal::Blocked {
                    payment: Box::new(shown),
                }
                .into());
            }
            None
        } else {
            self.user_gates(&target, &record, shown, &options).await?
        };
        let obligation =
            self.obligation(&request.payee, &request.request_hash, period.start_secs());
        let deadline = body.attempt_deadline(Some(&billing))?;
        let restored = self.restored_attempts(&obligation).await?;
        let step = self
            .claim_step(&obligation, deadline, restored, &options)
            .await?;
        if self.deadline_passed(deadline) {
            return Err(self.past_deadline(&target, None).await?);
        }
        let not_before = self.not_before(&period, &options)?;
        let request_write = self
            .attempted_write(&target, &record, request, period.start_secs())
            .await?;
        let (destination, carrier, reserve, bound) = self
            .period_destination(
                &request.payee,
                &stored,
                &body,
                &billing,
                held_period.as_ref(),
                &options,
            )
            .await?;
        let entry = JournalEntry {
            attempt_id: AttemptId::generate()?.to_text(),
            obligation,
            payee: request.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request.request_hash)),
            natural_key: period_target.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            destination,
            deadline,
            stage: Stage::Recorded,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            acceptance_cert: None,
            acceptance_released: false,
            rail_proof: None,
            paid_at_ms: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            request: Some(KeptMessage::of(&stored)),
            record_held: false,
            transfer_bound: Some(bound),
            failed_on_claim: false,
            billing_period: Some(KeptPeriod::of(&period)),
        };
        self.execute(Plan {
            entry,
            record: target,
            carrier,
            step,
            reserve,
            create: None,
            on_record: true,
            conditions: vec![not_before],
            request_write,
            by,
            unconfirmed: options.unconfirmed_identity_confirmed,
            end_hold,
        })
        .await
    }

    /// The attempt's `server_time_in(s, null)` on a Billing Period (Paykit spec 8.2, "Not
    /// before its start"): `s` the period's start once the latest service time verified is
    /// at or past it; before it, the period is paid only by the user who confirmed paying
    /// it early, and `s` is that latest service time, so the service never applies the
    /// attempt before the user agreed. The claim was read just before, so the time is
    /// current.
    fn not_before(&self, period: &PeriodRef, options: &PayOptions) -> Result<Condition> {
        let seen = self.service_time();
        let start = match seen {
            Some(t) if t >= period.starts_at => period.starts_at,
            Some(t) if options.early_payment_confirmed.is_some() => t,
            _ => return Err(Refusal::PeriodNotStarted.into()),
        };
        Ok(Condition::ServerTimeIn {
            start: Some(start),
            end: None,
        })
    }

    /// The request record's write in the batch of an attempt on the Billing Period starting
    /// at `start`, when `record`, at `target`, doesn't list it in `attempted_periods`
    /// (Paykit spec 8.2, Paykit wire 5.3): the period added at the record's next version,
    /// and every listed period whose claim, read now, has no open attempt dropped, each
    /// with a `claim_step` condition at the step read, so an attempt on it meanwhile fails
    /// the batch. `None` for a period listed already, whose attempt writes no record.
    ///
    /// # Errors
    ///
    /// [`Refusal::AttemptedPeriodsFull`] with 16 listed and none finished; the client's
    /// errors.
    async fn attempted_write(
        &mut self,
        target: &ActivityTarget,
        record: &ActivityRecord,
        request: &RequestRef,
        start: u64,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        if record.attempted_periods.contains(&start) {
            return Ok(None);
        }
        let (finished, conditions) = self.finished_periods(record, request).await?;
        let list = attempted_with(&record.attempted_periods, start, &finished)
            .ok_or(Refusal::AttemptedPeriodsFull)?;
        let mut next = record.clone();
        next.attempted_periods = list;
        let value = next.to_json()?;
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&value), conditions)
                .await?,
        ))
    }

    /// The periods `record` lists in `attempted_periods` whose claims, read now, have no
    /// open attempt, with the `claim_step` condition of each at the step read (Paykit wire
    /// 5.3).
    pub(crate) async fn finished_periods(
        &mut self,
        record: &ActivityRecord,
        request: &RequestRef,
    ) -> Result<(Vec<u64>, Vec<Condition>)> {
        let mut finished = Vec::new();
        let mut conditions = Vec::new();
        for listed in &record.attempted_periods {
            let claim_key = self.obligation(&request.payee, &request.request_hash, *listed);
            let view = self.app.read_claim(&claim_key).await?.map(|read| read.view);
            if let Some(step) = finished_step(view.as_ref()) {
                finished.push(*listed);
                conditions.push(Condition::ClaimStep { claim_key, step });
            }
        }
        Ok((finished, conditions))
    }

    /// The destination of a payment of Billing Period `billing` of the request `stored`,
    /// `body` (Paykit spec 8.2): period details the payer's period record, `held`, holds,
    /// which is the only place a payer pays details from, so a proof names only details its
    /// record can store (Paykit wire 5.3): the ones `options` names by `msg_id`, or by
    /// default the latest unexpired ones that carry a destination. Without such details,
    /// the request's own destination, which serves only the first period of a request with
    /// `per_period_endpoints`: a later one waits for period details with its destination.
    async fn period_destination(
        &mut self,
        payee: &[u8; 32],
        stored: &StoredMessage,
        body: &PaymentRequest,
        billing: &BillingPeriod,
        held: Option<&ActivityRecord>,
        options: &PayOptions,
    ) -> Result<Chosen> {
        let now = self.now();
        let mut details: Vec<StoredMessage> = held
            .map(|record| record.details().into_iter().cloned().collect())
            .unwrap_or_default();
        details.sort_by(|a, b| period_details_order(b.message(), a.message()));
        let usable = |d: &StoredMessage| {
            matches!(d.body(), Body::PaymentPeriodDetails(p)
                if p.payment_endpoints.is_some() && now < p.expires_at)
        };
        let chosen = match &options.period_details {
            Some(msg_id) => Some(
                details
                    .into_iter()
                    .find(|d| &d.message().msg_id == msg_id)
                    .filter(|d| usable(d))
                    .ok_or_else(|| {
                        Refusal::NoDestination(
                            "Paykit spec 8.2: the period record holds no such unexpired period \
                             details with a destination"
                                .into(),
                        )
                    })?,
            ),
            None => details.into_iter().find(|d| usable(d)),
        };
        if let Some(details) = chosen {
            let Body::PaymentPeriodDetails(period) = details.body() else {
                return Err(unsupported("the period details' endpoints"));
            };
            let allowed = |id: &PaymentEndpointIdentifier| {
                body.accepted_payment_endpoint_identifiers
                    .as_ref()
                    .is_none_or(|ids| ids.contains(id))
                    && options.endpoint.as_ref().is_none_or(|want| want == id)
            };
            let gathered: Vec<Choice> = period
                .payment_endpoints
                .iter()
                .flatten()
                .filter(|(id, _)| allowed(id))
                .map(|(id, payload)| {
                    let destination = Destination::new(
                        details.message().from.app_id,
                        id,
                        payload,
                        DestinationSource::Request,
                        None,
                        false,
                    );
                    let carrier = Carrier::Message(Box::new(details.clone()));
                    Choice::new(id, payload, destination, carrier, None)
                })
                .collect();
            if gathered.is_empty() {
                return Err(Refusal::NoDestination(
                    "Paykit spec 8.2: the period details have no such endpoint".into(),
                )
                .into());
            }
            return self
                .choose(payee, gathered, &body.amount, options)?
                .ok_or_else(|| unsupported("the period details' endpoints"));
        }
        let first = body
            .recurrence
            .as_ref()
            .and_then(|r| r.first().ok())
            .is_some_and(|first| first == *billing);
        if body.per_period_endpoints && !first {
            return Err(Refusal::NoDestination(
                "Paykit spec 8.2: no period details with this Billing Period's destination yet"
                    .into(),
            )
            .into());
        }
        self.request_destination(payee, stored, body, options).await
    }

    /// The payee sends the payer period details for one Billing Period of the recurring
    /// request whose `payee_activity` record is `record` (Paykit spec 8.2, Paykit wire 4.9):
    /// the period's destination, its conversion rates, or both, valid until
    /// `details.expires_at`. They are written into the payee's period record in the batch
    /// that queues them, conditioned on the version read, and sent once it is applied; a
    /// record that changed meanwhile is read again and the write built again. Each details
    /// stays valid until its own expiry: a later one replaces none. Returns the `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::NotFound`] for a
    /// record this app doesn't hold, or a period record; [`Refusal::Usage`] for a one-time
    /// request, or a period that holds 8 details already; [`Refusal::NotSubscribed`] before
    /// the payer's acceptance; [`Refusal::Rejected`] or [`Refusal::Cancelled`];
    /// [`crate::PaykitError::Lib`] with `Validation` for details that break Paykit wire 4.9 against
    /// the request, such as a period that isn't one of its own, or that expired by the
    /// device clock; [`Refusal::NotNow`] while the period record's value waits for its scope
    /// key; [`crate::PaykitError::Conflict`] when the record kept changing; the client's errors.
    pub async fn send_period_details(
        &mut self,
        record: &[u8; 32],
        details: PeriodDetails,
    ) -> Result<String> {
        self.refresh_own()?;
        self.records_known().await?;
        self.require_payee()?;
        let mut tries = 0;
        let msg_id = loop {
            let (target, bound) = self.payee_record(record).await?;
            let (stored, request) = request_of(&bound).ok_or(Refusal::NotFound)?;
            if !request.is_recurring() {
                return Err(Refusal::Usage(
                    "Paykit spec 8.2: only a recurring request has period details".into(),
                )
                .into());
            }
            if rejected(&bound) {
                return Err(Refusal::Rejected.into());
            }
            if cancelled(&bound) {
                return Err(Refusal::Cancelled.into());
            }
            if bound.response.is_none() {
                return Err(Refusal::NotSubscribed.into());
            }
            let body = details.to_body_for(request)?;
            if details.expires_at <= self.now() {
                return Err(paykit_lib::Error::Validation(
                    "Paykit wire 4.9: period details that expired".into(),
                )
                .into());
            }
            let request_hash = stored.hash();
            let period = ActivityTarget::payee(
                period_key(&self.identity(), &request_hash, &details.billing_period),
                target.label().as_ref(),
            );
            let key = self.activity_key(&period)?;
            if let Some(held) = self.app.shared_record(&key).await? {
                if crate::hold::awaits_key(&held.content) {
                    return Err(crate::hold::value_awaits_key());
                }
            }
            let held = self.held_activity(&period).await?;
            if held.is_some_and(|r| r.details().len() >= MAX_PERIOD_DETAILS) {
                return Err(Refusal::Usage(
                    "Paykit spec 8.2: a Billing Period has at most 8 period details".into(),
                )
                .into());
            }
            let signed = self.sign(
                bound.counterparty,
                Kind::PaymentPeriodDetails,
                vec![request_hash],
                body,
            )?;
            match self
                .send_with_record(signed, &period, Role::PeriodDetails, Vec::new(), Vec::new())
                .await
            {
                Ok(msg_id) => break msg_id,
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        self.app.flush_message(&msg_id).await?;
        Ok(msg_id)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::activity::{empty, place};
    use crate::testkit::{object, Party};

    fn details(bob: &Party, alice: &Party, request: [u8; 32], expires: &str) -> StoredMessage {
        bob.message(
            alice,
            "paykit.payment_period_details",
            vec![request],
            object(json!({
                "billing_period": {
                    "starts_at": "2026-02-28T00:00:00.000Z",
                    "ends_at": "2026-03-31T00:00:00.000Z"
                },
                "expires_at": expires,
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" }
            })),
        )
    }

    fn parties() -> (Party, Party) {
        (
            Party::new("alice", "wallet", &["pay", "contacts"]),
            Party::new("bob", "shop", &["request"]),
        )
    }

    #[test]
    fn test_periods_details_latest_held_first_none_replaced() {
        let (alice, bob) = parties();
        let mut record = empty(ActivitySide::Payer, bob.id());
        let first = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
        let second = details(&bob, &alice, [7; 32], "2026-03-02T00:00:00.000Z");
        let (earlier, later) = if period_details_order(first.message(), second.message())
            == std::cmp::Ordering::Less
        {
            (first, second)
        } else {
            (second, first)
        };
        assert!(place(&mut record, Role::PeriodDetails, later.clone()));
        assert!(place(&mut record, Role::PeriodDetails, earlier.clone()));
        assert_eq!(record.period_details.as_ref().unwrap().hash(), later.hash());
        assert_eq!(record.earlier_details.len(), 1);
        assert_eq!(record.earlier_details[0].hash(), earlier.hash());
        assert_eq!(record.request_hash, Some([7; 32]));
        let json = record.to_json().unwrap();
        let read = ActivityRecord::read(&json, &alice.id(), ActivitySide::Payer).unwrap();
        let (request, view) = period_view([1; 32], &read).unwrap();
        assert_eq!(request, [7; 32]);
        assert_eq!(view.details[0].msg_id, later.message().msg_id);
        assert_eq!(view.details.len(), 2);
        // The same details again change nothing.
        assert!(!place(&mut record, Role::PeriodDetails, later));
    }

    #[test]
    fn test_periods_details_past_eight_stored_no_more() {
        let (alice, bob) = parties();
        let mut record = empty(ActivitySide::Payer, bob.id());
        for _ in 0..MAX_PERIOD_DETAILS {
            let d = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
            assert!(place(&mut record, Role::PeriodDetails, d));
        }
        assert_eq!(record.details().len(), MAX_PERIOD_DETAILS);
        let ninth = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
        assert!(place(&mut record, Role::PeriodDetails, ninth.clone()));
        assert_eq!(record.details().len(), MAX_PERIOD_DETAILS);
        assert!(record.details().iter().all(|d| d.hash() != ninth.hash()));
        // Processed, not stored: only reflected.
        assert!(record.reflects(&ninth.message().msg_id));
        record.to_json().unwrap();
    }

    #[test]
    fn test_periods_details_as_late_with_another_hash_leave_the_held_ones() {
        // Two details with one `created_at` and `msg_id` but other bodies: an equivocation,
        // the held ones staying (Paykit wire 4.9).
        let (alice, bob) = parties();
        let held = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
        let mut message = pubky_switch_core::message::Message::new(
            "paykit.payment_period_details",
            pubky_switch_core::message::Sender::of_app(bob.id(), &bob.keys),
            alice.id(),
            held.message().created_at_ms,
            vec![[7; 32]],
            held.message().body.clone(),
        )
        .unwrap();
        message.msg_id = held.message().msg_id.clone();
        message.body.insert(
            "expires_at".into(),
            serde_json::Value::String("2026-03-09T00:00:00.000Z".into()),
        );
        let plaintext = message.sign(&bob.keys).unwrap();
        let other =
            StoredMessage::new(bob.id(), bob.cert.signed(), plaintext, None, &alice.id()).unwrap();
        assert_ne!(other.hash(), held.hash());
        let mut record = empty(ActivitySide::Payer, bob.id());
        assert!(place(&mut record, Role::PeriodDetails, held.clone()));
        place(&mut record, Role::PeriodDetails, other);
        assert_eq!(record.details().len(), 1);
        assert_eq!(record.period_details.as_ref().unwrap().hash(), held.hash());
        record.to_json().unwrap();
    }

    #[test]
    fn test_periods_named_period_of_the_request_only() {
        let request = PaymentRequest::from_body(&object(json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "plan-1",
            "proposal_expires_at": null,
            "recurrence": {
                "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
            }
        })))
        .unwrap();
        let (alice, bob) = parties();
        let good = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
        assert_eq!(
            named_period(&request, &good).unwrap().unwrap().start_secs(),
            1_772_236_800
        );
        let other = bob.message(
            &alice,
            "paykit.payment_period_details",
            vec![[7; 32]],
            object(json!({
                "billing_period": {
                    "starts_at": "2026-02-27T00:00:00.000Z",
                    "ends_at": "2026-03-31T00:00:00.000Z"
                },
                "expires_at": "2026-03-01T00:00:00.000Z",
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" }
            })),
        );
        assert!(named_period(&request, &other).is_err());
    }
}
