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
//! - **Paying a period** ([`Paykit::pay_period`]) follows the paying order of every payment
//!   (Paykit spec 7.2), on the period's own obligation. Accepting a subscription pays
//!   nothing by itself: the host calls it for each period, on its user's confirmation or by
//!   its own policy, which is an automatic payment for every rule that names one (Paykit spec
//!   8.2, "Paying periods"). The attempt carries `server_time_in(s, null)`, `s` the period's
//!   start, or the time the user confirmed paying it early; an attempt on a period the
//!   request record doesn't list in `attempted_periods` writes the record in its batch,
//!   adding the period and dropping the finished ones, each conditioned on its claim's step
//!   (Paykit wire 5.3, "Attempted periods"). The proof goes into the period record, names the
//!   period and the details it was paid by. By its own policy the app pays a period only
//!   within its automatic window ([`not_automatic_now`]), never one of a request or period
//!   marked manual-only ([`Paykit::mark_period_manual_only`]), and its batch writes the
//!   period record at the version it read.

use std::collections::HashMap;

use paykit_lib::bodies::{period_details_order, PaymentRequest, PeriodDetails};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::{BillingPeriod, PeriodRef};
use paykit_lib::records::{
    ActivityRecord, ActivitySide, DestinationSource, RecordBinding, StoredMessage,
    MAX_ATTEMPTED_PERIODS, MAX_PERIOD_DETAILS,
};
use paykit_lib::{AttemptId, Body, PaymentEndpointIdentifier, Rate};
use pubky_switch_client::app::{Initiator, KeptReason, OpBatch, RecordContent, RecordRead};
use pubky_switch_core::claims::{ClaimStatus, ClaimView, Outcome};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind};

use crate::activity::{ActivityTarget, Role};
use crate::destination::Carrier;
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::{Destination, JournalEntry, KeptAmount, KeptPeriod, Stage};
use crate::pay::{
    unsupported, BlockedPayment, Choice, Chosen, DestinationFrom, PayOptions, PayOutcome,
    PaymentMode, Plan,
};
use crate::paykit::Paykit;
use crate::receipts::{received_receipt, ReceivedReceipt};
use crate::records::{conflict, KeptMessage};
use crate::requests::{cancelled, rejected, request_of, MessageDelivery, RequestRef, SentMessage};

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
    /// The record holds a proof of the period's payment, credited or not.
    pub paid: bool,
    /// The record holds the payee's receipt: the payment is credited to this period. A
    /// proof held without it is `paid` and not `credited`, such as one whose settlement
    /// credits another occurrence already, or one not confirmed yet.
    pub credited: bool,
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
            credited: record.receipt.is_some(),
            // The request's reference stays on the request's view.
            receipt: received_receipt(key, record, None),
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

/// The end of a Billing Period's automatic window (Paykit spec 8.2, "Paying periods"): the
/// earlier of the start of the next period, `period.ends_at`, and the period's attempt
/// deadline, `deadline`, when the request has a payment deadline
/// ([`PaymentRequest::attempt_deadline`]). An attempt on the period by an app's own policy
/// is made, and dispatched, only before it, judged on the highest service time verified.
pub(crate) fn automatic_window_end(period: &PeriodRef, deadline: Option<u64>) -> u64 {
    deadline.map_or(period.ends_at, |deadline| deadline.min(period.ends_at))
}

/// Whether a Billing Period's automatic window ([`automatic_window_end`]) has ended on `t`,
/// the highest service time verified (Paykit spec 8.2, "Paying periods"). With no time
/// verified, nothing shows it hasn't: it counts as ended, so nothing is dispatched by an
/// app's own policy on a time the SDK hasn't verified.
pub(crate) fn automatic_window_ended(
    period: &PeriodRef,
    deadline: Option<u64>,
    t: Option<u64>,
) -> bool {
    t.is_none_or(|t| t >= automatic_window_end(period, deadline))
}

/// Why `period` of the recurring `request` isn't paid by an app's own policy now (Paykit
/// spec 8.2, "Paying periods"), as [`not_automatic_now`] judges it, `held` the request
/// record's write the app holds, as whether a proof or a verified stub gave its version and
/// the service time it dates it at, and `t` the highest service time verified. The
/// acceptance's recorded time is that write's time, only when it is authenticated and
/// dated: a write nothing dates, or none held, pays no period automatically. `t` is taken
/// as at least that time.
///
/// # Errors
///
/// As [`not_automatic_now`].
pub(crate) fn not_automatic_for(
    request: &PaymentRequest,
    period: &BillingPeriod,
    held: Option<(bool, Option<u64>)>,
    t: Option<u64>,
) -> std::result::Result<Option<&'static str>, paykit_lib::Error> {
    let written =
        held.and_then(|(authenticated, written_at_ms)| written_at_ms.filter(|_| authenticated));
    let Some(accepted_at) = written else {
        return Ok(Some("no proof dates the request record's write"));
    };
    not_automatic_now(
        request,
        period,
        accepted_at,
        t.unwrap_or(0).max(accepted_at),
    )
}

