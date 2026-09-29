use std::collections::HashSet;

use super::derivation::canonical_allowance_id;
use crate::{
    domain::private_stream::{canonical_event_id, is_allowance_kind, is_payment_request_kind},
    storage::StorageState,
    OutboundPrivateMessageStatus, PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
};

#[derive(PartialEq, Eq, Hash)]
struct Evidence<'a> {
    counterparty: &'a PubkyPublicKey,
    receiver: &'a PaykitReceiverPath,
    outbound: bool,
    position: u64,
    raw: &'a str,
    kind: Option<&'a str>,
}

impl Evidence<'_> {
    fn is_payment_lifecycle(&self) -> bool {
        self.kind.is_some_and(is_allowance_kind)
            || is_payment_request_kind(self.kind)
            || canonical_allowance_id(self.raw).is_some()
            || carries_payment_request_id(self.raw)
    }

    fn event_key(&self) -> Option<(&PubkyPublicKey, &PaykitReceiverPath, String)> {
        canonical_event_id(self.raw).map(|id| (self.counterparty, self.receiver, id))
    }
}

// Restore must not roll back known authority or conflict evidence for the same
// payer. Payment Request Cancellation and malformed/conflicting events can
// revoke payment eligibility even when wallet accounting has not changed.
// Keep direction, local FIFO position and exact bytes: those determine
// lifecycle causality and Event ID conflicts. Do not splice newer queues into
// old Noise checkpoints. Reject the entire restore before replacing any state.
pub(crate) fn ensure_payment_lifecycle_history_retained(
    current: &StorageState,
    restored: &StorageState,
) -> Result<()> {
    let current = evidence(current);
    let restored = evidence(restored);
    let event_ids = current
        .iter()
        .chain(&restored)
        .filter(|item| item.is_payment_lifecycle())
        .filter_map(Evidence::event_key)
        .collect::<HashSet<_>>();
    let restored_items = restored.iter().collect::<HashSet<_>>();
    for item in &current {
        let relevant = item.is_payment_lifecycle()
            || item.event_key().is_some_and(|key| event_ids.contains(&key));
        if relevant && !restored_items.contains(item) {
            return Err(PaykitSdkError::Protocol {
                context: "Backup would discard retained Allowance or Payment Request history"
                    .into(),
                source: None,
            });
        }
    }
    Ok(())
}

// Unknown-version carriers can still identify a request and conflict with its
// lifecycle. Retain them without requiring a successfully parsed event.
fn carries_payment_request_id(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            paykit_lib::PaymentRequestId::new(value.get("payment_request_id")?.as_str()?).ok()
        })
        .is_some()
}

fn evidence(state: &StorageState) -> Vec<Evidence<'_>> {
    state
        .private_stream_items
        .iter()
        .map(|item| Evidence {
            counterparty: &item.counterparty,
            receiver: &item.counterparty_receiver_path,
            outbound: false,
            position: item.stream_item_id,
            raw: &item.raw_json,
            kind: item.parsed_kind.as_deref(),
        })
        .chain(
            state
                .outbound_private_messages
                .iter()
                .filter(|item| {
                    !matches!(
                        item.status,
                        OutboundPrivateMessageStatus::Invalid
                            | OutboundPrivateMessageStatus::Superseded
                    )
                })
                .map(|item| Evidence {
                    counterparty: &item.counterparty,
                    receiver: &item.counterparty_receiver_path,
                    outbound: true,
                    position: item.outbound_message_id,
                    raw: &item.raw_json,
                    kind: Some(&item.kind),
                }),
        )
        .collect()
}
