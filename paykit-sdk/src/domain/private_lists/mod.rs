//! Private Payment List latest-state records.

use std::{collections::HashMap, fmt};

use chrono::{DateTime, Utc};
use paykit_lib::{
    parse_private_payment_list_json, serialize_private_payment_list_json,
    PaymentEndpointIdentifier, PaymentEndpointPayload, PrivateMessageKind, PrivatePaymentList,
};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use crate::domain::outbound_private::enqueue_private_message;
use crate::{
    domain::adapters::{PrivatePaymentEndpointReservation, PrivateReceivingDetail},
    domain::outbound_private::enqueue_private_message_with_link_lease,
    domain::private_stream::PrivateStreamParseStatus,
    storage::{
        OutboundPrivateMessageRecord, PeerLinkOperationLease, PrivateStreamItemRecord,
        StorageAdapter,
    },
    PaykitReceiverPath, PubkyPublicKey, Result,
};

/// Derived latest-state view of a counterparty's Private Payment List.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListView {
    /// Stream item id of the latest valid list.
    pub latest_stream_item_id: Option<u64>,
    /// Current endpoint payloads keyed by identifier string.
    pub payment_endpoints: HashMap<String, String>,
    /// Receive time of the latest valid list.
    pub last_refresh_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for PrivatePaymentListView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identifiers = self.payment_endpoints.keys().collect::<Vec<_>>();
        f.debug_struct("PrivatePaymentListView")
            .field("latest_stream_item_id", &self.latest_stream_item_id)
            .field("payment_endpoint_identifiers", &identifiers)
            .field("last_refresh_at", &self.last_refresh_at)
            .finish()
    }
}

/// Report from syncing Private Payment Lists for local contacts.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListSyncReport {
    /// Counterparties that had a current Private Payment List queued.
    pub queued: Vec<PrivatePaymentListSyncChange>,
    /// Counterparties that had an empty Private Payment List queued.
    pub cleared: Vec<PrivatePaymentListSyncChange>,
    /// Counterparties that could not be queued or cleared.
    pub failed: Vec<PrivatePaymentListSyncChange>,
}

impl fmt::Debug for PrivatePaymentListSyncReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentListSyncReport")
            .field("queued", &self.queued.len())
            .field("cleared", &self.cleared.len())
            .field("failed", &self.failed.len())
            .finish()
    }
}

/// One counterparty result from a Private Payment List sync.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListSyncChange {
    /// Counterparty affected by the sync.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Queued outbound message id, when queueing succeeded.
    pub outbound_message_id: Option<u64>,
    /// Error text, when queueing failed.
    pub error: Option<String>,
}

impl fmt::Debug for PrivatePaymentListSyncChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentListSyncChange")
            .field("counterparty", &self.counterparty.redacted_app_key())
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("outbound_message_id", &self.outbound_message_id)
            .field("error", &self.error.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Reservation-backed Private Payment List update for one counterparty.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListReservationUpdate {
    /// Counterparty that should receive the Private Payment List.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Complete reserved receiving details to share with this counterparty.
    ///
    /// An empty list queues an empty Private Payment List for this counterparty.
    pub reservations: Vec<PrivatePaymentEndpointReservation>,
}

impl fmt::Debug for PrivatePaymentListReservationUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentListReservationUpdate")
            .field("counterparty", &self.counterparty.redacted_app_key())
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("reservations", &self.reservations.len())
            .finish()
    }
}

/// Failed delivery after a Private Payment List was queued.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListDeliveryFailure {
    /// Counterparty whose outbound delivery failed.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Outbound message id, when the failure is tied to one message.
    pub outbound_message_id: Option<u64>,
    /// Reservation id, when the failure is tied to reservation cleanup.
    pub reservation_id: Option<String>,
    /// Delivery or cleanup error.
    pub error: String,
}

impl fmt::Debug for PrivatePaymentListDeliveryFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentListDeliveryFailure")
            .field("counterparty", &self.counterparty.redacted_app_key())
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("outbound_message_id", &self.outbound_message_id)
            .field("reservation_id", &self.reservation_id)
            .field(
                "error",
                &format_args!("<redacted:{} bytes>", self.error.len()),
            )
            .finish()
    }
}

