use std::fmt;

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    validation::{parse_utc_timestamp, validate_uuid_v4},
    AllowanceId, EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
    PrivateMessageKind, Result,
};

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
            .field(
                "accepted_payment_endpoint_identifiers",
                &self.accepted_payment_endpoint_identifiers,
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
    /// Payment Endpoint Identifier used for the payment execution.
    pub(super) payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// Optional Allowance used for this payment, scoped to the exact Encrypted Link.
    pub(super) allowance_id: Option<AllowanceId>,
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
            payment_endpoint_identifier,
            allowance_id: None,
            proof,
        }
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

    /// Validate this proof against the immutable terms of a specific Payment Request.
    ///
    /// Checks stateless correlation only: request ID, Payment Reference,
    /// Billing Period presence/shape, and accepted endpoint identifier. Caller
    /// state still owns lifecycle, role, dedupe, settlement, recurrence
    /// eligibility, and FX or cross-asset policy.
    pub fn validate_for_request(&self, request: &PaymentRequest) -> Result<()> {
        if self.version != 1 || self.kind != PrivateMessageKind::PaymentProof {
            return Err(PaykitError::Validation(
                "Payment Proof must have version 1 and kind paykit.payment_proof".into(),
            ));
        }
        if request.version != 1 || request.kind != PrivateMessageKind::PaymentRequest {
            return Err(PaykitError::Validation(
                "Payment Request must have version 1 and kind paykit.payment_request".into(),
            ));
        }
        request.request.validate()?;
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
        {
            return Err(PaykitError::Validation(
                "Payment Proof payment_endpoint_identifier is not accepted by Payment Request"
                    .into(),
            ));
        }
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
        }
    }
}

/// A recognized Payment Request protocol Event Message plus the raw JSON
/// payload received from the Encrypted Link.
#[derive(Clone, PartialEq)]
pub struct PaymentRequestEventMessage {
    /// Private Message Kind selected from the message header.
    pub kind: PrivateMessageKind,
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
        if self.accepted_payment_endpoint_identifiers.is_empty() {
            return Err(PaykitError::Validation(
                "accepted_payment_endpoint_identifiers must not be empty".into(),
            ));
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
    /// Access the Payment Endpoint Identifier.
    pub fn payment_endpoint_identifier(&self) -> &PaymentEndpointIdentifier {
        &self.payment_endpoint_identifier
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
            proposal_expires_at: None,
            recurrence: None,
            metadata: JsonMap::new(),
        })
    }
}

impl PaymentRequestTermsBuilder {
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
mod tests {
    use super::*;

    fn request_terms() -> PaymentRequestTerms {
        PaymentRequestTerms {
            amount: PaymentAmount::new("0.001", "btc").unwrap(),
            payment_reference: PaymentReference::new("invoice-2026-0001").unwrap(),
            proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
            recurrence: None,
            accepted_payment_endpoint_identifiers: vec![PaymentEndpointIdentifier::new(
                "btc-lightning-bolt11",
            )
            .unwrap()],
            metadata: JsonMap::new(),
        }
    }

