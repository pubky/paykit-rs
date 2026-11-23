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
//!   (Paykit wire 5.3 "Binding"). The payer stores details only for a subscription it
//!   accepted and that runs, and only for a period not shown paid that is the next to
//!   start, or one started whose payment deadline hasn't passed, or, without one, one of the
//!   16 latest to have started ([`stores_details`]); the payee sends them only for such a
//!   period.
//! - **Views:** a request's view lists the period records the app holds for it
//!   ([`PeriodView`]), each with its details, latest first.

use std::collections::HashMap;

use paykit_lib::bodies::{period_details_order, PaymentRequest, PeriodDetails};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::{BillingPeriod, PeriodRef};
use paykit_lib::records::{
    ActivityRecord, ActivitySide, RecordBinding, StoredMessage, MAX_ATTEMPTED_PERIODS,
    MAX_PERIOD_DETAILS,
};
use paykit_lib::{AttemptId, Body, PaymentEndpointIdentifier, Rate};
use pubky_switch_client::app::{Initiator, KeptReason, RecordContent, RecordRead, SharedRecord};
use pubky_switch_core::claims::{ClaimStatus, ClaimView, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind};

use crate::activity::{ActivityTarget, Role};
use crate::app_records::PublicAppFailure;
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::{JournalEntry, KeptAmount, KeptPeriod, Stage};
use crate::pay::{BlockedPayment, Chosen, PayOptions, PayResult, Plan, Pricing};
use crate::paykit::Paykit;
use crate::receipts::{period_request_target, received_receipt, ReceivedReceipt};
use crate::records::{conflict, KeptMessage};
use crate::requests::{
    accepted, cancelled, payment_ended, payment_history_lost, rejected, request_of,
    MessageDelivery, RequestRef, SentMessage,
};

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

/// What the host authorizes for this own-policy Billing Period (Paykit spec 7.2, 8.2).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeriodPrice<'a> {
    /// Pay an eligible endpoint in the requested asset at the requested amount.
    /// A destination priced by a signed rate still needs its own authorization.
    RequestedAmount,
    /// Return the signed rate quote for this endpoint, or a refusal. This choice
    /// never opens an attempt, pays a plain endpoint or asks for a Private Payment List.
    QuoteFor(PaymentEndpointIdentifier),
    /// Pay only this exact quote, authorized by the user or their bounded policy.
    AtRate(&'a crate::PricedPayment),
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
    /// Every period details the record holds that is valid against its request, latest
    /// first (Paykit wire 4.9, 5.3): the latest is the payer's default, and each stays valid
    /// until its own expiry. Details invalid against the request aren't shown.
    pub details: Vec<PeriodDetailsView>,
    /// The record holds a proof of the period's payment, credited or not.
    pub paid: bool,
    /// The payee issued a receipt for this period. The payee shows the credit
    /// once it holds the signed proof the receipt confirms. The payer can see
    /// the payee's receipt before it holds the proof (Paykit wire 4.11).
    pub credited: bool,
    /// The receipt shown for the credit.
    pub receipt: Option<ReceivedReceipt>,
}

/// The period details `record` holds that are valid against `request`, the request of its
/// Billing Period (Paykit wire 4.9, 5.3). A reader of a period record ignores the others, for
/// any reason of wire 4.9, such as rates under a request without `per_period` conversion
/// terms, or a sender the request's label doesn't let send them (Paykit spec 3.4): nothing
/// shows them or pays by them.
pub(crate) fn valid_details<'a>(
    record: &'a ActivityRecord,
    request: &PaymentRequest,
) -> Vec<&'a StoredMessage> {
    record
        .details()
        .into_iter()
        .filter(|stored| match stored.body() {
            Body::PaymentPeriodDetails(details) => {
                details.check_against(request).is_ok() && stored.check_payee_sender(request).is_ok()
            }
            _ => false,
        })
        .collect()
}

/// A request's endpoints apply to every occurrence or just the first (Paykit spec 8.2).
pub(crate) fn needs_period_destination(
    request: &PaymentRequest,
    period: PeriodRef,
) -> Result<bool> {
    let first = request
        .recurrence
        .as_ref()
        .ok_or_else(PaykitError::corrupt)?
        .first()?;
    Ok(request.per_period_endpoints && period != PeriodRef::from(first))
}

/// A later destination must be supplied by the app named in the original request.
pub(crate) fn destination_app_permitted(request: &PaymentRequest, app: [u8; 16]) -> bool {
    !matches!(&request.destination, paykit_lib::bodies::RequestDestination::RequiredApp(required) if *required != app)
}

/// `error`, the refusal of a period's last destination, with the note, when
/// `device_clock_behind`, that eligible period details expired only by the service's time,
/// so their destination wasn't tried (Paykit spec 7.2).
fn with_clock_note(error: PaykitError, device_clock_behind: bool) -> PaykitError {
    const NOTE: &str = "; details expired by the service's time: the device clock is behind it";
    match error.refusal() {
        Some(Refusal::NoDestination(why)) if device_clock_behind => {
            Refusal::NoDestination(format!("{why}{NOTE}")).into()
        }
        Some(Refusal::UnsupportedEndpoint(why)) if device_clock_behind => {
            Refusal::UnsupportedEndpoint(format!("{why}{NOTE}")).into()
        }
        _ => error,
    }
}

/// The signed rate interval of period details (Paykit wire 4.9), without a payer or
/// payee time policy. Callers first validate the details against their request and use
/// the interval only when the selected payment is priced by these rates.
pub(crate) fn rate_window(details: &StoredMessage) -> Option<(u64, u64)> {
    let Body::PaymentPeriodDetails(body) = details.body() else {
        return None;
    };
    Some((
        paykit_lib::periods::rates_start(details.message().created_at_ms),
        body.expires_at,
    ))
}

/// Whether both payer clocks are inside the signed rate interval (Paykit wire 4.9).
pub(crate) fn rates_current(details: &StoredMessage, device: u64, service: Option<u64>) -> bool {
    rate_window(details).is_some_and(|(start, end)| {
        service
            .is_some_and(|time| paykit_lib::periods::payer_may_use_rates(start, end, device, time))
    })
}

/// A requested payment must belong to the signed request and its payer. A bound
/// activity already requires all its signed messages to name the same occurrence;
/// the record key alone identifies only the start of a Billing Period (Paykit wire 4.4, 5.3).
pub(crate) fn request_for_payment<'a>(
    parent: &'a ActivityRecord,
    record: &ActivityRecord,
    binding: &RecordBinding,
) -> Option<(&'a StoredMessage, &'a PaymentRequest)> {
    let (signed, request) = request_of(parent)?;
    let hash = match binding {
        RecordBinding::Request { request_hash } if !request.is_recurring() => request_hash,
        RecordBinding::Period {
            request_hash,
            billing_period,
        } => {
            request.named_period(billing_period).ok()?;
            request_hash
        }
        _ => return None,
    };
    (parent.counterparty == record.counterparty && signed.hash() == *hash)
        .then_some((signed, request))
}

/// The view of a period record, `record` held at `key`, of a Billing Period of `request`;
/// `None` for a record of another kind. It shows only the details valid against `request`
/// ([`valid_details`]).
fn period_view(
    key: [u8; 32],
    record: &ActivityRecord,
    request: &PaymentRequest,
) -> Option<PeriodView> {
    let Ok(RecordBinding::Period { billing_period, .. }) = record.binding() else {
        return None;
    };
    let mut held = valid_details(record, request);
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
    // The request's reference stays on the request's view.
    let receipt = received_receipt(key, record, Some(request)).map(|receipt| ReceivedReceipt {
        payment_reference: None,
        ..receipt
    });
    Some(PeriodView {
        record: key,
        billing_period,
        details,
        paid: record.shows_paid(),
        credited: receipt.is_some(),
        receipt,
    })
}

/// The period records that belong to the signed request in `parent`, in period order.
pub(crate) fn period_views(
    held: Vec<([u8; 32], ActivityRecord)>,
    parent: &ActivityRecord,
) -> Vec<PeriodView> {
    let mut views: Vec<PeriodView> = held
        .iter()
        .filter_map(|(key, record)| {
            let (_, request) = request_for_payment(parent, record, &record.binding().ok()?)?;
            period_view(*key, record, request)
        })
        .collect();
    views.sort_by_key(|v| v.billing_period.starts_at);
    views
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

/// How many of the latest Billing Periods to have started take period details on a request
/// without a payment deadline (Paykit spec 8.2): a payer that fell behind can still pay an
/// earlier one.
pub(crate) const STARTED_PERIODS_WITH_DETAILS: i64 = 16;

/// Whether a Billing Period takes period details now ([`takes_details`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Window {
    /// It does: the payee sends them, and the payer stores them.
    Takes,
    /// It doesn't: the payee refuses them, and the payer takes them with no effect.
    Closed,
    /// No service time is verified yet, so which periods have started can't be told: the
    /// payee refuses them for now, and the payer holds them until a time is verified, never
    /// taking them with no effect on a time it hasn't verified.
    NoTime,
}

