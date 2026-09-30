use std::{collections::HashMap, fmt};

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    validation::{parse_utc_timestamp, validate_uuid_v4},
    AllowanceId, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PaymentReference, PrivateMessageKind, Result,
};

use super::{PaymentConversion, PaymentConversionQuote, PaymentDeadline};

/// UUID-v4 identifier for one Payment Request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaymentRequestId(String);

impl PaymentRequestId {
    /// Create a Payment Request ID from a UUID-v4 string.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        validate_uuid_v4(id.into(), "Payment Request ID").map(Self)
    }

    /// Generate a fresh Payment Request ID.
    pub fn new_v4() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Access the canonical UUID string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PaymentRequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PaymentRequestId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Recurrence unit for recurring Payment Requests.
///
/// This enum is intentionally exhaustive. Adding a variant must produce
/// compile-time failures in canonical wire serialization and SDK conversion
/// matches until the new Recurrence unit is mapped explicitly. Do not add
/// `#[non_exhaustive]` without a coordinated team decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecurrenceUnit {
    /// Minute-based recurrence.
    Minute,
    /// Hour-based recurrence.
    Hour,
    /// Day-based recurrence.
    Day,
    /// Week-based recurrence.
    Week,
    /// Month-based recurrence.
    Month,
    /// Year-based recurrence.
    Year,
}

impl RecurrenceUnit {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "minute" => Ok(Self::Minute),
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            "year" => Ok(Self::Year),
            _ => Err(PaykitError::Validation(format!(
                "unsupported Recurrence unit '{value}'"
            ))),
        }
    }
}

/// Schedule object for a recurring Payment Request.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::Recurrence) {
///     value.every = 0;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recurrence {
    /// Positive interval count.
    pub(super) every: u32,
    /// Recurrence unit.
    pub(super) unit: RecurrenceUnit,
    /// RFC3339 UTC timestamp using `Z`.
    pub(super) starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub(super) anchor: String,
    /// Optional RFC3339 UTC timestamp using `Z`, after `starts_at` when
    /// present.
    pub(super) ends_at: Option<String>,
}

/// Payment Request terms set by the payee.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentRequestTerms) {
///     value.accepted_payment_endpoint_identifiers = Vec::new();
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct PaymentRequestTerms {
    /// Requested amount.
    pub(super) amount: PaymentAmount,
    /// Payee-provided correlation value copied into Payment Proof messages.
    pub(super) payment_reference: PaymentReference,
    /// Proposal expiry before acceptance. `None` means no protocol-level
    /// proposal expiry.
    pub(super) proposal_expires_at: Option<String>,
    /// Optional recurrence. `None` means one-time request.
    pub(super) recurrence: Option<Recurrence>,
    /// Accepted Payment Endpoint Identifiers.
    pub(super) accepted_payment_endpoint_identifiers: Vec<PaymentEndpointIdentifier>,
    /// Immutable Payment Endpoints owned by `required_app_id`, when bound.
    pub(super) payment_endpoints:
        Option<HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>>,
    /// Optional conversion policy; absence leaves conversion to wallet policy.
    pub(super) conversion: Option<PaymentConversion>,
    /// Optional deadline for actual payment, independent of proposal acceptance.
    pub(super) payment_deadline: Option<PaymentDeadline>,
    /// Payee application whose Payment Endpoint must be paid, when constrained.
    pub(super) required_app_id: Option<crate::PaykitAppId>,
    /// Application-specific JSON metadata.
    pub(super) metadata: JsonMap<String, JsonValue>,
}

impl fmt::Debug for PaymentRequestTerms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequestTerms")
            .field("amount", &"<redacted>")
            .field("payment_reference", &"<redacted>")
            .field("proposal_expires_at", &self.proposal_expires_at)
            .field("recurrence", &self.recurrence)
            .field("conversion", &self.conversion)
            .field("payment_deadline", &self.payment_deadline)
            .field(
                "accepted_payment_endpoint_identifiers",
                &self.accepted_payment_endpoint_identifiers,
            )
            .field("required_app_id", &self.required_app_id)
            .field(
                "payment_endpoints",
                &self.payment_endpoints.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "metadata",
                &format_args!("<redacted:{} fields>", self.metadata.len()),
            )
            .finish()
    }
}

