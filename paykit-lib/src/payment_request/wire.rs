use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::{
    shared_wire::{BillingPeriodWire, PaymentAmountWire, RequiredNullable},
    validation::{
        invalid_data, invalid_private_wire, json_error_category, private_message_parse_error,
        private_message_parse_error_with_context, validate_outgoing_version_kind,
        validate_wire_version_kind,
    },
    EventId, PaykitError, PaymentAmount, PaymentEndpointIdentifier, PaymentReference,
    PrivateMessageKind, Result,
};

use super::types::{
    BillingPeriod, PaymentProof, PaymentRequest, PaymentRequestAcceptance,
    PaymentRequestCancellation, PaymentRequestId, PaymentRequestRejection, PaymentRequestTerms,
    Recurrence, RecurrenceUnit,
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
    #[serde(default)]
    metadata: JsonMap<String, JsonValue>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentRequestWire {
    version: u8,
    kind: String,
    event_id: String,
    payment_request_id: String,
    request: PaymentRequestTermsWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BasicEventWire {
    version: u8,
    kind: String,
    event_id: String,
    payment_request_id: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_string_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PaymentProofWire {
    version: u8,
    kind: String,
    event_id: String,
    payment_request_id: String,
    payment_reference: String,
    billing_period: RequiredNullable<BillingPeriodWire>,
    payment_endpoint_identifier: String,
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
        let terms = Self {
            amount: PaymentAmount::from(wire.amount),
            payment_reference: PaymentReference::new(wire.payment_reference)?,
            proposal_expires_at: wire.proposal_expires_at.into_inner(),
            recurrence: wire
                .recurrence
                .into_inner()
                .map(Recurrence::try_from)
                .transpose()?,
            accepted_payment_endpoint_identifiers,
            metadata: wire.metadata,
        };
        terms.validate()?;
        Ok(terms)
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
            metadata: terms.metadata.clone(),
        }
    }
}

impl TryFrom<RecurrenceWire> for Recurrence {
    type Error = PaykitError;

    fn try_from(wire: RecurrenceWire) -> Result<Self> {
        let recurrence = Self {
            every: wire.every,
            unit: RecurrenceUnit::parse(&wire.unit)?,
            starts_at: wire.starts_at,
            anchor: wire.anchor,
            ends_at: wire.ends_at.into_inner(),
        };
        recurrence.validate()?;
        Ok(recurrence)
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

fn deserialize_optional_string_no_null<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = JsonValue::deserialize(deserializer)?;
    match value {
        JsonValue::String(value) => Ok(Some(value)),
        _ => Err(serde::de::Error::custom(
            "reason must be a string when present",
        )),
    }
}

impl From<&PaymentRequest> for PaymentRequestWire {
    fn from(event: &PaymentRequest) -> Self {
        Self {
            version: event.version,
            kind: event.kind.as_str().to_string(),
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

impl From<&PaymentRequestAcceptance> for BasicEventWire {
    fn from(event: &PaymentRequestAcceptance) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            None,
        )
    }
}

impl From<&PaymentRequestRejection> for BasicEventWire {
    fn from(event: &PaymentRequestRejection) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            event.reason.clone(),
        )
    }
}

impl From<&PaymentRequestCancellation> for BasicEventWire {
    fn from(event: &PaymentRequestCancellation) -> Self {
        Self::new(
            event.version,
            event.kind,
            &event.event_id,
            &event.payment_request_id,
            event.reason.clone(),
        )
    }
}

impl BasicEventWire {
    fn new(
        version: u8,
        kind: PrivateMessageKind,
        event_id: &EventId,
        payment_request_id: &PaymentRequestId,
        reason: Option<String>,
    ) -> Self {
        Self {
            version,
            kind: kind.as_str().to_string(),
            event_id: event_id.as_str().to_string(),
            payment_request_id: payment_request_id.as_str().to_string(),
            reason,
        }
    }
}

impl From<&PaymentProof> for PaymentProofWire {
    fn from(event: &PaymentProof) -> Self {
        Self {
            version: event.version,
            kind: event.kind.as_str().to_string(),
            event_id: event.event_id.as_str().to_string(),
            payment_request_id: event.payment_request_id.as_str().to_string(),
            payment_reference: event.payment_reference.as_str().to_string(),
            billing_period: RequiredNullable::from(
                event.billing_period.as_ref().map(BillingPeriodWire::from),
            ),
            payment_endpoint_identifier: event.payment_endpoint_identifier.as_str().to_string(),
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
        let billing_period = wire.billing_period.into_inner().map(BillingPeriod::from);
        if let Some(period) = &billing_period {
            period.validate()?;
        }
        Ok(Self {
            version: 1,
            kind: PrivateMessageKind::PaymentProof,
            event_id: EventId::new(wire.event_id)?,
            payment_request_id: PaymentRequestId::new(wire.payment_request_id)?,
            payment_reference: PaymentReference::new(wire.payment_reference)?,
            billing_period,
            payment_endpoint_identifier: PaymentEndpointIdentifier::new(
                wire.payment_endpoint_identifier,
            )?,
            proof: wire.proof,
        })
    }
}

pub(super) fn serialize_payment_request_json(event: &PaymentRequest) -> Result<String> {
    validate_outgoing_version_kind(
        event.version,
        event.kind,
        PrivateMessageKind::PaymentRequest,
        "Payment Request",
    )?;
    event.request.validate()?;
    // Outbound serialize of locally constructed data: keep the serde source,
    // keep the context static.
    serde_json::to_string(&PaymentRequestWire::from(event))
        .map_err(|err| invalid_data("failed to serialize Payment Request JSON", Some(err.into())))
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
            format!("failed to serialize {label} JSON"),
            Some(err.into()),
        )
    })
}

