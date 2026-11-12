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

use std::collections::HashMap;

use paykit_lib::bodies::{period_details_order, PaymentRequest, PeriodDetails};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::periods::PeriodRef;
use paykit_lib::records::{
    ActivityRecord, ActivitySide, RecordBinding, StoredMessage, MAX_PERIOD_DETAILS,
};
use paykit_lib::{Body, PaymentEndpointIdentifier, Rate};
use pubky_switch_core::http::ErrorCode;

use crate::activity::{ActivityTarget, Role};
use crate::error::{Refusal, Result};
use crate::paykit::Paykit;
use crate::receipts::{received_receipt, ReceivedReceipt};
use crate::records::conflict;
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
    /// stays valid until its own expiry: a later one replaces none. Returns the `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `request` or a label; [`Refusal::NotFound`] for a
    /// record this app doesn't hold, or a period record; [`Refusal::Usage`] for a one-time
    /// request, or a period that holds 8 details already; [`Refusal::NotSubscribed`] before
    /// the payer's acceptance; [`Refusal::Rejected`] or [`Refusal::Cancelled`];
    /// [`PaykitError::Lib`] with `Validation` for details that break Paykit wire 4.9 against
    /// the request, such as a period that isn't one of its own, or that expired by the
    /// device clock; [`Refusal::NotNow`] while the period record's value waits for its scope
    /// key; [`PaykitError::Conflict`] when the record kept changing; the client's errors.
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