/// Time interval a recurring Payment Proof applies to.
///
/// # Validation
///
/// [`BillingPeriod::new`] validates the interval before construction. Payment
/// Proof and Receipt parsers use the same rules, which require
/// `starts_at` and `ends_at` to be RFC3339 timestamps with a `Z` suffix and
/// `ends_at` to be strictly later than `starts_at`.
///
/// Receipt Access still requires [`ReceiptAccess::validate`](crate::ReceiptAccess::validate)
/// for its request context. A valid interval alone does not establish the
/// associated Payment Request ID.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::BillingPeriod) {
///     value.ends_at = "invalid".into();
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BillingPeriod {
    /// RFC3339 UTC timestamp using `Z`.
    pub(super) starts_at: String,
    /// RFC3339 UTC timestamp using `Z`, after `starts_at`.
    pub(super) ends_at: String,
}

/// `paykit.payment_request` Event Message.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentRequest) {
///     value.version = 0;
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct PaymentRequest {
    /// Message version. Currently always `1`.
    pub(super) version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequest`].
    pub(super) kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub(super) event_id: EventId,
    /// Stable Payment Request ID.
    pub(super) payment_request_id: PaymentRequestId,
    /// Immutable request terms.
    pub(super) request: PaymentRequestTerms,
}

impl fmt::Debug for PaymentRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequest")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field("request", &self.request)
            .finish()
    }
}

impl PaymentRequest {
    pub(super) fn validate(&self) -> Result<()> {
        crate::validation::validate_outgoing_version_kind(
            self.version,
            self.kind,
            PrivateMessageKind::PaymentRequest,
            "Payment Request",
        )?;
        self.request.validate()
    }

    /// Construct a Payment Request proposal using protocol version 1.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        request: PaymentRequestTerms,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentRequest,
            event_id,
            payment_request_id,
            request,
        }
    }
}

/// `paykit.payment_request_acceptance` Event Message.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentRequestAcceptance) {
///     value.kind = paykit_lib::PrivateMessageKind::PaymentProof;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestAcceptance {
    /// Message version. Currently always `1`.
    pub(super) version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestAcceptance`].
    pub(super) kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub(super) event_id: EventId,
    /// Stable Payment Request ID.
    pub(super) payment_request_id: PaymentRequestId,
}

impl PaymentRequestAcceptance {
    /// Construct a Payment Request acceptance using protocol version 1.
    pub fn new(event_id: EventId, payment_request_id: PaymentRequestId) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentRequestAcceptance,
            event_id,
            payment_request_id,
        }
    }
}

/// `paykit.payment_request_rejection` Event Message.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentRequestRejection) {
///     value.version = 0;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestRejection {
    /// Message version. Currently always `1`.
    pub(super) version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestRejection`].
    pub(super) kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub(super) event_id: EventId,
    /// Stable Payment Request ID.
    pub(super) payment_request_id: PaymentRequestId,
    /// Optional informational reason.
    pub(super) reason: Option<String>,
}

impl PaymentRequestRejection {
    /// Construct a Payment Request rejection using protocol version 1.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        reason: Option<String>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentRequestRejection,
            event_id,
            payment_request_id,
            reason,
        }
    }
}

/// `paykit.payment_request_cancellation` Event Message.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentRequestCancellation) {
///     value.version = 0;
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestCancellation {
    /// Message version. Currently always `1`.
    pub(super) version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestCancellation`].
    pub(super) kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub(super) event_id: EventId,
    /// Stable Payment Request ID.
    pub(super) payment_request_id: PaymentRequestId,
    /// Optional informational reason.
    pub(super) reason: Option<String>,
}

impl PaymentRequestCancellation {
    /// Construct a Payment Request cancellation using protocol version 1.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        reason: Option<String>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentRequestCancellation,
            event_id,
            payment_request_id,
            reason,
        }
    }
}

