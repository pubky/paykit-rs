//! The counterparty send hold at each delivery attempt (Paykit spec 8.1, "Blocked
//! counterparties", "Messages already queued").
//!
//! The client asks [`crate::PaykitProtocol`] before every delivery or re-send of an
//! outbox message: direct, flushed, resumed, from a kept batch, or carried by reload or
//! restore. A message the hold governs goes only while the current deny decision
//! permits it, so one queued or signed before a block waits once the block is recorded.
//! An exception goes from the signed message and the durable state that establishes its
//! role, never from its kind alone: a payee's cancellation by its kept side, a payment's
//! acceptance by the evidence written before it was queued. An exception covers the first
//! delivery and its retries only: a re-send answering a `resend_request` goes only while
//! the deny decision permits it, whatever its kind (Paykit spec 9).

use std::collections::BTreeMap;

use paykit_lib::collections::Collection;
use paykit_lib::kinds::Kind;
use paykit_lib::records::ActivitySide;
use pubky_switch_client::app::{
    AppEvent, DeliveryOrigin, DeliveryView, KeptBatchStatus, QueuedMessage, SENT_RETENTION_MS,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::activity::{KeptSide, SideTag};
use crate::error::Result;
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::paykit::Paykit;
use crate::receive;
use crate::state::{self, key, Tx};

/// Why an outgoing acceptance may go while sends to its payee are held, kept at
/// `ex/<msg_id>` before or with the batch that queues it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendExemption {
    pub(crate) msg_id: String,
    pub(crate) reason: ExemptionReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExemptionReason {
    /// The acceptance reports a payment the payment method was already asked to make.
    DispatchedPayment,
    /// The user confirmed accepting this subscription of a blocked payee. Only its
    /// original delivery and retries of it go, never a re-send or a copy reconstructed
    /// from a shared record.
    ConfirmedWhileBlocked {
        #[serde(with = "serde_b64")]
        payee: [u8; 32],
        #[serde(with = "serde_b64")]
        request_hash: [u8; 32],
    },
}

impl SendExemption {
    pub(crate) fn dispatched(msg_id: &str) -> Self {
        Self {
            msg_id: msg_id.to_owned(),
            reason: ExemptionReason::DispatchedPayment,
        }
    }

    pub(crate) fn confirmed_while_blocked(
        msg_id: &str,
        payee: [u8; 32],
        request_hash: [u8; 32],
    ) -> Self {
        Self {
            msg_id: msg_id.to_owned(),
            reason: ExemptionReason::ConfirmedWhileBlocked {
                payee,
                request_hash,
            },
        }
    }
}

/// The deny decision for the recipient, as the app's own block, unblock and copy of the
/// deny list give it (Paykit spec 8.1, "How fresh the deny list is").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DenyDecision {
    Permits,
    Blocked,
    /// The copy is older than its refresh interval, never read, or an own change is
    /// undecided: the send waits for a read of the lists.
    Unknown,
}

pub(crate) fn deny_decision(delivery: &DeliveryView<'_>) -> Result<DenyDecision> {
    let read = state::delivery_get::<u64>(delivery, key::LISTS_READ)?;
    if !read.is_some_and(|at| receive::lists_read_is_fresh(at, delivery.now_ms())) {
        return Ok(DenyDecision::Unknown);
    }
    let Some(denied) = state::delivery_get::<Vec<[u8; 32]>>(delivery, key::DENIED)? else {
        return Ok(DenyDecision::Unknown);
    };
    let to = &delivery.message().to;
    if let Some(mark) = state::delivery_get::<state::OwnBlockMark>(delivery, &key::own_block(to))? {
        let kept = match mark.unblock_batch() {
            Some(batch) => matches!(
                delivery.kept_batch(batch)?,
                Some(KeptBatchStatus::Kept { .. })
            ),
            None => false,
        };
        return Ok(match mark.counts(kept) {
            Some(false) => DenyDecision::Permits,
            Some(true) => DenyDecision::Blocked,
            None => DenyDecision::Unknown,
        });
    }
    Ok(if denied.contains(to) {
        DenyDecision::Blocked
    } else {
        DenyDecision::Permits
    })
}