/// Why `period` of the recurring `request` isn't paid by an app's own policy now (Paykit
/// spec 8.2, "Paying periods"), the payer's acceptance recorded at `accepted_at` and `t` the
/// highest service time verified, at least `accepted_at`; `None` when it may be:
///
/// - **From one period on:** the request's first Billing Period when the acceptance was
///   recorded before it starts, else the period containing that time, and none when that
///   time is at or after the end of the request's last period. An earlier period ended
///   before the acceptance and is paid only by hand.
/// - **Within its automatic window**, before [`automatic_window_end`]: a period whose window
///   ended is paid only by hand, so an app that accepts late or comes back after being
///   offline never pays a run of earlier periods at once. A period that hasn't started on
///   `t` passes here: no attempt is made before its start, which the attempt's own
///   condition refuses ([`Paykit::not_before`]).
///
/// # Errors
///
/// As [`PaymentRequest::attempt_deadline`], and the request's recurrence, which only a
/// caller's own value fails.
pub(crate) fn not_automatic_now(
    request: &PaymentRequest,
    period: &BillingPeriod,
    accepted_at: u64,
    t: u64,
) -> std::result::Result<Option<&'static str>, paykit_lib::Error> {
    let Some(recurrence) = request.recurrence.as_ref() else {
        return Ok(Some("not a recurring request"));
    };
    let first = recurrence.first()?;
    let from = if accepted_at < first.starts_at {
        first.index
    } else {
        if recurrence
            .last()?
            .is_some_and(|last| accepted_at >= last.ends_at)
        {
            return Ok(Some(
                "the acceptance was recorded after the last Billing Period ended",
            ));
        }
        recurrence.rule().index_at(accepted_at)?
    };
    if period.index < from {
        return Ok(Some(
            "the Billing Period ended before the acceptance was recorded",
        ));
    }
    let end = automatic_window_end(
        &PeriodRef::from(*period),
        request.attempt_deadline(Some(period))?,
    );
    if t >= end {
        return Ok(Some("the Billing Period's automatic window has ended"));
    }
    Ok(None)
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
/// conditioned (Paykit wire 5.3): 0 for no claim. `None` while an attempt is open or its
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

/// What a payment of a Billing Period needs from the period details it is paid by (Paykit
/// spec 8.2): a destination, for a later period of a request with `per_period_endpoints`,
/// and rates, for a payment priced by them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DetailsNeeds {
    pub(crate) destination: bool,
    pub(crate) rates: bool,
}

impl DetailsNeeds {
    /// Whether `details` are valid at device time `now`, before their `expires_at`, and carry
    /// all that is needed.
    fn carried_by(self, details: &StoredMessage, now: u64) -> bool {
        let Body::PaymentPeriodDetails(body) = details.body() else {
            return false;
        };
        now < body.expires_at
            && (!self.destination || body.payment_endpoints.is_some())
            && (!self.rates || body.rates.as_ref().is_some_and(|r| !r.is_empty()))
    }
}