/// `paykit.payment_proof` Event Message.
///
/// Fields cannot be mutated after validated construction.
///
/// ```compile_fail,E0616
/// fn cannot_mutate(mut value: paykit_lib::PaymentProof) {
///     value.kind = paykit_lib::PrivateMessageKind::PaymentRequest;
/// }
/// ```
#[derive(Clone, PartialEq)]
pub struct PaymentProof {
    /// Message version. Currently always `1`.
    pub(super) version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentProof`].
    pub(super) kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub(super) event_id: EventId,
    /// Stable Payment Request ID.
    pub(super) payment_request_id: PaymentRequestId,
    /// Payment Reference copied from the accepted Payment Request.
    pub(super) payment_reference: PaymentReference,
    /// Billing period. Required for recurring requests, `None` for one-time requests.
    pub(super) billing_period: Option<BillingPeriod>,
    /// Application whose endpoint was used for the payment.
    pub(super) payment_app_id: crate::PaykitAppId,
    /// Payment Endpoint Identifier used for the payment execution.
    pub(super) payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// Optional Allowance used for this payment, scoped to the exact Encrypted Link.
    pub(super) allowance_id: Option<AllowanceId>,
    /// Event ID of the selected recurring conversion quote, when required.
    pub(super) conversion_quote_id: Option<EventId>,
    /// Method-specific proof object.
    pub(super) proof: JsonMap<String, JsonValue>,
}

impl fmt::Debug for PaymentProof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentProof")
            .field("version", &self.version)
            .field("kind", &self.kind)
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field("payment_reference", &"<redacted>")
            .field("billing_period", &self.billing_period)
            .field("payment_app_id", &self.payment_app_id)
            .field("allowance_id", &self.allowance_id)
            .field(
                "payment_endpoint_identifier",
                &self.payment_endpoint_identifier,
            )
            .field(
                "proof",
                &format_args!("<redacted:{} fields>", self.proof.len()),
            )
            .finish()
    }
}

impl PaymentProof {
    /// Construct a Payment Proof using protocol version 1.
    pub fn new(
        event_id: EventId,
        payment_request_id: PaymentRequestId,
        payment_reference: PaymentReference,
        billing_period: Option<BillingPeriod>,
        payment_app_id: crate::PaykitAppId,
        payment_endpoint_identifier: PaymentEndpointIdentifier,
        proof: JsonMap<String, JsonValue>,
    ) -> Self {
        Self {
            version: 1,
            kind: PrivateMessageKind::PaymentProof,
            event_id,
            payment_request_id,
            payment_reference,
            billing_period,
            payment_app_id,
            payment_endpoint_identifier,
            allowance_id: None,
            conversion_quote_id: None,
            proof,
        }
    }

    /// Bind this proof to a previously issued recurring conversion quote.
    pub fn with_conversion_quote_id(mut self, quote_id: EventId) -> Self {
        self.conversion_quote_id = Some(quote_id);
        self
    }

    /// Report the Allowance used for this payment execution.
    ///
    /// The caller must use its persisted payment association. This is reporting
    /// only; it does not authorize payment, validate usage, or change accounting.
    pub fn with_allowance_id(mut self, allowance_id: AllowanceId) -> Self {
        self.allowance_id = Some(allowance_id);
        self
    }

    /// Access the reported Allowance ID, when this payment used an Allowance.
    pub fn allowance_id(&self) -> Option<&AllowanceId> {
        self.allowance_id.as_ref()
    }

    pub(super) fn validate(&self) -> Result<()> {
        crate::validation::validate_outgoing_version_kind(
            self.version,
            self.kind,
            PrivateMessageKind::PaymentProof,
            "Payment Proof",
        )?;
        if let Some(period) = &self.billing_period {
            period.validate()?;
        }
        validate_method_specific_proof(&self.proof)
    }

    /// Validate this proof against the immutable terms of a specific Payment Request.
    ///
    /// Checks stateless correlation only: request ID, Payment Reference,
    /// Billing Period presence/shape, accepted endpoint and conversion selection.
    /// Use `validate_conversion_quote` to also check a selected quote. Caller
    /// state owns lifecycle, role, dedupe, settlement and recurrence eligibility.
    pub fn validate_for_request(&self, request: &PaymentRequest) -> Result<()> {
        self.validate()?;
        request.validate()?;
        if self.payment_request_id != request.payment_request_id {
            return Err(PaykitError::Validation(
                "Payment Proof payment_request_id must match Payment Request".into(),
            ));
        }
        if self.payment_reference != request.request.payment_reference {
            return Err(PaykitError::Validation(
                "Payment Proof payment_reference must match Payment Request".into(),
            ));
        }
        if !request
            .request
            .accepted_payment_endpoint_identifiers
            .contains(&self.payment_endpoint_identifier)
            || request
                .request
                .payment_endpoints
                .as_ref()
                .is_some_and(|endpoints| !endpoints.contains_key(&self.payment_endpoint_identifier))
        {
            return Err(PaykitError::Validation(
                "Payment Proof payment_endpoint_identifier is not accepted by Payment Request"
                    .into(),
            ));
        }
        if request
            .request
            .required_app_id
            .as_ref()
            .is_some_and(|app_id| app_id != &self.payment_app_id)
        {
            return Err(PaykitError::Validation(
                "Payment Proof payment_app_id does not match the Payment Request".into(),
            ));
        }
        self.validate_conversion(&request.request)?;
        match (&request.request.recurrence, &self.billing_period) {
            (None, Some(_)) => Err(PaykitError::Validation(
                "Payment Proof billing_period must be null for one-time Payment Requests".into(),
            )),
            (Some(_), None) => Err(PaykitError::Validation(
                "Payment Proof billing_period is required for recurring Payment Requests".into(),
            )),
            (_, Some(period)) => period.validate(),
            (None, None) => Ok(()),
        }
    }
}

const ERC20_PAYMENT_PROOF_TYPE: &str = "erc20-transfer-eip712";
const UINT256_MAX_DECIMAL: &str =
    "115792089237316195423570985008687907853269984665640564039457584007913129639935";

fn validate_method_specific_proof(proof: &JsonMap<String, JsonValue>) -> Result<()> {
    if proof.get("type").and_then(JsonValue::as_str) != Some(ERC20_PAYMENT_PROOF_TYPE) {
        return Ok(());
    }
    let index = proof
        .get("receipt_log_index")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| {
            PaykitError::Validation(
                "ERC-20 Payment Proof receipt_log_index must be a decimal string".into(),
            )
        })?;
    let canonical = index == "0"
        || (!index.is_empty()
            && !index.starts_with('0')
            && index.bytes().all(|byte| byte.is_ascii_digit()));
    if !canonical
        || index.len() > UINT256_MAX_DECIMAL.len()
        || (index.len() == UINT256_MAX_DECIMAL.len() && index > UINT256_MAX_DECIMAL)
    {
        return Err(PaykitError::Validation(
            "ERC-20 Payment Proof receipt_log_index must be a canonical uint256 decimal string"
                .into(),
        ));
    }
    Ok(())
}

/// One recognized Payment Request protocol Event Message in FIFO receive order.
///
/// This enum is intentionally exhaustive. Adding a variant must produce
/// compile-time failures in serialization, lifecycle, and replay matches until
/// the new Payment Request Event Message is classified explicitly. Do not add
/// `#[non_exhaustive]` without a coordinated team decision.
#[derive(Clone, PartialEq)]
pub enum PaymentRequestEvent {
    /// `paykit.payment_request` proposal event.
    Request(PaymentRequest),
    /// `paykit.payment_request_acceptance` event.
    Acceptance(PaymentRequestAcceptance),
    /// `paykit.payment_request_rejection` event.
    Rejection(PaymentRequestRejection),
    /// `paykit.payment_request_cancellation` event.
    Cancellation(PaymentRequestCancellation),
    /// Payee-issued recurring conversion quote.
    ConversionQuote(PaymentConversionQuote),
    /// `paykit.payment_proof` event.
    Proof(PaymentProof),
}

impl fmt::Debug for PaymentRequestEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaymentRequestEvent")
            .field("kind", &self.kind())
            .field("event_id", &self.event_id())
            .field("payment_request_id", &self.payment_request_id())
            .finish()
    }
}