/// Whether the payer stores the period details `stored` for the request `record` holds
/// (Paykit spec 8.2), `period` the payer's period record of the period they name, if it
/// reads and binds: the one rule every path that stores them follows: at receive, once a
/// request record comes for details kept before it, and when a write the switch refused is
/// made later.
///
/// - **A subscription running:** only for a request the payer accepted, as v0.1 holds
///   details before acceptance invalid, and that neither side rejected or cancelled, nor
///   is `stopped` by a payee's cancellation the app keeps and hasn't placed yet (Paykit
///   spec 8.1, the kept-copy stop): a subscription that runs no more has no periods to pay.
/// - **A period that takes details** on `t`, the highest service time verified
///   ([`takes_details`]). So a payee can make the payer hold period records only for
///   periods it may still pay.
///
/// # Errors
///
/// As [`takes_details`].
pub(crate) fn stores_details(
    record: &ActivityRecord,
    period: Option<&ActivityRecord>,
    stored: &StoredMessage,
    t: Option<u64>,
    stopped: bool,
) -> std::result::Result<Window, paykit_lib::Error> {
    if record.response.is_none() || rejected(record) || cancelled(record) || stopped {
        return Ok(Window::Closed);
    }
    let Some((_, request)) = request_of(record) else {
        return Ok(Window::Closed);
    };
    let Some(Ok(named)) = stored
        .body()
        .billing_period()
        .map(|named| request.named_period(named))
    else {
        return Ok(Window::Closed);
    };
    takes_details(
        request,
        &named,
        period.is_some_and(ActivityRecord::shows_paid),
        t,
    )
}

/// Whether `period` of the recurring `request` takes period details on `t`, the highest
/// service time verified (Paykit spec 8.2): a period not shown `paid` that is the next to
/// start, or one that has started and, with a payment deadline, whose deadline hasn't
/// passed, or, without one, is one of the [`STARTED_PERIODS_WITH_DETAILS`] latest to have
/// started. The deadline is the last instant an attempt on the period is taken: the
/// attempt's `deadline` is the millisecond after it ([`PaymentRequest::attempt_deadline`],
/// Paykit spec 7.1), so its own millisecond takes details. The payee sends details only for
/// such a period, and the payer stores them only for one, by this same rule. With no time
/// verified yet, which periods have started can't be told, as for every time the SDK judges
/// on the service's clock, never on the device's: [`Window::NoTime`], for a period not
/// shown paid.
///
/// # Errors
///
/// [`paykit_lib::Error::Invalid`] for a payment deadline past 64 bits, which only a
/// caller's own value holds ([`PaymentRequest::attempt_deadline`]).
pub(crate) fn takes_details(
    request: &PaymentRequest,
    period: &BillingPeriod,
    paid: bool,
    t: Option<u64>,
) -> std::result::Result<Window, paykit_lib::Error> {
    let Some(recurrence) = request.recurrence.as_ref() else {
        return Ok(Window::Closed);
    };
    let Ok(first) = recurrence.first() else {
        return Ok(Window::Closed);
    };
    if paid {
        return Ok(Window::Closed);
    }
    let Some(t) = t else {
        return Ok(Window::NoTime);
    };
    let open = |takes: bool| {
        if takes {
            Window::Takes
        } else {
            Window::Closed
        }
    };
    // The latest period to have started, the request's last once it has ended; none
    // before the first starts.
    let last = recurrence.last().ok().flatten().map(|last| last.index);
    let latest = recurrence
        .rule()
        .index_at(t)
        .ok()
        .map(|k| last.map_or(k, |last| k.min(last)))
        .filter(|k| *k >= first.index);
    let Some(k) = latest else {
        return Ok(open(period.index == first.index));
    };
    if Some(period.index) == k.checked_add(1) {
        return Ok(Window::Takes);
    }
    if period.index > k {
        return Ok(Window::Closed);
    }
    Ok(open(match request.attempt_deadline(Some(period))? {
        Some(deadline) => t < deadline,
        None => k.saturating_sub(period.index) < STARTED_PERIODS_WITH_DETAILS,
    }))
}

/// The natural key of the period record of `request_hash`'s Billing Period `period`, the
/// request `payee`'s (Paykit wire 5.3).
pub(crate) fn period_key(payee: &[u8; 32], request_hash: &[u8; 32], period: &PeriodRef) -> Vec<u8> {
    natural_key::period_record(payee, request_hash, period.start_secs())
}

/// The refusal of a write over a period record held as a value the app can't read: not now,
/// never written over (Paykit spec 8.1).
fn period_record_unread() -> PaykitError {
    Refusal::NotNow("Paykit wire 5.3: the period record held doesn't read".into()).into()
}

/// Period records with their keys, by the key of the request record they belong to.
pub(crate) type PeriodsByRequest = HashMap<[u8; 32], Vec<([u8; 32], ActivityRecord)>>;

impl Paykit {
    /// Open against the held write, or check the service before creating a period at zero.
    async fn period_payment_base(
        &mut self,
        target: &ActivityTarget,
    ) -> Result<Option<ActivityRecord>> {
        if let Some(held) = self.app.shared_record(&self.activity_key(target)?).await? {
            return self.checked_period_activity(target, &held).map(Some);
        }
        Box::pin(self.current_period_activity(target, None)).await
    }

