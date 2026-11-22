//! The payment method belongs to the host. The SDK journals and reuses its attempt ID.
use paykit_lib::{
    Asset, AttemptId, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use paykit_sdk::rail::{AllowanceDispatch, CandidateId, ProofCheck};
use paykit_sdk::{
    EndpointSelection, PaymentEndpointCandidate, Rail, RailDeclaration, RailError, RailOutcome,
    RailPaid, RailPayment, Transfer,
};
use pubky_switch_client::transport::BoxFuture;
use serde_json::{Map, Value};
use std::sync::Arc;

/// An exact payment amount and its asset (Paykit wire 1.2).
#[derive(Clone, uniffi::Record)]
pub struct Amount {
    /// The asset identifier, in the protocol's canonical spelling.
    pub asset: String,
    /// The exact decimal amount, encoded as a string.
    pub value: String,
}

impl From<&PaymentAmount> for Amount {
    fn from(v: &PaymentAmount) -> Self {
        Self {
            asset: v.asset.as_str().into(),
            value: v.value.as_str().into(),
        }
    }
}

impl TryFrom<Amount> for PaymentAmount {
    type Error = paykit_lib::Error;
    fn try_from(v: Amount) -> Result<Self, Self::Error> {
        Ok(Self {
            asset: Asset::new(&v.asset)?,
            value: Decimal::new(&v.value)?,
        })
    }
}

/// What a payment method declares (Paykit spec 7.2, "What a payment method declares").
#[derive(uniffi::Record)]
pub struct MethodDeclaration {
    /// A repeat under the same idempotency key returns the payment made, while the method
    /// remembers the key.
    pub idempotent: bool,
    /// How long the method remembers an idempotency key; `None` for good.
    pub idempotency_retention_ms: Option<u64>,
    /// The method's history can show the outcome of a dispatch.
    pub history: bool,
    /// How long the history keeps a dispatch; `None` for good.
    pub history_retention_ms: Option<u64>,
    /// The history lives with the account, not only on the paying device, so a restored device
    /// sees every dispatch made after its backup.
    pub history_survives_restore: bool,
    /// The longest a dispatch takes to complete or fail, in milliseconds: a day for an ordinary
    /// Lightning payment (Switch spec 10.2).
    pub longest_completion_ms: u64,
    /// How far, in milliseconds, the method's verified payment time may lie before the
    /// request's signed `created_at` for a payment made after it (Paykit spec 7.1, "Not
    /// before the request"). The payee credits no evidence dated earlier than `created_at`
    /// less this. Cover the method's time precision (at least 1,000 ms for whole seconds),
    /// the skew of the clock that dates the evidence, and the method's timestamp rules: two
    /// hours is a conservative early tolerance for a Bitcoin block timestamp. A block
    /// timestamp ahead of real time can also put a payment made in time past a deadline, and
    /// no tolerance covers that.
    pub payment_time_tolerance_ms: u64,
}

/// A Payment Endpoint a payment may take, handed to `Rail::select_endpoints`: its candidate
/// ID, its Payment Endpoint Identifier and its payload.
#[derive(uniffi::Record)]
pub struct EndpointCandidate {
    /// The candidate's ID in this call.
    pub id: u32,
    /// The Payment Endpoint Identifier offered by this candidate.
    pub identifier: String,
    /// The payload, sensitive, which only the rail interprets.
    pub payload: String,
}

/// A failure of the payment method's call (Paykit spec 7.2). `RailError::Unavailable` is
/// ambiguous: a payment may have been submitted, so it means nothing yet and the attempt
/// stays open. `RailError::Refused` is authoritative: nothing was and nothing will be
/// submitted, so a first dispatch it answers is written `failed`, its journal entry is
/// finished, and a replacement payment may follow.
#[derive(Debug, Clone, uniffi::Error, thiserror::Error)]
pub enum MethodError {
    /// The method can't be reached now, or its call timed out: a payment may have been submitted,
    /// and the SDK reads it as nothing yet.
    #[error("payment method unavailable: {reason}")]
    Unavailable {
        /// The content-free explanation supplied by the payment method.
        reason: String,
    },
    /// Authoritative: nothing was and nothing will be submitted for this call.
    #[error("payment method definitively refused submission: {reason}")]
    Refused {
        /// The content-free explanation supplied by the payment method.
        reason: String,
    },
    /// The method's answer to `Rail::select_endpoints` breaks that call's rules: an ID it wasn't
    /// handed, or one ID returned twice, or both as payable and in error.
    #[error("invalid endpoint selection: {reason}")]
    InvalidSelection {
        /// The content-free explanation supplied by the payment method.
        reason: String,
    },
}

impl From<MethodError> for RailError {
    fn from(v: MethodError) -> Self {
        match v {
            MethodError::Unavailable { reason } => Self::Unavailable(reason),
            MethodError::Refused { reason } => Self::Refused(reason),
            MethodError::InvalidSelection { reason } => Self::InvalidSelection(reason),
        }
    }
}

/// The reason a particular offered endpoint could not be evaluated.
#[derive(uniffi::Record)]
pub struct EndpointFailure {
    /// The candidate ID supplied to the callback for this selection.
    pub id: u32,
    /// The failure reported for this candidate.
    pub error: MethodError,
}
/// The method's answer to `Rail::select_endpoints`: the candidates it pays, in the order to
/// try them, and the ones it couldn't judge now, each by the `CandidateId` it was handed.
#[derive(uniffi::Record)]
pub struct SelectedEndpoints {
    /// The candidates the method pays, best first, each once.
    pub payable: Vec<u32>,
    /// Failures for individual candidates, including when another candidate is payable.
    pub failures: Vec<EndpointFailure>,
}

/// Validated before the method returns a quote to the SDK.
#[derive(uniffi::Object)]
pub struct MethodTransfer(Transfer);

#[uniffi::export]
impl MethodTransfer {
    /// Validate the exact amount and maximum fee the payment method would submit. The fee
    /// is the largest the payee can influence through the destination it chose. The SDK
    /// checks it only against a fee cap, and these bindings set none: the payment method's
    /// own limit, if it has one, is the only bound on the fee (Paykit spec 7.2 step 1).
    #[uniffi::constructor]
    pub fn new(amount: Option<Amount>, max_fee: Option<String>) -> Result<Arc<Self>, MethodError> {
        let amount = amount
            .map(PaymentAmount::try_from)
            .transpose()
            .map_err(invalid_answer)?;
        let fee = max_fee
            .as_deref()
            .map(Decimal::new)
            .transpose()
            .map_err(invalid_answer)?;
        Ok(Arc::new(Self(Transfer::new(amount, fee))))
    }
}

fn invalid_answer(_: paykit_lib::Error) -> MethodError {
    MethodError::Unavailable {
        reason: "invalid amount or identifier from payment method".into(),
    }
}

/// One payment to make: the endpoint, the amount, and the attempt ID as the idempotency key
/// (Paykit spec 7.2).
#[derive(uniffi::Record)]
pub struct MethodPayment {
    /// The selected Payment Endpoint Identifier.
    pub endpoint: String,
    /// The payload, which only the rail interprets.
    pub payload: String,
    /// The amount to pay.
    pub amount: Amount,
    /// The SDK's original idempotency key. Preserve it through submission and lookup.
    pub attempt_id: String,
    /// The request's payment reference, copied into the payment where the method allows it.
    pub payment_reference: Option<String>,
}
/// The outcome of a dispatch, as the method reports it.
#[derive(uniffi::Enum)]
pub enum MethodOutcome {
    /// Verified success.
    Paid {
        /// The method-defined payment proof, encoded as a JSON object.
        proof_json: Vec<u8>,
        /// The time in Unix milliseconds the payment method verified, when available.
        paid_at_ms: Option<u64>,
    },
    /// Definitive failure: no money moved, and none will.
    Failed,
    /// Nothing yet: in flight, or the method can't tell.
    Pending,
}
impl MethodOutcome {
    fn into_rail(self) -> Result<RailOutcome, RailError> {
        Ok(match self {
            Self::Pending => RailOutcome::Pending,
            Self::Failed => RailOutcome::Failed,
            Self::Paid {
                proof_json,
                paid_at_ms,
            } => {
                let proof = serde_json::from_slice(&proof_json)
                    .map_err(|_| RailError::Unavailable("invalid payment proof".into()))?;
                let mut paid = RailPaid::new(proof);
                if let Some(at) = paid_at_ms {
                    paid = paid.with_paid_at_ms(at);
                }
                RailOutcome::Paid(paid)
            }
        })
    }
}

/// A payment submitted or possibly submitted through the payer's wallet.
#[derive(uniffi::Record)]
pub struct MethodDispatch {
    /// The original idempotency key used to pay.
    pub attempt_id: String,
    /// The amount transferred, or durably bound to the dispatch, excluding fees.
    pub amount: Amount,
    /// The method's outcome, including a payment whose result is still unknown.
    pub outcome: MethodOutcome,
}
/// A proof a payee checks by its own method (Paykit spec 8.1 step 4).
#[derive(uniffi::Record)]
pub struct MethodProof {
    /// The selected Payment Endpoint Identifier.
    pub endpoint: String,
    /// The payload of that endpoint, when the payee knows it.
    pub payload: Option<String>,
    /// The amount the payee expects: the request's, or the proof's own for a payment without a
    /// request.
    pub amount: Amount,
    /// The attempt the proof reports.
    pub attempt_id: String,
    /// The method-defined payment proof, encoded as a JSON object.
    pub proof_json: Vec<u8>,
}

/// Methods must complete without acquiring the wallet they serve. A timeout or unknown
/// submission returns Unavailable. Refused means no submission took place or can follow.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait PaymentMethod: Send + Sync {
    /// Report the method's idempotency, history and maximum completion guarantees.
    fn declaration(&self) -> MethodDeclaration;
    /// Return payable candidate IDs and any per-candidate failures.
    fn select_endpoints(
        &self,
        candidates: Vec<EndpointCandidate>,
        amount: Option<Amount>,
    ) -> Result<SelectedEndpoints, MethodError>;
    /// Describe the proposed transfer and fee bound before authorizing payment.
    fn transfer(&self, endpoint: String, payload: String, amount: Amount) -> Arc<MethodTransfer>;
    /// Return the supported fractional digits for this endpoint's transfer amount.
    fn fraction_digits(&self, endpoint: String) -> Option<u32>;
    /// Submit the selected endpoint payment under its original idempotency key.
    async fn pay(&self, payment: MethodPayment) -> Result<MethodOutcome, MethodError>;
    /// Look up the original attempt by its idempotency key without starting a transfer.
    async fn payment(&self, attempt_id: String) -> Result<Option<MethodOutcome>, MethodError>;
    /// Complete account-wide dispatch inventory, including manual, failed and pending attempts.
    /// Return null for incomplete coverage and an empty list only for proven empty history.
    async fn allowance_history(
        &self,
        payer: Vec<u8>,
    ) -> Result<Option<Vec<MethodDispatch>>, MethodError>;
    /// Verify the proof against the original payment and return its verified payment time,
    /// in Unix milliseconds, or null when it can't be verified. That time decides credit:
    /// the payee credits the request only when it lies within the request's times, so
    /// return the time the payment's evidence dates it, never a placeholder.
    async fn verify(&self, proof: MethodProof) -> Result<Option<u64>, MethodError>;
    /// Extract the method-defined settlement ID for deduplication, when available.
    fn settlement_id(&self, proof_json: Vec<u8>) -> Option<Vec<u8>>;
}

