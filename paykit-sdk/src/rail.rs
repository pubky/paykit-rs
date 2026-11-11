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
//! - **which endpoints it pays** ([`Rail::endpoint_identifiers`]): the Payment Endpoint
//!   Identifiers it can pay, in the order it wants them tried; the SDK pays the first of them
//!   a destination offers, as the v0.1 SDK's adapter chose and ordered the payable candidates;
//! - **what it would transfer** to a destination for an amount, before any write
//!   ([`Rail::transfer`], Paykit spec 7.2 step 1);
//! - **pay** an endpoint with the attempt ID as the idempotency key ([`Rail::pay`]);
//! - **read** the outcome of a dispatch by its idempotency key, with the proof and the
//!   verified payment time ([`Rail::payment`]), and, for a payee, check a proof it received
//!   by its own method ([`Rail::verify`], Paykit spec 8.1 step 4).
//!
//! An outcome is one of three, as in the v0.1 SDK (Paykit spec 7.2, "Failed means
//! definitively not paid"): verified success, definitive failure, or nothing yet. A call that
//! errs after a payment may have been submitted is nothing yet, never a failure; only
//! [`RailError::Refused`] is authoritative, and a first dispatch it answers is written
//! failed.
//!
//! `FakeRail` (feature `test-support`, tests only) is an in-memory rail for tests.

use paykit_lib::{AttemptId, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload};
use pubky_switch_client::transport::BoxFuture;
use serde_json::{Map, Value};

#[cfg(feature = "test-support")]
mod fake;

#[cfg(feature = "test-support")]
pub use fake::{Behaviour, FakeRail, FAKE_ENDPOINT_IDENTIFIERS, FAKE_LONGEST_COMPLETION_MS};

/// A failure of the payment method's call (Paykit spec 7.2). [`RailError::Unavailable`] is
/// ambiguous: a payment may have been submitted, so it means nothing yet and the attempt
/// stays open. [`RailError::Refused`] is authoritative: nothing was and nothing will be
/// submitted, so a first dispatch it answers is written `failed`, its journal entry is
/// finished, and a replacement payment may follow.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RailError {
    /// The method can't be reached now, or its call timed out: a payment may have been
    /// submitted, and the SDK reads it as nothing yet.
    #[error("the payment method is unavailable: {0}")]
    Unavailable(String),
    /// Authoritative: nothing was and nothing will be submitted for this call. A rail returns
    /// it only when it knows that, such as for a payload it can't pay or funds it doesn't
    /// hold, never for a call whose fate it can't tell: "failed" means definitively not paid
    /// (Paykit spec 7.2). The text names why, never a payload.
    #[error("the payment method refused the call: {0}")]
    Refused(String),
}

/// What a payment method declares (Paykit spec 7.2, "What a payment method declares").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The longest a dispatch takes to complete or fail, in milliseconds: a day for an
    /// ordinary Lightning payment (Switch spec 10.2). An attempt's `orphan_after` is at
    /// least this plus [`crate::PRE_DISPATCH_BUDGET_MS`], so no other own app can resolve it
    /// while its payment may still run.
    pub longest_completion_ms: u64,
}

impl RailDeclaration {
    /// A method that takes at most `longest_completion_ms` to complete or fail, and declares
    /// nothing else: not idempotent, and no history. Each `with_` setter declares one more
    /// thing it does.
    pub fn new(longest_completion_ms: u64) -> Self {
        Self {
            idempotent: false,
            idempotency_retention_ms: None,
            history: false,
            history_retention_ms: None,
            history_survives_restore: false,
            longest_completion_ms,
        }
    }

    /// A repeat under the same idempotency key returns the payment made, while the method
    /// remembers the key.
    #[must_use]
    pub fn with_idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// How long the method remembers an idempotency key; for good when not set.
    #[must_use]
    pub fn with_idempotency_retention_ms(mut self, retention_ms: u64) -> Self {
        self.idempotency_retention_ms = Some(retention_ms);
        self
    }

