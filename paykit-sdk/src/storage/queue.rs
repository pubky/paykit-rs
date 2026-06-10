use chrono::{DateTime, Utc};
use paykit_lib::PrivateMessageKind;

use crate::{
    identity::PubkyPublicKey,
    outbound_private::OutboundPrivateMessageStatus,
    storage::{OutboundPrivateMessageRecord, StorageState},
};

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
        });
        if is_supersedable_private_list {
            continue;
        }
        return is_claimable_outbound_private_message(message, stale_before, failed_retry_after)
            || is_stale_sending_outbound_private_message(message, stale_before);
    }

    false
}

pub(super) fn supersede_outdated_private_payment_lists(
    state: &mut StorageState,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) {
    let latest_private_list_id = state
        .outbound_private_messages
        .iter()
        .filter(|message| {
            &message.counterparty == counterparty
                && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                && !matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Invalid
                        | OutboundPrivateMessageStatus::RecoveryRequired
                        | OutboundPrivateMessageStatus::Superseded
                )
        })
        .map(|message| message.outbound_message_id)
        .max();
    let Some(latest_private_list_id) = latest_private_list_id else {
        return;
    };

    for message in state.outbound_private_messages.iter_mut() {
        if &message.counterparty == counterparty
            && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
            && message.outbound_message_id < latest_private_list_id
            && message.status != OutboundPrivateMessageStatus::Sending
            && is_claimable_outbound_private_message(message, stale_before, failed_retry_after)
        {
            message.status = OutboundPrivateMessageStatus::Superseded;
            message.updated_at = now;
            message.last_error = None;
        }
    }
}
