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
//!   accepted and that runs, and only for one of the 16 latest periods to have started,
//!   or the first that hasn't ([`stores_details`]); the payee sends them only for such a
//!   period.
//! - **Views:** a request's view lists the period records the app holds for it
//!   ([`PeriodView`]), each with its details, latest first.

use std::collections::HashMap;

use paykit_lib::bodies::{period_details_order, PaymentRequest, PeriodDetails};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::{BillingPeriod, PeriodRef, Recurrence};
use paykit_lib::records::{
    ActivityRecord, ActivitySide, RecordBinding, StoredMessage, MAX_PERIOD_DETAILS,
};
use paykit_lib::{Body, PaymentEndpointIdentifier, Rate};
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

/// How many of the latest Billing Periods to have started take period details (Paykit
/// spec 8.2): a payer that fell behind can still pay an earlier one.
pub(crate) const STARTED_PERIODS_WITH_DETAILS: i64 = 16;

/// Whether the payer stores the period details `stored` for the request `record` holds
/// (Paykit spec 8.2), the one rule every path that stores them follows: at receive, once a
/// request record comes for details kept before it, and when a write the switch refused is
/// made later.
///
/// - **A subscription running:** only for a request the payer accepted, as v0.1 holds
///   details before acceptance invalid, and that neither side rejected or cancelled, nor
///   is `stopped` by a payee's cancellation the app keeps and hasn't placed yet (Paykit
///   spec 8.1, the kept-copy stop): a subscription that runs no more has no periods to pay.
/// - **A period that takes details:** one of the latest periods to have started, or the
///   first that hasn't, on `t`, the highest service time verified ([`takes_details`]). So
///   a payee can't make the payer hold records for periods without bound.
pub(crate) fn stores_details(
    record: &ActivityRecord,
    stored: &StoredMessage,
    t: Option<u64>,
    stopped: bool,
) -> bool {
    if record.response.is_none() || rejected(record) || cancelled(record) || stopped {
        return false;
    }
    let Some((_, request)) = request_of(record) else {
        return false;
    };
    let (Some(recurrence), Some(Ok(period))) = (
        request.recurrence.as_ref(),
        stored
            .body()
            .billing_period()
            .map(|named| request.named_period(named)),
    ) else {
        return false;
    };
    takes_details(recurrence, &period, t)
}