/// Whether a message of `kind` that the counterparty hold governs may go now. An app
/// whose certificate doesn't read the lists judges no deny list, so holds no first delivery.
///
/// What ends a message held here:
/// - a rejection or a payer's cancellation is never sent once 30 days have passed since
///   its `created_at`, less the clock tolerance; the receive step then ends its outbox copy
///   and tells the host ([`Paykit::give_up_expired_stops`], Paykit spec 8.1);
/// - a re-send ends at the client's replay deadline, its first acceptance plus 30 days
///   (Switch spec 11);
/// - a request, period details, a list request and a subscription acceptance not
///   confirmed while blocked have no deadline in the outbox, where an original carries
///   none: each waits until the deny decision permits its counterparty, and then goes.
pub(crate) fn may_deliver(delivery: &DeliveryView<'_>, kind: Kind) -> Result<bool> {
    let reads_lists = receive::reads_lists(delivery.cert().scopes());
    // Re-sending an answer requires `paykit_contacts` and a known deny copy that permits
    // the counterparty, what settles a payment included (Paykit spec 3.1, 8.1, 9).
    if delivery.resend() {
        return Ok(reads_lists && deny_decision(delivery)? == DenyDecision::Permits);
    }
    if !reads_lists {
        return Ok(true);
    }
    let message = delivery.message();
    match kind {
        // A proof and a receipt report a payment already made (Paykit spec 8.1).
        Kind::PaymentProof | Kind::PaymentReceipt => return Ok(true),
        Kind::PaymentRequestRejection => {
            if past_replay_window(message.created_at_ms, delivery.now_ms()) {
                return Ok(false);
            }
        }
        Kind::PaymentRequestCancellation => {
            let side = state::delivery_get::<KeptSide>(delivery, &key::side(&message.msg_id))?;
            match side.map(|kept| kept.side) {
                Some(SideTag::Payee) => return Ok(true),
                Some(SideTag::Payer)
                    if past_replay_window(message.created_at_ms, delivery.now_ms()) =>
                {
                    return Ok(false);
                }
                _ => {}
            }
        }
        Kind::PaymentRequestAcceptance => {
            let exemption = state::delivery_get::<SendExemption>(
                delivery,
                &key::send_exemption(&message.msg_id),
            )?;
            match exemption.map(|e| e.reason) {
                Some(ExemptionReason::DispatchedPayment) => return Ok(true),
                Some(ExemptionReason::ConfirmedWhileBlocked {
                    payee,
                    request_hash,
                }) if payee == message.to
                    && message.refs.as_slice() == [request_hash]
                    && delivery.origin() == DeliveryOrigin::Original =>
                {
                    return Ok(deny_decision(delivery)? != DenyDecision::Unknown);
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(deny_decision(delivery)? == DenyDecision::Permits)
}

/// Whether a stop signed at `created_at_ms` is past its 30 days, less the clock tolerance,
/// at `now_ms`: held until then, it is never sent (Paykit spec 8.1, "Blocked
/// counterparties").
pub(crate) fn past_replay_window(created_at_ms: u64, now_ms: u64) -> bool {
    now_ms
        > created_at_ms
            .saturating_sub(CLOCK_TOLERANCE_MS)
            .saturating_add(SENT_RETENTION_MS)
}

/// A rejection or payer's cancellation past its 30 days whose outbox copy the receive step
/// ends, kept at `gs/<msg_id>` from before the client ends it until its report of the end
/// is taken, so the host is told of it once, crash or not (Paykit spec 8.1).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpiredStop {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) request_hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
}

impl Paykit {
    /// End the outbox copies of this app's rejections and payer's cancellations that are
    /// past 30 days from their `created_at`, less the clock tolerance, by the later of the
    /// device clock and the service time verified: such a stop is never sent (Paykit spec
    /// 8.1, "Blocked counterparties"), and a client original carries no deadline of its own.
    /// Each is ended through the client's withdrawal, which reports it undelivered when no
    /// try of it can have reached the payee's switch, and unknown otherwise; that report is
    /// taken at once, the host told in the commit that takes it
    /// ([`crate::NoticeKind::MessageGivenUp`] or [`crate::NoticeKind::PossiblyDelivered`]). The request
    /// record that holds the stop keeps it, so no own app pays or accepts the request.
    ///
    /// A stop is found by the request record that holds it, signed by this app: one whose
    /// record isn't held or doesn't read yet, such as one a kept batch will queue, is ended
    /// at a later receive.
    pub(crate) async fn give_up_expired_stops(&mut self) -> Result<()> {
        let stops = [
            Kind::PaymentRequestRejection.as_str(),
            Kind::PaymentRequestCancellation.as_str(),
        ];
        let queued: BTreeMap<String, QueuedMessage> = self
            .app
            .queued_messages(crate::protocol::BACKUP_NAME)
            .await?
            .into_iter()
            .filter(|queued| !queued.resend && stops.contains(&queued.kind.as_str()))
            .map(|queued| (queued.msg_id.clone(), queued))
            .collect();
        if queued.is_empty() {
            return Ok(());
        }
        let own = self.identity();
        let app_id = self.app_id();
        let mut expired = Vec::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(value) = held.content.value() else {
                continue;
            };
            let Some(record) = crate::records::bind_activity(
                value,
                &own,
                ActivitySide::Payer,
                None,
                &held.key,
                |t| self.app.record_key(t).ok(),
                &|p| self.rail.settlement_id(p),
            ) else {
                continue;
            };
            for stored in [&record.response, &record.cancellation]
                .into_iter()
                .flatten()
            {
                let message = stored.message();
                let Some(copy) = queued.get(&message.msg_id) else {
                    continue;
                };
                let [request_hash] = message.refs.as_slice() else {
                    continue;
                };
                if stored.sender() != own
                    || stored.cert().app_id() != app_id
                    || copy.hash != stored.hash()
                    || copy.to != message.to
                    || !self.past_replay_window(stored)
                {
                    continue;
                }
                expired.push(ExpiredStop {
                    msg_id: message.msg_id.clone(),
                    payee: message.to,
                    request_hash: *request_hash,
                    hash: copy.hash,
                });
            }
        }
        for stop in expired {
            let at = key::expired_stop(&stop.msg_id);
            self.local_put(&at, &stop).await?;
            if self
                .app
                .withdraw_message(crate::protocol::BACKUP_NAME, &stop.msg_id)
                .await?
            {
                self.take_delivery_duty(&stop.msg_id).await?;
            } else {
                self.local_delete(&at).await?;
            }
        }
        Ok(())
    }

    /// Take, in `tx`, the client's report that the outbox copy of a stop ended past its 30
    /// days ([`Paykit::give_up_expired_stops`]): the host is told it was given up when no
    /// try of it can have reached the payee's switch, and that it may have been delivered
    /// otherwise. Whether `event` is such a report.
    pub(crate) async fn take_expired_stop(&self, tx: &mut Tx, event: &AppEvent) -> Result<bool> {
        let (msg_id, undelivered) = match event {
            AppEvent::MessageUndelivered {
                msg_id,
                resend: false,
                ..
            } => (msg_id, true),
            AppEvent::DeliveryUnknown { msg_id, .. } => (msg_id, false),
            _ => return Ok(false),
        };
        let at = key::expired_stop(msg_id);
        let Some(stop) = self.local_get::<ExpiredStop>(&at).await? else {
            return Ok(false);
        };
        let now = self.service_time().unwrap_or(0);
        let (notice_key, notice) = if undelivered {
            (
                key::message_notice(&stop.payee, &stop.msg_id),
                KeptNotice::given_up(
                    stop.payee,
                    &stop.msg_id,
                    Some(stop.request_hash),
                    now,
                    GivenUpFrom::Origin(GivenUpOrigin::HeldAnswer),
                ),
            )
        } else {
            (
                key::restored_delivery(&stop.payee, &stop.msg_id, &stop.hash),
                KeptNotice::delivery_unknown(&stop.payee, &stop.msg_id, &stop.hash, now),
            )
        };
        if self.kept_notice(&notice_key).await?.is_none() {
            tx.put(&notice_key, &notice)?;
        }
        tx.delete(&at);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_send_exemption_current_structure() {
        let confirmed = SendExemption::confirmed_while_blocked(
            "123e4567-e89b-42d3-a456-426614174000",
            [1; 32],
            [2; 32],
        );
        for value in [
            SendExemption::dispatched("123e4567-e89b-42d3-a456-426614174000"),
            confirmed,
        ] {
            let json = serde_json::to_value(&value).unwrap();
            assert_eq!(
                serde_json::from_value::<SendExemption>(json.clone()).unwrap(),
                value
            );
            let mut extra = json.clone();
            extra
                .as_object_mut()
                .unwrap()
                .insert("extra".into(), serde_json::json!(1));
            assert!(serde_json::from_value::<SendExemption>(extra).is_err());
        }
    }
}
