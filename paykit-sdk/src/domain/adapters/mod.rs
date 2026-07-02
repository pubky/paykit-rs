use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};

use crate::{identity::PubkyPublicKey, PaykitSdkError, PubkySessionAccess, Result};

/// Provides live Pubky session access to one app-owned Paykit runtime.
///
/// The provider is the boundary where the app or bindings expose current Pubky
/// access from platform storage and auth flows.
#[async_trait]
pub trait PubkySessionProvider: Send + Sync {
    /// Load live Pubky access for storage and Encrypted Link workflows.
    ///
    /// Returning `None` means no live session access is currently available. It
    /// does not sign the SDK out or clear identity-scoped storage; explicit
    /// sign-out is a separate runtime operation.
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;

    /// Load public Pubky storage for unauthenticated counterparty reads.
    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>>;

    /// Clear local Pubky session access during sign-out.
    async fn clear_session_access(&self) -> Result<()>;
}

/// Adapter for payment-method-specific receiving and payment-target selection.
///
/// Paykit SDK routes endpoints and private messages, but payment execution,
/// settlement detection, proof validation, balances, fees, and method-specific
/// risk policy remain adapter or application responsibilities.
#[async_trait]
pub trait PaymentAdapter: Send + Sync {
    /// Return current receiving details for a scope.
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> Result<Vec<ReceivingDetail>>;

    /// Reserve receiving details for a counterparty's Private Payment List.
    ///
    /// Returning `None` means this adapter does not handle reservations and the
    /// SDK should use regular receiving details.
    ///
    /// Returning `Some` means the returned reservations are the complete set of
    /// private receiving details to share for that counterparty.
    ///
    /// Reservation happens in the payment adapter before the SDK can persist
    /// linked records. Adapters that return reservations should make them
    /// idempotent, expiring, or otherwise safe to abandon if the process stops
    /// before the SDK queues a Private Payment List.
    ///
    /// The SDK cancels reservations that become invalid before they are sent.
    /// Once a reservation-backed detail has been shared, payment-specific
    /// settlement, expiry, and cleanup remain adapter responsibilities.
    async fn reserve_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
    ) -> Result<Option<Vec<PaymentEndpointReservation>>> {
        Ok(None)
    }

    /// Cancel a previously reserved receiving detail.
    ///
    /// Adapters that return reservations must implement this explicitly so
    /// cleanup cannot silently succeed while backend reservations remain held.
    async fn cancel_receiving_detail_reservation(
        &self,
        _cancellation: &PaymentEndpointReservationCancellation,
    ) -> Result<()> {
        Err(PaykitSdkError::PaymentAdapter {
            context: "adapter does not support receiving-detail reservation cancellation".into(),
            source: None,
        })
    }

    /// Return payable candidates in adapter-preferred order.
    async fn select_payment_endpoints(
        &self,
        request: &PaymentEndpointSelectionRequest,
    ) -> Result<Vec<PaymentEndpointCandidate>>;

    /// Build a payment target from a payable endpoint.
    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;
}

/// Scope used when asking for receiving details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ReceivingDetailScope {
    /// Details intended for public Payment Endpoints.
    Public,
    /// Details intended for one counterparty's Private Payment List.
    Private {
        /// Counterparty that will receive the private details.
        counterparty: PubkyPublicKey,
    },
}

/// Payment-method-specific receiving detail returned by an adapter.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for ReceivingDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReceivingDetail")
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Receiving detail reserved by the payment adapter.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointReservation {
    /// Adapter-stable reservation id; non-empty, at most 128 bytes, no control characters.
    pub reservation_id: String,
    /// Reserved receiving detail.
    pub receiving_detail: ReceivingDetail,
    /// Optional reservation expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Adapter-provided attribution metadata.
    pub attribution: HashMap<String, String>,
}

/// Request passed to cancel a receiving-detail reservation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointReservationCancellation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Counterparty the reservation was intended for.
    pub counterparty: PubkyPublicKey,
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Hash of the reserved endpoint payload.
    pub payload_hash: String,
    /// Adapter-provided attribution metadata from the reservation.
    pub attribution: HashMap<String, String>,
}

impl fmt::Debug for PaymentEndpointReservationCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointReservationCancellation")
            .field("reservation_id", &self.reservation_id)
            .field("counterparty", &self.counterparty)
            .field("identifier", &self.identifier)
            .field("payload_hash", &self.payload_hash)
            .field(
                "attribution",
                &format_args!("<redacted:{} fields>", self.attribution.len()),
            )
            .finish()
    }
}

impl fmt::Debug for PaymentEndpointReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointReservation")
            .field("reservation_id", &self.reservation_id)
            .field("receiving_detail", &self.receiving_detail)
            .field("expires_at", &self.expires_at)
            .field(
                "attribution",
                &format_args!("<redacted:{} fields>", self.attribution.len()),
            )
            .finish()
    }
}

/// Candidate endpoint to check or pay.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointCandidate {
    /// Counterparty that published the endpoint.
    pub counterparty: PubkyPublicKey,
    /// Where the endpoint was discovered.
    pub source: PaymentEndpointSource,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for PaymentEndpointCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointCandidate")
            .field("counterparty", &self.counterparty)
            .field("source", &self.source)
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Source of a discovered Payment Endpoint candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PaymentEndpointSource {
    /// Endpoint came from a counterparty-specific Private Payment List.
    PrivatePaymentList,
    /// Endpoint came from a public Payment Endpoint.
    PublicPaymentEndpoint,
}

/// Optional amount context for endpoint selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAmountContext {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Request passed to the payment adapter for payable endpoint ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: PubkyPublicKey,
    /// Optional amount context.
    pub amount: Option<PaymentAmountContext>,
    /// Candidate endpoints in SDK preference order.
    pub candidates: Vec<PaymentEndpointCandidate>,
}

/// Payment-method-specific execution payload produced by an adapter.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentTarget {
    /// Method-specific target payload.
    pub payload: String,
}

impl fmt::Debug for PaymentTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentTarget")
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

fn redacted_payload(payload: &str) -> String {
    format!("<redacted:{} bytes>", payload.len())
}

#[cfg(test)]
mod tests;
