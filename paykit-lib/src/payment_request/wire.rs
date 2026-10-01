use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    shared_wire::{
        deserialize_optional_no_null, BillingPeriodWire, PaymentAmountWire, RequiredNullable,
    },
    validation::{invalid_data, invalid_plaintext_json, invalid_wire, validate_wire_version_kind},
    AllowanceId, EventId, PaykitAppId, PaykitError, PaymentEndpointIdentifier, PaymentReference,
    PrivateMessageKind, Result,
};

use super::{
    types::{
        BillingPeriod, PaymentProof, PaymentRequest, PaymentRequestAcceptance,
        PaymentRequestCancellation, PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
        Recurrence, RecurrenceUnit,
    },
    PaymentConversion, PaymentConversionQuote, PaymentDeadline,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecurrenceWire {
    every: u32,
    unit: String,
    starts_at: String,
    anchor: String,
    ends_at: RequiredNullable<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentRequestTermsWire {
    amount: PaymentAmountWire,
    payment_reference: String,
    proposal_expires_at: RequiredNullable<String>,
    recurrence: RequiredNullable<RecurrenceWire>,
    accepted_payment_endpoint_identifiers: Vec<String>,
    required_app_id: RequiredNullable<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_no_null",
        skip_serializing_if = "Option::is_none"
    )]
    conversion: Option<PaymentConversion>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_no_null",
        skip_serializing_if = "Option::is_none"
    )]
    payment_deadline: Option<PaymentDeadline>,
    #[serde(default)]
    metadata: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentRequestWire {
    version: u8,
    kind: String,
    app_id: String,
    event_id: String,
    payment_request_id: String,
    request: PaymentRequestTermsWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BasicEventWire {
    version: u8,
    kind: String,
    app_id: String,
    event_id: String,
    payment_request_id: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentProofWire {
    version: u8,
    kind: String,
    app_id: String,
    event_id: String,
    payment_request_id: String,
    payment_reference: String,
    billing_period: RequiredNullable<BillingPeriodWire>,
    payment_app_id: String,
    payment_endpoint_identifier: String,
    #[serde(default, deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    allowance_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_no_null",
        skip_serializing_if = "Option::is_none"
    )]
    conversion_quote_id: Option<String>,
    proof: JsonMap<String, JsonValue>,
}

impl TryFrom<PaymentRequestTermsWire> for PaymentRequestTerms {
    type Error = PaykitError;

    fn try_from(wire: PaymentRequestTermsWire) -> Result<Self> {
        let accepted_payment_endpoint_identifiers = wire
            .accepted_payment_endpoint_identifiers
            .into_iter()
            .map(PaymentEndpointIdentifier::new)
            .collect::<Result<Vec<_>>>()?;
        Self::builder(
            wire.amount.try_into_with_label("Payment Request amount")?,
            PaymentReference::new(wire.payment_reference)?,
            accepted_payment_endpoint_identifiers,
        )
        .proposal_expires_at(wire.proposal_expires_at.into_inner())
        .recurrence(
            wire.recurrence
                .into_inner()
                .map(Recurrence::try_from)
                .transpose()?,
        )
        .required_app_id(
            wire.required_app_id
                .into_inner()
                .map(PaykitAppId::new)
                .transpose()?,
        )
        .conversion(wire.conversion)
        .payment_deadline(wire.payment_deadline)
        .metadata(wire.metadata)
        .build()
    }
}

impl From<&PaymentRequestTerms> for PaymentRequestTermsWire {
    fn from(terms: &PaymentRequestTerms) -> Self {
        Self {
            amount: PaymentAmountWire::from(&terms.amount),
            payment_reference: terms.payment_reference.as_str().to_string(),
            proposal_expires_at: RequiredNullable::from(terms.proposal_expires_at.clone()),
            recurrence: RequiredNullable::from(terms.recurrence.as_ref().map(RecurrenceWire::from)),
            accepted_payment_endpoint_identifiers: terms
                .accepted_payment_endpoint_identifiers
                .iter()
                .map(|identifier| identifier.as_str().to_string())
                .collect(),
            required_app_id: RequiredNullable::from(
                terms
                    .required_app_id
                    .as_ref()
                    .map(|app_id| app_id.as_str().to_owned()),
            ),
            conversion: terms.conversion.clone(),
            payment_deadline: terms.payment_deadline.clone(),
            metadata: terms.metadata.clone(),
        }
    }
}

impl TryFrom<RecurrenceWire> for Recurrence {
    type Error = PaykitError;

    fn try_from(wire: RecurrenceWire) -> Result<Self> {
        Self::try_from(crate::RecurrenceConfig {
            every: wire.every,
            unit: RecurrenceUnit::parse(&wire.unit)?,
            starts_at: wire.starts_at,
            anchor: wire.anchor,
            ends_at: wire.ends_at.into_inner(),
        })
    }
}

