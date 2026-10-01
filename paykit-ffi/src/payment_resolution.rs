use std::sync::Arc;

use paykit_sdk::{
    PaymentTarget, PreparedPrivateContactPayment, PrivateContactPaymentResolution,
    PrivatePaymentResolutionState, PrivatePaymentResolutionStatus, PublicContactPaymentResolution,
    PublicPaymentEndpointLoadFailure, PublicPaymentEndpointLoadFailureKind,
    PublicPaymentResolutionStatus, ResolvedPrivatePaymentEndpoint, ResolvedPublicPaymentEndpoint,
};

use crate::{
    conversions_common::parse_payment_request_id,
    payment_adapter::{FfiPaymentAmountContext, FfiPaymentPayload, FfiPaymentTarget},
    private_links::{
        FfiLinkedPeerHandshakeReport, FfiOutboundPrivateSendReport, FfiPrivateStreamIntakeReport,
    },
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key},
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
    /// Registered apps were found, but none of their endpoint lists could be loaded.
    Unavailable,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Category of an app-specific public Payment Endpoint load failure.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPublicPaymentEndpointLoadFailureKind {
    /// Pubky storage could not be reached or read.
    Transport,
    /// The app's published endpoint data was invalid.
    InvalidData,
    /// The bounded aggregate could not include this app's endpoint list.
    ResourceLimit,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Failure to load one registered app's public Payment Endpoints.
#[derive(uniffi::Record, Clone)]
pub struct FfiPublicPaymentEndpointLoadFailure {
    /// App whose endpoint list could not be loaded.
    pub app_id: String,
    /// Stable failure category for application handling.
    pub kind: FfiPublicPaymentEndpointLoadFailureKind,
    /// Human-readable context without an underlying transport cause.
    pub context: String,
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
    /// No Private Payment List newer than the caller's consumed version is available.
    WaitingForUpdatedPaymentList,
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
#[derive(uniffi::Record, Clone)]
pub struct FfiResolvedPublicPaymentEndpoint {
    /// Counterparty that published the endpoint.
    pub counterparty: String,
    /// Application that published the endpoint.
    pub app_id: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: FfiPaymentTarget,
}

/// Private Payment Endpoint paired with its adapter-built payment target.
#[derive(uniffi::Record, Clone)]
pub struct FfiResolvedPrivatePaymentEndpoint {
    /// Counterparty that privately shared the endpoint.
    pub counterparty: String,
    /// Application that privately shared the endpoint.
    pub app_id: String,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: Arc<FfiPaymentPayload>,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: FfiPaymentTarget,
}

/// Result of resolving public Payment Endpoints for one counterparty.
#[derive(uniffi::Record, Clone)]
pub struct FfiPublicContactPaymentResolution {
    /// Public payment resolution outcome.
    pub status: FfiPublicPaymentResolutionStatus,
    /// Payable public Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<FfiResolvedPublicPaymentEndpoint>,
    /// Registered apps whose endpoint lists could not be loaded.
    pub failures: Vec<FfiPublicPaymentEndpointLoadFailure>,
}

/// Result of resolving a Private Payment List for one counterparty.
#[derive(uniffi::Record, Clone)]
pub struct FfiPrivateContactPaymentResolution {
    /// Private payment resolution outcome.
    pub status: FfiPrivatePaymentResolutionStatus,
    /// Encrypted Link and Private Payment List state observed during resolution.
    pub state: FfiPrivatePaymentResolutionState,
    /// Opaque freshness token for the Private Payment List used by this result.
    pub private_payment_list_version: Option<u64>,
    /// Payable private Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<FfiResolvedPrivatePaymentEndpoint>,
}

/// Result of preparing private contact state and resolving private endpoints.
#[derive(uniffi::Record, Clone)]
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

impl_redacted_debug!(
    FfiPublicPaymentEndpointLoadFailure,
    FfiResolvedPublicPaymentEndpoint,
    FfiResolvedPrivatePaymentEndpoint,
    FfiPublicContactPaymentResolution,
    FfiPrivateContactPaymentResolution,
    FfiPreparedPrivateContactPayment,
);

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Resolve payable private endpoints for one counterparty.
    ///
    /// Pass the last consumed list version to require a newer Private Payment
    /// List. The returned version and endpoints come from the same local list
    /// snapshot.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<FfiPrivateContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_private_contact_payment(
                parse_public_key(counterparty)?,
                amount.map(Into::into),
                after_private_payment_list_version,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve payable public Payment Endpoints for one counterparty.
    pub async fn resolve_public_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
    ) -> Result<FfiPublicContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_public_contact_payment(parse_public_key(counterparty)?, amount.map(Into::into))
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve private endpoints allowed by an actionable received Payment Request.
    pub async fn resolve_private_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
        after_private_payment_list_version: Option<u64>,
    ) -> Result<FfiPrivateContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_private_payment_request(
                parse_public_key(counterparty)?,
                &parse_payment_request_id(payment_request_id)?,
                after_private_payment_list_version,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve public endpoints allowed by an actionable received Payment Request.
    pub async fn resolve_public_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
    ) -> Result<FfiPublicContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_public_payment_request(
                parse_public_key(counterparty)?,
                &parse_payment_request_id(payment_request_id)?,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Prepare private contact state, then resolve private endpoints.
    ///
    /// Pass the last consumed list version to require a newer Private Payment
    /// List after private messages have been refreshed.
    pub async fn prepare_and_resolve_private_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
        after_private_payment_list_version: Option<u64>,
        max_advance_steps: u32,
    ) -> Result<FfiPreparedPrivateContactPayment, PaykitFfiError> {
        self.runtime
            .prepare_and_resolve_private_contact_payment(
                parse_public_key(counterparty)?,
                amount.map(Into::into),
                after_private_payment_list_version,
                max_advance_steps,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Prepare private state, then resolve endpoints allowed by a Payment Request.
    pub async fn prepare_and_resolve_private_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
        after_private_payment_list_version: Option<u64>,
        max_advance_steps: u32,
    ) -> Result<FfiPreparedPrivateContactPayment, PaykitFfiError> {
        self.runtime
            .prepare_and_resolve_private_payment_request(
                parse_public_key(counterparty)?,
                &parse_payment_request_id(payment_request_id)?,
                after_private_payment_list_version,
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
            PublicPaymentResolutionStatus::Unavailable => Self::Unavailable,
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
            PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList => {
                Self::WaitingForUpdatedPaymentList
            }
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
            app_id: value.endpoint.app_id.to_string(),
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
            app_id: value.endpoint.app_id.to_string(),
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
            failures: value.failures.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<PublicPaymentEndpointLoadFailureKind> for FfiPublicPaymentEndpointLoadFailureKind {
    fn from(value: PublicPaymentEndpointLoadFailureKind) -> Self {
        match value {
            PublicPaymentEndpointLoadFailureKind::Transport => Self::Transport,
            PublicPaymentEndpointLoadFailureKind::InvalidData => Self::InvalidData,
            PublicPaymentEndpointLoadFailureKind::ResourceLimit => Self::ResourceLimit,
            _ => Self::Unknown,
        }
    }
}

impl From<PublicPaymentEndpointLoadFailure> for FfiPublicPaymentEndpointLoadFailure {
    fn from(value: PublicPaymentEndpointLoadFailure) -> Self {
        Self {
            app_id: value.app_id.to_string(),
            kind: value.kind.into(),
            context: value.context,
        }
    }
}

impl From<PrivateContactPaymentResolution> for FfiPrivateContactPaymentResolution {
    fn from(value: PrivateContactPaymentResolution) -> Self {
        Self {
            status: value.status.into(),
            state: value.state.into(),
            private_payment_list_version: value.private_payment_list_version,
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

    fn app_id() -> paykit_sdk::PaykitAppId {
        paykit_sdk::PaykitAppId::new("bitkit").unwrap()
    }

    #[test]
    fn test_public_payment_resolution_maps_public_endpoint() {
        let resolution = PublicContactPaymentResolution {
            status: PublicPaymentResolutionStatus::Payable,
            payable_endpoints: vec![ResolvedPublicPaymentEndpoint {
                endpoint: PublicPaymentEndpointCandidate {
                    counterparty: public_key(),
                    app_id: app_id(),
                    identifier: "btc-mainnet-address".into(),
                    payload: "bc1qpublic".into(),
                },
                target: PaymentTarget {
                    payload: "public-target".into(),
                },
            }],
            failures: Vec::new(),
        };

        let ffi = FfiPublicContactPaymentResolution::from(resolution);

        assert_eq!(ffi.status, FfiPublicPaymentResolutionStatus::Payable);
        assert_eq!(ffi.payable_endpoints[0].payload.export_text(), "bc1qpublic");
        assert!(ffi.failures.is_empty());
    }

    #[test]
    fn test_public_payment_resolution_maps_app_load_failure() {
        let resolution = PublicContactPaymentResolution {
            status: PublicPaymentResolutionStatus::Unavailable,
            payable_endpoints: Vec::new(),
            failures: vec![PublicPaymentEndpointLoadFailure {
                app_id: app_id(),
                kind: PublicPaymentEndpointLoadFailureKind::InvalidData,
                context: "invalid endpoint listing".into(),
            }],
        };

        let ffi = FfiPublicContactPaymentResolution::from(resolution);

        assert_eq!(ffi.status, FfiPublicPaymentResolutionStatus::Unavailable);
        assert_eq!(ffi.failures.len(), 1);
        assert_eq!(ffi.failures[0].app_id, "bitkit");
        assert_eq!(
            ffi.failures[0].kind,
            FfiPublicPaymentEndpointLoadFailureKind::InvalidData
        );
    }

    #[test]
    fn test_public_payment_resolution_maps_resource_limit_failure() {
        let kind = FfiPublicPaymentEndpointLoadFailureKind::from(
            PublicPaymentEndpointLoadFailureKind::ResourceLimit,
        );

        assert_eq!(kind, FfiPublicPaymentEndpointLoadFailureKind::ResourceLimit);
    }

    #[test]
    fn test_private_payment_resolution_maps_private_state() {
        let resolution = PrivateContactPaymentResolution {
            status: PrivatePaymentResolutionStatus::Payable,
            state: PrivatePaymentResolutionState::Available,
            private_payment_list_version: Some(7),
            payable_endpoints: vec![ResolvedPrivatePaymentEndpoint {
                endpoint: PrivatePaymentEndpointCandidate {
                    counterparty: public_key(),
                    app_id: app_id(),
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
        assert_eq!(ffi.private_payment_list_version, Some(7));
        assert_eq!(
            ffi.payable_endpoints[0].payload.export_text(),
            "bc1qprivate"
        );
        assert_eq!(
            format!("{:?}", ffi.payable_endpoints[0]),
            "FfiResolvedPrivatePaymentEndpoint(<redacted>)"
        );
        assert_eq!(
            format!("{ffi:?}"),
            "FfiPrivateContactPaymentResolution(<redacted>)"
        );
    }

    #[test]
    fn test_public_payment_resolution_records_redact_debug() {
        let endpoint = FfiResolvedPublicPaymentEndpoint {
            counterparty: "counterparty-secret".into(),
            app_id: "app-secret".into(),
            identifier: "identifier-secret".into(),
            payload: Arc::new(FfiPaymentPayload::new("payload-secret".into())),
            target: FfiPaymentTarget {
                payload: Arc::new(FfiPaymentPayload::new("target-secret".into())),
            },
        };
        let failure = FfiPublicPaymentEndpointLoadFailure {
            app_id: "failure-app-secret".into(),
            kind: FfiPublicPaymentEndpointLoadFailureKind::InvalidData,
            context: "failure-context-secret".into(),
        };
        let resolution = FfiPublicContactPaymentResolution {
            status: FfiPublicPaymentResolutionStatus::Payable,
            payable_endpoints: vec![endpoint.clone()],
            failures: vec![failure.clone()],
        };

        assert_eq!(
            format!("{endpoint:?}"),
            "FfiResolvedPublicPaymentEndpoint(<redacted>)"
        );
        assert_eq!(
            format!("{failure:?}"),
            "FfiPublicPaymentEndpointLoadFailure(<redacted>)"
        );
        assert_eq!(
            format!("{resolution:?}"),
            "FfiPublicContactPaymentResolution(<redacted>)"
        );
    }

    #[test]
    fn test_prepared_private_payment_redacts_nested_reports() {
        let resolution = FfiPrivateContactPaymentResolution {
            status: FfiPrivatePaymentResolutionStatus::NoEndpoint,
            state: FfiPrivatePaymentResolutionState::NoPrivateEndpoint,
            private_payment_list_version: None,
            payable_endpoints: Vec::new(),
        };
        let prepared = FfiPreparedPrivateContactPayment {
            resolution,
            link_report: None,
            receive_report: None,
            outbound_report: None,
        };

        assert_eq!(
            format!("{prepared:?}"),
            "FfiPreparedPrivateContactPayment(<redacted>)"
        );
    }

    #[test]
    fn test_private_payment_resolution_maps_waiting_status() {
        let status = FfiPrivatePaymentResolutionStatus::from(
            PrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList,
        );

        assert_eq!(
            status,
            FfiPrivatePaymentResolutionStatus::WaitingForUpdatedPaymentList
        );
    }
}
