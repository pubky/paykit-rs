use std::sync::Arc;

use paykit_sdk::{
    PaymentTarget, PreparedPrivateContactPayment, PrivateContactPaymentResolution,
    PrivatePaymentResolutionState, PrivatePaymentResolutionStatus, PublicContactPaymentResolution,
    PublicPaymentResolutionStatus, ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
};

use crate::{
    payment_adapter::{FfiPaymentAmountContext, FfiPaymentPayload, FfiPaymentTarget},
    private_links::{
        FfiLinkedPeerHandshakeReport, FfiOutboundPrivateSendReport, FfiPrivateStreamIntakeReport,
    },
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key, parse_receiver_path},
    PaykitFfiError,
};

/// Result category for public Payment Endpoint resolution.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPublicPaymentResolutionStatus {
    /// A payable public Payment Endpoint was found.
    Payable,
    /// No public Payment Endpoint was found.
    NoEndpoint,
    /// Public Payment Endpoints exist but are unsupported.
    UnsupportedEndpoint,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Result category for private Payment Endpoint resolution.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPrivatePaymentResolutionStatus {
    /// A payable private Payment Endpoint was found.
    Payable,
    /// No private Payment Endpoint was found.
    NoEndpoint,
    /// Private Payment Endpoints exist but are unsupported.
    UnsupportedEndpoint,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Encrypted Link and Private Payment List state observed during resolution.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPrivatePaymentResolutionState {
    /// Private Payment List candidates were available for resolution.
    Available,
    /// No Private Payment List candidate was available.
    NoPrivateEndpoint,
    /// Private payment state is blocked by Encrypted Link recovery.
    RecoveryPending,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Public Payment Endpoint paired with its adapter-built payment target.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiResolvedPublicPaymentEndpoint {
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: FfiPaymentTarget,
}

/// Private Payment Endpoint paired with its adapter-built payment target.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiResolvedPrivatePaymentEndpoint {
    /// Counterparty that privately shared the endpoint.
    pub counterparty: String,
    /// Counterparty Paykit receiver path.
    pub counterparty_receiver_path: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: FfiPaymentTarget,
}

/// Result of resolving public Payment Endpoints for one counterparty.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPublicContactPaymentResolution {
    /// Public payment resolution outcome.
    pub status: FfiPublicPaymentResolutionStatus,
    /// Payable public Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<FfiResolvedPublicPaymentEndpoint>,
}

/// Result of resolving a Private Payment List for one counterparty.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPrivateContactPaymentResolution {
    /// Private payment resolution outcome.
    pub status: FfiPrivatePaymentResolutionStatus,
    /// Encrypted Link and Private Payment List state observed during resolution.
    pub state: FfiPrivatePaymentResolutionState,
    /// Payable private Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<FfiResolvedPrivatePaymentEndpoint>,
}