pub(super) fn serialize_acceptance_json(event: &PaymentRequestAcceptance) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from(event),
        PrivateMessageKind::PaymentRequestAcceptance,
        "Payment Request Acceptance",
    )
}

pub(super) fn serialize_rejection_json(event: &PaymentRequestRejection) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from(event),
        PrivateMessageKind::PaymentRequestRejection,
        "Payment Request Rejection",
    )
}

pub(super) fn serialize_cancellation_json(event: &PaymentRequestCancellation) -> Result<String> {
    serialize_basic_event_json(
        &BasicEventWire::from(event),
        PrivateMessageKind::PaymentRequestCancellation,
        "Payment Request Cancellation",
    )
}

pub(super) fn serialize_payment_proof_json(event: &PaymentProof) -> Result<String> {
    validate_outgoing_version_kind(
        event.version,
        event.kind,
        PrivateMessageKind::PaymentProof,
        "Payment Proof",
    )?;
    if let Some(period) = &event.billing_period {
        period.validate()?;
    }
    // Outbound serialize of locally constructed data: keep the serde source,
    // keep the context static.
    serde_json::to_string(&PaymentProofWire::from(event))
        .map_err(|err| invalid_data("failed to serialize Payment Proof JSON", Some(err.into())))
}

// SECURITY / REDACTION: the parse_* functions below consume decrypted
// private-message plaintext. Their errors carry static contexts plus a typed
// redacted category source only; serde detail and offending field values are
// deliberately dropped because these errors can cross the FFI boundary as
// exception text.
pub(super) fn parse_payment_request_json(json: &str) -> Result<PaymentRequest> {
    let wire: PaymentRequestWire = serde_json::from_str(json).map_err(|err| {
        private_message_parse_error(
            "failed to parse Payment Request JSON",
            json_error_category(&err),
        )
    })?;
    PaymentRequest::try_from(wire).map_err(|err| invalid_private_wire(err, "Payment Request"))
}

pub(super) fn parse_acceptance_json(json: &str) -> Result<PaymentRequestAcceptance> {
    let wire = parse_basic_event_json(json, "Payment Request Acceptance")?;
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
            .map_err(|err| invalid_private_wire(err, "Payment Request Acceptance"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_private_wire(err, "Payment Request Acceptance"))?,
    })
}

pub(super) fn parse_rejection_json(json: &str) -> Result<PaymentRequestRejection> {
    let wire = parse_basic_event_json(json, "Payment Request Rejection")?;
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
            .map_err(|err| invalid_private_wire(err, "Payment Request Rejection"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_private_wire(err, "Payment Request Rejection"))?,
        reason: wire.reason,
    })
}

pub(super) fn parse_cancellation_json(json: &str) -> Result<PaymentRequestCancellation> {
    let wire = parse_basic_event_json(json, "Payment Request Cancellation")?;
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
            .map_err(|err| invalid_private_wire(err, "Payment Request Cancellation"))?,
        payment_request_id: PaymentRequestId::new(wire.payment_request_id)
            .map_err(|err| invalid_private_wire(err, "Payment Request Cancellation"))?,
        reason: wire.reason,
    })
}