pub(crate) struct HostRail {
    pub method: Arc<dyn PaymentMethod>,
    pub identifiers: Vec<PaymentEndpointIdentifier>,
}

impl Rail for HostRail {
    fn declaration(&self) -> RailDeclaration {
        let d = self.method.declaration();
        let mut v = RailDeclaration::new(d.longest_completion_ms, d.payment_time_tolerance_ms)
            .with_idempotent(d.idempotent)
            .with_history(d.history)
            .with_history_survives_restore(d.history_survives_restore);
        if let Some(ms) = d.idempotency_retention_ms {
            v = v.with_idempotency_retention_ms(ms);
        }
        if let Some(ms) = d.history_retention_ms {
            v = v.with_history_retention_ms(ms);
        }
        v
    }

    fn endpoint_identifiers(&self) -> Vec<PaymentEndpointIdentifier> {
        self.identifiers.clone()
    }

    fn select_endpoints(
        &self,
        candidates: &[PaymentEndpointCandidate],
        amount: Option<&PaymentAmount>,
    ) -> Result<EndpointSelection, RailError> {
        let choices = self.method.select_endpoints(
            candidates
                .iter()
                .map(|c| EndpointCandidate {
                    id: c.id.get(),
                    identifier: c.endpoint_id.as_str().into(),
                    payload: c.payload.as_str().into(),
                })
                .collect(),
            amount.map(Amount::from),
        )?;
        let mut selected =
            EndpointSelection::new(choices.payable.into_iter().map(CandidateId::new).collect());
        for f in choices.failures {
            selected = selected.with_error(CandidateId::new(f.id), f.error.into());
        }
        Ok(selected)
    }

