use std::{collections::HashMap, fmt, sync::Arc};

use paykit_lib::PaymentReference;
use paykit_sdk::PaymentProofSubmission;

use crate::{
    errors::validation_error, json::FfiPrivateJsonObject,
    private_lists::FfiOutboundPrivateMessageStatus, sdk::FfiPaykitSdk, PaykitFfiError,
};

mod conversions;
#[cfg(test)]
mod tests;

use conversions::{parse_payment_request_id, parse_public_key, payment_request_records_to_ffi};

/// Payment Reference text with redacted debug output.
#[derive(uniffi::Object)]
pub struct FfiPaymentReference {
    text: String,
}

impl fmt::Debug for FfiPaymentReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FfiPaymentReference(<redacted:{} chars>)",
            self.text.chars().count()
        )
    }
}

impl FfiPaymentReference {
    pub(crate) fn from_validated_text(text: String) -> Self {
        Self { text }
    }
}

#[uniffi::export]
impl FfiPaymentReference {
    /// Create a Payment Reference after validating it.
    #[uniffi::constructor]
    pub fn new(text: String) -> Result<Self, PaykitFfiError> {
        PaymentReference::new(text.clone()).map_err(|err| validation_error(err.to_string()))?;
        Ok(Self { text })
    }

    /// Export the reference text for explicit payment execution or display.
    pub fn export_text(&self) -> String {
        self.text.clone()
    }
}

/// Payment Amount fields used by Payment Requests.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaymentRequestAmount {
    /// Decimal amount text.
    pub value: String,
    /// Asset code or unit.
    pub asset: String,
}

/// Time interval a recurring Payment Proof applies to.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiBillingPeriod {
    /// RFC3339 UTC start timestamp.
    pub starts_at: String,
    /// RFC3339 UTC end timestamp.
    pub ends_at: String,
}

/// Recurrence fields for a recurring Payment Request.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaymentRequestRecurrence {
    /// Positive interval count.
    pub every: u32,
    /// Unit string: minute, hour, day, week, month, or year.
    pub unit: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub anchor: String,
    /// Optional RFC3339 UTC timestamp using `Z`, after `starts_at` when
    /// present.
    pub ends_at: Option<String>,
}

/// Units of payment asset owed per requested asset unit.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiConversionRate {
    /// Payment asset code.
    pub asset: String,
    /// Positive decimal multiplier; never convert through floating point.
    pub value: String,
}

impl fmt::Debug for FfiConversionRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FfiConversionRate(<redacted>)")
    }
}

/// Conversion terms; absence leaves conversion to the wallets' policies.
#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiPaymentConversion {
    /// Immutable conversion rates, also fixed across recurring installments.
    Fixed {
        /// Exact rates; omitted cross-asset currencies cannot be used.
        rates: Vec<FfiConversionRate>,
    },
    /// Cross-asset recurring payments require a quote for their Billing Period.
    PerPeriod,
}

/// Deadline for actual payment, independent of proposal acceptance expiry.
#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiPaymentDeadline {
    /// Absolute one-time deadline in RFC3339 UTC.
    At {
        /// RFC3339 UTC timestamp with uppercase T and Z.
        timestamp: String,
    },
    /// Recurring deadline in elapsed seconds after the Billing Period start.
    PeriodStart {
        /// Nonnegative elapsed seconds from each period start.
        seconds: u64,
    },
}

/// Payee-issued quote retained for payment verification after expiry.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentConversionQuoteRecord {
    /// Quote identity (Event ID).
    pub event_id: String,
    /// Period for which these rates were offered.
    pub billing_period: FfiBillingPeriod,
    /// Payment asset units per requested asset unit.
    pub rates: Vec<FfiConversionRate>,
    /// Inclusive start of the payment validity interval, in RFC3339 UTC.
    pub valid_from: String,
    /// Inclusive actual-payment deadline, in RFC3339 UTC.
    pub expires_at: String,
    /// Local outbound status, when issued locally.
    pub outbound_status: Option<FfiOutboundPrivateMessageStatus>,
}

