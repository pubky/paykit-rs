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
    ActivityRecord, ActivitySide, RecordBinding, StoredMessage, MAX_PERIOD_DETAILS,
};
use paykit_lib::{Body, PaymentEndpointIdentifier, Rate};
use pubky_switch_client::app::{KeptReason, RecordContent, RecordRead};
use pubky_switch_core::http::ErrorCode;

use crate::activity::{ActivityTarget, Role};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::receipts::{received_receipt, ReceivedReceipt};
use crate::records::conflict;
use crate::requests::{cancelled, rejected, request_of, MessageDelivery, SentMessage};

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
    /// written or queued while sends to the payer are held: at an app holding `contacts`,
    /// while the copy of the deny list holds the payer, or while that copy isn't known, which
    /// a read of the lists is tried for first (Paykit spec 8.1, "Blocked counterparties"), as
    /// a new request is held. An app without `contacts` reads no lists, and holds nothing.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::NotFound`] for a
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
