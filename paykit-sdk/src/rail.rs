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
//! - **which endpoints it pays** ([`Rail::select_endpoints`]): of the Payment Endpoints a
//!   payment may take, handed in all at once as candidates with the payment's amount, the
//!   ones it pays, in the order to try them, as the v0.1 SDK's payment adapter ordered the
//!   payable candidates; the SDK pays the first of them whose transfer check passes;
//! - **which identifiers it can pay** ([`Rail::endpoint_identifiers`]), for an app's record
//!   to list as its `capabilities` (Paykit spec 4);
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

/// A candidate's ID in one call of [`Rail::select_endpoints`]: opaque, it only names one of
/// the candidates handed in, and a rail returns it as it was given. The SDK numbers the
/// candidates of a call in the order it hands them, so the same candidates get the same
/// IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CandidateId(u32);

impl CandidateId {
    /// The ID with the value `value`, for a binding that carries IDs as numbers.
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// The ID's value.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// A Payment Endpoint a payment may take, handed to [`Rail::select_endpoints`]: its
/// candidate ID, its Payment Endpoint Identifier and its payload.
///
/// The payload is sensitive: it is the payee's destination, such as an invoice or an
/// address, that only the rail interprets. It stays out of `Debug`, and a rail never logs it.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaymentEndpointCandidate {
    /// The candidate's ID in this call.
    pub id: CandidateId,
    /// The Payment Endpoint Identifier.
    pub endpoint_id: PaymentEndpointIdentifier,
    /// The payload, sensitive, which only the rail interprets.
    pub payload: PaymentEndpointPayload,
}

impl PaymentEndpointCandidate {
    /// The candidate `id`, of the endpoint `endpoint_id` with `payload`.
    pub fn new(
        id: CandidateId,
        endpoint_id: PaymentEndpointIdentifier,
        payload: PaymentEndpointPayload,
    ) -> Self {
        Self {
            id,
            endpoint_id,
            payload,
        }
    }
}

impl std::fmt::Debug for PaymentEndpointCandidate {
    // The payload stays out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PaymentEndpointCandidate({}, {})",
            self.id.get(),
            self.endpoint_id
        )
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

    /// Of `candidates`, the ones the method pays, in the order to try them, each once: their
    /// IDs, best first (Paykit spec 7.2). `amount` is the payment's amount, when there is
    /// one: a request's, or a payment without a request's.
    ///
    /// The candidates are every Payment Endpoint the payment may take, across every payee
    /// app it may use, handed in at once, in this order:
    ///
    /// - a request's own endpoints: by Payment Endpoint Identifier;
    /// - the payee's public endpoints: by App ID, then by identifier;
    /// - the payee's Private Payment Lists in use: newest version first, then by App ID,
    ///   then by identifier, a single-use endpoint already used at its list's version left
    ///   out.
    ///
    /// Only endpoints the payment may use are handed in: those of the request's accepted
    /// identifiers, and the one the caller named, if any. A payload is sensitive: see
    /// [`PaymentEndpointCandidate`].
    ///
    /// The SDK takes the first of the returned candidates whose transfer check passes
    /// ([`Rail::transfer`]; Paykit spec 7.2 step 1), so a malformed or amount-incompatible
    /// payload falls to the next. The order returned is the whole choice: of two apps
    /// offering the same identifier, the one returned first is paid. None returned refuses
    /// the payment as [`crate::Refusal::UnsupportedEndpoint`], but for Private Payment Lists
    /// a newer one may serve, when one has expired or a single-use endpoint is used up: then
    /// the payee is asked for it ([`crate::Refusal::WaitingForList`]). Every one failing the
    /// transfer check refuses it as [`crate::Refusal::Transfer`], the first one's refusal. An
    /// ID that isn't one of the candidates', or one returned twice, refuses the call as
    /// [`RailError::Refused`], nothing written.
    ///
    /// The choice is journaled with the attempt before the method pays: a payment resumed
    /// later pays the endpoint chosen, whatever the method would choose then.
    ///
    /// For Private Payment Lists, a call that returns none may be followed by one handing in
    /// only the single-use endpoints left out as used, to tell whether a newer list may
    /// serve: none of those is paid.
    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        amount: Option<&PaymentAmount>,
    ) -> Vec<CandidateId>;

    /// The Payment Endpoint Identifiers the method can pay to: what an app's record lists as
    /// its `capabilities.endpoint_identifiers`, so payees can offer matching destinations
    /// (Paykit spec 4, Paykit wire 3.2), once the SDK publishes its own record. The SDK
    /// never chooses a destination by them: [`Rail::select_endpoints`] does.
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

    #[test]
    fn test_rail_candidate_debug_hides_the_payload() {
        let candidate = PaymentEndpointCandidate::new(
            CandidateId::new(3),
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaymentEndpointPayload::new("lnbc-secret-invoice").unwrap(),
        );
        let text = format!("{candidate:?}");
        assert!(!text.contains("secret"), "{text}");
        assert!(
            text.contains("btc-lightning-bolt11") && text.contains('3'),
            "{text}"
        );
    }
}
