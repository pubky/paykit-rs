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

use paykit_lib::kinds::Kind;
use pubky_switch_client::app::{DeliveryOrigin, DeliveryView, KeptBatchStatus};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};

use crate::activity::{KeptSide, SideTag};
use crate::error::Result;
use crate::receive;
use crate::state::{self, key};

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
/// whose certificate doesn't read the lists judges no deny list, so holds nothing.
pub(crate) fn may_deliver(delivery: &DeliveryView<'_>, kind: Kind) -> Result<bool> {
    if !receive::reads_lists(delivery.cert().scopes()) {
        return Ok(true);
    }
    // Re-sending an answer requires a known deny copy that permits the counterparty,
    // what settles a payment included (Paykit spec 8.1, 9).
    if delivery.resend() {
        return Ok(deny_decision(delivery)? == DenyDecision::Permits);
    }
    let message = delivery.message();
    match kind {
        // A proof and a receipt report a payment already made (Paykit spec 8.1).
        Kind::PaymentProof | Kind::PaymentReceipt => return Ok(true),
        Kind::PaymentRequestCancellation => {
            let side = state::delivery_get::<KeptSide>(delivery, &key::side(&message.msg_id))?;
            if side.is_some_and(|kept| kept.side == SideTag::Payee) {
                return Ok(true);
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