    #[test]
    fn payment_request_terms_reject_empty_endpoint_list() {
        let mut terms = request_terms();
        terms.accepted_payment_endpoint_identifiers.clear();
        let err = terms.validate().unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("must not be empty"))
        );
    }

    #[test]
    fn recurrence_rejects_non_utc_timestamp() {
        let recurrence = Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-06-01T00:00:00+01:00".to_string(),
            anchor: "2026-06-01T00:00:00Z".to_string(),
            ends_at: None,
        };
        let err = recurrence.validate().unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("Z suffix")));
    }

    fn recurrence_with_ends_at(ends_at: Option<&str>) -> Recurrence {
        Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-07-01T00:00:00Z".to_string(),
            anchor: "2026-07-01T00:00:00Z".to_string(),
            ends_at: ends_at.map(str::to_string),
        }
    }

    #[test]
    fn recurrence_rejects_ends_at_before_starts_at() {
        // specs/payment-requests.md: non-null ends_at MUST be after starts_at.
        let recurrence = recurrence_with_ends_at(Some("2026-06-01T00:00:00Z"));
        let err = recurrence.validate().unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
        );
    }

    #[test]
    fn recurrence_rejects_ends_at_equal_to_starts_at() {
        let recurrence = recurrence_with_ends_at(Some("2026-07-01T00:00:00Z"));
        let err = recurrence.validate().unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
        );
    }

    #[test]
    fn recurrence_accepts_ends_at_after_starts_at() {
        let recurrence = recurrence_with_ends_at(Some("2026-08-01T00:00:00Z"));
        recurrence.validate().unwrap();
    }

    #[test]
    fn payment_reference_is_not_required_to_be_uuid() {
        let terms = request_terms();
        assert_eq!(terms.payment_reference.as_str(), "invoice-2026-0001");
    }

    #[test]
    fn payment_request_event_debug_redacts_private_payloads() {
        let request = PaymentRequest::new(
            EventId::new_v4(),
            PaymentRequestId::new_v4(),
            PaymentRequestTerms {
                metadata: JsonMap::from_iter([(
                    "note".to_string(),
                    JsonValue::String("private request note".to_string()),
                )]),
                ..request_terms()
            },
        );
        let proof = PaymentProof::new(
            EventId::new_v4(),
            request.payment_request_id.clone(),
            request.request.payment_reference.clone(),
            None,
            request.request.accepted_payment_endpoint_identifiers[0].clone(),
            JsonMap::from_iter([(
                "preimage".to_string(),
                JsonValue::String("private proof secret".to_string()),
            )]),
        );
        let message = PaymentRequestEventMessage {
            kind: PrivateMessageKind::PaymentProof,
            event_id: Some(proof.event_id.clone()),
            payment_request_id: Some(proof.payment_request_id.clone()),
            raw_json: r#"{"proof":{"preimage":"raw proof secret"}}"#.to_string(),
            event: Ok(PaymentRequestEvent::Proof(proof.clone())),
        };

        assert!(!format!("{request:?}").contains("invoice-2026-0001"));
        assert!(!format!("{proof:?}").contains("invoice-2026-0001"));
        assert!(!format!("{request:?}").contains("private request note"));
        assert!(!format!("{proof:?}").contains("private proof secret"));
        let debug = format!("{message:?}");
        assert!(!debug.contains("raw proof secret"));
        assert!(!debug.contains("private proof secret"));
        assert!(debug.contains("<redacted:"));
    }

    fn payment_request() -> PaymentRequest {
        PaymentRequest::new(
            EventId::new_v4(),
            PaymentRequestId::new_v4(),
            request_terms(),
        )
    }

    fn payment_proof_for(request: &PaymentRequest) -> PaymentProof {
        PaymentProof::new(
            EventId::new_v4(),
            request.payment_request_id.clone(),
            request.request.payment_reference.clone(),
            None,
            request.request.accepted_payment_endpoint_identifiers[0].clone(),
            JsonMap::new(),
        )
    }

    #[test]
    fn payment_proof_validates_for_matching_one_time_request() {
        let request = payment_request();
        let proof = payment_proof_for(&request);
        proof.validate_for_request(&request).unwrap();
    }

    #[test]
    fn payment_proof_rejects_mismatched_reference() {
        let request = payment_request();
        let mut proof = payment_proof_for(&request);
        proof.payment_reference = PaymentReference::new("other-reference").unwrap();
        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("payment_reference"))
        );
    }

    #[test]
    fn payment_proof_rejects_mismatched_request_id() {
        let request = payment_request();
        let mut proof = payment_proof_for(&request);
        proof.payment_request_id = PaymentRequestId::new_v4();
        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("payment_request_id"))
        );
    }

    #[test]
    fn payment_proof_rejects_unaccepted_endpoint() {
        let request = payment_request();
        let mut proof = payment_proof_for(&request);
        proof.payment_endpoint_identifier = PaymentEndpointIdentifier::new("btc-onchain").unwrap();
        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("not accepted")));
    }

    #[test]
    fn payment_proof_rejects_billing_period_for_one_time_request() {
        let request = payment_request();
        let mut proof = payment_proof_for(&request);
        proof.billing_period = Some(BillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            ends_at: "2026-07-01T00:00:00Z".to_string(),
        });
        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("one-time")));
    }

    #[test]
    fn payment_proof_rejects_invalid_billing_period_shape() {
        let mut request = payment_request();
        request.request.recurrence = Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            anchor: "2026-06-01T00:00:00Z".to_string(),
            ends_at: None,
        });
        let mut proof = payment_proof_for(&request);
        proof.billing_period = Some(BillingPeriod {
            starts_at: "2026-07-01T00:00:00Z".to_string(),
            ends_at: "2026-06-01T00:00:00Z".to_string(),
        });

        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
        );
    }

    #[test]
    fn payment_proof_requires_billing_period_for_recurring_request() {
        let mut request = payment_request();
        request.request.recurrence = Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            anchor: "2026-06-01T00:00:00Z".to_string(),
            ends_at: None,
        });
        let proof = payment_proof_for(&request);
        let err = proof.validate_for_request(&request).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("required")));
    }

    #[test]
    fn payment_proof_validates_for_recurring_request_with_billing_period() {
        let mut request = payment_request();
        request.request.recurrence = Some(Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            anchor: "2026-06-01T00:00:00Z".to_string(),
            ends_at: None,
        });
        let mut proof = payment_proof_for(&request);
        proof.billing_period = Some(BillingPeriod {
            starts_at: "2026-06-01T00:00:00Z".to_string(),
            ends_at: "2026-07-01T00:00:00Z".to_string(),
        });
        proof.validate_for_request(&request).unwrap();
    }
}
