use std::fmt;

use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    validation::{parse_utc_timestamp, validate_uuid_v4},
    EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recurrence {
    /// Positive interval count.
    pub every: u32,
    /// Recurrence unit.
    pub unit: RecurrenceUnit,
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`.
    pub anchor: String,
    /// Optional RFC3339 UTC timestamp using `Z`, after `starts_at` when
    /// present.
    pub ends_at: Option<String>,
}

/// Payment Request terms set by the payee.
#[derive(Clone, PartialEq)]
pub struct PaymentRequestTerms {
    /// Requested amount.
    pub amount: PaymentAmount,
    /// Payee-provided correlation value copied into Payment Proof messages.
    pub payment_reference: PaymentReference,
    /// Proposal expiry before acceptance. `None` means no protocol-level
    /// proposal expiry.
    pub proposal_expires_at: Option<String>,
    /// Optional recurrence. `None` means one-time request.
    pub recurrence: Option<Recurrence>,
    /// Accepted Payment Endpoint Identifiers.
    pub accepted_payment_endpoint_identifiers: Vec<PaymentEndpointIdentifier>,
    /// Payee application whose Payment Endpoint must be paid, when constrained.
    pub required_app_id: Option<crate::PaykitAppId>,
    /// Application-specific JSON metadata.
    pub metadata: JsonMap<String, JsonValue>,
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
            .field("required_app_id", &self.required_app_id)
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
/// `BillingPeriod` has no public validating constructor or standalone validator.
/// Direct struct construction and later field mutation are unchecked. Payment
/// Proof serialization and parsing, and Receipt preparation and parsing, require
/// `starts_at` and `ends_at` to be RFC3339 timestamps with a `Z` suffix and
/// `ends_at` to be strictly later than `starts_at`.
///
/// [`serialize_receipt_access_json`](crate::serialize_receipt_access_json) does
/// not validate these fields. Callers must first use
/// [`ReceiptAccess::validate`](crate::ReceiptAccess::validate).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BillingPeriod {
    /// RFC3339 UTC timestamp using `Z`.
    pub starts_at: String,
    /// RFC3339 UTC timestamp using `Z`, after `starts_at`.
    pub ends_at: String,
}

/// `paykit.payment_request` Event Message.
#[derive(Clone, PartialEq)]
pub struct PaymentRequest {
    /// Message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequest`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Stable Payment Request ID.
    pub payment_request_id: PaymentRequestId,
    /// Immutable request terms.
    pub request: PaymentRequestTerms,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestAcceptance {
    /// Message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestAcceptance`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Stable Payment Request ID.
    pub payment_request_id: PaymentRequestId,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestRejection {
    /// Message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestRejection`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Stable Payment Request ID.
    pub payment_request_id: PaymentRequestId,
    /// Optional informational reason.
    pub reason: Option<String>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaymentRequestCancellation {
    /// Message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentRequestCancellation`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Stable Payment Request ID.
    pub payment_request_id: PaymentRequestId,
    /// Optional informational reason.
    pub reason: Option<String>,
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
#[derive(Clone, PartialEq)]
pub struct PaymentProof {
    /// Message version. Currently always `1`.
    pub version: u8,
    /// Private message kind. Currently [`PrivateMessageKind::PaymentProof`].
    pub kind: PrivateMessageKind,
    /// Event ID for idempotent processing.
    pub event_id: EventId,
    /// Stable Payment Request ID.
    pub payment_request_id: PaymentRequestId,
    /// Payment Reference copied from the accepted Payment Request.
    pub payment_reference: PaymentReference,
    /// Billing period. Required for recurring requests, `None` for one-time requests.
    pub billing_period: Option<BillingPeriod>,
    /// Application whose endpoint was used for the payment.
    pub payment_app_id: crate::PaykitAppId,
    /// Payment Endpoint Identifier used for the payment execution.
    pub payment_endpoint_identifier: PaymentEndpointIdentifier,
    /// Method-specific proof object.
    pub proof: JsonMap<String, JsonValue>,
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
            proof,
        }
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
    pub(super) fn validate(&self) -> Result<()> {
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

#[cfg(test)]
mod tests;
