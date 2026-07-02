use std::sync::Arc;

use paykit_sdk::{
    storage::OutboundPrivateMessageRecord, ContactPaymentResolution,
    ContactPaymentResolutionPrivateState, ContactPaymentResolutionRequest,
    ContactPaymentResolutionStatus, OutboundPrivateMessageStatus, PaymentTarget,
    PrivatePaymentListView, ResolvedPaymentEndpoint,
};

use crate::{
    payment_adapter::{
        FfiPaymentAmountContext, FfiPaymentEndpointSource, FfiPaymentPayload, FfiPaymentTarget,
    },
    private_links::FfiPrivateOperationError,
    sdk::FfiPaykitSdk,
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

/// Latest valid Private Payment List view for one counterparty.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivatePaymentListView {
    /// Stream item id of the latest valid list.
    pub latest_stream_item_id: Option<u64>,
    /// Current endpoint payloads sorted by identifier.
    pub payment_endpoints: Vec<FfiPrivatePaymentListEndpoint>,
    /// Receive time of the latest valid list as RFC3339 text.
    pub last_refresh_at: Option<String>,
}

/// Result category for contact payment resolution.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiContactPaymentResolutionStatus {
    /// A payable endpoint was found.
    Payable,
    /// No endpoint was found.
    NoEndpoint,
    /// Endpoints exist but are unsupported.
    UnsupportedEndpoint,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Private-payment state observed while resolving a contact payment.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiContactPaymentResolutionPrivateState {
    /// Private Payment List candidates were available for resolution.
    Available,
    /// No Private Payment List candidate was available.
    NoPrivateEndpoint,
    /// Private payment state is blocked by link recovery.
    RecoveryPending,
    /// The local identity cannot establish private links.
    PublicOnlySession,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Request to resolve payable endpoints for one counterparty.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiContactPaymentResolutionRequest {
    /// Counterparty to pay.
    pub counterparty: String,
    /// Optional amount context used by the payment adapter.
    pub amount: Option<FfiPaymentAmountContext>,
    /// Include public Payment Endpoints after private candidates.
    pub include_public_endpoints: bool,
}

/// Payment Endpoint paired with the target needed to pay through it.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiResolvedPaymentEndpoint {
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Where the endpoint was discovered.
    pub source: FfiPaymentEndpointSource,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: FfiPaymentTarget,
}

/// Result of resolving contact Payment Endpoints.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiContactPaymentResolution {
    /// General payment resolution outcome.
    pub status: FfiContactPaymentResolutionStatus,
    /// Private-payment-specific state for this resolution.
    pub private_state: FfiContactPaymentResolutionPrivateState,
    /// Payable Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<FfiResolvedPaymentEndpoint>,
}

#[uniffi::export]
impl FfiPaykitSdk {
    /// Return the latest valid Private Payment List view for a counterparty.
    pub async fn current_private_payment_list(
        &self,
        counterparty: String,
    ) -> Result<Option<FfiPrivatePaymentListView>, PaykitFfiError> {
        self.runtime
            .current_private_payment_list(&parse_public_key(counterparty)?)
            .await
            .map(|view| view.map(Into::into))
            .map_err(Into::into)
    }