/// Immutable terms for a Payment Request proposal.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaymentRequestTerms {
    /// Requested amount.
    pub amount: FfiPaymentRequestAmount,
    /// Payee-provided payment correlation value.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Proposal expiry before acceptance.
    pub proposal_expires_at: Option<String>,
    /// Optional recurrence.
    pub recurrence: Option<FfiPaymentRequestRecurrence>,
    /// Accepted Payment Endpoint Identifier strings.
    pub accepted_payment_endpoint_identifiers: Vec<String>,
    /// Immutable request-bound Payment Endpoints owned by `required_app_id`.
    /// When present, payment must not fall back outside this nonempty map.
    pub payment_endpoints: Option<HashMap<String, String>>,
    /// Application that must handle this payment, when constrained.
    pub required_app_id: Option<String>,
    /// Optional immutable conversion policy.
    pub conversion: Option<FfiPaymentConversion>,
    /// Optional actual-payment deadline.
    pub payment_deadline: Option<FfiPaymentDeadline>,
    /// Application-specific metadata encoded as a JSON object.
    pub metadata: Arc<FfiPrivateJsonObject>,
}

impl fmt::Debug for FfiPaymentRequestTerms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPaymentRequestTerms")
            .field("amount", &"<redacted>")
            .field("payment_reference", &self.payment_reference)
            .field("proposal_expires_at", &self.proposal_expires_at)
            .field("recurrence", &self.recurrence)
            .field(
                "accepted_payment_endpoint_identifiers",
                &self.accepted_payment_endpoint_identifiers,
            )
            .field("required_app_id", &self.required_app_id)
            .field(
                "payment_endpoints",
                &self.payment_endpoints.as_ref().map(|_| "<redacted>"),
            )
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Local role for one Payment Request.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPaymentRequestLocalRole {
    /// Local identity is expected to pay.
    Payer,
    /// Local identity expects to receive payment.
    Payee,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// SDK-derived Payment Request lifecycle state.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPaymentRequestLifecycleState {
    /// Proposal is known locally and remains actionable.
    Proposed,
    /// Proposal is past its expiry.
    ProposalExpired,
    /// Acceptance is present locally.
    Accepted,
    /// Rejection is present locally.
    Rejected,
    /// Cancellation is present locally.
    Canceled,
    /// A one-time Payment Proof is present locally.
    ProofSubmitted,
    /// Recurring request acceptance is present locally.
    ActiveRecurring,
    /// A local outbound event may require private-link recovery.
    RecoveryRequired,
    /// Event ordering, dedupe, or lifecycle validation found an invalid state.
    InvalidConflict,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Filter for listing Payment Requests.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaymentRequestFilter {
    /// Restrict results to one counterparty.
    pub counterparty: Option<String>,
    /// Restrict results to one local role.
    pub local_role: Option<FfiPaymentRequestLocalRole>,
    /// Restrict results to lifecycle states. Empty means all states.
    pub states: Vec<FfiPaymentRequestLifecycleState>,
    /// Restrict results by whether the request has recurrence terms.
    pub recurring: Option<bool>,
    /// Inspect inbound proposals only; use the full view for actionable state and quotes.
    pub received_only: bool,
}

/// Payment Proof captured in a derived Payment Request record.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentProofRecord {
    /// Event ID.
    pub event_id: String,
    /// Outbound message id, when proof was sent locally.
    pub outbound_message_id: Option<u64>,
    /// Local outbound delivery status, when proof was queued locally.
    pub outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Stream item id, when proof was received from the counterparty.
    pub stream_item_id: Option<u64>,
    /// Payment Reference copied from the proof.
    pub payment_reference: Arc<FfiPaymentReference>,
    /// Optional Billing Period copied from the proof.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Application whose endpoint was used for the payment.
    pub payment_app_id: String,
    /// Payment Endpoint Identifier used for payment.
    pub payment_endpoint_identifier: String,
    /// Optional canonical Allowance ID reported for this payment execution.
    pub allowance_id: Option<String>,
    /// Selected recurring quote Event ID.
    pub conversion_quote_id: Option<String>,
    /// Method-specific proof object encoded as JSON.
    pub proof: Arc<FfiPrivateJsonObject>,
    /// Local record time for this proof as RFC3339 text.
    pub recorded_at: String,
}