impl PaymentRequestEvent {
    /// Return the Private Message Kind for this event.
    pub fn kind(&self) -> PrivateMessageKind {
        match self {
            Self::Request(event) => event.kind,
            Self::Acceptance(event) => event.kind,
            Self::Rejection(event) => event.kind,
            Self::Cancellation(event) => event.kind,
            Self::Proof(event) => event.kind,
            Self::ConversionQuote(event) => event.kind(),
        }
    }

    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        match self {
            Self::Request(event) => &event.event_id,
            Self::Acceptance(event) => &event.event_id,
            Self::Rejection(event) => &event.event_id,
            Self::Cancellation(event) => &event.event_id,
            Self::Proof(event) => &event.event_id,
            Self::ConversionQuote(event) => event.event_id(),
        }
    }

    /// Access the Payment Request ID shared by this lifecycle event.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        match self {
            Self::Request(event) => &event.payment_request_id,
            Self::Acceptance(event) => &event.payment_request_id,
            Self::Rejection(event) => &event.payment_request_id,
            Self::Cancellation(event) => &event.payment_request_id,
            Self::Proof(event) => &event.payment_request_id,
            Self::ConversionQuote(event) => event.payment_request_id(),
        }
    }
}

/// A recognized Payment Request protocol Event Message plus the raw JSON
/// payload received from the Encrypted Link.
#[derive(Clone, PartialEq)]
pub struct PaymentRequestEventMessage {
    /// Private Message Kind selected from the message header.
    pub kind: PrivateMessageKind,
    /// Application that created this event.
    pub app_id: Option<crate::PaykitAppId>,
    /// Parsed top-level Event ID when present and valid.
    pub event_id: Option<EventId>,
    /// Parsed top-level Payment Request ID when present and valid.
    pub payment_request_id: Option<PaymentRequestId>,
    /// Raw JSON plaintext as sent over the Encrypted Link.
    pub raw_json: String,
    /// Parsed event, or an error string explaining why this recognized message
    /// failed structural validation.
    pub event: std::result::Result<PaymentRequestEvent, String>,
}

