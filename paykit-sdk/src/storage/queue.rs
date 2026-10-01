use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use paykit_lib::PrivateMessageKind;

use crate::{
    domain::outbound_private::OutboundPrivateMessageStatus,
    identity::PubkyPublicKey,
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

pub(super) fn inactive_outbound_private_message_blocks_queue(
    message: &OutboundPrivateMessageRecord,
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
) -> bool {
    !retired_apps.contains(&message.app_id)
        && (message.status != OutboundPrivateMessageStatus::Pending
            || message.kind != PrivateMessageKind::PrivatePaymentList.as_str())
}

pub(crate) fn outbound_private_queue_head_is_claimable(
    messages: &[OutboundPrivateMessageRecord],
    registered_apps: &HashSet<paykit_lib::PaykitAppId>,
    retired_apps: &HashSet<paykit_lib::PaykitAppId>,
    stale_before: DateTime<Utc>,
    failed_retry_after: DateTime<Utc>,
) -> bool {
    let latest_private_list_ids = messages
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
        .fold(HashMap::new(), |mut latest, message| {
            latest
                .entry(message.app_id.clone())
                .and_modify(|id: &mut u64| *id = (*id).max(message.outbound_message_id))
                .or_insert(message.outbound_message_id);
            latest
        });
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
        let is_supersedable_private_list = message.status == OutboundPrivateMessageStatus::Pending
            && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
            && latest_private_list_ids
                .get(&message.app_id)
                .is_some_and(|latest| message.outbound_message_id < *latest);
        if is_supersedable_private_list {
            continue;
        }
        if !registered_apps.contains(&message.app_id) {
            if !inactive_outbound_private_message_blocks_queue(message, retired_apps) {
                continue;
            }
            return false;
        }
        if retired_apps.contains(&message.app_id) {
            continue;
        }
        return is_claimable_outbound_private_message(message, stale_before, failed_retry_after);
    }

    false
}

pub(super) fn supersede_outdated_private_payment_lists(
    state: &mut StorageState,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) {
    let latest_private_list_ids = state
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
        .fold(HashMap::new(), |mut latest, message| {
            latest
                .entry(message.app_id.clone())
                .and_modify(|id: &mut u64| *id = (*id).max(message.outbound_message_id))
                .or_insert(message.outbound_message_id);
            latest
        });
    if latest_private_list_ids.is_empty() {
        return;
    }

    for message in state.outbound_private_messages.iter_mut() {
        if &message.counterparty == counterparty
            && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
            && latest_private_list_ids
                .get(&message.app_id)
                .is_some_and(|latest| message.outbound_message_id < *latest)
            && message.status == OutboundPrivateMessageStatus::Pending
        {
            message.status = OutboundPrivateMessageStatus::Superseded;
            message.updated_at = now;
            message.last_error = None;
        }
    }
}