/// SDK-derived Payment Request lifecycle record.
#[derive(uniffi::Record, Clone, Debug)]
pub struct FfiPaymentRequestRecord {
    /// Counterparty associated with the private stream.
    pub counterparty: String,
    /// Stable Payment Request ID.
    pub payment_request_id: String,
    /// Local role, when known.
    pub local_role: Option<FfiPaymentRequestLocalRole>,
    /// Derived local lifecycle state.
    pub state: FfiPaymentRequestLifecycleState,
    /// Stream item id of the proposal event.
    pub proposal_stream_item_id: Option<u64>,
    /// Outbound message id of the proposal event.
    pub proposal_outbound_message_id: Option<u64>,
    /// Local outbound delivery status for the proposal event.
    pub proposal_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Proposal Event ID.
    pub proposal_event_id: Option<String>,
    /// Application that created the proposal.
    pub proposal_app_id: Option<String>,
    /// Payer application associated with the request, independent of its current execution claim.
    pub payer_app_id: Option<String>,
    /// Paykit App currently preparing to execute this payment.
    pub execution_claim_app_id: Option<String>,
    /// Immutable terms from the proposal.
    pub terms: Option<FfiPaymentRequestTerms>,
    /// Acceptance Event ID.
    pub accepted_event_id: Option<String>,
    /// Local outbound delivery status for an acceptance event.
    pub accepted_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Rejection Event ID.
    pub rejected_event_id: Option<String>,
    /// Local outbound delivery status for a rejection event.
    pub rejected_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Cancellation Event ID.
    pub canceled_event_id: Option<String>,
    /// Local outbound delivery status for a cancellation event.
    pub canceled_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Immutable conversion quotes, including expired historical quotes.
    pub conversion_quotes: Vec<FfiPaymentConversionQuoteRecord>,
    /// Payment Proof records in local record order.
    pub payment_proofs: Vec<FfiPaymentProofRecord>,
    /// Last inbound stream item applied to this record.
    pub last_stream_item_id: Option<u64>,
    /// Last outbound message applied to this record.
    pub last_outbound_message_id: Option<u64>,
    /// Local delivery status of the last outbound message applied to this record.
    pub last_outbound_status: Option<FfiOutboundPrivateMessageStatus>,
    /// Last event local record time as RFC3339 text.
    pub last_event_at: Option<String>,
    /// Invalid state reason, when available.
    pub invalid_reason: Option<String>,
}

/// Method-specific Payment Proof submission data.
#[derive(uniffi::Record, Clone, PartialEq, Eq)]
pub struct FfiPaymentProofSubmission {
    /// Billing Period for recurring Payment Requests.
    pub billing_period: Option<FfiBillingPeriod>,
    /// Application whose endpoint was used for the payment.
    pub payment_app_id: String,
    /// Payment Endpoint Identifier used for payment.
    pub payment_endpoint_identifier: String,
    /// Canonical Allowance ID from the wallet's persisted payment association, when used.
    /// This field reports usage; it does not authorize or account for payment.
    pub allowance_id: Option<String>,
    /// Selected recurring quote Event ID.
    pub conversion_quote_id: Option<String>,
    /// Method-specific proof object encoded as JSON.
    pub proof: Arc<FfiPrivateJsonObject>,
}

