//! Contact records and contact payment resolution types.

use serde::{Deserialize, Serialize};

use crate::{
    PaymentAmountContext, PaymentEndpointCandidate, PaymentEndpointEvaluation, PubkyPublicKey,
};

/// Result category for contact payment resolution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContactPaymentResolutionStatus {
    /// A payable endpoint was found.
    Payable,
    /// No endpoint was found.
    NoEndpoint,
    /// Endpoints exist but are unsupported.
    UnsupportedEndpoint,
    /// Private recovery is still in progress.
    PrivateRecoveryPending,
    /// The local identity cannot establish private links.
    PublicOnlySession,
}

/// Request to resolve a payable endpoint for one counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolutionRequest {
    /// Counterparty to pay.
    pub counterparty: PubkyPublicKey,
    /// Optional amount context used by the payment adapter.
    pub amount: Option<PaymentAmountContext>,
}

/// Result of resolving a contact payment endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactPaymentResolution {
    /// Resolution status.
    pub status: ContactPaymentResolutionStatus,
    /// Selected endpoint, when one is payable.
    pub selected_endpoint: Option<PaymentEndpointCandidate>,
    /// Adapter evaluations from candidate checks.
    pub evaluations: Vec<PaymentEndpointEvaluation>,
    /// Whether public Payment Endpoints were used after private candidates.
    pub used_public_fallback: bool,
}