impl fmt::Debug for PaymentRequestEventMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parsed_kind = self.event.as_ref().ok().map(PaymentRequestEvent::kind);
        f.debug_struct("PaymentRequestEventMessage")
            .field("kind", &self.kind)
            .field("app_id", &self.app_id)
            .field("event_id", &self.event_id)
            .field("payment_request_id", &self.payment_request_id)
            .field(
                "raw_json",
                &format_args!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("parsed_kind", &parsed_kind)
            .field("validation_error", &self.validation_error())
            .finish()
    }
}

impl PaymentRequestEventMessage {
    /// Return the Private Message Kind for this event message.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }

    /// Return the application that created this event, when valid.
    pub fn app_id(&self) -> Option<&crate::PaykitAppId> {
        self.app_id.as_ref()
    }

    /// Whether the recognized event message parsed successfully.
    pub fn is_valid(&self) -> bool {
        self.event.is_ok()
    }

    /// Access the parsed event when structural validation succeeded.
    pub fn parsed_event(&self) -> Option<&PaymentRequestEvent> {
        self.event.as_ref().ok()
    }

    /// Access the validation error when structural validation failed.
    pub fn validation_error(&self) -> Option<&str> {
        self.event.as_ref().err().map(String::as_str)
    }

    /// Access the Event ID.
    ///
    /// Returns `None` when the recognized message is malformed and the Event ID
    /// could not be parsed as a valid Event ID.
    pub fn event_id(&self) -> Option<&EventId> {
        self.event_id.as_ref()
    }

    /// Access the Payment Request ID shared by this lifecycle event.
    ///
    /// Returns `None` when the recognized message is malformed and the Payment
    /// Request ID could not be parsed as a valid Payment Request ID.
    pub fn payment_request_id(&self) -> Option<&PaymentRequestId> {
        self.payment_request_id.as_ref()
    }
}