/// Whether `period` of `recurrence` takes period details on `t`, the highest service time
/// verified (Paykit spec 8.2): one of the [`STARTED_PERIODS_WITH_DETAILS`] latest periods to
/// have started, or the first that hasn't. The payee sends details only for such a period,
/// and the payer stores them only for one, by this same rule. A period's payment deadline
/// doesn't bound it. With no time verified yet, no period has started, as for every time
/// the SDK judges on the service's clock, never on the device's: only the request's first
/// period takes details then.
pub(crate) fn takes_details(
    recurrence: &Recurrence,
    period: &BillingPeriod,
    t: Option<u64>,
) -> bool {
    let Ok(first) = recurrence.first() else {
        return false;
    };
    // The latest period to have started, the request's last once it has ended; none
    // before the first starts.
    let last = recurrence.last().ok().flatten().map(|last| last.index);
    let latest = t
        .and_then(|t| recurrence.rule().index_at(t).ok())
        .map(|k| last.map_or(k, |last| k.min(last)))
        .filter(|k| *k >= first.index);
    match latest {
        None => period.index == first.index,
        Some(k) => {
            Some(period.index) == k.checked_add(1)
                || (period.index <= k
                    && k.saturating_sub(period.index) < STARTED_PERIODS_WITH_DETAILS)
        }
    }
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

    /// The payee sends the payer period details for one Billing Period of the recurring
    /// request whose `payee_activity` record is `record` (Paykit spec 8.2, Paykit wire 4.9):
    /// the period's destination, its conversion rates, or both, valid until
    /// `details.expires_at`. They are written into the payee's period record in the batch
    /// that queues them, conditioned on the version read, and sent once it is applied; a
    /// record that changed meanwhile is read again and the write built again. Each details
    /// stays valid until its own expiry: a later one replaces none. They are sent only for
    /// a period that takes details, the periods the payer stores them for: one of the 16
    /// latest to have started, or the next one, on the highest service time verified
    /// ([`takes_details`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::NotFound`] for a
    /// record this app doesn't hold, or a period record; [`Refusal::Usage`] for a one-time
    /// request, or a period that holds 8 details already; [`Refusal::NotSubscribed`] before
    /// the payer's acceptance; [`Refusal::Rejected`] or [`Refusal::Cancelled`];
    /// [`crate::PaykitError::Lib`] with `Validation` for details that break Paykit wire 4.9 against
    /// the request, such as a period that isn't one of its own, one that doesn't take
    /// details now, or details that expired by the device clock; [`Refusal::NotNow`] while the
    /// period record's value waits for its scope key; [`crate::PaykitError::Conflict`] when
    /// the record kept changing; the client's errors. A failure that leaves the batch kept
    /// (the network, `429`, `503`) is no error: the details are reported sent,
    /// [`MessageDelivery::Kept`], so the host doesn't send them again.
    pub async fn send_period_details(
        &mut self,
        record: &[u8; 32],
        details: PeriodDetails,
    ) -> Result<SentMessage> {
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
            // Only for a period that takes details, the periods the payer stores them for,
            // on the highest service time verified (Paykit spec 8.2). A
            // time the app verified a while ago may not show the period started: the
            // request record is read again, which verifies the service's time now.
            let period = request.named_period(&details.billing_period)?;
            let recurrence = request.recurrence.ok_or(Refusal::NotFound)?;
            let request_hash = stored.hash();
            if !takes_details(&recurrence, &period, self.service_time()) {
                self.app.read_record(&target.record_target()).await?;
            }
            if !takes_details(&recurrence, &period, self.service_time()) {
                return Err(paykit_lib::Error::Validation(
                    "Paykit spec 8.2: period details for one of the 16 latest periods to have started, or the next one"
                        .into(),
                )
                .into());
            }
            if details.expires_at <= self.now() {
                return Err(paykit_lib::Error::Validation(
                    "Paykit wire 4.9: period details that expired".into(),
                )
                .into());
            }
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
            let msg_id = signed.msg_id().to_owned();
            match self
                .send_with_record(signed, &period, Role::PeriodDetails, Vec::new(), Vec::new())
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

    #[test]
    fn test_periods_stores_details_with_no_verified_time_first_period_only() {
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
        // No time verified: no period has started, so only the first, not started, takes
        // details.
        assert!(stores_details(&record, &first, None, false));
        assert!(!stores_details(&record, &second, None, false));
        // A verified time decides, the payment deadline 1 s after the start bounding
        // nothing: in the first period, it and the next take details.
        let jan_31 = 1_769_817_600_000;
        assert!(stores_details(&record, &first, Some(jan_31 + 2_000), false));
        assert!(stores_details(
            &record,
            &second,
            Some(jan_31 + 2_000),
            false
        ));
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
        assert!(!stores_details(&record, &second, t, false));
        let reply = |kind: &str| alice.message(&bob, kind, vec![request.hash()], object(json!({})));
        record.response = Some(reply("paykit.payment_request_rejection"));
        assert!(!stores_details(&record, &second, t, false));
        record.response = Some(reply("paykit.payment_request_acceptance"));
        assert!(stores_details(&record, &second, t, false));
        // A payee's cancellation kept and not placed yet stops it, as one the record holds.
        assert!(!stores_details(&record, &second, t, true));
        record.cancellation = Some(reply("paykit.payment_request_cancellation"));
        assert!(!stores_details(&record, &second, t, false));
    }
}
