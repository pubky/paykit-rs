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

use paykit_lib::kinds::Kind;
use pubky_switch_client::app::{
    AppEvent, DeliveryOrigin, DeliveryView, KeptBatchStatus, QueuedMessage, SENT_RETENTION_MS,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::activity::{KeptSide, SideTag};
use crate::error::Result;
use crate::hold::AnswerRecord;
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
///   and tells the host ([`Paykit::give_up_expired_stops`], Paykit spec 8.1). Here the
///   window is judged on the device clock, since the delivery view carries no service
///   time; the receive step judges it on the later of the device clock and the service
///   time verified, so it ends every copy this holds as past its window, and may end one
///   this would still let go while the service time runs ahead of the device clock;
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

/// A rejection or payer's cancellation this app queued, kept at `qs/<msg_id>` in the
/// commit that queues it, for as long as its outbox copy is owed: what the receive step
/// needs to end that copy past its 30 days and tell the host, which the outbox doesn't
/// show (Paykit spec 8.1). `given_up` is set before the client ends the copy, with whether
/// the host is told, and the entry goes once the client's report of the end is taken, so
/// the host is told once, crash or not.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QueuedStop {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) payee: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) request_hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
    pub(crate) created_at_ms: u64,
    pub(crate) given_up: Option<GivenUp>,
}

/// How a stop's outbox copy past its window is ended, by what its request record says of
/// it ([`crate::hold::AnswerRecord`], Paykit spec 8.1, "Undelivered messages").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum GivenUp {
    /// The record holds the stop, or lost it with a write: the host is told.
    Told,
    /// The record is gone, or holds another stop that covers it: nothing is told of it
    /// as given up, though a possible delivery still is.
    Silent,
}

impl QueuedStop {
    /// The entry of `signed`, a stop answering the request `request_hash`, just signed.
    pub(crate) fn signed(
        signed: &pubky_switch_client::app::SignedMessage,
        request_hash: [u8; 32],
    ) -> Result<Self> {
        let message = pubky_switch_core::message::Message::parse(
            signed.plaintext().json(),
            paykit_lib::kinds::OPAQUE_MEMBERS,
        )?;
        Ok(Self {
            msg_id: message.msg_id,
            payee: message.to,
            request_hash,
            hash: signed.hash(),
            created_at_ms: message.created_at_ms,
            given_up: None,
        })
    }

    /// The entry of `stored`, a stop answering the request `request_hash`, queued again
    /// from its record.
    pub(crate) fn stored(
        stored: &paykit_lib::records::StoredMessage,
        request_hash: [u8; 32],
    ) -> Self {
        let message = stored.message();
        Self {
            msg_id: message.msg_id.clone(),
            payee: message.to,
            request_hash,
            hash: stored.hash(),
            created_at_ms: message.created_at_ms,
            given_up: None,
        }
    }
}

impl Paykit {
    /// End the outbox copies of this app's rejections and payer's cancellations that are
    /// past 30 days from their `created_at`, less the clock tolerance, by the later of the
    /// device clock and the service time verified: such a stop is never sent, and a client
    /// original carries no deadline of its own. As for one held in its record
    /// ([`Paykit::queue_own_again`]), it is decided at the first receive past that time at
    /// which the copy of the deny list is known and doesn't hold the payee, with the records
    /// listed and no scope key awaited (Paykit spec 8.1, "Blocked counterparties"), on what
    /// its request record says of it ([`Paykit::answer_record`]): nothing is decided while
    /// the record doesn't read or is paused as divergent; the host is told it was given up
    /// when the record holds the stop or lost it with a write, and not when the record is
    /// gone or another stop covers it, since that notice requires a record that holds the
    /// message ("Undelivered messages"); a possible delivery is always told. Each is ended
    /// through the client's
    /// withdrawal, which reports it undelivered when no try of it can have reached the
    /// payee's switch, and unknown otherwise; that report is taken at once, the host told
    /// in the commit that takes it ([`crate::NoticeKind::MessageGivenUp`] or
    /// [`crate::NoticeKind::PossiblyDelivered`]). An entry whose copy the outbox no longer
    /// holds, delivered, refused or ended otherwise, goes.
    pub(crate) async fn give_up_expired_stops(&mut self) -> Result<()> {
        let kept = self.local_scan::<QueuedStop>(key::QUEUED_STOPS).await?;
        if kept.is_empty() {
            return Ok(());
        }
        let queued: BTreeMap<String, QueuedMessage> = self
            .app
            .queued_messages(crate::protocol::BACKUP_NAME)
            .await?
            .into_iter()
            .filter(|queued| !queued.resend)
            .map(|queued| (queued.msg_id.clone(), queued))
            .collect();
        let now = self.window_now();
        let mut gone = Tx::default();
        let mut expired = Vec::new();
        for (_, stop) in kept {
            let at = key::queued_stop(&stop.msg_id);
            let owed = queued
                .get(&stop.msg_id)
                .is_some_and(|copy| copy.hash == stop.hash && copy.to == stop.payee);
            if !owed {
                // One given up keeps its entry until the client's report of the end is
                // taken, which tells the host as it was decided.
                if stop.given_up.is_none() {
                    gone.delete(&at);
                }
                continue;
            }
            if past_replay_window(stop.created_at_ms, now)
                && !self.delivery_held(&stop.payee).await?
            {
                expired.push(stop);
            }
        }
        self.local_commit(gone).await?;
        for mut stop in expired {
            let at = key::queued_stop(&stop.msg_id);
            stop.given_up =
                match Box::pin(self.answer_record(&stop.payee, &stop.request_hash, &stop.msg_id))
                    .await?
                {
                    // Nothing is decided, and no decision left by an end that didn't happen
                    // applies to a report the host causes meanwhile.
                    AnswerRecord::Undecided => {
                        if stop.given_up.take().is_some() {
                            self.local_put(&at, &stop).await?;
                        }
                        continue;
                    }
                    AnswerRecord::Holds | AnswerRecord::Lacks => Some(GivenUp::Told),
                    AnswerRecord::Gone | AnswerRecord::Covered => Some(GivenUp::Silent),
                };
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
    /// try of it can have reached the payee's switch, unless its record left nothing to tell
    /// of, and always that it may have been delivered otherwise. Whether `event` is such a
    /// report: one about a stop the receive step didn't give up stays for the host.
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
        let at = key::queued_stop(msg_id);
        let Some((stop, given_up)) = self
            .local_get::<QueuedStop>(&at)
            .await?
            .and_then(|stop| stop.given_up.map(|given_up| (stop, given_up)))
        else {
            return Ok(false);
        };
        tx.delete(&at);
        // Only a stop never delivered goes untold once its record is gone or covered: one
        // that may have been delivered is always told, with or without a record (Paykit spec
        // 8.1, "Undelivered messages"; 9).
        if undelivered && given_up == GivenUp::Silent {
            return Ok(true);
        }
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