impl Recurrence {
    /// Check `every`, timestamp formats, and that a non-null `ends_at` is
    /// after `starts_at`. `anchor` ordering is not constrained.
    pub(super) fn validate(&self) -> Result<()> {
        if self.every == 0 {
            return Err(PaykitError::Validation(
                "Recurrence every must be a positive integer".into(),
            ));
        }
        let starts_at = parse_utc_timestamp(&self.starts_at, "Recurrence starts_at")?;
        parse_utc_timestamp(&self.anchor, "Recurrence anchor")?;
        if let Some(ends_at) = &self.ends_at {
            let ends_at = parse_utc_timestamp(ends_at, "Recurrence ends_at")?;
            if ends_at <= starts_at {
                return Err(PaykitError::Validation(
                    "Recurrence ends_at must be after starts_at".into(),
                ));
            }
        }
        Ok(())
    }
}

impl BillingPeriod {
    pub(crate) fn validate_with_label(&self, label: &str) -> Result<()> {
        let starts_at = parse_utc_timestamp(&self.starts_at, &format!("{label} starts_at"))?;
        let ends_at = parse_utc_timestamp(&self.ends_at, &format!("{label} ends_at"))?;
        if ends_at <= starts_at {
            return Err(PaykitError::Validation(format!(
                "{label} ends_at must be after starts_at"
            )));
        }
        Ok(())
    }

    pub(super) fn validate(&self) -> Result<()> {
        self.validate_with_label("Billing Period")
    }
}

impl PaymentRequestTerms {
    pub(crate) fn validate(&self) -> Result<()> {
        self.amount.validate_with_label("Payment Request amount")?;
        if let Some(proposal_expires_at) = &self.proposal_expires_at {
            parse_utc_timestamp(proposal_expires_at, "Payment Request proposal_expires_at")?;
        }
        if let Some(recurrence) = &self.recurrence {
            recurrence.validate()?;
        }
        self.validate_conversion()?;
        if self.accepted_payment_endpoint_identifiers.is_empty() {
            return Err(PaykitError::Validation(
                "accepted_payment_endpoint_identifiers must not be empty".into(),
            ));
        }
        self.validate_payment_endpoints()?;
        Ok(())
    }

