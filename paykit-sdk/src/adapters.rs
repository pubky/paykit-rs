use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt};

use crate::{identity::PubkyPublicKey, PubkySessionAccess, Result};

/// Provides live Pubky session access to the SDK.
///
/// The provider owns platform-specific auth handoff, secure persistence, and
/// key rotation. The SDK consumes the returned Pubky access for Paykit
/// workflows.
#[async_trait]
pub trait PubkySessionProvider: Send + Sync {
    /// Load live Pubky access for storage and Encrypted Link workflows.
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>>;

    /// Load public Pubky storage for unauthenticated counterparty reads.
    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        let Some(session_access) = self.load_session_access().await? else {
            return Ok(None);
        };
        Ok(Some(session_access.outbox_client.public_storage()))
    }

    /// Clear local Pubky session access during sign-out.
    async fn clear_session_access(&self) -> Result<()>;
}

/// Adapter for payment-method-specific endpoint publication and selection.
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
    async fn reserve_receiving_details(
        &self,
        _request: &PaymentEndpointReservationRequest,
    ) -> Result<Option<Vec<PaymentEndpointReservation>>> {
        Ok(None)
    }

    /// Release a previously reserved receiving detail, when supported.
    async fn release_receiving_detail_reservation(
        &self,
        _release: &PaymentEndpointReservationRelease,
    ) -> Result<()> {
        Ok(())
    }

    /// Rank and evaluate candidate endpoints for a payment.
    async fn select_payment_endpoint(
        &self,
        request: &PaymentEndpointSelectionRequest,
    ) -> Result<PaymentEndpointSelection>;

    /// Build a payment target from a compatible endpoint.
    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
    ) -> Result<PaymentTarget>;
}

/// Scope used when asking for receiving details.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReceivingDetailScope {
    /// Details intended for public Payment Endpoints.
    Public,
    /// Details intended for one counterparty's Private Payment List.
    Private {
        /// Counterparty that will receive the private details.
        counterparty: PubkyPublicKey,
    },
}

/// Request passed to the payment adapter for receiving-detail reservation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointReservationRequest {
    /// Counterparty whose Private Payment List will receive the reserved details.
    pub counterparty: PubkyPublicKey,
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
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Reserved receiving detail.
    pub receiving_detail: ReceivingDetail,
    /// Optional reservation expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Adapter-provided attribution metadata.
    pub attribution: HashMap<String, String>,
}

/// Request passed to release a receiving-detail reservation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointReservationRelease {
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

impl fmt::Debug for PaymentEndpointReservationRelease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointReservationRelease")
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

/// Request passed to the payment adapter for endpoint selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointSelectionRequest {
    /// Counterparty being paid.
    pub counterparty: PubkyPublicKey,
    /// Optional amount context.
    pub amount: Option<PaymentAmountContext>,
    /// Candidate endpoints in SDK preference order.
    pub candidates: Vec<PaymentEndpointCandidate>,
}

/// Adapter evaluation for one candidate endpoint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointEvaluation {
    /// Candidate being evaluated.
    pub candidate: PaymentEndpointCandidate,
    /// Compatibility status.
    pub compatibility: EndpointCompatibility,
    /// Adapter priority, where lower values are preferred.
    pub priority: Option<u32>,
}

impl fmt::Debug for PaymentEndpointEvaluation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointEvaluation")
            .field("candidate", &self.candidate)
            .field("compatibility", &self.compatibility)
            .field("priority", &self.priority)
            .finish()
    }
}

/// Adapter endpoint selection result.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointSelection {
    /// Selected payable candidate, when one is available.
    pub selected: Option<PaymentEndpointCandidate>,
    /// Evaluations for candidates considered by the adapter.
    pub evaluations: Vec<PaymentEndpointEvaluation>,
}

impl fmt::Debug for PaymentEndpointSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentEndpointSelection")
            .field("selected", &self.selected)
            .field("evaluations", &self.evaluations)
            .finish()
    }
}

/// Compatibility result for one endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointCompatibility {
    /// The endpoint can be paid by the adapter.
    Payable,
    /// The endpoint kind is unsupported.
    Unsupported {
        /// Optional adapter-specific reason.
        reason: Option<String>,
    },
    /// The endpoint is recognized but stale/unusable.
    Stale {
        /// Optional adapter-specific reason.
        reason: Option<String>,
    },
    /// The endpoint cannot be used for the requested amount.
    AmountIncompatible {
        /// Optional adapter-specific reason.
        reason: Option<String>,
    },
}

/// Payment target produced by an adapter.
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
mod tests {
    use super::*;

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    #[test]
    fn test_endpoint_debug_redacts_payloads() {
        let candidate = PaymentEndpointCandidate {
            counterparty: counterparty(),
            source: PaymentEndpointSource::PrivatePaymentList,
            identifier: "btc-lightning-bolt11".into(),
            payload: "ln-private-secret".into(),
        };
        let selection = PaymentEndpointSelection {
            selected: Some(candidate.clone()),
            evaluations: vec![PaymentEndpointEvaluation {
                candidate,
                compatibility: EndpointCompatibility::Payable,
                priority: Some(0),
            }],
        };
        let target = PaymentTarget {
            payload: "method-specific-target".into(),
        };

        let debug = format!("{selection:?} {target:?}");

        assert!(!debug.contains("ln-private-secret"));
        assert!(!debug.contains("method-specific-target"));
        assert!(debug.contains("<redacted:17 bytes>"));
        assert!(debug.contains("<redacted:22 bytes>"));
    }
}
