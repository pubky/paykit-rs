use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{identity::PubkyPublicKey, PubkySessionAccess, Result};

/// Provides local Pubky session material to the SDK.
#[async_trait]
pub trait PubkySessionProvider: Send + Sync {
    /// Load live Pubky access for storage and Encrypted Link workflows.
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;

    /// Clear local Pubky session access during sign-out.
    async fn clear_session_access(&self) -> Result<()>;
}

/// Adapter for payment-method-specific endpoint and execution behavior.
#[async_trait]
pub trait PaymentAdapter: Send + Sync {
    /// Return current receiving details for a scope.
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> Result<Vec<ReceivingDetail>>;

    /// Check whether the app/payment backend can pay an endpoint.
    async fn is_endpoint_payable(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<EndpointCompatibility>;

    /// Build a payment target from a compatible endpoint.
    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;

    /// Execute a Payment Request through the payment backend.
    async fn execute_payment_request(
        &self,
        request: &PaymentRequestExecution,
    ) -> Result<PaymentExecutionResult>;
}

/// Optional adapter for contact-scoped or single-use receiving details.
#[async_trait]
pub trait EndpointReservationAdapter: Send + Sync {
    /// Reserve receiving details for a counterparty/method.
    async fn reserve(
        &self,
        counterparty: PubkyPublicKey,
        method: paykit_lib::PaymentEndpointIdentifier,
    ) -> Result<ReservedReceivingDetail>;

    /// Rotate reserved receiving details after use.
    async fn rotate_after_use(
        &self,
        reservation_id: ReservationId,
    ) -> Result<Option<ReservedReceivingDetail>>;
}

/// Optional adapter for platform/runtime scheduling.
#[async_trait]
pub trait SchedulerAdapter: Send + Sync {
    /// Schedule a recurring payment job.
    async fn schedule(&self, job: ScheduledPaymentJob) -> Result<()>;

    /// Cancel a scheduled job.
    async fn cancel(&self, job_id: ScheduledJobId) -> Result<()>;
}

/// Minimal profile data used by SDK-managed Paykit-facing workflows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileRecord {
    /// Profile owner public key.
    pub public_key: PubkyPublicKey,
    /// Optional display name.
    pub display_name: Option<String>,
    /// Optional image pointer.
    pub image: Option<String>,
}

/// Minimal contact data used by SDK-managed Paykit-facing workflows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactRecord {
    /// Contact public key.
    pub public_key: PubkyPublicKey,
    /// Optional local display snapshot.
    pub display_name: Option<String>,
}

/// Paykit-facing profile update requested by the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileUpdate {
    /// Optional display name.
    pub display_name: Option<String>,
    /// Optional image pointer.
    pub image: Option<String>,
}

/// Scope used when asking for receiving details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceivingDetailScope {
    /// Details intended for public Payment Endpoints.
    Public,
    /// Details intended for one counterparty's Private Payment List.
    Private { counterparty: PubkyPublicKey },
}

/// Payment-method-specific receiving detail returned by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivingDetail {
    /// Paykit endpoint identifier.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

/// Candidate endpoint to check or pay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointCandidate {
    /// Counterparty that published the endpoint.
    pub counterparty: PubkyPublicKey,
    /// Paykit endpoint identifier.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

/// Compatibility result for one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointCompatibility {
    /// The endpoint can be paid by the adapter.
    Payable,
    /// The endpoint kind is unsupported.
    Unsupported { reason: Option<String> },
    /// The endpoint is recognized but stale/unusable.
    Stale { reason: Option<String> },
}

/// Payment target produced by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentTarget {
    /// Method-specific target payload.
    pub payload: String,
}

/// Payment Request execution input passed to an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentRequestExecution {
    /// Payment Request ID.
    pub payment_request_id: String,
    /// Method-specific payment target.
    pub target: PaymentTarget,
}

/// Result returned after payment execution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentExecutionResult {
    /// Method-specific proof payload, if available.
    pub proof: Option<String>,
    /// Whether the payment backend considers the payment settled.
    pub settled: bool,
}

/// Identifier for adapter-managed endpoint reservations.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReservationId(pub String);

/// Reserved receiving detail returned by an adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedReceivingDetail {
    /// Adapter reservation identifier.
    pub reservation_id: ReservationId,
    /// Receiving detail bound to the reservation.
    pub receiving_detail: ReceivingDetail,
}

/// Identifier for scheduled jobs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScheduledJobId(pub String);

/// Recurring payment job passed to the scheduler adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledPaymentJob {
    /// Job identifier.
    pub job_id: ScheduledJobId,
    /// Payment Request ID associated with the job.
    pub payment_request_id: String,
}