/// The one period details a payment of a Billing Period is paid by by default, of `held`,
/// latest first (Paykit spec 8.2): the latest valid ones carrying what it `needs`, so a
/// destination and rates never come from two details. With nothing needed, the latest valid
/// ones carrying a destination, the period's own, or none: the request's destination is paid
/// then.
///
/// # Errors
///
/// [`Refusal::WaitingForPeriodDetails`] when something is needed and no valid details carry
/// it all: the payer waits for them.
pub(crate) fn details_paid_by(
    held: &[StoredMessage],
    now: u64,
    needs: DetailsNeeds,
) -> std::result::Result<Option<&StoredMessage>, Refusal> {
    if !needs.destination && !needs.rates {
        let destination = DetailsNeeds {
            destination: true,
            rates: false,
        };
        return Ok(held.iter().find(|d| destination.carried_by(d, now)));
    }
    held.iter()
        .find(|d| needs.carried_by(d, now))
        .map(Some)
        .ok_or(Refusal::WaitingForPeriodDetails)
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

/// The payer's period record of `period` of `request` holding no message, only the members
/// that name it (Paykit wire 5.3): the record a mark creates, and that a payment by policy's
/// batch creates for a period that has none yet.
fn bare_period_record(request: &RequestRef, period: &PeriodRef) -> ActivityRecord {
    let mut record = crate::activity::empty(ActivitySide::Payer, request.payee);
    record.request_hash = Some(request.request_hash);
    record.billing_period = Some(*period);
    record
}

/// The refusal of a write over a period record held as a value the app can't read: not now,
/// never written over (Paykit spec 8.1).
fn period_record_unread() -> PaykitError {
    Refusal::NotNow("Paykit wire 5.3: the period record held doesn't read".into()).into()
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

    /// Whether the app holds, though it may not open or bind it, the payee's request record
    /// a period record `bound` at `target` belongs to: any content but a delete.
    pub(crate) async fn request_record_held(
        &self,
        target: &ActivityTarget,
        bound: &ActivityRecord,
    ) -> Result<bool> {
        let Ok(RecordBinding::Period { request_hash, .. }) = bound.binding() else {
            return Ok(false);
        };
        let request = ActivityTarget::payee(
            natural_key::request_record(&self.identity(), &request_hash),
            target.label().as_ref(),
        );
        Ok(self
            .app
            .shared_record(&self.activity_key(&request)?)
            .await?
            .is_some_and(|held| held.content != RecordContent::Deleted))
    }

    /// Pay one Billing Period, `period`, of the subscription `request` (Paykit spec 7.2,
    /// 8.2): the paying order of every payment, on the period's obligation, its proof naming
    /// the period and the period details it was paid by. Accepting a subscription pays
    /// nothing: the host calls this for each period. `by` says who decided it:
    ///
    /// - [`Initiator::User`]: the user confirmed paying this period, as [`Paykit::pay_request`]
    ///   pays a one-time request, with the same confirmations in `options`.
    /// - [`Initiator::Automatic`]: the app pays it by its own policy, without asking its user,
    ///   which spec 8.2 leaves to the app as v0.1 did, only for a request never accepted under
    ///   an Allowance, neither the period nor the request marked manual-only (Paykit spec
    ///   8.3), and only from one period on and within the period's automatic window: from
    ///   the request's first Billing Period when the acceptance was recorded before it
    ///   starts, else from the one containing the time it was recorded, and each before the
    ///   earlier of the next period's start and, with a payment deadline, the period's
    ///   attempt deadline, judged on the highest service time verified, at the attempt and
    ///   again before the dispatch. A period past its window is paid by hand. It is an
    ///   automatic payment for every rule that names one: the deny list and the request
    ///   record read fresh before the attempt and again before the dispatch, the clock
    ///   bound, no doubtful claim, no request held after a restore, no record divergent or
    ///   seen so, no destination whose transfer isn't the amount requested, and no payment
    ///   priced by rates. Its batch writes the period record at the version read, creating
    ///   it when there is none, so a mark another own app wrote since fails it. `options`
    ///   carries no user confirmation then. Its proof carries no `allowance_id`: no
    ///   Allowance is involved.
    ///
    /// The destination: one period details the payer's period record holds, the ones its
    /// proof names, by default the latest unexpired ones carrying what the payment needs
    /// (`options.period_details` names others), else the request's own, which serve only the
    /// first period when the request has `per_period_endpoints`. A period that hasn't started on the latest service time
    /// verified is paid only by the user, after confirming paying it early
    /// ([`PayOptions::with_early_payment_confirmed`]).
    ///
    /// # Errors
    ///
    /// As [`Paykit::pay_request`], and: [`Refusal::Usage`] for a one-time request, a period
    /// that isn't one of the request's, or a payment by policy that carries a user's
    /// confirmation; [`Refusal::ManualOnly`] for a payment by policy of a request that names
    /// an Allowance, or of a period marked manual-only, on its record or its request's;
    /// [`Refusal::NotAutomatic`] for a payment by policy of a period before the first one
    /// the acceptance lets be paid automatically, or past its automatic window, nothing
    /// written, and, once its attempt is journaled, [`Refusal::AttemptFailed`] with it for
    /// a window that ended before the dispatch; [`Refusal::Divergent`] while the request
    /// record or the period record is divergent or was seen so, by policy always and by
    /// hand until the user confirms it; [`Refusal::NotSubscribed`] before the
    /// subscription is accepted;
    /// [`Refusal::AlreadyPaid`] for a period whose record holds its proof;
    /// [`Refusal::PeriodNotStarted`]; [`Refusal::AttemptedPeriodsFull`];
    /// [`Refusal::WaitingForPeriodDetails`] for a later period of a request with
    /// `per_period_endpoints` and no valid period details with its destination yet; [`Refusal::NotNow`] while the period
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
        self.may_attempt(automatic).map_err(crate::pay::not_now)?;
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
        // Divergent after a provider loss, or seen so: the request record pauses every
        // period of the request, its upload maybe holding a cancellation, a `manual_only`
        // mark or a reassociation, and the period record its own period. No payment by
        // policy while either view shows it so or it is paused, and none by hand until the
        // user has seen that the payee may have cancelled it (Paykit spec 6); a period past
        // its deadline, which no confirmation lets be paid, is refused for its deadline
        // instead, as a one-time request is.
        let record_key = self.activity_key(&target)?;
        let period_record_key = self.activity_key(&period_target)?;
        let deadline = body.attempt_deadline(Some(&billing))?;
        if automatic {
            for key in [record_key, period_record_key] {
                if Box::pin(self.pause_at(&key)).await?.blocks_automatic() {
                    return Err(self.divergent_refusal(key).await?);
                }
            }
        } else if !self.deadline_passed(deadline) {
            // As for a one-time request, a period whose destination's app is no longer
            // current, or that requires an app the payee removed, is refused for that instead
            // of paused: no confirmation could let it be paid (Paykit spec 6). The app judged
            // is the one of the carrier that would pay this period: the period details it
            // would be paid by, whichever payee app sent them, else the request's own
            // destination. With no carrier for it yet, such as a later period of a request
            // with `per_period_endpoints` waiting for its details, no app is judged.
            let mut paused = false;
            for key in [record_key, period_record_key] {
                paused |= Box::pin(self.pause_at(&key)).await?.blocks_by_hand();
            }
            if paused {
                let details = match self.period_details_for(
                    &body,
                    &billing,
                    held_period.as_ref(),
                    &options,
                ) {
                    Ok(details) => Some(details),
                    Err(error) if error.refusal().is_some() => None,
                    Err(error) => return Err(error),
                };
                let from = match &details {
                    Some(Some(details)) => Some(DestinationFrom::Endpoints(details)),
                    Some(None) => DestinationFrom::of_request(&stored, &body.destination),
                    None => None,
                };
                Box::pin(self.refuse_unpayable(&request.payee, from, &options)).await?;
            }
            Box::pin(self.refuse_while_paused(&[record_key, period_record_key])).await?;
        }
        let end_hold = if automatic {
            // A request accepted under an Allowance is paid automatically only under that
            // one, never by the app's policy: with it ended, each occurrence is paid by hand
            // (Paykit spec 8.3, "One Allowance per request"). A period marked manual-only,
            // or any period of a request whose record holds the mark, is paid by hand too
            // (Paykit spec 8.2, 8.3, "Manual only").
            let under_allowance =
                record.allowance_id.is_some() || !record.reassociations.is_empty();
            let marked = record.manual_only || held_period.as_ref().is_some_and(|r| r.manual_only);
            if under_allowance || marked {
                return Err(Refusal::ManualOnly.into());
            }
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
        let restored = self.restored_attempts(&obligation).await?;
        let step = self
            .claim_step(&obligation, deadline, restored, &options)
            .await?;
        if self.deadline_passed(deadline) {
            return Err(self.past_deadline(&target, None).await?);
        }
        // Paid by policy only from one period on and within the period's automatic window,
        // judged on the service time the claim's read just verified (Paykit spec 8.2).
        if automatic {
            self.automatic_window(&record_key, &body, &billing).await?;
        }
        let conditions = vec![self.not_before(&period, &options)?];
        let mut record_writes = Vec::new();
        if let Some(write) = self
            .attempted_write(&target, &record, request, period.start_secs())
            .await?
        {
            record_writes.push(write);
        }
        if automatic {
            record_writes.push(
                self.policy_period_write(&period_target, held_period.as_ref(), request, &period)
                    .await?,
            );
        }
        let (destination, carrier, reserve, bound) = self
            .period_pay_destination(
                &request.payee,
                &stored,
                &body,
                &billing,
                held_period.as_ref(),
                &options,
                by,
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
            receipted: false,
            allowance_id: None,
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
            conditions,
            record_writes,
            by,
            unconfirmed: options.unconfirmed_identity_confirmed,
            end_hold,
            executor: None,
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

    /// A payment of Billing Period `billing` of the recurring request `body` by the app's own
    /// policy is made only from one period on and within the period's automatic window
    /// ([`not_automatic_for`], Paykit spec 8.2, "Paying periods"), judged on the time the
    /// payer's acceptance was recorded and the highest service time verified, at least that
    /// time. The acceptance's recorded time is the service time of the write of the request
    /// record, at `record_key`, that the app holds, as a proof or a verified stub dates it:
    /// a write nothing dates pays nothing automatically.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotAutomatic`] naming the rule, nothing written: the period stays payable
    /// by hand; the store's errors.
    async fn automatic_window(
        &self,
        record_key: &[u8; 32],
        body: &PaymentRequest,
        billing: &BillingPeriod,
    ) -> Result<()> {
        // The write held, not the acceptance's own: any write of the request record after
        // the acceptance is at or after it, and every period that ended before that write's
        // time has a window that ended too, so the periods payable automatically are the
        // same as from the acceptance's own time.
        let held = self
            .app
            .shared_record(record_key)
            .await?
            .map(|held| (held.authenticated, held.written_at_ms));
        match not_automatic_for(body, billing, held, self.service_time())? {
            Some(why) => Err(Refusal::NotAutomatic(why.into()).into()),
            None => Ok(()),
        }
    }

    /// The period record's write in the batch of an attempt on `period` by the app's own
    /// policy (Paykit spec 8.2, "Paying periods"): at the version read, as an executor's
    /// batch writes its record, so a `manual_only` another own app marked since, or a period
    /// record another created meanwhile, fails the batch. The record `held` shows the
    /// payment as being handled; with none, it is created holding no message (Paykit wire
    /// 5.3).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] for a period record held whose value doesn't read, never written
    /// over; the client's errors.
    async fn policy_period_write(
        &mut self,
        target: &ActivityTarget,
        held: Option<&ActivityRecord>,
        request: &RequestRef,
        period: &PeriodRef,
    ) -> Result<(Vec<Condition>, OpKind)> {
        let next = match held {
            Some(record) => {
                let mut next = record.clone();
                // A record holding no message carries no status (Paykit wire 5.3).
                if !next.stored().is_empty() {
                    next.status = Some("paying".into());
                }
                next
            }
            None => {
                let content = self
                    .app
                    .shared_record(&self.activity_key(target)?)
                    .await?
                    .map(|held| held.content);
                if !matches!(content, None | Some(RecordContent::Deleted)) {
                    return Err(period_record_unread());
                }
                bare_period_record(request, period)
            }
        };
        Ok(self
            .app
            .record_op(&target.record_target(), Some(&next.to_json()?), Vec::new())
            .await?)
    }

    /// Mark the Billing Period `period` of the recurring request `request` manual-only on
    /// its period record (Paykit spec 8.3, "Manual only"; Paykit wire 5.3): no app pays the
    /// period automatically once the write lands, an executor or an app by its own policy.
    /// The mark is never lifted. The record is written at the version read; a period with
    /// no period record yet, one not started among them, is marked by the write that
    /// creates its record, holding the mark and no message, built on no version, so an
    /// automatic attempt's batch that creates the record meanwhile makes one of the two
    /// fail. A mark that fails so is built again on the record read again, at most three
    /// times in all. A write kept unanswered returns `Ok` and lands with the app's
    /// next write or read, before any batch of an automatic attempt, which sends kept
    /// batches first; a kept batch the client later drops leaves no mark. A whole
    /// subscription is marked on its request record ([`Paykit::mark_manual_only`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a request this app doesn't hold; [`Refusal::Usage`] for a
    /// one-time request, or a period that isn't one of the request's; [`Refusal::NotNow`]
    /// while the period record's value waits for its scope key, or is held as a value that
    /// doesn't read, never written over; [`PaykitError::Conflict`] when the record kept
    /// changing; the client's errors.
    pub async fn mark_period_manual_only(
        &mut self,
        request: &RequestRef,
        period: &PeriodRef,
    ) -> Result<()> {
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_pay()?;
            let (_, record) = self.payer_request(request).await?;
            let (_, body) = request_of(&record).ok_or(Refusal::NotFound)?;
            if !body.is_recurring() {
                return Err(Refusal::Usage(
                    "Paykit spec 8.3: a one-time request is marked with mark_manual_only".into(),
                )
                .into());
            }
            let billing = body.named_period(period).map_err(|_| {
                Refusal::Usage("Paykit wire 4.4: not a Billing Period of the request".into())
            })?;
            let period = PeriodRef::from(billing);
            let target =
                ActivityTarget::payer(period_key(&request.payee, &request.request_hash, &period));
            let key = self.activity_key(&target)?;
            let mut tries = 0;
            loop {
                let content = self.app.shared_record(&key).await?.map(|held| held.content);
                if content.as_ref().is_some_and(crate::hold::awaits_key) {
                    return Err(crate::hold::value_awaits_key());
                }
                let next = match self.held_activity(&target).await? {
                    Some(held) if held.manual_only => return Ok(()),
                    Some(mut held) => {
                        held.manual_only = true;
                        held
                    }
                    None if matches!(content, None | Some(RecordContent::Deleted)) => {
                        let mut bare = bare_period_record(request, &period);
                        bare.manual_only = true;
                        bare
                    }
                    None => return Err(period_record_unread()),
                };
                let write = self
                    .app
                    .record_op(&target.record_target(), Some(&next.to_json()?), Vec::new())
                    .await?;
                match self.app.submit_batch(OpBatch::new().entry(write)).await {
                    Ok(_) => return Ok(()),
                    Err(error) if error.kept_batch().is_some() => return Ok(()),
                    Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                        tries += 1;
                        if tries >= WRITE_TRIES {
                            return Err(conflict(error.into()));
                        }
                        self.app.read_record(&target.record_target()).await?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await
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
    /// `body` (Paykit spec 8.2): the period details it is paid by, which the proof names, the
    /// ones `options` names by `msg_id` or by default the ones [`details_paid_by`] picks
    /// among those the payer's period record, `held`, holds for a period that takes details
    /// now ([`takes_details`]), which is the only place a payer
    /// pays details from, so a proof names only details its record can store (Paykit wire
    /// 5.3); without such details, the request's own destination, which serves only the first
    /// period of a request with `per_period_endpoints`, looked up as `by` starts the payment.
    #[allow(clippy::too_many_arguments)]
    async fn period_pay_destination(
        &mut self,
        payee: &[u8; 32],
        stored: &StoredMessage,
        body: &PaymentRequest,
        billing: &BillingPeriod,
        held: Option<&ActivityRecord>,
        options: &PayOptions,
        by: Initiator,
    ) -> Result<Chosen> {
        if let Some(details) = self.period_details_for(body, billing, held, options)? {
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
        let accepted = crate::executor::accepted_ids(
            body.accepted_payment_endpoint_identifiers.as_deref(),
            None,
        );
        self.request_destination(payee, stored, body, options, accepted, by)
            .await
    }

    /// The period details a payment of Billing Period `billing` of the request `body` is
    /// paid by (Paykit spec 8.2): the ones `options` names by `msg_id`, or by default the
    /// ones [`details_paid_by`] picks, among those the payer's period record, `held`, holds
    /// for a period that takes details now ([`takes_details`]). `None` when none are: the
    /// request's own destination pays the period then.
    ///
    /// # Errors
    ///
    /// [`Refusal::NoDestination`] for details `options` names that the record doesn't hold
    /// unexpired with a destination; [`Refusal::WaitingForPeriodDetails`] as
    /// [`details_paid_by`].
    fn period_details_for(
        &self,
        body: &PaymentRequest,
        billing: &BillingPeriod,
        held: Option<&ActivityRecord>,
        options: &PayOptions,
    ) -> Result<Option<StoredMessage>> {
        let now = self.now();
        // Only details of a period that takes them now, the periods the payer stores them
        // for ([`takes_details`]), are paid by.
        let paid = held.is_some_and(ActivityRecord::shows_paid);
        let takes = matches!(
            takes_details(body, billing, paid, self.service_time()),
            Ok(Window::Takes)
        );
        let mut details: Vec<StoredMessage> = held
            .filter(|_| takes)
            .map(|record| record.details().into_iter().cloned().collect())
            .unwrap_or_default();
        details.sort_by(|a, b| period_details_order(b.message(), a.message()));
        let first = body
            .recurrence
            .as_ref()
            .and_then(|r| r.first().ok())
            .is_some_and(|first| first == *billing);
        // Rates price no payment yet: each one is to an endpoint of the requested asset.
        let needs = DetailsNeeds {
            destination: body.per_period_endpoints && !first,
            rates: false,
        };
        Ok(match &options.period_details {
            Some(msg_id) => Some(
                details
                    .iter()
                    .find(|d| &d.message().msg_id == msg_id)
                    .filter(|d| {
                        let needs = DetailsNeeds {
                            destination: true,
                            ..needs
                        };
                        needs.carried_by(d, now)
                    })
                    .cloned()
                    .ok_or_else(|| {
                        Refusal::NoDestination(
                            "Paykit spec 8.2: the period record holds no such unexpired period \
                             details with a destination"
                                .into(),
                        )
                    })?,
            ),
            None => details_paid_by(&details, now, needs)?.cloned(),
        })
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
        self.records_known().await?;
        self.require_payee()?;
        // Period details go to no payer whose sends are held (Paykit spec 8.1, "Blocked
        // counterparties"), refused before anything is signed, written or queued: they are
        // sent once the payer is unblocked or the lists are read.
        let (_, held) = self.payee_record(record).await?;
        self.refuse_while_sends_held(&held.counterparty).await?;
        let mut tries = 0;
        let msg_id = loop {
            // Judged on what the switch holds now: the request record read again shows a
            // rejection or a cancellation another own app recorded meanwhile, and it and the
            // period record read again below verify the service's time now, which the
            // period-details window is judged on (Paykit spec 8.2): a time verified a while
            // ago can show a period's deadline not passed yet.
            let (target, _) = self.payee_record(record).await?;
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
            match self.app.read_record_fresh(&into.record_target()).await? {
                RecordRead::Fresh(_)
                | RecordRead::Kept {
                    reason: KeptReason::NotFound | KeptReason::NotNewer,
                    ..
                } => {}
                RecordRead::Kept {
                    reason: KeptReason::Unreachable(why),
                    ..
                } => return Err(Refusal::SwitchUnavailable(why).into()),
                _ => return Err(period_record_unread()),
            }
            let held = match self
                .app
                .shared_record(&self.activity_key(&into)?)
                .await?
                .map(|held| held.content)
            {
                None | Some(RecordContent::Deleted) => None,
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
            let signed = self.sign(
                bound.counterparty,
                Kind::PaymentPeriodDetails,
                vec![request_hash],
                body,
            )?;
            let msg_id = signed.msg_id().to_owned();
            match self
                .send_with_record(signed, &into, Role::PeriodDetails, Vec::new(), Vec::new())
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
            Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]),
            Party::new("bob", "shop", &["paykit_request"]),
        )
    }

    fn claim(state: &str, outcome: &str) -> ClaimView {
        let open = state == "ATTEMPTING";
        let owner = "4ppx81nsrtmpoke6yjfueg957h";
        let attempt = "1OjeFqsP4dPATeMe9xu5-Q";
        ClaimView::from_value(&json!({
            "attempt_id": if open { json!(attempt) } else { json!(null) },
            "attempts": [{ "attempt_id": attempt, "duplicate": false,
                "opened_at": "2026-01-01T00:00:00.000Z", "orphan_after": 86400,
                "orphaned": false, "outcome": outcome, "owner": owner,
                "possibly_duplicate": false }],
            "claim_key": "QZGfAHpm0NnUuyVE9zAHkr-1o0_2ofsJwVJJ2KQ8NCU",
            "deadline": null, "divergent": false, "next_possibly_duplicate": false,
            "owner": if open { json!(owner) } else { json!(null) },
            "state": state, "step": 3, "tombstone": false
        }))
        .unwrap()
    }

    /// Details from bob for January of request `[7; 32]`, with a destination, rates, or both,
    /// valid until `expires_at`.
    fn carrying(
        bob: &Party,
        alice: &Party,
        endpoints: bool,
        rates: bool,
        expires_at: u64,
    ) -> StoredMessage {
        let mut body = json!({
            "billing_period": {
                "starts_at": "2026-02-28T00:00:00.000Z",
                "ends_at": "2026-03-31T00:00:00.000Z"
            },
            "expires_at": pubky_switch_core::time::rfc3339(expires_at),
        });
        if endpoints {
            body["payment_endpoints"] = json!({ "btc-lightning-bolt11": "lnbc1period" });
        }
        if rates {
            body["rates"] = json!([{ "asset": "usd", "value": "50000" }]);
        }
        bob.message(
            alice,
            "paykit.payment_period_details",
            vec![[7; 32]],
            object(body),
        )
    }

    #[test]
    fn test_periods_details_paid_by_the_latest_valid_carrying_what_is_needed() {
        let (alice, bob) = parties();
        let now = 1_000;
        // Latest first: rates only, then expired with both, then a destination, then both.
        let held = [
            carrying(&bob, &alice, false, true, now + 1),
            carrying(&bob, &alice, true, true, now),
            carrying(&bob, &alice, true, false, now + 1),
            carrying(&bob, &alice, true, true, now + 1),
        ];
        let needs = |destination, rates| DetailsNeeds { destination, rates };
        let hash = |d: Option<&StoredMessage>| d.map(StoredMessage::hash);
        // Nothing needed: the latest valid with a destination, the period's own.
        assert_eq!(
            hash(details_paid_by(&held, now, needs(false, false)).unwrap()),
            Some(held[2].hash())
        );
        assert_eq!(
            hash(details_paid_by(&held, now, needs(true, false)).unwrap()),
            Some(held[2].hash())
        );
        assert_eq!(
            hash(details_paid_by(&held, now, needs(false, true)).unwrap()),
            Some(held[0].hash())
        );
        // Both needed: one details carrying both, never one's destination and another's rates.
        assert_eq!(
            hash(details_paid_by(&held, now, needs(true, true)).unwrap()),
            Some(held[3].hash())
        );
        let apart = [held[0].clone(), held[2].clone()];
        assert_eq!(
            details_paid_by(&apart, now, needs(true, true)).map(hash),
            Err(Refusal::WaitingForPeriodDetails)
        );
        // Nothing valid: none when nothing is needed, a wait otherwise.
        assert_eq!(
            details_paid_by(&held, now + 1, needs(false, false)).map(hash),
            Ok(None)
        );
        assert_eq!(
            details_paid_by(&held, now + 1, needs(true, false)).map(hash),
            Err(Refusal::WaitingForPeriodDetails)
        );
    }

    #[test]
    fn test_periods_attempted_with_adds_in_order_and_drops_finished() {
        assert_eq!(attempted_with(&[], 5, &[]), Some(vec![5]));
        assert_eq!(attempted_with(&[2, 9], 5, &[]), Some(vec![2, 5, 9]));
        assert_eq!(attempted_with(&[2, 9], 5, &[2]), Some(vec![5, 9]));
        // 16 listed: another goes only once one of them finished.
        let full: Vec<u64> = (1..=16).collect();
        assert_eq!(attempted_with(&full, 20, &[]), None);
        let mut freed: Vec<u64> = (2..=16).collect();
        freed.push(20);
        assert_eq!(attempted_with(&full, 20, &[1]), Some(freed));
    }

    #[test]
    fn test_periods_finished_step_only_without_an_open_or_unknown_attempt() {
        assert_eq!(finished_step(None), Some(0));
        assert_eq!(finished_step(Some(&claim("FREE", "failed"))), Some(3));
        assert_eq!(finished_step(Some(&claim("SETTLED", "done"))), Some(3));
        assert_eq!(finished_step(Some(&claim("ATTEMPTING", "open"))), None);
        assert_eq!(finished_step(Some(&claim("FREE", "unknown"))), None);
        // `unknown` counts as open, on a claim settled since by another attempt too.
        let mut settled = claim("SETTLED", "done");
        let mut other = settled.attempts[0].clone();
        other.attempt_id = [2; 16];
        other.outcome = Outcome::Unknown;
        settled.attempts.push(other);
        assert_eq!(finished_step(Some(&settled)), None);
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

    /// A monthly request anchored at 2026-01-01 and starting 2026-01-15, with `extra` members
    /// in its body.
    fn monthly_body(extra: serde_json::Value) -> PaymentRequest {
        let (alice, bob) = parties();
        let mut body = json!({
            "amount": { "asset": "btc", "value": "1" },
            "payment_reference": "plan-1",
            "proposal_expires_at": null,
            "recurrence": {
                "anchor": "2026-01-01T00:00:00.000Z", "ends_at": null, "every": 1,
                "starts_at": "2026-01-15T00:00:00.000Z", "unit": "month"
            }
        });
        for (name, value) in extra.as_object().unwrap() {
            body[name] = value.clone();
        }
        let stored = bob.message(&alice, "paykit.payment_request", Vec::new(), object(body));
        let Body::PaymentRequest(request) = stored.body() else {
            panic!("not a request");
        };
        request.clone()
    }

    const DEC_15: u64 = 1_765_756_800_000;
    const JAN_1: u64 = 1_767_225_600_000;
    const FEB_1: u64 = 1_769_904_000_000;
    const MAR_1: u64 = 1_772_323_200_000;
    const DAY: u64 = 86_400_000;

    fn period_at(request: &PaymentRequest, t: u64) -> BillingPeriod {
        request.recurrence.unwrap().rule().containing(t).unwrap()
    }

    #[test]
    fn test_periods_automatic_from_the_first_period_for_an_acceptance_before_it() {
        // Accepted before the first period starts: the first is paid automatically, the one
        // containing `starts_at`, from its start boundary (Paykit spec 8.2's example).
        let request = monthly_body(json!({}));
        let january = period_at(&request, JAN_1);
        assert_eq!(january.starts_at, JAN_1);
        assert_eq!(
            not_automatic_now(&request, &january, DEC_15, JAN_1 + 20 * DAY).unwrap(),
            None
        );
        // Its window ends at the next period's start.
        assert!(not_automatic_now(&request, &january, DEC_15, FEB_1)
            .unwrap()
            .is_some_and(|why| why.contains("window")));
    }

    #[test]
    fn test_periods_automatic_from_the_period_containing_the_acceptance() {
        // Accepted on 2026-02-10: January ended before it and is paid only by hand; February,
        // which contains it, is paid automatically until March starts, and March isn't
        // judged here before its start.
        let request = monthly_body(json!({}));
        let accepted = FEB_1 + 9 * DAY;
        let (january, february, march) = (
            period_at(&request, JAN_1),
            period_at(&request, FEB_1),
            period_at(&request, MAR_1),
        );
        assert!(not_automatic_now(&request, &january, accepted, accepted)
            .unwrap()
            .is_some_and(|why| why.contains("before the acceptance")));
        assert_eq!(
            not_automatic_now(&request, &february, accepted, accepted).unwrap(),
            None
        );
        assert_eq!(
            not_automatic_now(&request, &march, accepted, accepted).unwrap(),
            None
        );
        assert!(not_automatic_now(&request, &february, accepted, MAR_1)
            .unwrap()
            .is_some_and(|why| why.contains("window")));
    }

    #[test]
    fn test_periods_automatic_window_ends_at_the_attempt_deadline_when_earlier() {
        // Each period due a day after its start: February's window ends then, its own
        // millisecond the last within it, well before March starts.
        let request = monthly_body(json!({
            "payment_deadline": { "type": "period_start", "seconds": 86_400 }
        }));
        let february = period_at(&request, FEB_1);
        assert_eq!(
            automatic_window_end(
                &PeriodRef::from(february),
                request.attempt_deadline(Some(&february)).unwrap()
            ),
            FEB_1 + DAY + 1
        );
        assert_eq!(
            not_automatic_now(&request, &february, FEB_1, FEB_1 + DAY).unwrap(),
            None
        );
        assert!(
            not_automatic_now(&request, &february, FEB_1, FEB_1 + DAY + 1)
                .unwrap()
                .is_some_and(|why| why.contains("window"))
        );
        // Without a deadline, the next period's start.
        let february = PeriodRef::from(february);
        assert_eq!(automatic_window_end(&february, None), MAR_1);
        assert_eq!(automatic_window_end(&february, Some(u64::MAX)), MAR_1);
    }

    #[test]
    fn test_periods_automatic_none_once_accepted_after_the_last_period_ended() {
        // The last period is February's, ended at March's start: an acceptance recorded then
        // lets no period be paid automatically.
        let request = monthly_body(json!({
            "recurrence": {
                "anchor": "2026-01-01T00:00:00.000Z", "ends_at": "2026-03-01T00:00:00.000Z",
                "every": 1, "starts_at": "2026-01-15T00:00:00.000Z", "unit": "month"
            }
        }));
        let february = period_at(&request, FEB_1);
        assert_eq!(
            not_automatic_now(&request, &february, MAR_1 - 1, MAR_1 - 1).unwrap(),
            None
        );
        assert!(not_automatic_now(&request, &february, MAR_1, MAR_1)
            .unwrap()
            .is_some_and(|why| why.contains("last Billing Period")));
    }

    #[test]
    fn test_periods_automatic_only_on_an_authenticated_dated_write() {
        // The acceptance counts as recorded at the request record's write only when a proof or
        // a verified stub dates it: an unauthenticated write, an undated one, or none held,
        // pays no period automatically (Paykit spec 8.2, "Paying periods").
        let request = monthly_body(json!({}));
        let january = period_at(&request, JAN_1);
        let t = Some(JAN_1 + DAY);
        assert_eq!(
            not_automatic_for(&request, &january, Some((true, Some(DEC_15))), t).unwrap(),
            None
        );
        for held in [Some((false, Some(DEC_15))), Some((true, None)), None] {
            assert!(
                not_automatic_for(&request, &january, held, t)
                    .unwrap()
                    .is_some_and(|why| why.contains("no proof dates")),
                "{held:?}"
            );
        }
    }

    #[test]
    fn test_periods_automatic_window_ended_without_a_verified_time() {
        // With no service time verified, nothing shows the window hasn't ended: no dispatch by
        // policy goes on (Paykit spec 8.2, "Paying periods").
        let request = monthly_body(json!({}));
        let january = PeriodRef::from(period_at(&request, JAN_1));
        assert!(automatic_window_ended(&january, None, None));
        assert!(!automatic_window_ended(&january, None, Some(FEB_1 - 1)));
        assert!(automatic_window_ended(&january, None, Some(FEB_1)));
        assert!(!automatic_window_ended(
            &january,
            Some(JAN_1 + DAY),
            Some(JAN_1 + DAY - 1)
        ));
        assert!(automatic_window_ended(
            &january,
            Some(JAN_1 + DAY),
            Some(JAN_1 + DAY)
        ));
    }
}