impl From<&Recurrence> for RecurrenceWire {
    fn from(recurrence: &Recurrence) -> Self {
        Self {
            every: recurrence.every,
            unit: recurrence.unit.as_str().to_string(),
            starts_at: recurrence.starts_at.clone(),
            anchor: recurrence.anchor.clone(),
            ends_at: RequiredNullable::from(recurrence.ends_at.clone()),
        }
    }
}

impl PaymentRequestWire {
    fn from_event(app_id: &PaykitAppId, event: &PaymentRequest) -> Self {
        Self {
            version: event.version,
            kind: event.kind.as_str().to_string(),
            app_id: app_id.as_str().to_string(),
            event_id: event.event_id.as_str().to_string(),
            payment_request_id: event.payment_request_id.as_str().to_string(),
            request: PaymentRequestTermsWire::from(&event.request),
        }
    }
}

impl TryFrom<PaymentRequestWire> for PaymentRequest {
    type Error = PaykitError;

    fn try_from(wire: PaymentRequestWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::PaymentRequest,
            "Payment Request event",
        )?;
        Ok(Self {
            version: 1,
            kind: PrivateMessageKind::PaymentRequest,
            event_id: EventId::new(wire.event_id)?,
            payment_request_id: PaymentRequestId::new(wire.payment_request_id)?,
            request: PaymentRequestTerms::try_from(wire.request)?,
        })
    }
}

impl BasicEventWire {
    fn from_acceptance(app_id: &PaykitAppId, event: &PaymentRequestAcceptance) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            None,
            app_id,
        )
    }

    fn from_rejection(app_id: &PaykitAppId, event: &PaymentRequestRejection) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            event.reason.clone(),
            app_id,
        )
    }

    fn from_cancellation(app_id: &PaykitAppId, event: &PaymentRequestCancellation) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            event.reason.clone(),
            app_id,
        )
    }

    fn new(
        version: u8,
        kind: PrivateMessageKind,
        event_id: &EventId,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
        app_id: &PaykitAppId,
    ) -> Self {
        Self {
            version,
            kind: kind.as_str().to_string(),
            app_id: app_id.as_str().to_string(),
            event_id: event_id.as_str().to_string(),
            payment_request_id: payment_request_id.as_str().to_string(),
            reason,
        }
    }
}

impl PaymentProofWire {
    fn from_event(app_id: &PaykitAppId, event: &PaymentProof) -> Self {
        Self {
            version: event.version,
            kind: event.kind.as_str().to_string(),
            app_id: app_id.as_str().to_string(),
            event_id: event.event_id.as_str().to_string(),
            payment_request_id: event.payment_request_id.as_str().to_string(),
            payment_reference: event.payment_reference.as_str().to_string(),
            billing_period: RequiredNullable::from(
                event.billing_period.as_ref().map(BillingPeriodWire::from),
            ),
            payment_app_id: event.payment_app_id.as_str().to_owned(),
            payment_endpoint_identifier: event.payment_endpoint_identifier.as_str().to_string(),
            allowance_id: event.allowance_id().map(|id| id.as_str().to_owned()),
            conversion_quote_id: event
                .conversion_quote_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            proof: event.proof.clone(),
        }
    }
}

impl TryFrom<PaymentProofWire> for PaymentProof {
    type Error = PaykitError;

    fn try_from(wire: PaymentProofWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::PaymentProof,
            "Payment Request event",
        )?;
        let billing_period = wire
            .billing_period
            .into_inner()
            .map(BillingPeriod::try_from)
            .transpose()?;

        let event = Self {
            version: 1,
            kind: PrivateMessageKind::PaymentProof,
            event_id: EventId::new(wire.event_id)?,
            payment_request_id: PaymentRequestId::new(wire.payment_request_id)?,
            payment_reference: PaymentReference::new(wire.payment_reference)?,
            billing_period,
            payment_app_id: PaykitAppId::new(wire.payment_app_id)?,
            payment_endpoint_identifier: PaymentEndpointIdentifier::new(
                wire.payment_endpoint_identifier,
            )?,
            allowance_id: wire.allowance_id.map(parse_allowance_id).transpose()?,
            conversion_quote_id: wire.conversion_quote_id.map(EventId::new).transpose()?,
            proof: wire.proof,
        };
        event.validate()?;
        Ok(event)
    }
}

fn parse_allowance_id(value: String) -> Result<AllowanceId> {
    let id = AllowanceId::new(&value)?;
    if id.as_str() != value {
        return Err(PaykitError::Validation(
            "Payment Proof allowance_id must use canonical UUID-v4 spelling".into(),
        ));
    }
    Ok(id)
}