/// Result of preparing private contact state and resolving private endpoints.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPreparedPrivateContactPayment {
    /// Private endpoint resolution after preparation.
    pub resolution: FfiPrivateContactPaymentResolution,
    /// Encrypted Link handshake/advance report, when setup was attempted.
    pub link_report: Option<FfiLinkedPeerHandshakeReport>,
    /// Private stream receive report, when messages were refreshed.
    pub receive_report: Option<FfiPrivateStreamIntakeReport>,
    /// Outbound private send report, when queued messages were processed.
    pub outbound_report: Option<FfiOutboundPrivateSendReport>,
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Resolve payable private endpoints for one counterparty.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        amount: Option<FfiPaymentAmountContext>,
    ) -> Result<FfiPrivateContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_private_contact_payment(
                parse_public_key(counterparty)?,
                parse_receiver_path(counterparty_receiver_path)?,
                amount.map(Into::into),
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve payable public Payment Endpoints for one counterparty.
    pub async fn resolve_public_contact_payment(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        amount: Option<FfiPaymentAmountContext>,
    ) -> Result<FfiPublicContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_public_contact_payment(
                parse_public_key(counterparty)?,
                parse_receiver_path(counterparty_receiver_path)?,
                amount.map(Into::into),
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Prepare private contact state, then resolve private endpoints.
    pub async fn prepare_and_resolve_private_contact_payment(
        &self,
        counterparty: String,
        counterparty_receiver_path: String,
        amount: Option<FfiPaymentAmountContext>,
        max_advance_steps: u32,
    ) -> Result<FfiPreparedPrivateContactPayment, PaykitFfiError> {
        self.runtime
            .prepare_and_resolve_private_contact_payment(
                parse_public_key(counterparty)?,
                parse_receiver_path(counterparty_receiver_path)?,
                amount.map(Into::into),
                max_advance_steps,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }
}

impl From<PublicPaymentResolutionStatus> for FfiPublicPaymentResolutionStatus {
    fn from(value: PublicPaymentResolutionStatus) -> Self {
        match value {
            PublicPaymentResolutionStatus::Payable => Self::Payable,
            PublicPaymentResolutionStatus::NoEndpoint => Self::NoEndpoint,
            PublicPaymentResolutionStatus::UnsupportedEndpoint => Self::UnsupportedEndpoint,
            _ => Self::Unknown,
        }
    }
}

impl From<PrivatePaymentResolutionStatus> for FfiPrivatePaymentResolutionStatus {
    fn from(value: PrivatePaymentResolutionStatus) -> Self {
        match value {
            PrivatePaymentResolutionStatus::Payable => Self::Payable,
            PrivatePaymentResolutionStatus::NoEndpoint => Self::NoEndpoint,
            PrivatePaymentResolutionStatus::UnsupportedEndpoint => Self::UnsupportedEndpoint,
            _ => Self::Unknown,
        }
    }
}

impl From<PrivatePaymentResolutionState> for FfiPrivatePaymentResolutionState {
    fn from(value: PrivatePaymentResolutionState) -> Self {
        match value {
            PrivatePaymentResolutionState::Available => Self::Available,
            PrivatePaymentResolutionState::NoPrivateEndpoint => Self::NoPrivateEndpoint,
            PrivatePaymentResolutionState::RecoveryPending => Self::RecoveryPending,
            _ => Self::Unknown,
        }
    }
}

impl From<PreparedPrivateContactPayment> for FfiPreparedPrivateContactPayment {
    fn from(value: PreparedPrivateContactPayment) -> Self {
        Self {
            resolution: value.resolution.into(),
            link_report: value.link_report.map(Into::into),
            receive_report: value.receive_report.map(Into::into),
            outbound_report: value.outbound_report.map(Into::into),
        }
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

impl From<ResolvedPublicPaymentEndpoint> for FfiResolvedPublicPaymentEndpoint {
    fn from(value: ResolvedPublicPaymentEndpoint) -> Self {
        Self {
            counterparty: app_public_key(&value.endpoint.counterparty),
            counterparty_receiver_path: value.endpoint.counterparty_receiver_path.to_string(),
            identifier: value.endpoint.identifier,
            payload: Arc::new(FfiPaymentPayload::new(value.endpoint.payload)),
            target: value.target.into(),
        }
    }
}

impl From<ResolvedPrivatePaymentEndpoint> for FfiResolvedPrivatePaymentEndpoint {
    fn from(value: ResolvedPrivatePaymentEndpoint) -> Self {
        Self {
            counterparty: app_public_key(&value.endpoint.counterparty),
            counterparty_receiver_path: value.endpoint.counterparty_receiver_path.to_string(),
            identifier: value.endpoint.identifier,
            payload: Arc::new(FfiPaymentPayload::new(value.endpoint.payload)),
            target: value.target.into(),
        }
    }
}

impl From<PublicContactPaymentResolution> for FfiPublicContactPaymentResolution {
    fn from(value: PublicContactPaymentResolution) -> Self {
        Self {
            status: value.status.into(),
            payable_endpoints: value
                .payable_endpoints
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

impl From<PrivateContactPaymentResolution> for FfiPrivateContactPaymentResolution {
    fn from(value: PrivateContactPaymentResolution) -> Self {
        Self {
            status: value.status.into(),
            state: value.state.into(),
            payable_endpoints: value
                .payable_endpoints
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use paykit_sdk::{
        PrivatePaymentEndpointCandidate, PublicPaymentEndpointCandidate,
        ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
    };

    fn public_key() -> paykit_sdk::PubkyPublicKey {
        parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into()).unwrap()
    }

    fn receiver_path() -> paykit_sdk::PaykitReceiverPath {
        paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap()
    }

    #[test]
    fn test_public_payment_resolution_maps_public_endpoint() {
        let resolution = PublicContactPaymentResolution {
            status: PublicPaymentResolutionStatus::Payable,
            payable_endpoints: vec![ResolvedPublicPaymentEndpoint {
                endpoint: PublicPaymentEndpointCandidate {
                    counterparty: public_key(),
                    counterparty_receiver_path: receiver_path(),
                    identifier: "btc-mainnet-address".into(),
                    payload: "bc1qpublic".into(),
                },
                target: PaymentTarget {
                    payload: "public-target".into(),
                },
            }],
        };

        let ffi = FfiPublicContactPaymentResolution::from(resolution);

        assert_eq!(ffi.status, FfiPublicPaymentResolutionStatus::Payable);
        assert_eq!(ffi.payable_endpoints[0].payload.export_text(), "bc1qpublic");
    }

    #[test]
    fn test_private_payment_resolution_maps_private_state() {
        let resolution = PrivateContactPaymentResolution {
            status: PrivatePaymentResolutionStatus::Payable,
            state: PrivatePaymentResolutionState::Available,
            payable_endpoints: vec![ResolvedPrivatePaymentEndpoint {
                endpoint: PrivatePaymentEndpointCandidate {
                    counterparty: public_key(),
                    counterparty_receiver_path: receiver_path(),
                    identifier: "btc-mainnet-address".into(),
                    payload: "bc1qprivate".into(),
                },
                target: PaymentTarget {
                    payload: "private-target".into(),
                },
            }],
        };

        let ffi = FfiPrivateContactPaymentResolution::from(resolution);

        assert_eq!(ffi.status, FfiPrivatePaymentResolutionStatus::Payable);
        assert_eq!(ffi.state, FfiPrivatePaymentResolutionState::Available);
        assert_eq!(
            ffi.payable_endpoints[0].payload.export_text(),
            "bc1qprivate"
        );
    }
}
