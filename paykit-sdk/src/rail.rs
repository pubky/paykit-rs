//! The payment method's interface ([`Rail`]): Paykit's narrow view of a payment rail
//! (Paykit spec 1, 7.2).
//!
//! Payment rails are outside Paykit. Paykit never interprets a Payment Endpoint Payload: the
//! rail reads it. What Paykit needs of a rail is the least the paying order and the payee's
//! check need:
//!
//! - **what it declares** ([`Rail::declaration`]): whether it is idempotent on the attempt ID
//!   and for how long, whether its history shows a dispatch's outcome and for how long, and
//!   whether that history survives a restore of the paying device (Paykit spec 7.2, "What a
//!   payment method declares");
//! - **what it would transfer** to a destination for an amount, before any write
//!   ([`Rail::transfer`], Paykit spec 7.2 step 1);
//! - **pay** an endpoint with the attempt ID as the idempotency key ([`Rail::pay`]);
//! - **read** the outcome of a dispatch by its idempotency key, with the proof and the
//!   verified payment time ([`Rail::payment`]), and, for a payee, check a proof it received
//!   by its own method ([`Rail::verify`], Paykit spec 8.1 step 4).
//!
//! An outcome is one of three, as in the v0.1 SDK (Paykit spec 7.2, "Failed means
//! definitively not paid"): verified success, definitive failure, or nothing yet. A call that
//! errs after a payment may have been submitted is nothing yet, never a failure.
//!
//! [`FakeRail`] (feature `test-support`, tests only) is an in-memory rail for tests.

use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::transport::BoxFuture;
use serde_json::{Map, Value};

#[cfg(feature = "test-support")]
mod fake;

#[cfg(feature = "test-support")]
pub use fake::{Behaviour, FakeRail};

/// A failure of the payment method's call. After a payment may have been submitted, a failed
/// call means nothing yet: the attempt stays open (Paykit spec 7.2).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RailError {
    /// The method can't be reached now, or its call timed out.
    #[error("the payment method is unavailable: {0}")]
    Unavailable(String),
    /// The method refused the call before anything was submitted: the text names why,
    /// never a payload.
    #[error("the payment method refused the call: {0}")]
    Refused(String),
}

/// What a payment method declares (Paykit spec 7.2, "What a payment method declares").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RailDeclaration {
    /// A repeat under the same idempotency key returns the payment made, while the method
    /// remembers the key.
    pub idempotent: bool,
    /// How long the method remembers an idempotency key; `None` for good.
    pub idempotency_retention_ms: Option<u64>,
    /// The method's history can show the outcome of a dispatch.
    pub history: bool,
    /// How long the history keeps a dispatch; `None` for good.
    pub history_retention_ms: Option<u64>,
    /// The history lives with the account, not only on the paying device, so a restored
    /// device sees every dispatch made after its backup.
    pub history_survives_restore: bool,
}

/// What the method would transfer to a destination, reported before any write (Paykit spec
/// 7.2 step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transfer {
    /// The amount and asset it would transfer; `None` when it can't tell before paying.
    pub amount: Option<PaymentAmount>,
    /// The largest fee it may charge on top, in the same asset, that the payee can influence
    /// through the destination it chose; `None` for none.
    pub max_fee: Option<paykit_lib::Decimal>,
}

/// One payment to make: the endpoint, the amount, and the attempt ID as the idempotency key
/// (Paykit spec 7.2).
#[derive(Clone, PartialEq, Eq)]
pub struct RailPayment {
    /// The Payment Endpoint Identifier paid.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload, which only the rail interprets.
    pub payload: PaymentEndpointPayload,
    /// The amount to pay.
    pub amount: PaymentAmount,
    /// The idempotency key: the attempt ID.
    pub idempotency_key: AttemptId,
    /// The request's payment reference, copied into the payment where the method allows it.
    pub payment_reference: Option<String>,
}

impl std::fmt::Debug for RailPayment {
    // The payload and reference stay out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RailPayment({})", self.endpoint_id)
    }
}

/// The outcome of a dispatch, as the method reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RailOutcome {
    /// Verified success.
    Paid {
        /// The opaque, method-specific proof (Paykit wire 4.10 `proof`).
        proof: Map<String, Value>,
        /// The independently verified payment time, in Unix milliseconds, when the method
        /// has one (Paykit spec 8.2).
        paid_at_ms: Option<u64>,
    },
    /// Definitive failure: no money moved, and none will.
    Failed,
    /// Nothing yet: in flight, or the method can't tell.
    Pending,
}

/// A proof a payee checks by its own method (Paykit spec 8.1 step 4).
#[derive(Clone, PartialEq, Eq)]
pub struct ProofCheck {
    /// The Payment Endpoint Identifier the proof names.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload of that endpoint, when the payee knows it.
    pub payload: Option<PaymentEndpointPayload>,
    /// The amount the payee expects: the request's, or the proof's own for a payment
    /// without a request.
    pub amount: PaymentAmount,
    /// The attempt the proof reports.
    pub attempt_id: AttemptId,
    /// The opaque proof.
    pub proof: Map<String, Value>,
}

impl std::fmt::Debug for ProofCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProofCheck({})", self.endpoint_id)
    }
}

/// A payment method, as Paykit uses it. Object-safe; the SDK holds an `Arc<dyn Rail>`.
///
/// Implementations live with the wallet: Paykit never interprets payloads or proofs.
pub trait Rail: Send + Sync {
    /// What the method declares (Paykit spec 7.2).
    fn declaration(&self) -> RailDeclaration;

    /// What paying `amount` to `payload` would transfer (Paykit spec 7.2 step 1): a payload
    /// that fixes an amount transfers that one, an amount-less one exactly `amount`.
    fn transfer(
        &self,
        endpoint_id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer;

    /// Pay, with the attempt ID as the idempotency key (Paykit spec 7.2 step 2).
    ///
    /// # Errors
    ///
    /// A [`RailError`]; once anything may have been submitted, the SDK reads it as nothing
    /// yet.
    fn pay(&self, payment: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>>;

    /// The method's history of the dispatch under `idempotency_key`, with its proof and
    /// verified payment time; `None` when the history shows none (Paykit spec 7.2, 7.3).
    ///
    /// # Errors
    ///
    /// A [`RailError`] when the history can't be read now.
    fn payment(
        &self,
        idempotency_key: AttemptId,
    ) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>>;

    /// The payee's own check of a proof (Paykit spec 8.1 step 4): the verified payment time
    /// when the method confirms the payment reached the endpoint for the amount; `None` when
    /// it doesn't.
    ///
    /// # Errors
    ///
    /// A [`RailError`] when the method can't check now.
    fn verify(&self, check: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rail_payment_debug_hides_the_payload() {
        let payment = RailPayment {
            endpoint_id: PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            payload: PaymentEndpointPayload::new("lnbc-secret-invoice").unwrap(),
            amount: PaymentAmount {
                value: paykit_lib::Decimal::new("1").unwrap(),
                asset: paykit_lib::Asset::new("btc").unwrap(),
            },
            idempotency_key: AttemptId::from_bytes([1; 16]),
            payment_reference: Some("order-secret".into()),
        };
        let text = format!("{payment:?}");
        assert!(!text.contains("secret"), "{text}");
        assert!(text.contains("btc-lightning-bolt11"));
    }
}
