//! Separate public and private payment resolution results.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{
    domain::linked_peers::LinkedPeerHandshakeReport,
    domain::outbound_private::OutboundPrivateSendReport,
    domain::private_stream::PrivateStreamIntakeReport, PaymentTarget,
    PrivatePaymentEndpointCandidate, PublicPaymentEndpointCandidate,
};

/// Result category for public Payment Endpoint resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PublicPaymentResolutionStatus {
    /// A payable public Payment Endpoint was found.
    Payable,
    /// No public Payment Endpoint was found.
    NoEndpoint,
    /// Public Payment Endpoints exist but are unsupported.
    UnsupportedEndpoint,
}

/// Result category for private Payment Endpoint resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PrivatePaymentResolutionStatus {
    /// A payable private Payment Endpoint was found.
    Payable,
    /// No private Payment Endpoint was found.
    NoEndpoint,
    /// Private Payment Endpoints exist but are unsupported.
    UnsupportedEndpoint,
}

/// Private state observed while resolving a Private Payment List.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PrivatePaymentResolutionState {
    /// Private Payment List candidates were available for resolution.
    Available,
    /// No Private Payment List candidate was available.
    NoPrivateEndpoint,
    /// Private payment state is blocked by Encrypted Link recovery.
    RecoveryPending,
}

/// Public Payment Endpoint paired with its adapter-built payment target.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPublicPaymentEndpoint {
    /// Payable public endpoint returned by the payment adapter.
    pub endpoint: PublicPaymentEndpointCandidate,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: PaymentTarget,
}

impl fmt::Debug for ResolvedPublicPaymentEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedPublicPaymentEndpoint")
            .field("endpoint", &self.endpoint)
            .field("target", &self.target)
            .finish()
    }
}

/// Private Payment Endpoint paired with its adapter-built payment target.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedPrivatePaymentEndpoint {
    /// Payable private endpoint returned by the payment adapter.
    pub endpoint: PrivatePaymentEndpointCandidate,
    /// Adapter-built target for executing payment through this endpoint.
    pub target: PaymentTarget,
}

impl fmt::Debug for ResolvedPrivatePaymentEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedPrivatePaymentEndpoint")
            .field("endpoint", &self.endpoint)
            .field("target", &self.target)
            .finish()
    }
}

/// Result of resolving public Payment Endpoints for one counterparty.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicContactPaymentResolution {
    /// Public payment resolution outcome.
    pub status: PublicPaymentResolutionStatus,
    /// Payable public Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<ResolvedPublicPaymentEndpoint>,
}

impl fmt::Debug for PublicContactPaymentResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicContactPaymentResolution")
            .field("status", &self.status)
            .field("payable_endpoints", &self.payable_endpoints)
            .finish()
    }
}

/// Result of resolving a Private Payment List for one counterparty.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateContactPaymentResolution {
    /// Private payment resolution outcome.
    pub status: PrivatePaymentResolutionStatus,
    /// Encrypted Link and Private Payment List state observed during resolution.
    pub state: PrivatePaymentResolutionState,
    /// Payable private Payment Endpoints in adapter-preferred order.
    pub payable_endpoints: Vec<ResolvedPrivatePaymentEndpoint>,
}

impl fmt::Debug for PrivateContactPaymentResolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateContactPaymentResolution")
            .field("status", &self.status)
            .field("state", &self.state)
            .field("payable_endpoints", &self.payable_endpoints)
            .finish()
    }
}

/// Result of preparing private contact state and resolving private endpoints.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedPrivateContactPayment {
    /// Private endpoint resolution after preparation.
    pub resolution: PrivateContactPaymentResolution,
    /// Encrypted Link handshake/advance report, when setup was attempted.
    pub link_report: Option<LinkedPeerHandshakeReport>,
    /// Private stream receive report, when messages were refreshed.
    pub receive_report: Option<PrivateStreamIntakeReport>,
    /// Outbound private send report, when queued messages were processed.
    pub outbound_report: Option<OutboundPrivateSendReport>,
}

impl fmt::Debug for PreparedPrivateContactPayment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreparedPrivateContactPayment")
            .field("resolution", &self.resolution)
            .field("link_report", &self.link_report)
            .field("receive_report", &self.receive_report)
            .field("outbound_report", &self.outbound_report)
            .finish()
    }
}
