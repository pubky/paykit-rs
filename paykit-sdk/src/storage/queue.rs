use chrono::{DateTime, Utc};
use paykit_lib::{
    inspect_private_application_message, PrivateMessageKind, PrivateMessageStructure,
};

use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    identity::PubkyPublicKey,
    storage::{OutboundPrivateMessageRecord, StorageState},
    PaykitReceiverPath,
};

/// Whether this outbound record is parked because its payload carries a
/// Private Message Kind this build does not recognize.
///
/// The decision is body-authoritative: the record is parked iff its
/// `raw_json` inspects as [`PrivateMessageStructure::UnknownKind`] (a
/// well-formed envelope whose body `kind` is unrecognized), regardless of the
/// record's `kind` column. A parked record was written by a newer build whose
/// intent this build cannot judge, so it is never claimed, mutated, or
/// invalidated; as the queue head it blocks the peer's FIFO queue until a
/// build that understands the kind processes it. Parking applies to every
/// head the claim path would otherwise consider: `Pending`, `Failed`, and
/// stale `Sending` records alike.
///
/// A recognized kind with an unsupported `version` still takes the existing
/// invalid-message path: versioned evolution of known kinds is a deferred
/// extension, and parking is deliberately scoped to unknown kinds only.
pub(crate) fn is_parked_unknown_kind_outbound_message(
    message: &OutboundPrivateMessageRecord,
) -> bool {
    inspect_private_application_message(&message.raw_json).structure
        == PrivateMessageStructure::UnknownKind
}

pub(super) fn is_claimable_outbound_private_message(
    message: &OutboundPrivateMessageRecord,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) -> bool {
    match message.status {
        OutboundPrivateMessageStatus::Pending => true,
        OutboundPrivateMessageStatus::Failed => message
            .last_attempt_at
            .is_none_or(|last_attempt_at| last_attempt_at <= failed_retry_after),
        OutboundPrivateMessageStatus::Sending => {
            is_stale_sending_outbound_private_message(message, stale_before)
        }
        OutboundPrivateMessageStatus::Sent
        | OutboundPrivateMessageStatus::Invalid
        | OutboundPrivateMessageStatus::RecoveryRequired
        | OutboundPrivateMessageStatus::Superseded => false,
    }
}

pub(super) fn is_stale_sending_outbound_private_message(
    message: &OutboundPrivateMessageRecord,
    stale_before: DateTime<Utc>,
) -> bool {
    message.status == OutboundPrivateMessageStatus::Sending
        && message
            .last_attempt_at
            .is_none_or(|last_attempt_at| last_attempt_at <= stale_before)
}

// Deliberately ignores parking for the head verdict: a peer whose queue head
// is parked keeps appearing in pending listings so the parked-head signal
// re-surfaces on every flush instead of silently disappearing. Parked records
// are still excluded from the supersede bookkeeping below (they are not
// Private Payment Lists this build can judge), mirroring
// `supersede_outdated_private_payment_lists`.
pub(crate) fn outbound_private_queue_head_is_claimable(
    messages: &[OutboundPrivateMessageRecord],
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) -> bool {
    let latest_private_list_id = messages
        .iter()
        .filter(|message| {
            message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                && !matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Invalid
                        | OutboundPrivateMessageStatus::RecoveryRequired
                        | OutboundPrivateMessageStatus::Superseded
                )
                && !is_parked_unknown_kind_outbound_message(message)
        })
        .map(|message| message.outbound_message_id)
        .max();
    let mut ordered = messages.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|message| message.outbound_message_id);

    for message in ordered {
        if matches!(
            message.status,
            OutboundPrivateMessageStatus::Sent
                | OutboundPrivateMessageStatus::Invalid
                | OutboundPrivateMessageStatus::RecoveryRequired
                | OutboundPrivateMessageStatus::Superseded
        ) {
            continue;
        }
        let is_supersedable_private_list = latest_private_list_id.is_some_and(|latest| {
            message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                && message.outbound_message_id < latest
                && message.status != OutboundPrivateMessageStatus::Sending
                && is_claimable_outbound_private_message(message, stale_before, failed_retry_after)
                && !is_parked_unknown_kind_outbound_message(message)
        });
        if is_supersedable_private_list {
            continue;
        }
        return is_claimable_outbound_private_message(message, stale_before, failed_retry_after)
            || is_stale_sending_outbound_private_message(message, stale_before);
    }

    false
}

// Parking is body-authoritative while this pass keys on the `kind` column, so
// a record whose column says Private Payment List but whose body carries an
// unknown kind must be excluded from BOTH the latest-list selection and the
// mutation loop: a parked record is never mutated (it would be silently
// retired as Superseded and leapfrogged), and it never counts as the latest
// list (it can never send, so it must not retire genuine older lists).
pub(super) fn supersede_outdated_private_payment_lists(
    state: &mut StorageState,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) {
    let latest_private_list_id = state
        .outbound_private_messages
        .iter()
        .filter(|message| {
            &message.counterparty == counterparty
                && &message.counterparty_receiver_path == counterparty_receiver_path
                && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                && !matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Invalid
                        | OutboundPrivateMessageStatus::RecoveryRequired
                        | OutboundPrivateMessageStatus::Superseded
                )
                && !is_parked_unknown_kind_outbound_message(message)
        })
        .map(|message| message.outbound_message_id)
        .max();
    let Some(latest_private_list_id) = latest_private_list_id else {
        return;
    };

    for message in state.outbound_private_messages.iter_mut() {
        if &message.counterparty == counterparty
            && &message.counterparty_receiver_path == counterparty_receiver_path
            && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
            && message.outbound_message_id < latest_private_list_id
            && message.status != OutboundPrivateMessageStatus::Sending
            && is_claimable_outbound_private_message(message, stale_before, failed_retry_after)
            && !is_parked_unknown_kind_outbound_message(message)
        {
            message.status = OutboundPrivateMessageStatus::Superseded;
            message.updated_at = now;
            message.last_error = None;
        }
    }
}