/// Report from queueing and delivering Private Payment Lists.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListDeliveryReport {
    /// Counterparties that had a non-empty Private Payment List queued.
    pub queued: Vec<PrivatePaymentListSyncChange>,
    /// Counterparties that had an empty Private Payment List queued.
    pub cleared: Vec<PrivatePaymentListSyncChange>,
    /// Counterparties that could not be queued or cleared.
    pub failed_to_queue: Vec<PrivatePaymentListSyncChange>,
    /// Counterparties queued successfully but failed during outbound delivery.
    pub failed_to_deliver: Vec<PrivatePaymentListDeliveryFailure>,
}

impl fmt::Debug for PrivatePaymentListDeliveryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentListDeliveryReport")
            .field("queued", &self.queued.len())
            .field("cleared", &self.cleared.len())
            .field("failed_to_queue", &self.failed_to_queue.len())
            .field("failed_to_deliver", &self.failed_to_deliver.len())
            .finish()
    }
}

/// Load the current Private Payment List view for one counterparty.
pub(crate) async fn current_private_payment_list<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<Option<PrivatePaymentListView>>
where
    S: StorageAdapter,
{
    let items = storage
        .transaction(|tx| Ok(tx.private_stream_items(counterparty, counterparty_receiver_path)))
        .await?;
    derive_private_payment_list_view(items)
}

/// Queue a complete Private Payment List for delivery to one counterparty.
///
/// The list replaces the counterparty's latest Private Payment List view when
/// received, so callers should pass every endpoint they want to share.
#[cfg(test)]
pub(crate) async fn enqueue_private_payment_list<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    receiving_details: Vec<PrivateReceivingDetail>,
    now: DateTime<Utc>,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let payment_endpoints = normalize_private_receiving_details(receiving_details)?;
    let list = PrivatePaymentList::new(payment_endpoints);
    let raw_json = serialize_private_payment_list_json(&list)?;
    enqueue_private_message(
        storage,
        counterparty,
        counterparty_receiver_path,
        raw_json,
        now,
    )
    .await
}

/// Queue a complete Private Payment List while a peer operation lease is active.
pub(crate) async fn enqueue_private_payment_list_with_link_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    receiving_details: Vec<PrivateReceivingDetail>,
    now: DateTime<Utc>,
    lease: &PeerLinkOperationLease,
) -> Result<OutboundPrivateMessageRecord>
where
    S: StorageAdapter,
{
    let payment_endpoints = normalize_private_receiving_details(receiving_details)?;
    let list = PrivatePaymentList::new(payment_endpoints);
    let raw_json = serialize_private_payment_list_json(&list)?;
    enqueue_private_message_with_link_lease(storage, counterparty, raw_json, now, lease).await
}

pub(crate) fn normalize_private_receiving_details(
    details: Vec<PrivateReceivingDetail>,
) -> Result<HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
    let mut desired = HashMap::with_capacity(details.len());

    for detail in details {
        let identifier = PaymentEndpointIdentifier::new(detail.identifier)?;
        if desired.contains_key(&identifier) {
            return Err(crate::PaykitSdkError::Protocol {
                context: format!(
                    "duplicate Payment Endpoint identifier '{}'",
                    identifier.as_str()
                ),
                source: None,
            });
        }
        desired.insert(identifier, PaymentEndpointPayload::new(detail.payload));
    }

    Ok(desired)
}

/// Derive the latest valid Private Payment List view from private stream items.
pub(crate) fn derive_private_payment_list_view(
    mut items: Vec<PrivateStreamItemRecord>,
) -> Result<Option<PrivatePaymentListView>> {
    items.sort_by_key(|item| item.stream_item_id);

    let latest = items.into_iter().rev().find(|item| {
        item.parse_status == PrivateStreamParseStatus::Valid
            && item.known_paykit_kind.as_deref()
                == Some(PrivateMessageKind::PrivatePaymentList.as_str())
    });

    let Some(item) = latest else {
        return Ok(None);
    };

    let list = parse_private_payment_list_json(&item.raw_json)?;
    let payment_endpoints = list
        .payment_endpoints
        .into_iter()
        .map(|(identifier, payload)| (identifier.as_str().to_owned(), payload.as_str().to_owned()))
        .collect();

    Ok(Some(PrivatePaymentListView {
        latest_stream_item_id: Some(item.stream_item_id),
        payment_endpoints,
        last_refresh_at: Some(item.received_at),
    }))
}

#[cfg(test)]
mod tests;