    fn transfer(
        &self,
        id: &PaymentEndpointIdentifier,
        payload: &PaymentEndpointPayload,
        amount: &PaymentAmount,
    ) -> Transfer {
        self.method
            .transfer(id.as_str().into(), payload.as_str().into(), amount.into())
            .0
            .clone()
    }

    fn fraction_digits(&self, id: &PaymentEndpointIdentifier) -> Option<u32> {
        self.method.fraction_digits(id.as_str().into())
    }

    fn pay(&self, p: RailPayment) -> BoxFuture<'_, Result<RailOutcome, RailError>> {
        Box::pin(async move {
            self.method
                .pay(MethodPayment {
                    endpoint: p.endpoint_id.as_str().into(),
                    payload: p.payload.as_str().into(),
                    amount: (&p.amount).into(),
                    attempt_id: p.idempotency_key.to_text(),
                    payment_reference: p.payment_reference,
                })
                .await?
                .into_rail()
        })
    }

    fn payment(&self, id: AttemptId) -> BoxFuture<'_, Result<Option<RailOutcome>, RailError>> {
        Box::pin(async move {
            self.method
                .payment(id.to_text())
                .await?
                .map(MethodOutcome::into_rail)
                .transpose()
        })
    }

    fn allowance_history(
        &self,
        payer: [u8; 32],
    ) -> BoxFuture<'_, Result<Option<Vec<AllowanceDispatch>>, RailError>> {
        Box::pin(async move {
            let Some(history) = self.method.allowance_history(payer.to_vec()).await? else {
                return Ok(None);
            };
            let all = history
                .into_iter()
                .map(|entry| {
                    Ok(AllowanceDispatch {
                        attempt_id: AttemptId::parse(&entry.attempt_id).map_err(invalid_answer)?,
                        amount: PaymentAmount::try_from(entry.amount).map_err(invalid_answer)?,
                        outcome: entry.outcome.into_rail()?,
                    })
                })
                .collect::<Result<Vec<_>, RailError>>()?;
            Ok(Some(all))
        })
    }

    fn verify(&self, p: ProofCheck) -> BoxFuture<'_, Result<Option<u64>, RailError>> {
        Box::pin(async move {
            let proof_json = serde_json::to_vec(&p.proof)
                .map_err(|_| RailError::Unavailable("invalid proof".into()))?;
            Ok(self
                .method
                .verify(MethodProof {
                    endpoint: p.endpoint_id.as_str().into(),
                    payload: p.payload.map(|p| p.as_str().to_owned()),
                    amount: (&p.amount).into(),
                    attempt_id: p.attempt_id.to_text(),
                    proof_json,
                })
                .await?)
        })
    }

    fn settlement_id(&self, proof: &Map<String, Value>) -> Option<Vec<u8>> {
        self.method.settlement_id(serde_json::to_vec(proof).ok()?)
    }
}

impl From<uniffi::UnexpectedUniFFICallbackError> for MethodError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Unavailable {
            reason: "payment method callback interrupted".into(),
        }
    }
}