pub(super) fn serialize_payment_request_json(
    app_id: &PaykitAppId,
    event: &PaymentRequest,
) -> Result<String> {
    event.validate()?;
    serde_json::to_string(&PaymentRequestWire::from_event(app_id, event)).map_err(|err| {
        invalid_data(
            format!("failed to serialize Payment Request JSON: {err}"),
            Some(err.into()),
        )
    })
}

fn serialize_basic_event_json(
    event: &BasicEventWire,
    expected: PrivateMessageKind,
    label: &'static str,
) -> Result<String> {
    if event.version != 1 || event.kind != expected.as_str() {
        return Err(PaykitError::Validation(format!(
            "{label} must use version 1 and kind {}",
            expected.as_str()
        )));
    }
    serde_json::to_string(event).map_err(|err| {
        invalid_data(
            format!("failed to serialize {label} JSON: {err}"),
            Some(err.into()),
        )
    })
}

pub(super) fn serialize_acceptance_json(
    app_id: &PaykitAppId,
    event: &PaymentRequestAcceptance,
) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from_acceptance(app_id, event),
        PrivateMessageKind::PaymentRequestAcceptance,
        "Payment Request Acceptance",
    )
}

pub(super) fn serialize_rejection_json(
    app_id: &PaykitAppId,
    event: &PaymentRequestRejection,
) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from_rejection(app_id, event),
        PrivateMessageKind::PaymentRequestRejection,
        "Payment Request Rejection",
    )
}

pub(super) fn serialize_cancellation_json(
    app_id: &PaykitAppId,
    event: &PaymentRequestCancellation,
) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from_cancellation(app_id, event),
        PrivateMessageKind::PaymentRequestCancellation,
        "Payment Request Cancellation",
    )
}

pub(super) fn serialize_payment_proof_json(
    app_id: &PaykitAppId,
    event: &PaymentProof,
) -> Result<String> {
    event.validate()?;
    serde_json::to_string(&PaymentProofWire::from_event(app_id, event)).map_err(|err| {
        invalid_data(
            format!("failed to serialize Payment Proof JSON: {err}"),
            Some(err.into()),
        )
    })
}

pub(super) fn parse_payment_request_json(json: &str) -> Result<PaymentRequest> {
    // SECURITY / REDACTION: this parses decrypted private-message plaintext,
    // and serde_json errors can embed document fragments. Keep the context
    // static so field values never reach error chains, logs, or FFI text.
    let wire: PaymentRequestWire = serde_json::from_str(json)
        .map_err(|_| invalid_plaintext_json("failed to parse Payment Request JSON"))?;
    validate_app_id(&wire.app_id, "Payment Request")?;
    PaymentRequest::try_from(wire).map_err(|err| invalid_wire(err, "Payment Request"))
}

