use std::{collections::HashMap, fmt, sync::Arc};

use paykit_sdk::{
    storage::OutboundPrivateMessageRecord, OutboundPrivateMessageStatus,
    PrivatePaymentListDeliveryFailure, PrivatePaymentListDeliveryReport,
    PrivatePaymentListReservationUpdate, PrivatePaymentListSyncChange,
    PrivatePaymentListSyncReport, PrivatePaymentListView,
};

use crate::{
    payment_adapter::{
        payment_endpoint_reservation_from_parts, FfiPaymentPayload, FfiReceivingDetail,
    },
    private_links::FfiPrivateOperationError,
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key, parse_receiver_id},
    PaykitFfiError,
};

/// Delivery status for one queued outbound Private Application Message.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiOutboundPrivateMessageStatus {
    /// Message is queued and has not been sent.
    Pending,
    /// A worker is sending this message.
    Sending,
    /// Message was sent successfully.
    Sent,
    /// Last send attempt failed.
    Failed,
    /// The stored payload is invalid and must not be retried automatically.
    Invalid,
    /// Automatic retry is blocked until local Encrypted Link state is recovered.
    RecoveryRequired,
    /// Newer latest-state data made this message unnecessary to send.
    Superseded,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Queued outbound private message summary.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiQueuedPrivateMessage {
    /// Assigned outbound message id.
    pub outbound_message_id: u64,
    /// Counterparty public key.
    pub counterparty: String,
    /// Counterparty Paykit receiver folder id.
    pub counterparty_receiver_id: String,
    /// Private Message Kind string.
    pub kind: String,
    /// Delivery status.
    pub status: FfiOutboundPrivateMessageStatus,
    /// Number of send attempts.
    pub attempt_count: u32,
    /// Queue time as RFC3339 text.
    pub created_at: String,
    /// Last status update time as RFC3339 text.
    pub updated_at: String,
    /// Last send attempt time as RFC3339 text.
    pub last_attempt_at: Option<String>,
    /// Successful send time as RFC3339 text.
    pub sent_at: Option<String>,
    /// Last send error, when available.
    pub last_error: Option<Arc<FfiPrivateOperationError>>,
}

/// One endpoint in the latest Private Payment List view.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListEndpoint {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
}

/// Latest valid Private Payment List view for one counterparty receiver.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListView {
    /// Stream item id of the latest valid list.
    pub latest_stream_item_id: Option<u64>,
    /// Current endpoint payloads sorted by identifier.
    pub payment_endpoints: Vec<FfiPrivatePaymentListEndpoint>,
    /// Receive time of the latest valid list as RFC3339 text.
    pub last_refresh_at: Option<String>,
}

/// Report from syncing Private Payment Lists for local contact receivers.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListSyncReport {
    /// Counterparty receivers that had a current Private Payment List queued.
    pub queued: Vec<FfiPrivatePaymentListSyncChange>,
    /// Counterparty receivers that had an empty Private Payment List queued.
    pub cleared: Vec<FfiPrivatePaymentListSyncChange>,
    /// Counterparty receivers that could not be queued or cleared.
    pub failed: Vec<FfiPrivatePaymentListSyncChange>,
}

/// Plain reservation input for one Payment Endpoint.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentEndpointReservationInput {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
    /// Optional reservation expiry as RFC3339 text.
    pub expires_at: Option<String>,
    /// Adapter attribution metadata.
    pub attribution: HashMap<String, String>,
}

/// Reservation-backed Private Payment List input for one counterparty receiver.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListReservationUpdateInput {
    /// Counterparty that should receive the Private Payment List.
    pub counterparty: String,
    /// Counterparty Paykit receiver folder id.
    pub counterparty_receiver_id: String,
    /// Complete reserved receiving details to share with this counterparty.
    ///
    /// An empty list queues an empty Private Payment List for this counterparty.
    pub reservations: Vec<FfiPaymentEndpointReservationInput>,
}

/// Failed delivery after a Private Payment List was queued.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListDeliveryFailure {
    /// Counterparty whose outbound delivery failed.
    pub counterparty: String,
    /// Counterparty Paykit receiver folder id.
    pub counterparty_receiver_id: String,
    /// Outbound message id, when the failure is tied to one message.
    pub outbound_message_id: Option<u64>,
    /// Reservation id, when the failure is tied to reservation cleanup.
    pub reservation_id: Option<String>,
    /// Delivery or cleanup error.
    pub error: Arc<FfiPrivateOperationError>,
}