    fn validate_payment_endpoints(&self) -> Result<()> {
        let Some(endpoints) = &self.payment_endpoints else {
            return Ok(());
        };
        if endpoints.is_empty() {
            return Err(PaykitError::Validation(
                "payment_endpoints must not be empty".into(),
            ));
        }
        if self.required_app_id.is_none() {
            return Err(PaykitError::Validation(
                "payment_endpoints requires required_app_id".into(),
            ));
        }
        for (identifier, payload) in endpoints {
            if !self
                .accepted_payment_endpoint_identifiers
                .contains(identifier)
            {
                return Err(PaykitError::Validation(
                    "payment_endpoints keys must be accepted Payment Endpoint Identifiers".into(),
                ));
            }
            if payload.as_str().is_empty() {
                return Err(PaykitError::Validation(
                    "payment_endpoints payloads must not be empty".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Recurrence {
    /// Access the every.
    pub fn every(&self) -> u32 {
        self.every
    }
    /// Access the unit.
    pub fn unit(&self) -> RecurrenceUnit {
        self.unit
    }
    /// Access the preserved UTC start timestamp.
    pub fn starts_at(&self) -> &str {
        &self.starts_at
    }
    /// Access the anchor.
    pub fn anchor(&self) -> &str {
        &self.anchor
    }
    /// Access the end timestamp.
    pub fn ends_at(&self) -> &Option<String> {
        &self.ends_at
    }
}

impl PaymentRequestTerms {
    /// Access the amount.
    pub fn amount(&self) -> &PaymentAmount {
        &self.amount
    }
    /// Access the Payment Reference.
    pub fn payment_reference(&self) -> &PaymentReference {
        &self.payment_reference
    }
    /// Access the optional proposal expiry timestamp.
    pub fn proposal_expires_at(&self) -> &Option<String> {
        &self.proposal_expires_at
    }
    /// Access the recurrence.
    pub fn recurrence(&self) -> &Option<Recurrence> {
        &self.recurrence
    }
    /// Access the accepted Payment Endpoint Identifiers.
    pub fn accepted_payment_endpoint_identifiers(&self) -> &[PaymentEndpointIdentifier] {
        &self.accepted_payment_endpoint_identifiers
    }
    /// Access immutable request-bound Payment Endpoints, when present.
    ///
    /// These destinations belong to `required_app_id`. A payer must not fall
    /// back to endpoints outside this map. Absence leaves endpoint discovery unchanged.
    ///
    /// ```compile_fail,E0596
    /// fn cannot_mutate(value: paykit_lib::PaymentRequestTerms) {
    ///     value.payment_endpoints().unwrap().clear();
    /// }
    /// ```
    pub fn payment_endpoints(
        &self,
    ) -> Option<&HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>> {
        self.payment_endpoints.as_ref()
    }
    /// Access the optional payee App constraint.
    pub fn required_app_id(&self) -> Option<&crate::PaykitAppId> {
        self.required_app_id.as_ref()
    }
    /// Access the optional conversion policy.
    pub fn conversion(&self) -> Option<&PaymentConversion> {
        self.conversion.as_ref()
    }
    /// Access the optional payment deadline.
    pub fn payment_deadline(&self) -> Option<&PaymentDeadline> {
        self.payment_deadline.as_ref()
    }
    /// Access the metadata.
    pub fn metadata(&self) -> &JsonMap<String, JsonValue> {
        &self.metadata
    }
}

impl BillingPeriod {
    /// Access the preserved UTC start timestamp.
    pub fn starts_at(&self) -> &str {
        &self.starts_at
    }
    /// Access the end timestamp.
    pub fn ends_at(&self) -> &str {
        &self.ends_at
    }
}

impl PaymentRequest {
    /// Access the version.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Access the kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }
    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }
    /// Access the Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }
    /// Access the immutable request terms.
    pub fn request(&self) -> &PaymentRequestTerms {
        &self.request
    }
}

impl PaymentRequestAcceptance {
    /// Access the version.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Access the kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }
    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }
    /// Access the Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }
}

impl PaymentRequestRejection {
    /// Access the version.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Access the kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }
    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }
    /// Access the Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }
    /// Access the reason.
    pub fn reason(&self) -> &Option<String> {
        &self.reason
    }
}

impl PaymentRequestCancellation {
    /// Access the version.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Access the kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }
    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }
    /// Access the Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }
    /// Access the reason.
    pub fn reason(&self) -> &Option<String> {
        &self.reason
    }
}

impl PaymentProof {
    /// Access the version.
    pub fn version(&self) -> u8 {
        self.version
    }
    /// Access the kind.
    pub fn kind(&self) -> PrivateMessageKind {
        self.kind
    }
    /// Access the Event ID.
    pub fn event_id(&self) -> &EventId {
        &self.event_id
    }
    /// Access the Payment Request ID.
    pub fn payment_request_id(&self) -> &PaymentRequestId {
        &self.payment_request_id
    }
    /// Access the Payment Reference.
    pub fn payment_reference(&self) -> &PaymentReference {
        &self.payment_reference
    }
    /// Access the optional Billing Period.
    pub fn billing_period(&self) -> &Option<BillingPeriod> {
        &self.billing_period
    }
    /// Access the payee App whose endpoint was used for payment.
    pub fn payment_app_id(&self) -> &crate::PaykitAppId {
        &self.payment_app_id
    }
    /// Access the Payment Endpoint Identifier.
    pub fn payment_endpoint_identifier(&self) -> &PaymentEndpointIdentifier {
        &self.payment_endpoint_identifier
    }
    /// Access the selected recurring conversion quote, when required.
    pub fn conversion_quote_id(&self) -> Option<&EventId> {
        self.conversion_quote_id.as_ref()
    }
    /// Access the proof.
    pub fn proof(&self) -> &JsonMap<String, JsonValue> {
        &self.proof
    }
}

/// Unvalidated input for a recurring Payment Request schedule.
///
/// Convert with [`Recurrence::try_from`] to validate the complete schedule.
#[derive(Clone, Debug)]
pub struct RecurrenceConfig {
    /// Positive interval count.
    pub every: u32,
    /// Calendar or fixed-duration recurrence unit.
    pub unit: RecurrenceUnit,
    /// First UTC instant, using the `Z` suffix.
    pub starts_at: String,
    /// UTC schedule anchor; it need not precede the start.
    pub anchor: String,
    /// Optional exclusive end, strictly after the start.
    pub ends_at: Option<String>,
}

impl TryFrom<RecurrenceConfig> for Recurrence {
    type Error = PaykitError;
    fn try_from(value: RecurrenceConfig) -> Result<Self> {
        let recurrence = Self {
            every: value.every,
            unit: value.unit,
            starts_at: value.starts_at,
            anchor: value.anchor,
            ends_at: value.ends_at,
        };
        recurrence.validate()?;
        Ok(recurrence)
    }
}

impl BillingPeriod {
    /// Construct an ordered Billing Period with RFC3339 UTC `Z` timestamps.
    ///
    /// This validates the interval shape, not membership in a Recurrence.
    pub fn new(starts_at: impl Into<String>, ends_at: impl Into<String>) -> Result<Self> {
        Self::new_with_label(starts_at.into(), ends_at.into(), "Billing Period")
    }

    pub(crate) fn new_with_label(starts_at: String, ends_at: String, label: &str) -> Result<Self> {
        let period = Self { starts_at, ends_at };
        period.validate_with_label(label)?;
        Ok(period)
    }
}

/// Unvalidated builder for immutable Payment Request terms.
#[derive(Clone)]
pub struct PaymentRequestTermsBuilder(PaymentRequestTerms);

impl PaymentRequestTerms {
    /// Start constructing terms with their required amount, reference, and endpoints.
    ///
    /// Call [`PaymentRequestTermsBuilder::build`] to validate the complete terms.
    pub fn builder(
        amount: PaymentAmount,
        payment_reference: PaymentReference,
        accepted_payment_endpoint_identifiers: Vec<PaymentEndpointIdentifier>,
    ) -> PaymentRequestTermsBuilder {
        PaymentRequestTermsBuilder(Self {
            amount,
            payment_reference,
            accepted_payment_endpoint_identifiers,
            payment_endpoints: None,
            required_app_id: None,
            proposal_expires_at: None,
            recurrence: None,
            conversion: None,
            payment_deadline: None,
            metadata: JsonMap::new(),
        })
    }
}

impl PaymentRequestTermsBuilder {
    /// Bind payment to immutable Payment Endpoints owned by `required_app_id`.
    ///
    /// A supplied map must be nonempty, contain nonempty payloads, and use only
    /// accepted Payment Endpoint Identifiers. `required_app_id` must be set.
    /// `None` preserves endpoint discovery; `Some` forbids fallback outside the map.
    pub fn payment_endpoints(
        mut self,
        value: Option<HashMap<PaymentEndpointIdentifier, PaymentEndpointPayload>>,
    ) -> Self {
        self.0.payment_endpoints = value;
        self
    }
    /// Constrain payment to an endpoint owned by the specified payee App.
    pub fn required_app_id(mut self, value: Option<crate::PaykitAppId>) -> Self {
        self.0.required_app_id = value;
        self
    }
    /// Set the optional proposal expiry; past timestamps remain valid history.
    pub fn proposal_expires_at(mut self, value: Option<String>) -> Self {
        self.0.proposal_expires_at = value;
        self
    }
    /// Set a validated recurrence, or `None` for a one-time request.
    pub fn recurrence(mut self, value: Option<Recurrence>) -> Self {
        self.0.recurrence = value;
        self
    }
    /// Set the optional conversion policy.
    pub fn conversion(mut self, value: Option<PaymentConversion>) -> Self {
        self.0.conversion = value;
        self
    }
    /// Set the optional payment deadline.
    pub fn payment_deadline(mut self, value: Option<PaymentDeadline>) -> Self {
        self.0.payment_deadline = value;
        self
    }
    /// Set application-specific metadata without interpreting its contents.
    pub fn metadata(mut self, value: JsonMap<String, JsonValue>) -> Self {
        self.0.metadata = value;
        self
    }
    /// Validate the complete terms, including the non-empty endpoint list.
    pub fn build(self) -> Result<PaymentRequestTerms> {
        self.0.validate()?;
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests;