    /// Recheck the parent's and period's payment restrictions before calling the method.
    /// A newer service view cannot clear an unconfirmed divergence the app already saw.
    pub(crate) async fn current_period_activity(
        &mut self,
        target: &ActivityTarget,
        crossing_payee: Option<[u8; 32]>,
    ) -> Result<Option<ActivityRecord>> {
        if let Some(held) = self.app.shared_record(&self.activity_key(target)?).await? {
            let record = self.checked_period_activity(target, &held)?;
            self.pause_of(&held).await?;
            if payment_ended(&record, crossing_payee) {
                return Ok(Some(record));
            }
        }
        let held = match Box::pin(self.app.read_record_fresh(&target.record_target())).await? {
            RecordRead::Fresh(held) => held,
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record: None,
            } => return Ok(None),
            RecordRead::Kept {
                reason: KeptReason::Unreachable(why),
                ..
            } => return Err(Refusal::SwitchUnavailable(why).into()),
            RecordRead::Kept {
                record: Some(held), ..
            } if held.content == RecordContent::Deleted => {
                return Err(payment_history_lost().into())
            }
            _ => return Err(period_record_unread()),
        };
        self.checked_period_activity(target, &held).map(Some)
    }

    /// A delete can hide the only paid proof after the claim expires (Paykit spec 6).
    fn checked_period_activity(
        &self,
        target: &ActivityTarget,
        held: &SharedRecord,
    ) -> Result<ActivityRecord> {
        if held.content == RecordContent::Deleted {
            return Err(payment_history_lost().into());
        }
        if crate::hold::awaits_key(&held.content) {
            return Err(crate::hold::value_awaits_key());
        }
        if held.flagged || held.not_taken || held.different_write_at_held_version {
            return Err(period_record_unread());
        }
        let record = self
            .read_activity(target, Some(held))
            .ok_or_else(period_record_unread)?;
        if record.lost_to_delete.is_some() {
            return Err(payment_history_lost().into());
        }
        Ok(record)
    }

    /// The period records of `collection` this app holds that bind, with their keys, by the
    /// request they belong to; [`period_views`] shows them.
    pub(crate) async fn periods_of(&self, collection: Collection) -> Result<PeriodsByRequest> {
        let side = match collection {
            Collection::PayeeActivity => ActivitySide::Payee,
            _ => ActivitySide::Payer,
        };
        let mut bound = Vec::new();
        for held in self.app.records_of(collection.name()).await? {
            let Some(label) = collection.policy_label(&held.policy) else {
                continue;
            };
            if let Some(record) = self.bind_held(&held, side, label.as_ref()) {
                bound.push((held.key, label, record));
            }
        }
        self.group_periods(bound)
    }

    /// The period records among `bound`, records each bound at its key under the label its
    /// policy gives it, by the request record they belong to ([`Paykit::periods_of`]).
    pub(crate) fn group_periods(
        &self,
        bound: impl IntoIterator<Item = ([u8; 32], Option<paykit_lib::Label>, ActivityRecord)>,
    ) -> Result<PeriodsByRequest> {
        let own = self.identity();
        let mut out = PeriodsByRequest::new();
        for (key, label, record) in bound {
            if let Some(parent) = period_request_target(&record, &own, label.as_ref()) {
                out.entry(self.activity_key(&parent)?)
                    .or_default()
                    .push((key, record));
            }
        }
        Ok(out)
    }

    /// The parent of a billed period, using the request hash in its signed messages.
    pub(crate) fn payee_period_parent(
        &self,
        target: &ActivityTarget,
        bound: &ActivityRecord,
    ) -> Option<ActivityTarget> {
        period_request_target(bound, &self.identity(), target.label().as_ref())
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
        let Some(request) = self.payee_period_parent(target, bound) else {
            return Ok(None);
        };
        let held = self.credit_input(&self.activity_key(&request)?).await?;
        Ok(self.read_activity(&request, held.as_ref()))
    }

    /// Whether the app holds, though it may not open or bind it, the payee's request record
    /// a period record `bound` at `target` belongs to: any content but a delete.
    pub(crate) async fn request_record_held(
        &self,
        target: &ActivityTarget,
        bound: &ActivityRecord,
    ) -> Result<bool> {
        let Some(request) = self.payee_period_parent(target, bound) else {
            return Ok(false);
        };
        Ok(self
            .app
            .shared_record(&self.activity_key(&request)?)
            .await?
            .is_some_and(|held| held.content != RecordContent::Deleted))
    }

    /// The payee sends the payer period details for one Billing Period of the recurring
    /// request whose `payee_activity` record is `record` (Paykit spec 8.2, Paykit wire 4.9):
    /// the period's destination, its conversion rates, or both, valid until
    /// `details.expires_at`. They are written into the payee's period record in the batch
    /// that queues them, conditioned on the version read, and sent once it is applied; a
    /// record that changed meanwhile is read again and the write built again. The request
    /// record and the period record are read again first, so the window is judged on the
    /// service's time now and nothing is written over a write the app hasn't read. Each details
    /// stays valid until its own expiry: a later one replaces none. They are sent only for
    /// a period that takes details, the periods the payer stores them for
    /// (Paykit spec 8.2): one not shown paid that is the next to start, or one started
    /// whose payment deadline hasn't passed, or, for a request without one, one of the 16
    /// latest to have started, on the highest service time verified. Nothing is signed,
    /// written or queued while sends to the payer are held: at an app holding `paykit_contacts`,
    /// while the copy of the deny list holds the payer, or while that copy isn't known, which
    /// a read of the lists is tried for first (Paykit spec 8.1, "Blocked counterparties"), as
    /// a new request is held. An app without `paykit_contacts` reads no lists, and holds nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `paykit_request` or a label; [`Refusal::NotFound`] for a
    /// record this app doesn't hold, or a period record; [`Refusal::NotNow`] while sends to
    /// the payer are held, before anything else is judged, with nothing written or queued;
    /// [`Refusal::Usage`] for a one-time
    /// request, or a period that holds 8 details already; [`Refusal::NotSubscribed`] before
    /// the payer's acceptance; [`Refusal::Rejected`] or [`Refusal::Cancelled`];
    /// [`Refusal::AlreadyPaid`] for a period whose record shows it paid;
    /// [`crate::PaykitError::Lib`] with `Validation` for details that break Paykit wire 4.9
    /// against the request, such as a period that isn't one of its own, one that doesn't
    /// take details now, or details expired on the highest service time verified;
    /// [`Refusal::NotNow`] while the period record's value waits for its scope key, is held
    /// as a value that doesn't read, or the switch holds a newer write of it this app
    /// refused, and while no service time is verified to judge the period by;
    /// [`Refusal::SwitchUnavailable`] when the records can't be read again now;
    /// [`crate::PaykitError::Conflict`] when the record kept changing; the client's errors.
    /// A failure that leaves the batch kept (the network, `429`, `503`) is no error: the
    /// details are reported sent, [`MessageDelivery::Kept`], so the host doesn't send them
    /// again.
    pub async fn send_period_details(
        &mut self,
        record: &[u8; 32],
        details: PeriodDetails,
    ) -> Result<SentMessage> {
        // The call's future goes on the heap, as `receive`'s does: it nests deep.
        Box::pin(self.send_period_details_inner(record, details)).await
    }

    async fn send_period_details_inner(
        &mut self,
        record: &[u8; 32],
        details: PeriodDetails,
    ) -> Result<SentMessage> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.records_known().await?;
        self.require_payee()?;
        // Period details go to no payer whose sends are held (Paykit spec 8.1, "Blocked
        // counterparties"), refused before anything is signed, written or queued: they are
        // sent once the payer is unblocked or the lists are read.
        let (target, held) = self.payee_record(record).await?;
        self.refuse_while_sends_held(&held.counterparty).await?;
        let mut tries = 0;
        let msg_id = loop {
            // Judged on what the switch holds now: the request record read again shows a
            // rejection or a cancellation another own app recorded meanwhile, and it and the
            // period record read again below verify the service's time now, which the
            // period-details window is judged on (Paykit spec 8.2): a time verified a while
            // ago can show a period's deadline not passed yet.
            self.app.read_record(&target.record_target()).await?;
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
            let period = request.named_period(&details.billing_period)?;
            let request_hash = stored.hash();
            let into = ActivityTarget::payee(
                period_key(&self.identity(), &request_hash, &details.billing_period),
                target.label().as_ref(),
            );
            // The period record, read again, is never written over while its value isn't
            // open, or is held as a value that doesn't read or bind, nor while the switch
            // holds a newer write this app refused: either may hold what another own app
            // wrote (Paykit spec 8.1).
            // Nor while the switch shows another write at the version held, as recovery
            // uploads can leave: the client builds the next write on that one, which this
            // app hasn't read, and may hold a receipt (Switch spec 12.6 step 3).
            match self.app.read_record_fresh(&into.record_target()).await? {
                RecordRead::Fresh(_)
                | RecordRead::Kept {
                    reason:
                        KeptReason::NotFound | KeptReason::Older(_) | KeptReason::Overtaken { .. },
                    ..
                } => {}
                RecordRead::Kept {
                    reason: KeptReason::Unreachable(why),
                    ..
                } => return Err(Refusal::SwitchUnavailable(why).into()),
                _ => return Err(period_record_unread()),
            }
            let held = self.app.shared_record(&self.activity_key(&into)?).await?;
            if crate::records::activity_base_changed(held.as_ref()) {
                return Err(period_record_unread());
            }
            let held = match held.map(|held| held.content) {
                None => None,
                Some(RecordContent::Deleted) => return Err(payment_history_lost().into()),
                Some(content) if crate::hold::awaits_key(&content) => {
                    return Err(crate::hold::value_awaits_key());
                }
                Some(RecordContent::Value(_)) => match self.held_activity(&into).await? {
                    Some(held) => Some(held),
                    None => return Err(period_record_unread()),
                },
                Some(RecordContent::Unreadable(_)) => return Err(period_record_unread()),
            };
            let paid = held.as_ref().is_some_and(ActivityRecord::shows_paid);
            if paid {
                return Err(Refusal::AlreadyPaid.into());
            }
            // Only for a period that takes details, the periods the payer stores them for,
            // on the highest service time verified (Paykit spec 8.2).
            match takes_details(request, &period, paid, self.service_time())? {
                Window::Takes => {}
                Window::Closed => {
                    return Err(paykit_lib::Error::Validation(
                        "Paykit spec 8.2: period details only for the next period to start, or one started whose payment deadline hasn't passed, or, without one, one of the 16 latest to have started"
                            .into(),
                    )
                    .into());
                }
                Window::NoTime => {
                    return Err(Refusal::NotNow(
                        "Paykit spec 8.2: no service time verified yet to judge the period by"
                            .into(),
                    )
                    .into());
                }
            }
            // Expired on the highest service time verified, as every time the SDK judges:
            // details are valid until their `expires_at`, that millisecond included (Paykit
            // wire 4.9).
            if self.service_time().is_some_and(|t| details.expires_at < t) {
                return Err(paykit_lib::Error::Validation(
                    "Paykit wire 4.9: period details that expired".into(),
                )
                .into());
            }
            if held.is_some_and(|r| r.details().len() >= MAX_PERIOD_DETAILS) {
                return Err(Refusal::Usage(
                    "Paykit spec 8.2: a Billing Period has at most 8 period details".into(),
                )
                .into());
            }
            // Period details are an own write of another kind than a stop: over a request
            // record or period record paused as divergent, or whose kept messages wait, they
            // wait for the user's confirmation, nothing signed, written or queued (Paykit spec
            // 6): the upload may hold the payer's cancellation, or the period's credit.
            let period_record = self.activity_key(&into)?;
            Box::pin(self.refuse_while_paused(&[*record, period_record])).await?;
            let signed = self.sign_for_sale(
                &target,
                bound.counterparty,
                Kind::PaymentPeriodDetails,
                vec![request_hash],
                body,
            )?;
            let msg_id = signed.msg_id().to_owned();
            // What ends its outbox copy past its 30 days, in the commit that queues it
            // (Paykit spec 8.1).
            let kept = crate::state::local_put(
                &crate::state::key::queued_stop(&msg_id),
                &crate::send_holds::QueuedStop::signed(
                    &signed,
                    crate::send_holds::HeldKind::PeriodDetails,
                    Some(request_hash),
                    Some(&into),
                )?,
            )?;
            match self
                .send_with_record(signed, &into, Role::PeriodDetails, Vec::new(), vec![kept])
                .await
            {
                Ok(_) => break msg_id,
                // Kept to go at the next write or read: sent, as far as the caller goes, so a
                // retry doesn't make second details that count toward the 8.
                Err(error) if error.kept_batch().is_some() => {
                    let batch_id = error.kept_batch().ok_or_else(PaykitError::corrupt)?;
                    return Ok(SentMessage {
                        msg_id,
                        delivery: MessageDelivery::Kept { batch_id },
                    });
                }
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error));
                    }
                }
                Err(error) => return Err(error),
            }
        };
        Ok(SentMessage {
            delivery: self.delivered(&msg_id).await?,
            msg_id,
        })
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
/// conditioned (Paykit spec 8.2): 0 for no claim. `None` while an attempt is open or its
/// outcome `unknown`, which counts as open: it may have paid, and stays so until its owner
/// reports it or another own app writes it `failed` (Switch spec 10.2 row 5). The period
/// stays listed.
pub(crate) fn finished_step(view: Option<&ClaimView>) -> Option<u64> {
    match view {
        None => Some(0),
        Some(view)
            if view.status == ClaimStatus::Attempting
                || view
                    .attempts
                    .iter()
                    .any(|a| matches!(a.outcome, Outcome::Open | Outcome::Unknown)) =>
        {
            None
        }
        Some(view) => Some(view.step),
    }
}