    /// Queue the current complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: String,
    ) -> Result<FfiQueuedPrivateMessage, PaykitFfiError> {
        self.runtime
            .enqueue_private_payment_list(parse_public_key(counterparty)?)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve payable endpoints for one counterparty.
    pub async fn resolve_contact_payment(
        &self,
        request: FfiContactPaymentResolutionRequest,
    ) -> Result<FfiContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_contact_payment(request.try_into()?)
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
            counterparty: value.counterparty.to_string(),
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

impl From<ContactPaymentResolutionStatus> for FfiContactPaymentResolutionStatus {
    fn from(value: ContactPaymentResolutionStatus) -> Self {
        match value {
            ContactPaymentResolutionStatus::Payable => Self::Payable,
            ContactPaymentResolutionStatus::NoEndpoint => Self::NoEndpoint,
            ContactPaymentResolutionStatus::UnsupportedEndpoint => Self::UnsupportedEndpoint,
            _ => Self::Unknown,
        }
    }
}

impl From<ContactPaymentResolutionPrivateState> for FfiContactPaymentResolutionPrivateState {
    fn from(value: ContactPaymentResolutionPrivateState) -> Self {
        match value {
            ContactPaymentResolutionPrivateState::Available => Self::Available,
            ContactPaymentResolutionPrivateState::NoPrivateEndpoint => Self::NoPrivateEndpoint,
            ContactPaymentResolutionPrivateState::RecoveryPending => Self::RecoveryPending,
            ContactPaymentResolutionPrivateState::PublicOnlySession => Self::PublicOnlySession,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiContactPaymentResolutionRequest> for ContactPaymentResolutionRequest {
    type Error = PaykitFfiError;

    fn try_from(value: FfiContactPaymentResolutionRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            counterparty: parse_public_key(value.counterparty)?,
            amount: value.amount.map(Into::into),
            include_public_endpoints: value.include_public_endpoints,
        })
    }
}

impl From<FfiPaymentAmountContext> for paykit_sdk::PaymentAmountContext {
    fn from(value: FfiPaymentAmountContext) -> Self {
        Self {
            value: value.value,
            asset: value.asset,
        }
    }
}

impl From<PaymentTarget> for FfiPaymentTarget {
    fn from(value: PaymentTarget) -> Self {
        Self {
            payload: Arc::new(FfiPaymentPayload::new(value.payload)),
        }
    }
}

impl From<ResolvedPaymentEndpoint> for FfiResolvedPaymentEndpoint {
    fn from(value: ResolvedPaymentEndpoint) -> Self {
        Self {
            counterparty: value.endpoint.counterparty.to_string(),
            source: value.endpoint.source.into(),
            identifier: value.endpoint.identifier,
            payload: Arc::new(FfiPaymentPayload::new(value.endpoint.payload)),
            target: value.target.into(),
        }
    }
}

impl From<ContactPaymentResolution> for FfiContactPaymentResolution {
    fn from(value: ContactPaymentResolution) -> Self {
        Self {
            status: value.status.into(),
            private_state: value.private_state.into(),
            payable_endpoints: value
                .payable_endpoints
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

fn parse_public_key(value: String) -> Result<paykit_sdk::PubkyPublicKey, PaykitFfiError> {
    paykit_sdk::PubkyPublicKey::new(value).map_err(Into::into)
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
    use paykit_sdk::PaymentEndpointCandidate;
    use std::collections::HashMap;

    fn public_key() -> paykit_sdk::PubkyPublicKey {
        parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into()).unwrap()
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
    fn test_contact_payment_resolution_maps_endpoint_and_target() {
        let resolution = ContactPaymentResolution {
            status: ContactPaymentResolutionStatus::Payable,
            private_state: ContactPaymentResolutionPrivateState::Available,
            payable_endpoints: vec![ResolvedPaymentEndpoint {
                endpoint: PaymentEndpointCandidate {
                    counterparty: public_key(),
                    source: paykit_sdk::PaymentEndpointSource::PrivatePaymentList,
                    identifier: "btc-mainnet-address".into(),
                    payload: "bc1qprivate".into(),
                },
                target: PaymentTarget {
                    payload: "wallet-target".into(),
                },
            }],
        };

        let ffi = FfiContactPaymentResolution::from(resolution);

        assert_eq!(ffi.status, FfiContactPaymentResolutionStatus::Payable);
        assert_eq!(
            ffi.private_state,
            FfiContactPaymentResolutionPrivateState::Available
        );
        assert_eq!(ffi.payable_endpoints[0].identifier, "btc-mainnet-address");
        assert_eq!(
            ffi.payable_endpoints[0].payload.export_text(),
            "bc1qprivate"
        );
        assert_eq!(
            ffi.payable_endpoints[0].target.payload.export_text(),
            "wallet-target"
        );
    }

    #[test]
    fn test_queued_private_message_redacts_last_error() {
        let record = OutboundPrivateMessageRecord {
            outbound_message_id: 4,
            counterparty: public_key(),
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
}