impl fmt::Debug for FfiPaymentProofSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPaymentProofSubmission")
            .field("billing_period", &self.billing_period)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field(
                "allowance_id",
                &self.allowance_id.as_ref().map(|_| "<redacted>"),
            )
            .field("proof", &self.proof)
            .finish()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Inspect inbound proposals from one counterparty.
    /// Quotes and actionable state require the full view, which includes outbound acceptance.
    pub async fn received_payment_requests_from(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self
            .runtime
            .received_payment_requests_from(&parse_public_key(counterparty)?)
            .await?;
        payment_request_records_to_ffi(records)
    }

    /// Return Payment Requests involving one counterparty.
    pub async fn payment_requests_with(
        &self,
        counterparty: String,
    ) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self
            .runtime
            .payment_requests_with(&parse_public_key(counterparty)?)
            .await?;
        payment_request_records_to_ffi(records)
    }

    /// Return Payment Requests matching a local SDK filter.
    pub async fn list_payment_requests(
        &self,
        filter: FfiPaymentRequestFilter,
    ) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self
            .runtime
            .list_payment_requests(filter.try_into()?)
            .await?;
        payment_request_records_to_ffi(records)
    }

    /// Return all Payment Requests across non-blocked counterparties.
    pub async fn payment_requests(&self) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self.runtime.payment_requests().await?;
        payment_request_records_to_ffi(records)
    }

    /// Return accepted recurring Payment Requests across non-blocked counterparties.
    pub async fn active_recurring_payment_requests(
        &self,
    ) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self.runtime.active_recurring_payment_requests().await?;
        payment_request_records_to_ffi(records)
    }

    /// Return received Payment Requests that still need a payer response.
    ///
    /// A request's required Paykit App constrains the payee endpoint, not the
    /// payer app that responds.
    pub async fn actionable_received_payment_requests(
        &self,
    ) -> Result<Vec<FfiPaymentRequestRecord>, PaykitFfiError> {
        let records = self.runtime.actionable_received_payment_requests().await?;
        payment_request_records_to_ffi(records)
    }

    /// Queue a new Payment Request proposal and return local derived state.
    /// The caller must establish peer support before including conversion or payment deadlines.
    pub async fn propose_payment_request(
        &self,
        counterparty: String,
        terms: FfiPaymentRequestTerms,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        self.runtime
            .propose_payment_request(parse_public_key(counterparty)?, terms.try_into()?)
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Claim a received Payment Request before preparing payment execution.
    pub async fn claim_payment_request_for_execution(
        &self,
        counterparty: String,
        payment_request_id: String,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .claim_payment_request_for_execution(
                parse_public_key(counterparty)?,
                &payment_request_id,
            )
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Release this App's unresolved execution claim without rejecting the request.
    pub async fn release_payment_request_execution_claim(
        &self,
        counterparty: String,
        payment_request_id: String,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .release_payment_request_execution_claim(
                parse_public_key(counterparty)?,
                &payment_request_id,
            )
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Queue acceptance before external payment execution and return local derived state.
    pub async fn accept_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .accept_payment_request(parse_public_key(counterparty)?, &payment_request_id)
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Atomically claim a received Payment Request and durably queue acceptance, without sending it.
    pub async fn claim_and_accept_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .claim_and_accept_payment_request(parse_public_key(counterparty)?, &payment_request_id)
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Queue rejection for a received Payment Request and return local derived state.
    pub async fn reject_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
        reason: Option<String>,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .reject_payment_request(parse_public_key(counterparty)?, &payment_request_id, reason)
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Queue cancellation for a known non-terminal Payment Request.
    pub async fn cancel_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
        reason: Option<String>,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        self.runtime
            .cancel_payment_request(parse_public_key(counterparty)?, &payment_request_id, reason)
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Issue rates for one accepted recurring Billing Period; does not authorize payment.
    /// The caller establishes peer support and owns session creation, capability scope and key rotation.
    pub async fn quote_payment_request(
        &self,
        counterparty: String,
        payment_request_id: String,
        billing_period: FfiBillingPeriod,
        rates: Vec<FfiConversionRate>,
        expires_at: String,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        self.runtime
            .quote_payment_request(
                parse_public_key(counterparty)?,
                &parse_payment_request_id(payment_request_id)?,
                billing_period.try_into()?,
                rates.into_iter().map(Into::into).collect(),
                expires_at,
            )
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }

    /// Queue a Payment Proof for an accepted Payment Request.
    pub async fn submit_payment_proof(
        &self,
        counterparty: String,
        payment_request_id: String,
        proof: FfiPaymentProofSubmission,
    ) -> Result<FfiPaymentRequestRecord, PaykitFfiError> {
        let payment_request_id = parse_payment_request_id(payment_request_id)?;
        let proof = PaymentProofSubmission::try_from(proof)?;
        self.runtime
            .submit_payment_proof_submission(
                parse_public_key(counterparty)?,
                &payment_request_id,
                proof,
            )
            .await
            .map_err(Into::into)
            .and_then(FfiPaymentRequestRecord::try_from)
    }
}

/// Resolve an actual-payment deadline for the selected Billing Period.
/// The caller compares this with independently verified payment time.
#[uniffi::export]
pub fn payment_deadline_at(
    deadline: FfiPaymentDeadline,
    billing_period: Option<FfiBillingPeriod>,
) -> Result<String, PaykitFfiError> {
    let deadline: paykit_lib::PaymentDeadline = deadline.into();
    let period: Option<paykit_lib::BillingPeriod> =
        billing_period.map(TryInto::try_into).transpose()?;
    deadline
        .at(period.as_ref())
        .map_err(|err| validation_error(err.to_string()))
}