/// The payer's period record of `period` of `request` holding no message, only the members
/// that name it (Paykit wire 5.3): the record an attempt by hand creates, holding its mark,
/// for a period that has none yet.
fn bare_period_record(request: &RequestRef, period: &PeriodRef) -> ActivityRecord {
    let mut record = crate::activity::empty(ActivitySide::Payer, request.payee);
    record.request_hash = Some(request.request_hash);
    record.billing_period = Some(*period);
    record
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PeriodPayer {
    User,
    Policy,
    Executor,
}

fn automatic_period_held(
    record: &ActivityRecord,
    period: &PeriodRef,
    funding: Option<&paykit_lib::AllowanceId>,
) -> bool {
    record.manual_only
        || if record.billing_period.is_some() {
            record.allowance_id.is_some()
        } else {
            record.allowance_for(period.start_secs()) != funding
        }
}

pub(crate) fn automatic_window_ended(
    period: &PeriodRef,
    deadline: Option<u64>,
    time: Option<u64>,
) -> bool {
    time.is_none_or(|now| now >= deadline.unwrap_or(period.ends_at).min(period.ends_at))
}

/// The physical write checked by an app paying under its policy, and whether it held
/// the payee's signed stop that crosses this first dispatch (Paykit spec 8.1, 8.2).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AutomaticPeriodWrite {
    condition: Condition,
    pub(crate) crossed_stop: bool,
}

impl Paykit {
    /// Read a record used to authorize an automatic period payment from its current
    /// physical application. Applying the read can itself reveal a restricted write.
    pub(crate) async fn automatic_period_condition(
        &mut self,
        target: &ActivityTarget,
        period: PeriodRef,
        parent: bool,
        funding: Option<&paykit_lib::AllowanceId>,
        crossing_payee: Option<[u8; 32]>,
    ) -> Result<Option<AutomaticPeriodWrite>> {
        let Ok(RecordRead::Fresh(held)) = self.app.read_record_fresh(&target.record_target()).await
        else {
            return Ok(None);
        };
        if held.doubtful || self.pause_of(&held).await?.blocks_automatic() {
            return Ok(None);
        }
        let Ok(record) = self.checked_period_activity(target, &held) else {
            return Ok(None);
        };
        if record.billing_period != (!parent).then_some(period)
            || automatic_period_held(&record, &period, funding)
            || payment_ended(&record, crossing_payee)
        {
            return Ok(None);
        }
        if parent
            && (request_of(&record).is_none_or(|(_, request)| !request.is_recurring())
                || !accepted(&record)
                || !held.authenticated
                || held.written_at_ms.is_none()
                || self
                    .kept_stop_message(target)
                    .await?
                    .is_some_and(|stop| Some(stop.sender()) != crossing_payee)
                || self
                    .held_across_payee_stop(&held.key, &record, crossing_payee)
                    .await?)
        {
            return Ok(None);
        }
        Ok(Some(AutomaticPeriodWrite {
            condition: self.app.record_condition(&held.key).await?,
            crossed_stop: record.cancellation.is_some(),
        }))
    }

