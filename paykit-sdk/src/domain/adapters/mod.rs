use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};

use crate::{
    identity::PubkyPublicKey, PaykitReceiverPath, PaykitSdkError, PubkySessionAccess, Result,
};

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

/// Adapter for payment-method-specific public and private payment operations.
///
/// Paykit SDK routes endpoints and private messages, but payment execution,
/// settlement detection, proof validation, balances, fees, and method-specific
/// risk policy remain adapter or application responsibilities.
#[async_trait]
pub trait PaymentAdapter: Send + Sync {
    /// Return receiving details intended for public Payment Endpoints.
    async fn current_public_receiving_details(&self) -> Result<Vec<PublicReceivingDetail>> {
        Err(unsupported_operation("public receiving details"))
    }

    /// Return receiving details for one counterparty's Private Payment List.
    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Vec<PrivateReceivingDetail>> {
        Err(unsupported_operation("private receiving details"))
    }

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
    async fn reserve_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Result<Option<Vec<PrivatePaymentEndpointReservation>>> {
        Ok(None)
    }

    /// Cancel a previously reserved receiving detail.
    ///
    /// Adapters that return reservations must implement this explicitly so
    /// cleanup cannot silently succeed while backend reservations remain held.
    async fn cancel_private_receiving_detail_reservation(
        &self,
        _cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        Err(PaykitSdkError::PaymentAdapter {
            context: "adapter does not support receiving-detail reservation cancellation".into(),
            source: None,
        })
    }

    /// Return payable public candidates in adapter-preferred order.
    async fn select_public_payment_endpoints(
        &self,
        _request: &PublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<PublicPaymentEndpointCandidate>> {
        Err(unsupported_operation("public Payment Endpoint selection"))
    }

    /// Build a payment target from a payable public endpoint.
    async fn build_public_payment_target(
        &self,
        _endpoint: &PublicPaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Err(unsupported_operation("public payment target construction"))
    }

    /// Return payable private candidates in adapter-preferred order.
    async fn select_private_payment_endpoints(
        &self,
        _request: &PrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<PrivatePaymentEndpointCandidate>> {
        Err(unsupported_operation("private Payment Endpoint selection"))
    }

    /// Build a payment target from a payable private endpoint.
    async fn build_private_payment_target(
        &self,
        _endpoint: &PrivatePaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Err(unsupported_operation("private payment target construction"))
    }
}

/// Payment-method-specific receiving detail for public publication.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for PublicReceivingDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicReceivingDetail")
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Payment-method-specific receiving detail for a Private Payment List.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateReceivingDetail {
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for PrivateReceivingDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateReceivingDetail")
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Private receiving detail reserved by the payment adapter.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentEndpointReservation {
    /// Adapter-stable reservation id; non-empty, at most 128 bytes, no control characters.
    pub reservation_id: String,
    /// Reserved receiving detail.
    pub receiving_detail: PrivateReceivingDetail,
    /// Optional reservation expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Adapter-provided attribution metadata.
    pub attribution: HashMap<String, String>,
}

/// Request passed to cancel a receiving-detail reservation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentEndpointReservationCancellation {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Counterparty the reservation was intended for.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Hash of the reserved endpoint payload.
    pub payload_hash: String,
    /// Adapter-provided attribution metadata from the reservation.
    pub attribution: HashMap<String, String>,
}

impl fmt::Debug for PrivatePaymentEndpointReservationCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentEndpointReservationCancellation")
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

impl fmt::Debug for PrivatePaymentEndpointReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentEndpointReservation")
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

/// Public Payment Endpoint candidate to check or pay.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicPaymentEndpointCandidate {
    /// Counterparty that published the endpoint.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for PublicPaymentEndpointCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicPaymentEndpointCandidate")
            .field("counterparty", &self.counterparty)
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Private Payment Endpoint candidate to check or pay.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentEndpointCandidate {
    /// Counterparty that privately shared the endpoint.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Payment Endpoint Identifier string.
    pub identifier: String,
    /// Serialized endpoint payload.
    pub payload: String,
}

impl fmt::Debug for PrivatePaymentEndpointCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivatePaymentEndpointCandidate")
            .field("counterparty", &self.counterparty)
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
            .field("identifier", &self.identifier)
            .field("payload", &redacted_payload(&self.payload))
            .finish()
    }
}

/// Optional amount context for endpoint selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentAmountContext {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Request passed to the payment adapter for public endpoint ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicPaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Optional amount context.
    pub amount: Option<PaymentAmountContext>,
    /// Public candidate endpoints in SDK preference order.
    pub candidates: Vec<PublicPaymentEndpointCandidate>,
}

/// Request passed to the payment adapter for private endpoint ordering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Optional amount context.
    pub amount: Option<PaymentAmountContext>,
    /// Private candidate endpoints in SDK preference order.
    pub candidates: Vec<PrivatePaymentEndpointCandidate>,
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

fn unsupported_operation(operation: &str) -> PaykitSdkError {
    PaykitSdkError::PaymentAdapter {
        context: format!("adapter does not support {operation}"),
        source: None,
    }
}

#[cfg(test)]
mod tests;
