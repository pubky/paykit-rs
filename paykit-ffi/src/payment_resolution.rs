use std::sync::Arc;

use paykit_sdk::{
    ContactPaymentResolution, ContactPaymentResolutionPrivateState,
    ContactPaymentResolutionRequest, ContactPaymentResolutionStatus, PaymentTarget,
    PreparedContactPayment, ResolvedPaymentEndpoint,
};

use crate::{
    payment_adapter::{
        FfiPaymentAmountContext, FfiPaymentEndpointSource, FfiPaymentPayload, FfiPaymentTarget,
    },
    private_links::{
        FfiLinkedPeerHandshakeReport, FfiOutboundPrivateSendReport, FfiPrivateOperationError,
        FfiPrivateStreamIntakeReport,
    },
    sdk::FfiPaykitSdk,
    session::{app_public_key, parse_public_key},
    PaykitFfiError,
};

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

/// Result of preparing a contact payment and resolving payable endpoints.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPreparedContactPayment {
    /// Endpoint resolution after preparation.
    pub resolution: FfiContactPaymentResolution,
    /// Link handshake/advance report when the SDK attempted private setup.
    pub link_report: Option<FfiLinkedPeerHandshakeReport>,
    /// Private receive report when the SDK refreshed the private stream.
    pub receive_report: Option<FfiPrivateStreamIntakeReport>,
    /// Outbound send report when the SDK processed pending private messages.
    pub outbound_report: Option<FfiOutboundPrivateSendReport>,
    /// Private preparation error when public fallback was allowed.
    pub private_error: Option<Arc<FfiPrivateOperationError>>,
}

#[uniffi::export]
impl FfiPaykitSdk {
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

    /// Resolve payable private endpoints for one counterparty.
    pub async fn resolve_private_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
    ) -> Result<FfiContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_private_contact_payment(
                parse_public_key(counterparty)?,
                amount.map(Into::into),
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Resolve payable public endpoints for one counterparty.
    pub async fn resolve_public_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
    ) -> Result<FfiContactPaymentResolution, PaykitFfiError> {
        self.runtime
            .resolve_public_contact_payment(parse_public_key(counterparty)?, amount.map(Into::into))
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Prepare private contact state, then resolve payable endpoints.
    ///
    /// The SDK refreshes live session capability, ensures or advances the
    /// private link when possible, receives pending private messages, processes
    /// pending outbound private messages, then resolves endpoints private-first.
    /// Public endpoints are included only when requested.
    pub async fn prepare_and_resolve_contact_payment(
        &self,
        counterparty: String,
        amount: Option<FfiPaymentAmountContext>,
        include_public_endpoints: bool,
        max_advance_steps: u32,
    ) -> Result<FfiPreparedContactPayment, PaykitFfiError> {
        self.runtime
            .prepare_and_resolve_contact_payment(
                parse_public_key(counterparty)?,
                amount.map(Into::into),
                include_public_endpoints,
                max_advance_steps,
            )
            .await
            .map(Into::into)
            .map_err(Into::into)
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

impl From<PreparedContactPayment> for FfiPreparedContactPayment {
    fn from(value: PreparedContactPayment) -> Self {
        Self {
            resolution: value.resolution.into(),
            link_report: value.link_report.map(Into::into),
            receive_report: value.receive_report.map(Into::into),
            outbound_report: value.outbound_report.map(Into::into),
            private_error: value.private_error.map(|error| {
                private_error(
                    "contact_payment_preparation",
                    "private_preparation_error",
                    "private contact payment preparation failed",
                    error,
                )
            }),
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
            counterparty: app_public_key(&value.endpoint.counterparty),
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
    use paykit_sdk::PaymentEndpointCandidate;

    fn public_key() -> paykit_sdk::PubkyPublicKey {
        parse_public_key("8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io".into()).unwrap()
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
}