pub(super) fn parse_acceptance_json(json: &str) -> Result<PaymentRequestAcceptance> {
    let wire = parse_basic_event_json(json, "failed to parse Payment Request Acceptance JSON")?;
    validate_app_id(&wire.app_id, "Payment Request Acceptance")?;
    validate_wire_version_kind(
        wire.version,
        &wire.kind,
        PrivateMessageKind::PaymentRequestAcceptance,
        "Payment Request event",
    )?;
    if wire.reason.is_some() {
        return Err(invalid_data(
            "Payment Request Acceptance must not include reason".to_string(),
            None,
        ));
    }
    Ok(PaymentRequestAcceptance {
        version: 1,
        kind: PrivateMessageKind::PaymentRequestAcceptance,
        event_id: EventId::new(wire.event_id)
            .map_err(|err| invalid_wire(err, "Payment Request Acceptance"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_wire(err, "Payment Request Acceptance"))?,
    })
}

pub(super) fn parse_rejection_json(json: &str) -> Result<PaymentRequestRejection> {
    let wire = parse_basic_event_json(json, "failed to parse Payment Request Rejection JSON")?;
    validate_app_id(&wire.app_id, "Payment Request Rejection")?;
    validate_wire_version_kind(
        wire.version,
        &wire.kind,
        PrivateMessageKind::PaymentRequestRejection,
        "Payment Request event",
    )?;
    Ok(PaymentRequestRejection {
        version: 1,
        kind: PrivateMessageKind::PaymentRequestRejection,
        event_id: EventId::new(wire.event_id)
            .map_err(|err| invalid_wire(err, "Payment Request Rejection"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_wire(err, "Payment Request Rejection"))?,
        reason: wire.reason,
    })
}

pub(super) fn parse_cancellation_json(json: &str) -> Result<PaymentRequestCancellation> {
    let wire = parse_basic_event_json(json, "failed to parse Payment Request Cancellation JSON")?;
    validate_app_id(&wire.app_id, "Payment Request Cancellation")?;
    validate_wire_version_kind(
        wire.version,
        &wire.kind,
        PrivateMessageKind::PaymentRequestCancellation,
        "Payment Request event",
    )?;
    Ok(PaymentRequestCancellation {
        version: 1,
        kind: PrivateMessageKind::PaymentRequestCancellation,
        event_id: EventId::new(wire.event_id)
            .map_err(|err| invalid_wire(err, "Payment Request Cancellation"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_wire(err, "Payment Request Cancellation"))?,
        reason: wire.reason,
    })
}

pub(super) fn parse_payment_proof_json(json: &str) -> Result<PaymentProof> {
    // SECURITY / REDACTION: see `parse_payment_request_json`; the proof body
    // is decrypted private-message plaintext.
    let wire: PaymentProofWire = serde_json::from_str(json)
        .map_err(|_| invalid_plaintext_json("failed to parse Payment Proof JSON"))?;
    validate_app_id(&wire.app_id, "Payment Proof")?;
    PaymentProof::try_from(wire).map_err(|err| invalid_wire(err, "Payment Proof"))
}

fn validate_app_id(app_id: &str, label: &'static str) -> Result<()> {
    PaykitAppId::new(app_id)
        .map(|_| ())
        .map_err(|err| invalid_data(format!("{label} contains invalid App ID"), Some(err.into())))
}

pub(super) fn parse_event_header(
    json: &str,
) -> (
    Option<PaykitAppId>,
    Option<EventId>,
    Option<PaymentRequestId>,
) {
    let Ok(value) = serde_json::from_str::<JsonValue>(json) else {
        return (None, None, None);
    };
    let app_id = value
        .get("app_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| PaykitAppId::new(value).ok());
    let event_id = value
        .get("event_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| EventId::new(value).ok());
    let payment_request_id = value
        .get("payment_request_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| PaymentRequestId::new(value).ok());
    (app_id, event_id, payment_request_id)
}

fn parse_basic_event_json(json: &str, context: &'static str) -> Result<BasicEventWire> {
    // SECURITY / REDACTION: see `parse_payment_request_json`; these event
    // bodies are decrypted private-message plaintext.
    serde_json::from_str(json).map_err(|_| invalid_plaintext_json(context))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversionQuoteWire {
    version: u8,
    kind: String,
    app_id: String,
    event_id: String,
    payment_request_id: String,
    billing_period: BillingPeriodWire,
    rates: Vec<super::ConversionRate>,
    valid_from: String,
    expires_at: String,
}

impl ConversionQuoteWire {
    fn from_event(app_id: &PaykitAppId, event: &PaymentConversionQuote) -> Self {
        Self {
            version: event.version(),
            kind: event.kind().as_str().to_owned(),
            app_id: app_id.as_str().to_owned(),
            event_id: event.event_id().as_str().to_owned(),
            payment_request_id: event.payment_request_id().as_str().to_owned(),
            billing_period: BillingPeriodWire::from(event.billing_period()),
            rates: event.rates().to_vec(),
            valid_from: event.valid_from().to_owned(),
            expires_at: event.expires_at().to_owned(),
        }
    }
}

impl TryFrom<ConversionQuoteWire> for PaymentConversionQuote {
    type Error = PaykitError;

    fn try_from(wire: ConversionQuoteWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::PaymentConversionQuote,
            "Payment Conversion Quote",
        )?;
        Self::new(
            EventId::new(wire.event_id)?,
            PaymentRequestId::new(wire.payment_request_id)?,
            BillingPeriod::try_from(wire.billing_period)?,
            wire.rates,
            wire.valid_from,
            wire.expires_at,
        )
    }
}

pub(super) fn serialize_conversion_quote_json(
    app_id: &PaykitAppId,
    event: &PaymentConversionQuote,
) -> Result<String> {
    event.validate()?;
    serde_json::to_string(&ConversionQuoteWire::from_event(app_id, event)).map_err(|err| {
        invalid_data(
            "failed to serialize Payment Conversion Quote",
            Some(err.into()),
        )
    })
}

pub(super) fn parse_conversion_quote_json(json: &str) -> Result<PaymentConversionQuote> {
    // SECURITY / REDACTION: decrypted plaintext must not appear in serde error chains.
    let wire: ConversionQuoteWire = serde_json::from_str(json)
        .map_err(|_| invalid_plaintext_json("failed to parse Payment Conversion Quote JSON"))?;
    validate_app_id(&wire.app_id, "Payment Conversion Quote")?;
    PaymentConversionQuote::try_from(wire)
        .map_err(|err| invalid_wire(err, "Payment Conversion Quote"))
}

#[cfg(test)]
mod tests;