pub(super) fn parse_payment_proof_json(json: &str) -> Result<PaymentProof> {
    let wire: PaymentProofWire = serde_json::from_str(json).map_err(|err| {
        private_message_parse_error(
            "failed to parse Payment Proof JSON",
            json_error_category(&err),
        )
    })?;
    PaymentProof::try_from(wire).map_err(|err| invalid_private_wire(err, "Payment Proof"))
}

pub(super) fn parse_event_header_ids(json: &str) -> (Option<EventId>, Option<PaymentRequestId>) {
    let Ok(value) = serde_json::from_str::<JsonValue>(json) else {
        return (None, None);
    };
    let event_id = value
        .get("event_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| EventId::new(value).ok());
    let payment_request_id = value
        .get("payment_request_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| PaymentRequestId::new(value).ok());
    (event_id, payment_request_id)
}

fn parse_basic_event_json(json: &str, label: &'static str) -> Result<BasicEventWire> {
    serde_json::from_str(json).map_err(|err| {
        private_message_parse_error_with_context(
            format!("failed to parse {label} JSON"),
            json_error_category(&err),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PrivateMessageParseCategory;

    /// Assert the redacted parse-error shape: exact static context plus the
    /// expected typed category, with no other error text.
    fn assert_parse_error(
        err: &PaykitError,
        expected_context: &str,
        expected_category: PrivateMessageParseCategory,
    ) {
        match err {
            PaykitError::InvalidData { context, .. } => assert_eq!(context, expected_context),
            other => panic!("expected InvalidData, got: {other}"),
        }
        assert_eq!(
            err.private_message_parse_category(),
            Some(expected_category),
            "unexpected parse category for: {err}"
        );
    }

    fn request_terms() -> PaymentRequestTerms {
        PaymentRequestTerms {
            amount: PaymentAmount {
                value: "0.001".to_string(),
                asset: "btc".to_string(),
            },
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
    fn event_header_ids_are_parsed_independently() {
        let json = r#"{
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
        }"#;

        let (event_id, payment_request_id) = parse_event_header_ids(json);

        assert_eq!(
            event_id.as_ref().map(EventId::as_str),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
        );
        assert_eq!(
            payment_request_id.as_ref().map(PaymentRequestId::as_str),
            Some("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
        );
    }

    #[test]
    fn event_header_ids_keep_payment_request_id_when_event_id_is_missing() {
        let json = r#"{
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"
        }"#;

        let (event_id, payment_request_id) = parse_event_header_ids(json);

        assert!(event_id.is_none());
        assert_eq!(
            payment_request_id.as_ref().map(PaymentRequestId::as_str),
            Some("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33")
        );
    }

    #[test]
    fn event_header_ids_keep_event_id_when_payment_request_id_is_missing() {
        let json = r#"{
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101"
        }"#;

        let (event_id, payment_request_id) = parse_event_header_ids(json);

        assert_eq!(
            event_id.as_ref().map(EventId::as_str),
            Some("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101")
        );
        assert!(payment_request_id.is_none());
    }

    #[test]
    fn payment_request_requires_explicit_nullable_fields() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
    }

    #[test]
    fn payment_request_rejects_unknown_top_level_field() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            },
            "ignored_extra_field": true
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("ignored_extra_field"),
            "offending field name leaked: {err:?}"
        );
    }

    #[test]
    fn payment_request_rejects_unknown_request_field() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {},
                "unexpected": true
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("unexpected"),
            "offending field name leaked: {err:?}"
        );
    }

    #[test]
    fn payment_request_rejects_unknown_amount_field() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc", "currency": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("currency"),
            "offending field name leaked: {err:?}"
        );
    }

    #[test]
    fn payment_request_rejects_non_object_metadata() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": "not-an-object"
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }

    #[test]
    fn payment_request_defaults_omitted_metadata_to_empty_object() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": null,
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"]
            }
        }"#;

        let request = parse_payment_request_json(json).unwrap();
        assert!(request.request.metadata.is_empty());
    }

    #[test]
    fn payment_proof_requires_explicit_billing_period() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": {}
        }"#;

        let err = parse_payment_proof_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Proof JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
    }

    #[test]
    fn payment_proof_rejects_invalid_billing_period_order() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "billing_period": {
                "starts_at": "2026-07-01T00:00:00Z",
                "ends_at": "2026-06-01T00:00:00Z"
            },
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": {}
        }"#;

        let err = parse_payment_proof_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Proof failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("ends_at"),
            "validation detail leaked: {err:?}"
        );
    }

    #[test]
    fn payment_proof_rejects_non_object_proof() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_proof",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d105",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001",
            "billing_period": null,
            "payment_endpoint_identifier": "btc-lightning-bolt11",
            "proof": "not-an-object"
        }"#;

        let err = parse_payment_proof_json(json).unwrap_err();
        assert!(matches!(err, PaykitError::InvalidData { .. }));
    }

    #[test]
    fn payment_request_recurrence_requires_explicit_ends_at() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": {
                    "every": 1,
                    "unit": "month",
                    "starts_at": "2026-06-01T00:00:00Z",
                    "anchor": "2026-06-01T00:00:00Z"
                },
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
    }

    #[test]
    fn payment_request_rejects_invalid_recurrence_window_order() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {
                "amount": { "value": "0.001", "asset": "btc" },
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": null,
                "recurrence": {
                    "every": 1,
                    "unit": "month",
                    "starts_at": "2026-07-01T00:00:00Z",
                    "anchor": "2026-07-01T00:00:00Z",
                    "ends_at": "2026-06-01T00:00:00Z"
                },
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {}
            }
        }"#;

        let err = parse_payment_request_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Request failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("ends_at"),
            "validation detail leaked: {err:?}"
        );
    }

    fn recurrence_with_ends_at(ends_at: &str) -> Recurrence {
        Recurrence {
            every: 1,
            unit: RecurrenceUnit::Month,
            starts_at: "2026-07-01T00:00:00Z".to_string(),
            anchor: "2026-07-01T00:00:00Z".to_string(),
            ends_at: Some(ends_at.to_string()),
        }
    }

    #[test]
    fn payment_request_rejects_outgoing_recurrence_ends_at_before_starts_at() {
        let mut terms = request_terms();
        terms.recurrence = Some(recurrence_with_ends_at("2026-06-01T00:00:00Z"));
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);

        let err = serialize_payment_request_json(&event).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
        );
    }

    #[test]
    fn payment_request_rejects_outgoing_recurrence_ends_at_equal_to_starts_at() {
        let mut terms = request_terms();
        terms.recurrence = Some(recurrence_with_ends_at("2026-07-01T00:00:00Z"));
        let event = PaymentRequest::new(EventId::new_v4(), PaymentRequestId::new_v4(), terms);

        let err = serialize_payment_request_json(&event).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(ref msg) if msg.contains("ends_at must be after starts_at"))
        );
    }

    #[test]
    fn acceptance_reason_is_invalid_when_present() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_acceptance",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": "accepted"
        }"#;

        let err = parse_acceptance_json(json).unwrap_err();
        assert!(
            matches!(err, PaykitError::InvalidData { ref context, .. } if context.contains("must not include reason"))
        );
    }

    #[test]
    fn acceptance_rejects_wrong_kind_payment_reference_field() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_acceptance",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d102",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "payment_reference": "invoice-2026-0001"
        }"#;

        let err = parse_acceptance_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request Acceptance JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("payment_reference"),
            "offending field name leaked: {err:?}"
        );
    }

    #[test]
    fn rejection_reason_null_is_invalid_when_present() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_rejection",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d103",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": null
        }"#;

        let err = parse_rejection_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request Rejection JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
    }

    #[test]
    fn cancellation_reason_null_is_invalid_when_present() {
        let json = r#"{
            "version": 1,
            "kind": "paykit.payment_request_cancellation",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d104",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "reason": null
        }"#;

        let err = parse_cancellation_json(json).unwrap_err();
        assert_parse_error(
            &err,
            "failed to parse Payment Request Cancellation JSON",
            PrivateMessageParseCategory::InvalidStructure,
        );
    }

    #[test]
    fn payment_request_rejects_mutated_outgoing_kind() {
        let mut event = PaymentRequest::new(
            EventId::new_v4(),
            PaymentRequestId::new_v4(),
            request_terms(),
        );
        event.kind = PrivateMessageKind::PaymentProof;

        let err = serialize_payment_request_json(&event).unwrap_err();
        assert!(matches!(err, PaykitError::Validation(ref msg) if msg.contains("kind")));
    }

    /// Build Payment Request JSON that is valid except for the caller-chosen
    /// `proposal_expires_at` and `recurrence` JSON fragments, so each
    /// value-level test below varies exactly one field.
    fn payment_request_json_with(proposal_expires_at: &str, recurrence: &str) -> String {
        format!(
            r#"{{
            "version": 1,
            "kind": "paykit.payment_request",
            "event_id": "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101",
            "payment_request_id": "b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33",
            "request": {{
                "amount": {{ "value": "0.001", "asset": "btc" }},
                "payment_reference": "invoice-2026-0001",
                "proposal_expires_at": {proposal_expires_at},
                "recurrence": {recurrence},
                "accepted_payment_endpoint_identifiers": ["btc-lightning-bolt11"],
                "metadata": {{}}
            }}
        }}"#
        )
    }

    // The four tests below pin the value-level Validation -> InvalidData remap
    // (`invalid_private_wire`) for network-delivered Payment Request JSON: each payload
    // is structurally valid JSON whose failure is in a field value, and the
    // parser must surface `PaykitError::InvalidData` because the data arrived
    // from the network (see CLAUDE.md, Error Handling). The inner validation
    // detail is deliberately redacted (it can echo decrypted field values);
    // only the static label context and the typed InvalidStructure category
    // survive.

    #[test]
    fn test_payment_request_rejects_zero_recurrence_every() {
        let json = payment_request_json_with(
            "null",
            r#"{
                "every": 0,
                "unit": "month",
                "starts_at": "2026-06-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
        );

        let err = parse_payment_request_json(&json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Request failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("Recurrence every"),
            "validation detail leaked: {err:?}"
        );
    }

    #[test]
    fn test_payment_request_rejects_unsupported_recurrence_unit() {
        let json = payment_request_json_with(
            "null",
            r#"{
                "every": 1,
                "unit": "fortnight",
                "starts_at": "2026-06-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
        );

        let err = parse_payment_request_json(&json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Request failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("fortnight"),
            "offending unit value leaked: {err:?}"
        );
    }

    #[test]
    fn test_payment_request_rejects_unparseable_recurrence_timestamp() {
        // Z-suffixed so it passes the UTC-suffix gate and fails inside the
        // chrono RFC3339 parse (month 13 is out of range).
        let json = payment_request_json_with(
            "null",
            r#"{
                "every": 1,
                "unit": "month",
                "starts_at": "2026-13-01T00:00:00Z",
                "anchor": "2026-06-01T00:00:00Z",
                "ends_at": null
            }"#,
        );

        let err = parse_payment_request_json(&json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Request failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("2026-13-01"),
            "offending timestamp leaked: {err:?}"
        );
    }

    #[test]
    fn test_payment_request_rejects_malformed_proposal_expires_at() {
        // Exercises the missing-Z branch of parse_utc_timestamp; the
        // unparseable-recurrence test above covers the chrono branch.
        let json = payment_request_json_with(r#""not-a-timestamp""#, "null");

        let err = parse_payment_request_json(&json).unwrap_err();
        assert_parse_error(
            &err,
            "Payment Request failed structural validation",
            PrivateMessageParseCategory::InvalidStructure,
        );
        assert!(
            !format!("{err} {err:?}").contains("not-a-timestamp"),
            "offending timestamp leaked: {err:?}"
        );
    }

    /// Deterministic wire-level positive guard so the value-level rejection
    /// tests above cannot pass vacuously. Complements the probabilistic
    /// `valid_event_round_trips` proptest and the networked
    /// `recurring_payment_request_and_proof_with_billing_period_round_trip`
    /// test, neither of which pins this seam deterministically offline.
    #[test]
    fn test_payment_request_round_trips_fully_valid_recurrence() {
        let event = PaymentRequest::new(
            EventId::new("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d101").unwrap(),
            PaymentRequestId::new("b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33").unwrap(),
            PaymentRequestTerms {
                proposal_expires_at: Some("2026-06-01T00:00:00Z".to_string()),
                recurrence: Some(Recurrence {
                    every: 3,
                    unit: RecurrenceUnit::Week,
                    starts_at: "2026-06-01T00:00:00Z".to_string(),
                    anchor: "2026-06-01T00:00:00Z".to_string(),
                    ends_at: Some("2026-12-01T00:00:00Z".to_string()),
                }),
                ..request_terms()
            },
        );

        let json = serialize_payment_request_json(&event).unwrap();
        let parsed = parse_payment_request_json(&json).unwrap();
        assert_eq!(parsed, event);
    }
}