/// Report from queueing and delivering Private Payment Lists.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListDeliveryReport {
    /// Counterparty receivers that had a non-empty Private Payment List queued.
    pub queued: Vec<FfiPrivatePaymentListSyncChange>,
    /// Counterparty receivers that had an empty Private Payment List queued.
    pub cleared: Vec<FfiPrivatePaymentListSyncChange>,
    /// Counterparty receivers that could not be queued or cleared.
    pub failed_to_queue: Vec<FfiPrivatePaymentListSyncChange>,
    /// Counterparty receivers queued successfully but failed during outbound delivery.
    pub failed_to_deliver: Vec<FfiPrivatePaymentListDeliveryFailure>,
}

/// One counterparty receiver result from a Private Payment List sync.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivatePaymentListSyncChange {
    /// Counterparty affected by the sync.
    pub counterparty: String,
    /// Counterparty Paykit receiver folder id.
    pub counterparty_receiver_id: String,
    /// Queued outbound message id, when queueing succeeded.
    pub outbound_message_id: Option<u64>,
    /// Error text, when queueing failed.
    pub error: Option<String>,
}

impl fmt::Debug for FfiPrivatePaymentListSyncChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPrivatePaymentListSyncChange")
            .field("counterparty", &self.counterparty)
            .field("outbound_message_id", &self.outbound_message_id)
            .field("error", &self.error.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Return the latest valid Private Payment List view for a counterparty.
    pub async fn current_private_payment_list(
        &self,
        counterparty: String,
        counterparty_receiver_id: String,
    ) -> Result<Option<FfiPrivatePaymentListView>, PaykitFfiError> {
        self.runtime
            .current_private_payment_list(
                &parse_public_key(counterparty)?,
                &parse_receiver_id(counterparty_receiver_id)?,
            )
            .await
            .map(|view| view.map(Into::into))
            .map_err(Into::into)
    }

    /// Queue the current complete Private Payment List for one counterparty receiver.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: String,
        counterparty_receiver_id: String,
    ) -> Result<FfiQueuedPrivateMessage, PaykitFfiError> {
        self.runtime
            .enqueue_private_payment_list(
                parse_public_key(counterparty)?,
                parse_receiver_id(counterparty_receiver_id)?,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue an explicit complete Private Payment List for one counterparty receiver.
    pub async fn enqueue_private_payment_list_with_receiving_details(
        &self,
        counterparty: String,
        counterparty_receiver_id: String,
        receiving_details: Vec<FfiReceivingDetail>,
    ) -> Result<FfiQueuedPrivateMessage, PaykitFfiError> {
        let receiving_details = receiving_details
            .into_iter()
            .map(TryInto::try_into)
            .collect::<paykit_sdk::Result<Vec<_>>>()?;
        self.runtime
            .enqueue_private_payment_list_with_receiving_details(
                parse_public_key(counterparty)?,
                parse_receiver_id(counterparty_receiver_id)?,
                receiving_details,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue an empty Private Payment List for one counterparty receiver.
    pub async fn clear_private_payment_list(
        &self,
        counterparty: String,
        counterparty_receiver_id: String,
    ) -> Result<FfiQueuedPrivateMessage, PaykitFfiError> {
        self.runtime
            .clear_private_payment_list(
                parse_public_key(counterparty)?,
                parse_receiver_id(counterparty_receiver_id)?,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue an empty Private Payment List and process that counterparty's queue.
    pub async fn clear_private_payment_list_and_process_outbound(
        &self,
        counterparty: String,
        counterparty_receiver_id: String,
    ) -> Result<FfiPrivatePaymentListDeliveryReport, PaykitFfiError> {
        self.runtime
            .clear_private_payment_list_and_process_outbound(
                parse_public_key(counterparty)?,
                parse_receiver_id(counterparty_receiver_id)?,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue Private Payment List updates for saved local contacts.
    pub async fn sync_contact_private_payment_lists(
        &self,
        clear_unlisted_linked_peers: bool,
    ) -> Result<FfiPrivatePaymentListSyncReport, PaykitFfiError> {
        self.runtime
            .sync_contact_private_payment_lists(clear_unlisted_linked_peers)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue contact Private Payment Lists and process pending private messages.
    pub async fn sync_contact_private_payment_lists_and_process_outbound(
        &self,
        clear_unlisted_linked_peers: bool,
    ) -> Result<FfiPrivatePaymentListDeliveryReport, PaykitFfiError> {
        self.runtime
            .sync_contact_private_payment_lists_and_process_outbound(clear_unlisted_linked_peers)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Queue reservation-backed Private Payment Lists and process their queues.
    pub async fn sync_private_payment_lists_with_reservations_and_process_outbound(
        &self,
        updates: Vec<FfiPrivatePaymentListReservationUpdateInput>,
        clear_unlisted_linked_peers: bool,
    ) -> Result<FfiPrivatePaymentListDeliveryReport, PaykitFfiError> {
        let updates = updates
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, PaykitFfiError>>()?;
        self.runtime
            .sync_private_payment_lists_with_reservations_and_process_outbound(
                updates,
                clear_unlisted_linked_peers,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<OutboundPrivateMessageStatus> for FfiOutboundPrivateMessageStatus {
    fn from(value: OutboundPrivateMessageStatus) -> Self {
        match value {
            OutboundPrivateMessageStatus::Pending => Self::Pending,
            OutboundPrivateMessageStatus::Sending => Self::Sending,
            OutboundPrivateMessageStatus::Sent => Self::Sent,
            OutboundPrivateMessageStatus::Failed => Self::Failed,
            OutboundPrivateMessageStatus::Invalid => Self::Invalid,
            OutboundPrivateMessageStatus::RecoveryRequired => Self::RecoveryRequired,
            OutboundPrivateMessageStatus::Superseded => Self::Superseded,
            _ => Self::Unknown,
        }
    }
}

impl From<OutboundPrivateMessageRecord> for FfiQueuedPrivateMessage {
    fn from(value: OutboundPrivateMessageRecord) -> Self {
        Self {
            outbound_message_id: value.outbound_message_id,
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_id: value.counterparty_receiver_id.to_string(),
            kind: value.kind,
            status: value.status.into(),
            attempt_count: value.attempt_count,
            created_at: value.created_at.to_rfc3339(),
            updated_at: value.updated_at.to_rfc3339(),
            last_attempt_at: value.last_attempt_at.map(|time| time.to_rfc3339()),
            sent_at: value.sent_at.map(|time| time.to_rfc3339()),
            last_error: value.last_error.map(|error| {
                private_error(
                    "outbound_private_queue",
                    "last_send_error",
                    "last outbound private send error",
                    error,
                )
            }),
        }
    }
}

impl From<PrivatePaymentListView> for FfiPrivatePaymentListView {
    fn from(value: PrivatePaymentListView) -> Self {
        let mut payment_endpoints = value
            .payment_endpoints
            .into_iter()
            .map(|(identifier, payload)| FfiPrivatePaymentListEndpoint {
                identifier,
                payload: Arc::new(FfiPaymentPayload::new(payload)),
            })
            .collect::<Vec<_>>();
        payment_endpoints.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        Self {
            latest_stream_item_id: value.latest_stream_item_id,
            payment_endpoints,
            last_refresh_at: value.last_refresh_at.map(|time| time.to_rfc3339()),
        }
    }
}

impl From<PrivatePaymentListSyncReport> for FfiPrivatePaymentListSyncReport {
    fn from(value: PrivatePaymentListSyncReport) -> Self {
        Self {
            queued: value.queued.into_iter().map(Into::into).collect(),
            cleared: value.cleared.into_iter().map(Into::into).collect(),
            failed: value.failed.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PrivatePaymentListDeliveryReport> for FfiPrivatePaymentListDeliveryReport {
    fn from(value: PrivatePaymentListDeliveryReport) -> Self {
        Self {
            queued: value.queued.into_iter().map(Into::into).collect(),
            cleared: value.cleared.into_iter().map(Into::into).collect(),
            failed_to_queue: value.failed_to_queue.into_iter().map(Into::into).collect(),
            failed_to_deliver: value
                .failed_to_deliver
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<PrivatePaymentListSyncChange> for FfiPrivatePaymentListSyncChange {
    fn from(value: PrivatePaymentListSyncChange) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_id: value.counterparty_receiver_id.to_string(),
            outbound_message_id: value.outbound_message_id,
            error: value.error,
        }
    }
}

impl From<PrivatePaymentListDeliveryFailure> for FfiPrivatePaymentListDeliveryFailure {
    fn from(value: PrivatePaymentListDeliveryFailure) -> Self {
        Self {
            counterparty: app_public_key(&value.counterparty),
            counterparty_receiver_id: value.counterparty_receiver_id.to_string(),
            outbound_message_id: value.outbound_message_id,
            reservation_id: value.reservation_id,
            error: private_error(
                "private_payment_list_delivery",
                "delivery_failed",
                "private payment list delivery failed",
                value.error,
            ),
        }
    }
}

impl TryFrom<FfiPaymentEndpointReservationInput> for paykit_sdk::PaymentEndpointReservation {
    type Error = paykit_sdk::PaykitSdkError;

    fn try_from(value: FfiPaymentEndpointReservationInput) -> Result<Self, Self::Error> {
        payment_endpoint_reservation_from_parts(
            value.reservation_id,
            value.identifier,
            value.payload,
            value.expires_at,
            value.attribution,
        )
    }
}

impl TryFrom<FfiPrivatePaymentListReservationUpdateInput> for PrivatePaymentListReservationUpdate {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPrivatePaymentListReservationUpdateInput) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: parse_public_key(value.counterparty)?,
            counterparty_receiver_id: parse_receiver_id(value.counterparty_receiver_id)?,
            reservations: value
                .reservations
                .into_iter()
                .map(TryInto::try_into)
                .collect::<paykit_sdk::Result<Vec<_>>>()?,
        })
    }
}

fn private_error(
    category: &'static str,
    code: &'static str,
    context: &'static str,
    value: String,
) -> Arc<FfiPrivateOperationError> {
    Arc::new(FfiPrivateOperationError::new(
        category, code, context, value,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn public_key() -> paykit_sdk::PubkyPublicKey {
        parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into()).unwrap()
    }

    fn receiver_id() -> paykit_sdk::PaykitReceiverId {
        paykit_sdk::PaykitReceiverId::new("bitkit").unwrap()
    }

    #[test]
    fn test_private_payment_list_view_sorts_and_wraps_payloads() {
        let mut payment_endpoints = HashMap::new();
        payment_endpoints.insert("btc-z".into(), "payload-z".into());
        payment_endpoints.insert("btc-a".into(), "payload-a".into());
        let view = FfiPrivatePaymentListView::from(PrivatePaymentListView {
            latest_stream_item_id: Some(9),
            payment_endpoints,
            last_refresh_at: Some("2026-06-18T11:00:00Z".parse().unwrap()),
        });

        assert_eq!(view.latest_stream_item_id, Some(9));
        assert_eq!(view.payment_endpoints[0].identifier, "btc-a");
        assert_eq!(view.payment_endpoints[1].identifier, "btc-z");
        assert_eq!(view.payment_endpoints[0].payload.export_text(), "payload-a");
        assert_eq!(
            view.last_refresh_at.as_deref(),
            Some("2026-06-18T11:00:00+00:00")
        );
    }

    #[test]
    fn test_queued_private_message_redacts_last_error() {
        let record = OutboundPrivateMessageRecord {
            outbound_message_id: 4,
            counterparty: public_key(),
            counterparty_receiver_id: receiver_id(),
            kind: "paykit.private_payment_list".into(),
            raw_json: "{\"secret\":true}".into(),
            status: OutboundPrivateMessageStatus::Failed,
            attempt_count: 1,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_attempt_at: None,
            sent_at: None,
            last_error: Some("private send secret".into()),
        };

        let ffi = FfiQueuedPrivateMessage::from(record);

        assert_eq!(ffi.status, FfiOutboundPrivateMessageStatus::Failed);
        let error = ffi.last_error.unwrap();
        assert_eq!(error.category(), "outbound_private_queue");
        assert_eq!(error.code(), "last_send_error");
        assert_eq!(error.export_debug_details(), "private send secret");
        assert!(!format!("{error:?}").contains("private send secret"));
    }

    #[test]
    fn test_private_payment_list_sync_change_redacts_error_debug() {
        let change = FfiPrivatePaymentListSyncChange {
            counterparty: public_key().to_app_key(),
            counterparty_receiver_id: receiver_id().to_string(),
            outbound_message_id: None,
            error: Some("private queue failure".into()),
        };

        let debug = format!("{change:?}");

        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("private queue failure"));
    }

    #[test]
    fn test_private_payment_list_reservation_update_converts() {
        let update = FfiPrivatePaymentListReservationUpdateInput {
            counterparty: public_key().to_app_key(),
            counterparty_receiver_id: receiver_id().to_string(),
            reservations: vec![FfiPaymentEndpointReservationInput {
                reservation_id: "reservation-1".into(),
                identifier: "btc-lightning-bolt11".into(),
                payload: "ln-reserved".into(),
                expires_at: None,
                attribution: HashMap::from([("payment_hash".into(), "hash-1".into())]),
            }],
        };

        let update = PrivatePaymentListReservationUpdate::try_from(update).unwrap();

        assert_eq!(update.reservations.len(), 1);
        assert_eq!(update.reservations[0].reservation_id, "reservation-1");
        assert_eq!(
            update.reservations[0].receiving_detail.payload,
            "ln-reserved"
        );
        assert_eq!(
            update.reservations[0].attribution.get("payment_hash"),
            Some(&"hash-1".to_string())
        );
    }

    #[test]
    fn test_private_payment_list_delivery_failure_redacts_error() {
        let failure =
            FfiPrivatePaymentListDeliveryFailure::from(PrivatePaymentListDeliveryFailure {
                counterparty: public_key(),
                counterparty_receiver_id: receiver_id(),
                outbound_message_id: Some(7),
                reservation_id: Some("reservation-1".into()),
                error: "private delivery failure".into(),
            });

        assert_eq!(failure.outbound_message_id, Some(7));
        assert_eq!(failure.reservation_id.as_deref(), Some("reservation-1"));
        assert_eq!(failure.error.category(), "private_payment_list_delivery");
        assert_eq!(
            failure.error.export_debug_details(),
            "private delivery failure"
        );
        assert!(!format!("{failure:?}").contains("private delivery failure"));
    }
}