    /// The method's history can show the outcome of a dispatch.
    #[must_use]
    pub fn with_history(mut self, history: bool) -> Self {
        self.history = history;
        self
    }

    /// How long the history keeps a dispatch; for good when not set.
    #[must_use]
    pub fn with_history_retention_ms(mut self, retention_ms: u64) -> Self {
        self.history_retention_ms = Some(retention_ms);
        self
    }

    /// The history lives with the account, not only on the paying device, so a restored
    /// device sees every dispatch made after its backup.
    #[must_use]
    pub fn with_history_survives_restore(mut self, survives: bool) -> Self {
        self.history_survives_restore = survives;
        self
    }
}

/// What the method would transfer to a destination, reported before any write (Paykit spec
/// 7.2 step 1).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Transfer {
    /// The amount and asset it would transfer; `None` when it can't tell before paying.
    pub amount: Option<PaymentAmount>,
    /// The largest fee it may charge on top, in the same asset, that the payee can influence
    /// through the destination it chose; `None` for none.
    pub max_fee: Option<paykit_lib::Decimal>,
}

impl Transfer {
    /// What the method would transfer: `amount`, `None` when it can't tell before paying,
    /// and `max_fee`, the largest fee on top that the payee can influence through the
    /// destination it chose, `None` for none.
    pub fn new(amount: Option<PaymentAmount>, max_fee: Option<paykit_lib::Decimal>) -> Self {
        Self { amount, max_fee }
    }
}

/// One payment to make: the endpoint, the amount, and the attempt ID as the idempotency key
/// (Paykit spec 7.2).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum RailOutcome {
    /// Verified success.
    Paid(RailPaid),
    /// Definitive failure: no money moved, and none will.
    Failed,
    /// Nothing yet: in flight, or the method can't tell.
    Pending,
}

/// A verified success, as the method reports it ([`RailOutcome::Paid`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RailPaid {
    /// The opaque, method-specific proof (Paykit wire 4.10 `proof`).
    pub proof: Map<String, Value>,
    /// The independently verified payment time, in Unix milliseconds, when the method has
    /// one (Paykit spec 8.2).
    pub paid_at_ms: Option<u64>,
}

impl RailPaid {
    /// A success with `proof`, and no verified payment time.
    pub fn new(proof: Map<String, Value>) -> Self {
        Self {
            proof,
            paid_at_ms: None,
        }
    }

    /// The independently verified payment time, in Unix milliseconds (Paykit spec 8.2).
    #[must_use]
    pub fn with_paid_at_ms(mut self, paid_at_ms: u64) -> Self {
        self.paid_at_ms = Some(paid_at_ms);
        self
    }
}

/// A proof a payee checks by its own method (Paykit spec 8.1 step 4).
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
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

    /// The Payment Endpoint Identifiers the method can pay, in the order it wants them
    /// tried, each once. Of the endpoints a destination offers, the SDK pays the first in
    /// this order; a destination offering endpoints but none of these is refused as
    /// [`crate::Refusal::UnsupportedEndpoint`]. They are what an app's record will list as
    /// its `capabilities` (Paykit wire 3.2) once the SDK publishes its own record.
    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier>;

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
    /// A [`RailError`]: [`RailError::Refused`] only when nothing was and nothing will be
    /// submitted, which the SDK writes `failed`; any other error the SDK reads as nothing
    /// yet, since a payment may have been submitted.
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

    /// The settlement ID of a proof's `proof` object made by this method (Paykit wire 1.5):
    /// the bytes the method defines to name one settlement, the same for every proof of it,
    /// read from the proof alone and deterministic, so a payee credits a payment without a
    /// request once whoever sends its proof (Paykit spec 8.4). `None` for a proof that names
    /// none, which isn't credited. The default reads the methods `paykit-lib` knows
    /// ([`paykit_lib::derive::settlement_id`]); a rail of another method overrides it.
    fn settlement_id(&self, proof: &Map<String, Value>) -> Option<Vec<u8>> {
        paykit_lib::derive::settlement_id(proof)
    }
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
