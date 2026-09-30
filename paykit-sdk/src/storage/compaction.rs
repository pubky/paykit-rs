use std::collections::{HashMap, HashSet};

use paykit_lib::PrivateMessageKind;

use super::StorageState;
use crate::{OutboundPrivateMessageStatus, PrivateStreamParseStatus};

/// Discard only obsolete Latest-State payloads, never Event Message history.
pub(super) fn compact_private_payment_lists(state: &mut StorageState) {
    let kind = PrivateMessageKind::PrivatePaymentList.as_str();
    let mut published = HashMap::new();
    let mut attempted = HashMap::new();
    for message in &state.outbound_private_messages {
        if message.kind != kind {
            continue;
        }
        let key = (message.counterparty.clone(), message.app_id.clone());
        if message.status == OutboundPrivateMessageStatus::Sent {
            published.insert(key.clone(), message.outbound_message_id);
        }
        if message.last_attempt_at.is_some() || message.status == OutboundPrivateMessageStatus::Sent
        {
            attempted.insert(key, message.outbound_message_id);
        }
    }
    // Reservation cleanup and app removal still need the records proving what
    // may have reached the peer, including an uncertain empty-list publication.
    let mut keep = state
        .payment_endpoint_reservations
        .values()
        .map(|reservation| reservation.outbound_message_id)
        .collect::<HashSet<_>>();
    keep.extend(published.into_values());
    keep.extend(attempted.into_values());
    state.outbound_private_messages.retain(|message| {
        message.kind != kind
            || message.prepared_send.is_some()
            || keep.contains(&message.outbound_message_id)
            || !matches!(
                message.status,
                OutboundPrivateMessageStatus::Sent | OutboundPrivateMessageStatus::Superseded
            )
    });

    let mut latest = HashMap::new();
    for item in &state.private_stream_items {
        if item.known_paykit_kind.as_deref() == Some(kind)
            && item.parse_status == PrivateStreamParseStatus::Valid
        {
            latest.insert(
                (item.counterparty.clone(), item.parsed_app_id.clone()),
                item.stream_item_id,
            );
        }
    }
    state.private_stream_items.retain(|item| {
        item.known_paykit_kind.as_deref() != Some(kind)
            || item.parse_status != PrivateStreamParseStatus::Valid
            || latest.get(&(item.counterparty.clone(), item.parsed_app_id.clone()))
                == Some(&item.stream_item_id)
    });
}