    /// Pay one Billing Period, `period`, of the subscription `request` by hand, to the
    /// request's own destination (Paykit spec 7.2, 8.2): the paying order of every payment,
    /// on the period's obligation, its proof naming the period and going into the payer's
    /// period record. Accepting a subscription pays nothing: the host calls this for each
    /// period its user confirms paying, with the confirmations of [`Paykit::pay_request`] in
    /// `options`. A payment priced by this period's signed details shows its rate and exact
    /// rounded amount for confirmation. The proof names the details used. The device and
    /// verified service clocks must still be inside their validity at dispatch.
    ///
    /// - The attempt carries `server_time_in(s, null)`: `s` the period's start, or, for a
    ///   period that hasn't started on the latest service time verified, that time, once the
    ///   user has confirmed paying it early ([`PayOptions::with_early_payment_confirmed`];
    ///   Paykit spec 8.2, "Not before its start").
    /// - An attempt on a period the request record doesn't list in `attempted_periods` writes
    ///   that record in its batch, adding the period and dropping the listed ones whose
    ///   claims have no open or `unknown` attempt, each conditioned on its claim's step
    ///   (Paykit spec 8.2, "Attempted periods").
    /// - Its batch marks the period `manual_only` on its period record, at the version read,
    ///   creating the record holding the mark and no message when the period has none
    ///   (Paykit spec 8.3, "Manual only"): no app pays that period automatically after it,
    ///   whatever the attempt's outcome. A record holding the mark already is only
    ///   conditioned on.
    /// - After payer-provider loss, a period of an earlier subscription may already have
    ///   been paid early. It stays held while its payment history is incomplete, including
    ///   when the user confirms paying it early (Paykit spec 7.3, 8.2).
    /// - A payee's cancellation prevents opening a new attempt. If it arrives after the
    ///   attempt batch succeeds, that attempt finishes and sends its proof (Paykit spec 8.1).
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`], and: [`Refusal::Usage`] for a one-time request, or a
    /// period that isn't one of the request's; [`Refusal::NotSubscribed`] before the
    /// subscription is accepted; [`Refusal::Divergent`] while the request record or the
    /// period record is divergent or was seen so, until the user confirms it;
    /// [`Refusal::AlreadyPaid`] for a period whose record holds its proof;
    /// [`Refusal::NoDestination`] for a later period of a request with
    /// `per_period_endpoints` until its signed details supply a usable destination;
    /// [`Refusal::PeriodNotStarted`]; [`Refusal::AttemptedPeriodsFull`];
    /// [`Refusal::NotNow`] while the period record's value waits for its scope key or
    /// doesn't read.
    pub async fn pay_period(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        options: PayOptions,
    ) -> PayResult {
        self.pay_selected_period(request, period, PeriodPayer::User, options)
            .await
    }

    /// Pay one Billing Period selected by the host's own recurring-payment policy (Paykit
    /// spec 8.2, "Paying periods"). The host supplies the accepted subscription and the
    /// period its policy authorizes. For a rate-priced period, return the exact quote
    /// from [`Refusal::PricedByRate`] once the user or their bounded policy approves it.
    /// Accepting a subscription schedules no payment.
    ///
    /// The request and period must be open, eligible for automatic payment, and free of
    /// Allowance funding. The SDK checks the automatic window before opening the attempt
    /// and again immediately before its first dispatch. The proof names only this period
    /// and attempt. Each new payment during a restore or missed-item hold requires the
    /// ordinary reconciliation or confirmation that clears that hold.
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_period`]. [`Refusal::ManualOnly`] holds a marked or
    /// Allowance-funded request or period. [`Refusal::NotAutomatic`] holds a period
    /// outside its automatic window or whose acceptance time cannot be verified.
    pub async fn pay_period_automatically(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        price: PeriodPrice<'_>,
    ) -> PayResult {
        let options = match price {
            PeriodPrice::RequestedAmount => PayOptions::default(),
            PeriodPrice::QuoteFor(endpoint) => PayOptions {
                endpoint: Some(endpoint),
                quote_only: true,
                ..PayOptions::default()
            },
            PeriodPrice::AtRate(quote) => PayOptions::default().with_rate_authorized(quote.clone()),
        };
        self.pay_selected_period(request, period, PeriodPayer::Policy, options)
            .await
    }

    /// Pay one Billing Period under the Allowance named by the accepted subscription
    /// for this occurrence. Its executor reserves the amount and opens the period's
    /// claim in the same batch (Paykit spec 8.2, 8.3). A payment requiring currency
    /// conversion is ineligible. For a later occurrence that needs its own destination, the
    /// wallet uses eligible signed period details. Missing details return [`Refusal::NoDestination`].
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_period_automatically`] and [`Paykit::pay_under_allowance`].
    /// An unfunded subscription, an ineligible period or an unverified grant cannot spend.
    pub async fn pay_period_under_allowance(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
    ) -> PayResult {
        self.pay_selected_period(
            request,
            period,
            PeriodPayer::Executor,
            PayOptions::default(),
        )
        .await
    }

    async fn pay_selected_period(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        payer: PeriodPayer,
        options: PayOptions,
    ) -> PayResult {
        let mut public_failures = Vec::new();
        let plan = Box::pin(self.plan_period_payment(
            request,
            period,
            payer,
            options,
            &mut public_failures,
        ))
        .await;
        let outcome = match plan {
            Ok(plan) => Box::pin(self.execute(plan)).await,
            Err(error) => Err(error),
        };
        PayResult {
            outcome,
            public_failures,
        }
    }

    async fn plan_period_payment(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
        payer: PeriodPayer,
        options: PayOptions,
        public_failures: &mut Vec<PublicAppFailure>,
    ) -> Result<Plan> {
        self.refresh_own()?;
        if options.quote_only {
            self.require_first_read()?;
            self.refuse_while_unlisted().await?;
        } else {
            Box::pin(self.first_read_before_writing()).await?;
            self.refresh_own()?;
            self.records_known().await?;
        }
        self.require_pay()?;
        let automatic = payer != PeriodPayer::User;
        let by = if automatic {
            Initiator::Automatic
        } else {
            Initiator::User
        };
        self.app
            .may_attempt(automatic)
            .map_err(crate::pay::not_now)?;
        self.refuse_while_payer_pass_owed().await?;
        let parent = ActivityTarget::payer(natural_key::request_record(
            &request.payee,
            &request.request_hash,
        ));
        self.period_payment_base(&parent).await?;
        let (target, record) = if options.quote_only {
            self.read_payer_request(request).await?
        } else {
            self.payer_request(request).await?
        };
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
        // Only a subscription the request record shows accepted is paid (Paykit spec 8.2).
        if record.response.is_none() {
            return Err(Refusal::NotSubscribed.into());
        }
        let period_target =
            ActivityTarget::payer(period_key(&request.payee, &request.request_hash, &period));
        let record_key = self.activity_key(&target)?;
        let period_record_key = self.activity_key(&period_target)?;
        let held_period = self.period_payment_base(&period_target).await?;
        // The period record shows it paid (Paykit spec 7.2).
        if held_period.as_ref().is_some_and(ActivityRecord::shows_paid) {
            return Err(Refusal::AlreadyPaid.into());
        }
        let funding_id = if payer == PeriodPayer::Executor {
            Some(
                record
                    .allowance_for(period.start_secs())
                    .ok_or(Refusal::ManualOnly)?,
            )
        } else {
            None
        };
        if automatic
            && (automatic_period_held(&record, &period, funding_id)
                || held_period
                    .as_ref()
                    .is_some_and(|held| automatic_period_held(held, &period, funding_id)))
        {
            return Err(Refusal::ManualOnly.into());
        }
        if automatic
            && held_period
                .as_ref()
                .is_some_and(|held| held.billing_period != Some(period))
        {
            return Err(period_record_unread());
        }
        // Divergent after a provider loss, or seen so: the request record pauses every period
        // of the request, its upload maybe holding a cancellation, and the period record its
        // own period. Paid by hand only once the user has seen that the payee may have
        // cancelled it (Paykit spec 6); a period that can't be paid anyway is refused for what
        // stops it instead, as a one-time request is: one past its deadline goes on to the
        // checks below, and one whose destination's app is no longer current, or that
        // requires an app the payee removed, is refused for that.
        let deadline = body.attempt_deadline(Some(&billing))?;
        if automatic {
            for key in [record_key, period_record_key] {
                if Box::pin(self.pause_at(&key)).await?.blocks_automatic() {
                    return Err(self.divergent_refusal(key).await?);
                }
            }
        } else if !self.deadline_passed(deadline) {
            let mut paused = false;
            for key in [record_key, period_record_key] {
                paused |= Box::pin(self.pause_at(&key)).await?.blocks_by_hand();
            }
            if paused {
                Box::pin(self.refuse_unpayable_request(
                    &request.payee,
                    &stored,
                    &body.destination,
                    &options,
                ))
                .await?;
                Box::pin(self.refuse_while_paused(&[record_key, period_record_key])).await?;
            }
        }
        // Held after a restore: paid only once the user has seen the payee may have
        // cancelled it, which ends the hold (Paykit spec 7.2, "What a restore loses").
        let end_hold = self
            .pass_hold(
                &record_key,
                &record,
                options.possibly_cancelled_confirmed.as_ref(),
            )
            .await?;
        // A blocked payee's period is paid only once the user confirms it (Paykit spec 8.1).
        let blocked = if options.quote_only {
            // Acting on a fresh deny-list read can publish an unblock floor. Leave it to
            // the real dispatch; a quote never authorizes that write.
            if self.floor_write_is_kept().await? {
                return Err(crate::floor::unreadable_floor());
            }
            if self.denied_as_last_read(&request.payee).await? {
                return Err(Refusal::NotNow("the payee is blocked".into()).into());
            }
            false
        } else if automatic {
            self.denied(&request.payee).await?
        } else {
            self.denied_for_user(&request.payee).await?
        };
        let obligation =
            self.obligation(&request.payee, &request.request_hash, period.start_secs());
        let restored = self.restored_attempts(&obligation).await?;
        let claim = self
            .payable_claim(
                &obligation,
                &stored,
                Some(period),
                deadline,
                restored,
                &options,
                by,
            )
            .await?;
        let step = claim.as_ref().map_or(0, |c| c.step);
        if self.deadline_passed(deadline) {
            return Err(self.past_deadline(&target, None).await?);
        }
        if automatic {
            let held = self.app.shared_record(&record_key).await?;
            let accepted_at = held
                .filter(|r| r.authenticated && !r.doubtful)
                .and_then(|r| r.written_at_ms)
                .ok_or_else(|| {
                    Refusal::NotAutomatic("no proof dates the accepted subscription's write".into())
                })?;
            if accepted_at >= period.ends_at {
                return Err(Refusal::NotAutomatic(
                    "the Billing Period ended before the verified subscription acceptance".into(),
                )
                .into());
            }
            let verified = self.service_time().map(|time| time.max(accepted_at));
            if automatic_window_ended(&period, deadline, verified) {
                return Err(Refusal::NotAutomatic(
                    "the Billing Period's automatic window has ended".into(),
                )
                .into());
            }
        }
        // The claim was read just before: the service time it verified is current.
        let mut conditions = vec![self.not_before(&period, &options)?];
        let funding = if let Some(id) = funding_id {
            let key = self
                .app
                .record_key(&crate::allowances::allowance_target(&request.payee, id))?;
            let reservation = Box::pin(self.allowance_reservation(&key, &request.payee)).await?;
            reservation.request(&record, claim.as_ref(), Some(period.start_secs()))?;
            Some(reservation)
        } else {
            None
        };
        let allowance_endpoints = funding.as_ref().and_then(|under| under.endpoints());
        let per_period_destination = needs_period_destination(&body, period)?;
        let mut details = held_period
            .as_ref()
            .map_or_else(Vec::new, |held| valid_details(held, &body));
        // Whether a period details' destination expired only by the service's time: the user
        // is told the device clock is behind it (Paykit spec 7.2).
        let mut device_clock_behind = false;
        let mut destinations = if per_period_destination {
            // A period details' destination is used while neither clock is at its expiry
            // (Paykit spec 7.2).
            let clocks = self.clocks();
            let named = options.selected_period_rate();
            // The details eligible as this period's destination, expired or not.
            let eligible = |signed: &StoredMessage| {
                matches!(signed.body(), Body::PaymentPeriodDetails(detail)
                    if detail.payment_endpoints.is_some()
                        && destination_app_permitted(&body, signed.cert().app_id())
                        && named.is_none_or(|id| id == signed.message().msg_id))
            };
            let expires_at = |signed: &StoredMessage| match signed.body() {
                Body::PaymentPeriodDetails(detail) => detail.expires_at,
                _ => 0,
            };
            device_clock_behind = details.iter().any(|signed| {
                let at = expires_at(signed);
                eligible(signed) && clocks.past(at) && at > clocks.device
            });
            let mut destinations: Vec<_> = details
                .iter()
                .copied()
                .filter(|signed| eligible(signed) && !clocks.past(expires_at(signed)))
                .collect();
            destinations.sort_by(|a, b| period_details_order(a.message(), b.message()));
            destinations
        } else {
            vec![&stored]
        };
        let mut unavailable = Refusal::NoDestination(
            "Paykit spec 8.2: no current signed details carry an eligible period destination"
                .into(),
        )
        .into();
        let mut payee_identity = None;
        let Chosen {
            destination,
            carrier,
            reserve,
            bound,
            payment_amount,
            paid,
            price,
        } = loop {
            let Some(candidate) = destinations.pop() else {
                return Err(with_clock_note(unavailable, device_clock_behind));
            };
            // For a later destination its own signed details must supply any per-period rate.
            let (rates, named) = if per_period_destination {
                (vec![candidate], None)
            } else {
                (std::mem::take(&mut details), options.selected_period_rate())
            };
            let pricing = self.period_pricing(request, period, &body, rates, by, named)?;
            let pricing = if funding.is_some() {
                pricing.under_allowance()
            } else {
                pricing
            };
            match self
                .request_destination(
                    &request.payee,
                    candidate,
                    &body,
                    &options,
                    allowance_endpoints,
                    &pricing,
                    &mut payee_identity,
                    public_failures,
                )
                .await
            {
                Ok(chosen) => break chosen,
                Err(error)
                    if per_period_destination
                        && matches!(
                            error.refusal(),
                            Some(Refusal::NoDestination(_) | Refusal::UnsupportedEndpoint(_))
                        ) =>
                {
                    unavailable = error
                }
                Err(error) => return Err(error),
            }
        };
        if blocked {
            let amount = paid.ok_or_else(|| {
                Refusal::NotNow(
                    "Paykit spec 8.1: the blocked payment's transfer amount is unknown".into(),
                )
            })?;
            let shown = BlockedPayment {
                payee: request.payee,
                request: Some(*request),
                period: Some(period),
                amount,
            };
            crate::pay::refuse_blocked(true, shown, options.blocked_confirmed.as_ref())?;
        }
        let mut record_writes = Vec::new();
        if let Some(write) = self
            .attempted_write(&target, &record, request, period.start_secs())
            .await?
        {
            record_writes.push(write);
        }
        // In the attempt's batch, at the version read, so no attempt by hand exists without
        // its mark, and a batch refused or in conflict writes neither (Paykit spec 8.3,
        // "Manual only").
        match self
            .period_attempt_write(&period_target, held_period.as_ref(), request, &period, by)
            .await?
        {
            Some(write) => record_writes.push(write),
            None => conditions.push(self.app.record_condition(&period_record_key).await?),
        }
        let entry = JournalEntry {
            user_started: by == Initiator::User,
            allowance: None,
            attempt_id: AttemptId::generate()?.to_text(),
            obligation,
            payee: request.payee,
            request_hash: Some(pubky_switch_core::enc::b64(&request.request_hash)),
            natural_key: period_target.natural_key.clone(),
            amount: KeptAmount::of(&body.amount),
            payment_amount: KeptAmount::of(&payment_amount),
            destination,
            deadline,
            stage: Stage::Recorded,
            step: None,
            label: body.label.as_ref().map(|l| l.as_str().to_owned()),
            payment_reference: Some(body.payment_reference.as_str().to_owned()),
            // A subscription is accepted before its periods are paid (Paykit spec 8.2).
            acceptance_owed: false,
            acceptance_msg_id: None,
            acceptance_plaintext: None,
            acceptance_cert: None,
            acceptance_released: false,
            rail_proof: None,
            paid_at_ms: None,
            offline: None,
            proof_msg_id: None,
            restored: false,
            journaled_at_ms: self.now(),
            allowance_expires_at_ms: None,
            carrier: None,
            carrier_record: None,
            attempt_proof: None,
            failed_by_method: false,
            never_dispatched: false,
            user_paid: false,
            request: Some(KeptMessage::of(&stored)),
            record_held: false,
            transfer_bound: Some(bound),
            price,
            failed_on_claim: false,
            billing_period: Some(KeptPeriod::of(&period)),
        };
        Ok(Plan {
            entry,
            record: target,
            carrier,
            step,
            reserve,
            create: None,
            on_record: true,
            allowance: funding,
            by,
            unconfirmed: options.unconfirmed_identity_confirmed,
            blocked_confirmed: options.blocked_confirmed,
            end_hold,
            conditions,
            record_writes,
        })
    }

    /// Select the eligible signed rate sources from this occurrence's bounded period record.
    /// The destination's endpoint then picks the newest of them that actually prices it.
    fn period_pricing<'a>(
        &self,
        request: &RequestRef,
        period: PeriodRef,
        body: &'a PaymentRequest,
        mut details: Vec<&'a StoredMessage>,
        by: Initiator,
        named: Option<&str>,
    ) -> Result<Pricing<'a>> {
        let pricing = Pricing::of_request(
            &body.amount,
            *request,
            Some(period),
            body.conversion.as_ref(),
            by,
        );
        if !matches!(
            body.conversion,
            Some(paykit_lib::bodies::Conversion::PerPeriod)
        ) {
            return if named.is_none() {
                Ok(pricing)
            } else {
                Err(
                    Refusal::Usage("Paykit spec 8.2: this request has no per-period rates".into())
                        .into(),
                )
            };
        }
        let service = self.service_time();
        let device = self.now();
        // A quote held by this occurrence can still be used until its signed expiry.
        details.retain(|signed| {
            matches!(signed.body(), Body::PaymentPeriodDetails(details) if details.rates.is_some())
                && named.is_none_or(|id| signed.message().msg_id == id)
                && rates_current(signed, device, service)
        });
        if named.is_some() && details.is_empty() {
            return Err(Refusal::NoDestination("Paykit spec 8.2: the selected period rates are unavailable or outside their validity".into()).into());
        }
        details.sort_by(|a, b| period_details_order(b.message(), a.message()));
        Ok(pricing.with_period_rates(details))
    }

    /// The attempt's `server_time_in(s, null)` on a Billing Period (Paykit spec 8.2, "Not
    /// before its start"): `s` the period's start once the latest service time verified is
    /// at or past it; before it, the period is paid only by the user who confirmed paying
    /// it early, and `s` is that latest service time, so the service never applies the
    /// attempt before the user agreed.
    fn not_before(&self, period: &PeriodRef, options: &PayOptions) -> Result<Condition> {
        let start = match self.service_time() {
            Some(t) if t >= period.starts_at => period.starts_at,
            Some(t) if options.early_payment_confirmed.is_some() => t,
            _ => return Err(Refusal::PeriodNotStarted.into()),
        };
        Ok(Condition::ServerTimeIn {
            start: Some(start),
            end: None,
        })
    }

    /// Write the period at the version the attempt judged (Paykit spec 8.2, 8.3).
    /// An attempt by hand sets the manual-only mark. When it is already marked the
    /// exact current write can be conditioned on directly.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] for a period record held whose value doesn't read, never written
    /// over; the client's errors.
    async fn period_attempt_write(
        &mut self,
        target: &ActivityTarget,
        held: Option<&ActivityRecord>,
        request: &RequestRef,
        period: &PeriodRef,
        by: Initiator,
    ) -> Result<Option<(Vec<Condition>, OpKind)>> {
        let mut next = match held {
            Some(record) if record.manual_only && by == Initiator::User => return Ok(None),
            Some(record) => record.clone(),
            None => {
                let content = self
                    .app
                    .shared_record(&self.activity_key(target)?)
                    .await?
                    .map(|held| held.content);
                if content.is_some() {
                    return Err(period_record_unread());
                }
                bare_period_record(request, period)
            }
        };
        if by == Initiator::User {
            next.manual_only = true;
        } else if !next.stored().is_empty() {
            next.status = Some("paying".into());
        }
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&next.to_json()?), Vec::new())
                .await?,
        ))
    }

    /// The request record's write in the batch of an attempt on the Billing Period starting
    /// at `start`, when `record`, at `target`, doesn't list it in `attempted_periods`
    /// (Paykit spec 8.2): the period added at the record's next version, and every listed
    /// period whose claim, read now, has no open attempt dropped, each with a `claim_step`
    /// condition at the step read, so an attempt on it meanwhile fails the batch. `None` for
    /// a period listed already, whose attempt writes no record.
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
        Ok(Some(
            self.app
                .record_op(&target.record_target(), Some(&next.to_json()?), conditions)
                .await?,
        ))
    }

    /// The periods `record` lists in `attempted_periods` whose claims, read now, have no
    /// open attempt, with the `claim_step` condition of each at the step read (Paykit spec
    /// 8.2, "Attempted periods").
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
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use paykit_lib::Label;

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

    /// A monthly usd request from 2026-01-31, with `conversion` as its conversion terms.
    fn monthly(conversion: Option<serde_json::Value>) -> PaymentRequest {
        let mut body = json!({
            "amount": { "asset": "usd", "value": "10" },
            "payment_reference": "plan-1",
            "proposal_expires_at": null,
            "payment_deadline": { "type": "period_start", "seconds": 86400 },
            "recurrence": {
                "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
            }
        });
        if let Some(conversion) = conversion {
            body["conversion"] = conversion;
        }
        PaymentRequest::from_body(&object(body)).unwrap()
    }

    fn parties() -> (Party, Party) {
        (
            Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]),
            Party::new(
                "bob",
                "shop",
                &["paykit_request", "paykit_settlements", "paykit_contacts"],
            ),
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
        assert_eq!(read.request_hash, Some([7; 32]));
        let view = period_view([1; 32], &read, &monthly(None)).unwrap();
        assert_eq!(view.details[0].msg_id, later.message().msg_id);
        assert_eq!(view.details.len(), 2);
        // The same details again change nothing.
        assert!(!place(&mut record, Role::PeriodDetails, later));
    }

    #[test]
    fn test_periods_details_invalid_against_the_request_left_out_of_the_valid_details() {
        let (alice, bob) = parties();
        let mut record = empty(ActivitySide::Payer, bob.id());
        let with_rates = bob.message(
            &alice,
            "paykit.payment_period_details",
            vec![[7; 32]],
            object(json!({
                "billing_period": {
                    "starts_at": "2026-02-28T00:00:00.000Z",
                    "ends_at": "2026-03-31T00:00:00.000Z"
                },
                "expires_at": "2026-03-01T00:00:00.000Z",
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" },
                "rates": [{ "asset": "btc", "value": "0.00001234" }]
            })),
        );
        assert!(place(&mut record, Role::PeriodDetails, with_rates.clone()));
        let per_period = monthly(Some(json!({ "type": "per_period" })));
        let view = period_view([1; 32], &record, &per_period).unwrap();
        assert_eq!(view.details.len(), 1);
        assert_eq!(view.details[0].rates.len(), 1);
        assert_eq!(valid_details(&record, &per_period).len(), 1);
        // Rates under fixed conversion terms, or none, make the details invalid against the
        // request: the record still binds, and neither shows them nor counts them valid.
        let fixed = monthly(Some(json!({ "type": "fixed",
            "rates": [{ "asset": "btc", "value": "0.00001234" }] })));
        for request in [fixed, monthly(None)] {
            let view = period_view([1; 32], &record, &request).unwrap();
            assert!(view.details.is_empty());
            assert!(valid_details(&record, &request).is_empty());
        }
        // Details with a destination only stay, beside them.
        let endpoints_only = details(&bob, &alice, [7; 32], "2026-03-02T00:00:00.000Z");
        assert!(place(
            &mut record,
            Role::PeriodDetails,
            endpoints_only.clone()
        ));
        let shown = valid_details(&record, &monthly(None));
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].hash(), endpoints_only.hash());
        let view = period_view([1; 32], &record, &monthly(None)).unwrap();
        assert_eq!(view.details.len(), 1);
        assert_eq!(view.details[0].msg_id, endpoints_only.message().msg_id);
    }

    /// The payee app of `label`, of bob's identity, holding only that labelled selling scope.
    fn labelled(label: &str) -> Party {
        Party::new(
            "bob",
            label,
            &[
                &format!("paykit_request:{label}"),
                "paykit_settlements",
                "paykit_contacts",
            ],
        )
    }

    #[test]
    fn test_periods_details_from_another_label_left_out_of_the_valid_details() {
        let (alice, bob) = parties();
        let shop1 = labelled("shop1");
        let mut labelled_request = monthly(None);
        labelled_request.label = Some(Label::parse("shop1").unwrap());
        for (sender, label, valid) in [
            (&shop1, Some(&labelled_request), true),
            (&labelled("shop2"), Some(&labelled_request), false),
            (&bob, Some(&labelled_request), true),
            (&shop1, None, false),
        ] {
            let request = label.cloned().unwrap_or_else(|| monthly(None));
            let mut record = empty(ActivitySide::Payer, bob.id());
            let held = details(sender, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
            assert!(place(&mut record, Role::PeriodDetails, held));
            // The record holds no request, so it binds whatever the sender; its reader
            // judges the details against the request record's request.
            record.to_json().unwrap();
            assert_eq!(valid_details(&record, &request).len(), usize::from(valid));
            let view = period_view([1; 32], &record, &request).unwrap();
            assert_eq!(view.details.len(), usize::from(valid));
        }
    }

    #[test]
    fn test_periods_receipt_from_another_label_credits_no_period() {
        let (alice, _) = parties();
        let mut request = monthly(None);
        request.label = Some(Label::parse("shop1").unwrap());
        for (sender, credited) in [(labelled("shop1"), true), (labelled("shop2"), false)] {
            let receipt = sender.message(
                &alice,
                "paykit.payment_receipt",
                vec![[3; 32], [7; 32]],
                object(json!({
                    "attempt_id": "DgCVYQKL1eyDacZvG6YhZg",
                    "billing_period": {
                        "starts_at": "2026-02-28T00:00:00.000Z",
                        "ends_at": "2026-03-31T00:00:00.000Z"
                    }
                })),
            );
            let mut record = empty(ActivitySide::Payer, sender.id());
            assert!(place(&mut record, Role::Receipt, receipt));
            let view = period_view([1; 32], &record, &request).unwrap();
            assert_eq!(view.credited, credited);
            assert_eq!(view.receipt.is_some(), credited);
            assert_eq!(
                received_receipt([1; 32], &record, Some(&request)).is_some(),
                credited
            );
        }
    }

    #[test]
    fn test_periods_receive_places_nothing_from_another_label() {
        // Check each original signer against the request. The period receipt belongs
        // beside the payer's signed proof, with the recurring request as its parent.
        let (alice, _) = parties();
        let shop1 = labelled("shop1");
        let mut body = json!({
            "amount": { "asset": "usd", "value": "10" },
            "payment_reference": "plan-1",
            "proposal_expires_at": null,
            "payment_deadline": { "type": "period_start", "seconds": 86400 },
            "recurrence": {
                "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
            }
        });
        body["label"] = json!("shop1");
        let request = shop1.message(&alice, "paykit.payment_request", Vec::new(), object(body));
        let mut record = empty(ActivitySide::Payer, shop1.id());
        assert!(place(&mut record, Role::Request, request.clone()));
        let target = ActivityTarget::payer(vec![1]);
        let terms = details(&shop1, &alice, request.hash(), "2026-03-01T00:00:00.000Z");
        let billing_period = terms.body().billing_period().copied();
        let attempt_id = paykit_lib::AttemptId::from_bytes([9; 16]);
        let (_, parent) = request_of(&record).unwrap();
        let proof_body = paykit_lib::bodies::PaymentProof {
            attempt_id,
            billing_period,
            payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
                .unwrap(),
            payment_app_id: shop1.app_id(),
            list_version: None,
            amount: None,
            allowance_id: None,
            label: parent.label.clone(),
            payment_reference: Some(parent.payment_reference.clone()),
            proof: object(json!({ "preimage": "00" })),
        };
        let proof = alice.message(
            &shop1,
            "paykit.payment_proof",
            vec![request.hash(), terms.hash()],
            proof_body.to_body(2).unwrap(),
        );
        let proof_hash = proof.hash();
        let mut period = empty(ActivitySide::Payer, shop1.id());
        assert!(place(&mut period, Role::PeriodDetails, terms));
        assert!(place(&mut period, Role::Proof, proof));
        let receipt_body = paykit_lib::bodies::PaymentReceipt {
            attempt_id,
            billing_period,
            metadata: None,
            payment_reference: None,
        }
        .to_body(2)
        .unwrap();
        for (sender, taken) in [(shop1, true), (labelled("shop2"), false)] {
            let held = details(&sender, &alice, request.hash(), "2026-03-01T00:00:00.000Z");
            let into = crate::receive::period_or_request(
                &alice.id(),
                &target,
                &record,
                &held,
                &request.hash(),
            );
            assert_eq!(into.is_some(), taken);
            let receipt = sender.message(
                &alice,
                "paykit.payment_receipt",
                vec![proof_hash, request.hash()],
                receipt_body.clone(),
            );
            assert!(crate::receive::receipt_on_record(&record, &receipt).is_none());
            assert_eq!(
                matches!(
                    crate::receive::period_receipt(&receipt, &period, &record),
                    crate::receive::PeriodReceipt::Matched
                ),
                taken
            );
        }
    }

    #[test]
    fn test_periods_request_record_refuses_another_labels_receipt_and_cancellation() {
        let (alice, _) = parties();
        let shop1 = labelled("shop1");
        let body = json!({
            "amount": { "asset": "usd", "value": "10" },
            "payment_reference": "order-1",
            "proposal_expires_at": null,
            "label": "shop1"
        });
        let request = shop1.message(&alice, "paykit.payment_request", Vec::new(), object(body));
        for (sender, passes) in [(&shop1, true), (&labelled("shop2"), false)] {
            for (kind, refs, body, role) in [
                (
                    "paykit.payment_receipt",
                    vec![[3; 32], request.hash()],
                    json!({ "attempt_id": "DgCVYQKL1eyDacZvG6YhZg" }),
                    Role::Receipt,
                ),
                (
                    "paykit.payment_request_cancellation",
                    vec![request.hash()],
                    json!({ "reason": null }),
                    Role::Cancellation,
                ),
            ] {
                let mut record = empty(ActivitySide::Payer, shop1.id());
                assert!(place(&mut record, Role::Request, request.clone()));
                let message = sender.message(&alice, kind, refs, object(body));
                assert!(place(&mut record, role, message));
                assert_eq!(record.to_json().is_ok(), passes, "{kind}");
            }
        }
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
        let mut body = held.message().body.clone();
        body.insert(
            "expires_at".into(),
            serde_json::Value::String("2026-03-09T00:00:00.000Z".into()),
        );
        let msg_id = held.message().msg_id.clone();
        let other = bob
            .try_message_with(
                &alice,
                "paykit.payment_period_details",
                vec![[7; 32]],
                body,
                |message| message.msg_id = msg_id,
            )
            .unwrap();
        assert_ne!(other.hash(), held.hash());
        let mut record = empty(ActivitySide::Payer, bob.id());
        assert!(place(&mut record, Role::PeriodDetails, held.clone()));
        place(&mut record, Role::PeriodDetails, other);
        assert_eq!(record.details().len(), 1);
        assert_eq!(record.period_details.as_ref().unwrap().hash(), held.hash());
        record.to_json().unwrap();
    }

    #[test]
    fn test_periods_proof_awaits_the_details_it_names_on_every_path() {
        // The one check of the receive step and both later writes: a proof naming details
        // its period record doesn't hold, absent or held without them, waits for them.
        let (alice, bob) = parties();
        let held = details(&bob, &alice, [7; 32], "2026-03-01T00:00:00.000Z");
        let body = paykit_lib::bodies::PaymentProof {
            attempt_id: paykit_lib::AttemptId::from_bytes([9; 16]),
            billing_period: Some(PeriodRef {
                starts_at: 1_772_236_800_000,
                ends_at: 1_774_915_200_000,
            }),
            payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
                .unwrap(),
            payment_app_id: [0; 16],
            list_version: None,
            amount: None,
            allowance_id: None,
            label: None,
            payment_reference: None,
            proof: object(json!({ "preimage": "00" })),
        }
        .to_body(2)
        .unwrap();
        let proof = alice.message(
            &bob,
            "paykit.payment_proof",
            vec![[7; 32], held.hash()],
            body,
        );
        let awaits = crate::receive::awaits_named_details;
        assert!(awaits(None, ActivitySide::Payee, &proof));
        let mut record = empty(ActivitySide::Payee, alice.id());
        assert!(awaits(Some(&record), ActivitySide::Payee, &proof));
        assert!(place(&mut record, Role::PeriodDetails, held));
        assert!(!awaits(Some(&record), ActivitySide::Payee, &proof));
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

    #[test]
    fn test_periods_stores_details_with_no_verified_time_waits_for_one() {
        // A monthly request from 2026-01-31, each period due 1 s after its start.
        let (alice, bob) = parties();
        let request = bob.message(
            &alice,
            "paykit.payment_request",
            Vec::new(),
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "plan-1",
                "proposal_expires_at": null,
                "payment_deadline": { "type": "period_start", "seconds": 1 },
                "recurrence": {
                    "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                    "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
                }
            })),
        );
        let mut record = empty(ActivitySide::Payer, bob.id());
        record.request = Some(request.clone());
        record.response = Some(alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![request.hash()],
            object(json!({})),
        ));
        let first = bob.message(
            &alice,
            "paykit.payment_period_details",
            vec![request.hash()],
            object(json!({
                "billing_period": {
                    "starts_at": "2026-01-31T00:00:00.000Z",
                    "ends_at": "2026-02-28T00:00:00.000Z"
                },
                "expires_at": "2026-03-01T00:00:00.000Z",
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" }
            })),
        );
        let second = details(&bob, &alice, request.hash(), "2026-03-01T00:00:00.000Z");
        // No time verified: which periods have started can't be told, so the details wait
        // for a verified time, the first period's as any other's.
        let window = |details: &StoredMessage, t: Option<u64>| {
            stores_details(&record, None, details, t, false).unwrap()
        };
        assert_eq!(window(&first, None), Window::NoTime);
        assert_eq!(window(&second, None), Window::NoTime);
        // A verified time decides. 2 s into the first period its payment deadline, 1 s after
        // its start, has passed: it takes no details, and the next to start does.
        let jan_31 = 1_769_817_600_000;
        let t = Some(jan_31 + 2_000);
        assert_eq!(window(&first, t), Window::Closed);
        assert_eq!(window(&second, t), Window::Takes);
        // Within its deadline, the first period takes them.
        assert_eq!(window(&first, Some(jan_31 + 500)), Window::Takes);
        // The deadline's own millisecond is within it: an attempt is still taken then, the
        // attempt's `deadline` being the millisecond after it.
        assert_eq!(window(&first, Some(jan_31 + 1_000)), Window::Takes);
        assert_eq!(window(&first, Some(jan_31 + 1_001)), Window::Closed);
    }

    #[test]
    fn test_periods_stores_details_not_for_a_period_shown_paid() {
        let (alice, bob) = parties();
        let request = bob.message(
            &alice,
            "paykit.payment_request",
            Vec::new(),
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "plan-1",
                "proposal_expires_at": null,
                "recurrence": {
                    "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                    "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
                }
            })),
        );
        let mut record = empty(ActivitySide::Payer, bob.id());
        record.request = Some(request.clone());
        record.response = Some(alice.message(
            &bob,
            "paykit.payment_request_acceptance",
            vec![request.hash()],
            object(json!({})),
        ));
        let second = details(&bob, &alice, request.hash(), "2026-03-01T00:00:00.000Z");
        let t = Some(1_772_236_800_000);
        let period = empty(ActivitySide::Payer, bob.id());
        assert_eq!(
            stores_details(&record, Some(&period), &second, t, false).unwrap(),
            Window::Takes
        );
        // The period record's paid state decides, whatever the time.
        let (_, body) = request_of(&record).unwrap();
        let named = body
            .named_period(second.body().billing_period().unwrap())
            .unwrap();
        assert_eq!(
            takes_details(body, &named, false, t).unwrap(),
            Window::Takes
        );
        assert_eq!(
            takes_details(body, &named, true, t).unwrap(),
            Window::Closed
        );
        // Shown paid, it takes none with no time verified too.
        assert_eq!(
            takes_details(body, &named, true, None).unwrap(),
            Window::Closed
        );
    }

    #[test]
    fn test_periods_stores_details_only_while_the_subscription_runs() {
        let (alice, bob) = parties();
        let request = bob.message(
            &alice,
            "paykit.payment_request",
            Vec::new(),
            object(json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "plan-1",
                "proposal_expires_at": null,
                "recurrence": {
                    "anchor": "2026-01-31T00:00:00.000Z", "ends_at": null, "every": 1,
                    "starts_at": "2026-01-31T00:00:00.000Z", "unit": "month"
                }
            })),
        );
        let second = details(&bob, &alice, request.hash(), "2026-03-01T00:00:00.000Z");
        let t = Some(1_772_236_800_000);
        let mut record = empty(ActivitySide::Payer, bob.id());
        record.request = Some(request.clone());
        // Not accepted: none stored.
        assert_eq!(
            stores_details(&record, None, &second, t, false).unwrap(),
            Window::Closed
        );
        let reply = |kind: &str| alice.message(&bob, kind, vec![request.hash()], object(json!({})));
        record.response = Some(reply("paykit.payment_request_rejection"));
        assert_eq!(
            stores_details(&record, None, &second, t, false).unwrap(),
            Window::Closed
        );
        record.response = Some(reply("paykit.payment_request_acceptance"));
        assert_eq!(
            stores_details(&record, None, &second, t, false).unwrap(),
            Window::Takes
        );
        // A payee's cancellation kept and not placed yet stops it, as one the record holds.
        assert_eq!(
            stores_details(&record, None, &second, t, true).unwrap(),
            Window::Closed
        );
        record.cancellation = Some(reply("paykit.payment_request_cancellation"));
        assert_eq!(
            stores_details(&record, None, &second, t, false).unwrap(),
            Window::Closed
        );
    }
}
