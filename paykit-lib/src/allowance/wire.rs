use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::{
    shared_wire::{deserialize_optional_no_null, RequiredNullable},
    validation::{invalid_plaintext_json, validate_wire_version_kind},
    EventId, PaykitError, PaymentEndpointIdentifier, PrivateMessageKind, Result,
};

use super::types::{
    AllowanceAcceptance, AllowanceAmountRange, AllowanceEnd, AllowanceEvent, AllowanceId,
    AllowancePeriod, AllowancePeriodLimit, AllowancePeriodUnit, AllowanceProposal,
    AllowanceRejection, AllowanceRole, AllowanceTerms,
};

const ALLOWANCE_V1_MESSAGE_MAX_LEN: usize = 1000;
const _: () = assert!(
    pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN >= ALLOWANCE_V1_MESSAGE_MAX_LEN,
    "pubky-noise must fit one complete Allowance V1 message",
);

/// Static label used in redacted version/kind errors.
const WIRE_LABEL: &str = "Allowance Event Message";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmountRangeWire {
    minimum: String,
    maximum: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeriodWire {
    kind: String,
    every: u64,
    unit: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_optional_no_null")]
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeriodLimitWire {
    amount_limit: RequiredNullable<String>,
    payment_count_limit: RequiredNullable<u64>,
    period: PeriodWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TermsWire {
    asset: String,
    per_payment_amount: RequiredNullable<AmountRangeWire>,
    period_limits: Vec<PeriodLimitWire>,
    lifetime_amount_limit: RequiredNullable<String>,
    active_from: RequiredNullable<String>,
    expires_at: RequiredNullable<String>,
    allowed_payment_endpoint_identifiers: RequiredNullable<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalWire {
    version: u8,
    kind: String,
    event_id: String,
    allowance_id: String,
    proposer_role: String,
    terms: TermsWire,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseWire {
    version: u8,
    kind: String,
    event_id: String,
    allowance_id: String,
    proposal_event_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EndWire {
    version: u8,
    kind: String,
    event_id: String,
    allowance_id: String,
    proposal_event_id: String,
    acceptance_event_id: RequiredNullable<String>,
}

impl TryFrom<AmountRangeWire> for AllowanceAmountRange {
    type Error = PaykitError;

    fn try_from(wire: AmountRangeWire) -> Result<Self> {
        Self::new(wire.minimum, wire.maximum)
    }
}

impl From<&AllowanceAmountRange> for AmountRangeWire {
    fn from(range: &AllowanceAmountRange) -> Self {
        Self {
            minimum: range.minimum().to_string(),
            maximum: range.maximum().to_string(),
        }
    }
}

impl TryFrom<PeriodWire> for AllowancePeriod {
    type Error = PaykitError;

    fn try_from(wire: PeriodWire) -> Result<Self> {
        let unit = AllowancePeriodUnit::parse(&wire.unit)?;
        match (wire.kind.as_str(), wire.anchor) {
            ("anchored", Some(anchor)) => Self::anchored(wire.every, unit, anchor),
            ("rolling", None) => Self::rolling(wire.every, unit),
            ("anchored", None) => Err(PaykitError::Validation(
                "anchored Allowance period requires anchor".into(),
            )),
            ("rolling", Some(_)) => Err(PaykitError::Validation(
                "rolling Allowance period must not include anchor".into(),
            )),
            _ => Err(PaykitError::Validation(
                "Allowance period kind is unsupported".into(),
            )),
        }
    }
}

impl From<&AllowancePeriod> for PeriodWire {
    fn from(period: &AllowancePeriod) -> Self {
        Self {
            kind: period.kind().as_str().to_string(),
            every: period.every(),
            unit: period.unit().as_str().to_string(),
            anchor: period.anchor().map(str::to_string),
        }
    }
}

impl TryFrom<PeriodLimitWire> for AllowancePeriodLimit {
    type Error = PaykitError;

    fn try_from(wire: PeriodLimitWire) -> Result<Self> {
        Self::new(
            wire.amount_limit.into_inner(),
            wire.payment_count_limit.into_inner(),
            AllowancePeriod::try_from(wire.period)?,
        )
    }
}

impl From<&AllowancePeriodLimit> for PeriodLimitWire {
    fn from(limit: &AllowancePeriodLimit) -> Self {
        Self {
            amount_limit: RequiredNullable::from(limit.amount_limit().map(str::to_string)),
            payment_count_limit: RequiredNullable::from(limit.payment_count_limit()),
            period: PeriodWire::from(limit.period()),
        }
    }
}

impl TryFrom<TermsWire> for AllowanceTerms {
    type Error = PaykitError;

    fn try_from(wire: TermsWire) -> Result<Self> {
        let endpoints = wire
            .allowed_payment_endpoint_identifiers
            .into_inner()
            .map(|identifiers| {
                identifiers
                    .into_iter()
                    .map(PaymentEndpointIdentifier::new)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let terms = Self {
            asset: wire.asset,
            per_payment_amount: wire
                .per_payment_amount
                .into_inner()
                .map(AllowanceAmountRange::try_from)
                .transpose()?,
            period_limits: wire
                .period_limits
                .into_iter()
                .map(AllowancePeriodLimit::try_from)
                .collect::<Result<Vec<_>>>()?,
            lifetime_amount_limit: wire.lifetime_amount_limit.into_inner(),
            active_from: wire.active_from.into_inner(),
            expires_at: wire.expires_at.into_inner(),
            allowed_payment_endpoint_identifiers: endpoints,
        };
        terms.validate()?;
        Ok(terms)
    }
}

impl From<&AllowanceTerms> for TermsWire {
    fn from(terms: &AllowanceTerms) -> Self {
        Self {
            asset: terms.asset().to_string(),
            per_payment_amount: RequiredNullable::from(
                terms.per_payment_amount().map(AmountRangeWire::from),
            ),
            period_limits: terms
                .period_limits()
                .iter()
                .map(PeriodLimitWire::from)
                .collect(),
            lifetime_amount_limit: RequiredNullable::from(
                terms.lifetime_amount_limit().map(str::to_string),
            ),
            active_from: RequiredNullable::from(terms.active_from().map(str::to_string)),
            expires_at: RequiredNullable::from(terms.expires_at().map(str::to_string)),
            allowed_payment_endpoint_identifiers: RequiredNullable::from(
                terms
                    .allowed_payment_endpoint_identifiers()
                    .map(|identifiers| {
                        identifiers
                            .iter()
                            .map(|identifier| identifier.as_str().to_string())
                            .collect()
                    }),
            ),
        }
    }
}

impl From<&AllowanceProposal> for ProposalWire {
    fn from(event: &AllowanceProposal) -> Self {
        Self {
            version: event.version(),
            kind: event.kind().as_str().to_string(),
            event_id: event.event_id().as_str().to_string(),
            allowance_id: event.allowance_id().as_str().to_string(),
            proposer_role: event.proposer_role().as_str().to_string(),
            terms: TermsWire::from(event.terms()),
        }
    }
}

impl TryFrom<ProposalWire> for AllowanceProposal {
    type Error = PaykitError;

    fn try_from(wire: ProposalWire) -> Result<Self> {
        validate_wire_version_kind(
            wire.version,
            &wire.kind,
            PrivateMessageKind::AllowanceProposal,
            WIRE_LABEL,
        )?;
        Ok(Self::new(
            parse_canonical(wire.event_id, EventId::new)?,
            parse_canonical(wire.allowance_id, AllowanceId::new)?,
            AllowanceRole::parse(&wire.proposer_role)?,
            AllowanceTerms::try_from(wire.terms)?,
        ))
    }
}

impl ResponseWire {
    fn from_parts(
        version: u8,
        kind: PrivateMessageKind,
        event_id: &EventId,
        allowance_id: &AllowanceId,
        proposal_event_id: &EventId,
    ) -> Self {
        Self {
            version,
            kind: kind.as_str().to_string(),
            event_id: event_id.as_str().to_string(),
            allowance_id: allowance_id.as_str().to_string(),
            proposal_event_id: proposal_event_id.as_str().to_string(),
        }
    }
}

impl From<&AllowanceAcceptance> for ResponseWire {
    fn from(event: &AllowanceAcceptance) -> Self {
        Self::from_parts(
            event.version(),
            event.kind(),
            event.event_id(),
            event.allowance_id(),
            event.proposal_event_id(),
        )
    }
}

impl From<&AllowanceRejection> for ResponseWire {
    fn from(event: &AllowanceRejection) -> Self {
        Self::from_parts(
            event.version(),
            event.kind(),
            event.event_id(),
            event.allowance_id(),
            event.proposal_event_id(),
        )
    }
}

impl From<&AllowanceEnd> for EndWire {
    fn from(event: &AllowanceEnd) -> Self {
        Self {
            version: event.version(),
            kind: event.kind().as_str().to_string(),
            event_id: event.event_id().as_str().to_string(),
            allowance_id: event.allowance_id().as_str().to_string(),
            proposal_event_id: event.proposal_event_id().as_str().to_string(),
            acceptance_event_id: RequiredNullable::from(
                event
                    .acceptance_event_id()
                    .map(|id| id.as_str().to_string()),
            ),
        }
    }
}

/// Parse `json` as the Allowance event selected by `kind`, or return `None`
/// when `kind` is not an Allowance kind.
///
/// Routing and dispatch live in this single `match`, which deliberately has no
/// wildcard arm: adding a `PrivateMessageKind` variant fails to compile until
/// it is explicitly routed to a parser or ignored here.
pub(super) fn parse_allowance_json(
    kind: PrivateMessageKind,
    json: &str,
) -> Option<Result<AllowanceEvent>> {
    let parse: fn(&str) -> Result<AllowanceEvent> = match kind {
        PrivateMessageKind::AllowanceProposal => parse_proposal,
        PrivateMessageKind::AllowanceAcceptance => parse_acceptance,
        PrivateMessageKind::AllowanceRejection => parse_rejection,
        PrivateMessageKind::AllowanceEnd => parse_end,
        // Non-Allowance kinds are ignored, producing nothing derived from
        // `json` (decrypted private payload), so there is no error context to
        // leak.
        PrivateMessageKind::PrivatePaymentList
        | PrivateMessageKind::ReceiptAccess
        | PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => return None,
    };
    // SECURITY / REDACTION: `json` is decrypted plaintext. Every structural or
    // validation failure collapses to one fixed error so no field value can
    // leak through error text.
    Some(
        validate_received_size(json)
            .and_then(|()| parse(json))
            .map_err(|_| invalid_allowance_message()),
    )
}

pub(super) fn serialize_allowance_json(event: &AllowanceEvent) -> Result<String> {
    match event {
        AllowanceEvent::Proposal(event) => serialize_proposal_json(event),
        AllowanceEvent::Acceptance(event) => serialize_acceptance_json(event),
        AllowanceEvent::Rejection(event) => serialize_rejection_json(event),
        AllowanceEvent::End(event) => serialize_end_json(event),
    }
}

pub(super) fn serialize_proposal_json(event: &AllowanceProposal) -> Result<String> {
    serialize_wire_json(&ProposalWire::from(event))
}

pub(super) fn serialize_acceptance_json(event: &AllowanceAcceptance) -> Result<String> {
    require_distinct_causal_ids(event.event_id(), event.proposal_event_id(), None)?;
    serialize_wire_json(&ResponseWire::from(event))
}

pub(super) fn serialize_rejection_json(event: &AllowanceRejection) -> Result<String> {
    require_distinct_causal_ids(event.event_id(), event.proposal_event_id(), None)?;
    serialize_wire_json(&ResponseWire::from(event))
}

pub(super) fn serialize_end_json(event: &AllowanceEnd) -> Result<String> {
    require_distinct_causal_ids(
        event.event_id(),
        event.proposal_event_id(),
        event.acceptance_event_id(),
    )?;
    serialize_wire_json(&EndWire::from(event))
}

/// Serialize a wire shape to compact JSON, rejecting a message larger than the
/// single-message `pubky-noise` plaintext limit.
fn serialize_wire_json<W: Serialize>(wire: &W) -> Result<String> {
    let json = serialize_wire_json_unbounded(wire)?;
    if json.len() > ALLOWANCE_V1_MESSAGE_MAX_LEN {
        return Err(PaykitError::Validation(format!(
            "Allowance Event Message exceeds {ALLOWANCE_V1_MESSAGE_MAX_LEN} bytes"
        )));
    }
    Ok(json)
}

fn serialize_wire_json_unbounded<W: Serialize>(wire: &W) -> Result<String> {
    serde_json::to_string(wire)
        .map_err(|_| PaykitError::Validation("failed to serialize Allowance Event Message".into()))
}

/// Best-effort top-level ID extraction for a message that failed typed parsing,
/// so malformed recognized messages can still be correlated and deduped.
pub(super) fn parse_event_header_ids(json: &str) -> (Option<EventId>, Option<AllowanceId>) {
    let Ok(value) = serde_json::from_str::<JsonValue>(json) else {
        return (None, None);
    };
    let event_id = value
        .get("event_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| parse_canonical(value.to_string(), EventId::new).ok());
    let allowance_id = value
        .get("allowance_id")
        .and_then(JsonValue::as_str)
        .and_then(|value| parse_canonical(value.to_string(), AllowanceId::new).ok());
    (event_id, allowance_id)
}

fn parse_proposal(json: &str) -> Result<AllowanceEvent> {
    AllowanceProposal::try_from(parse_wire::<ProposalWire>(json)?).map(AllowanceEvent::Proposal)
}

fn parse_acceptance(json: &str) -> Result<AllowanceEvent> {
    parse_response(
        json,
        PrivateMessageKind::AllowanceAcceptance,
        AllowanceAcceptance::new,
    )
    .map(AllowanceEvent::Acceptance)
}

fn parse_rejection(json: &str) -> Result<AllowanceEvent> {
    parse_response(
        json,
        PrivateMessageKind::AllowanceRejection,
        AllowanceRejection::new,
    )
    .map(AllowanceEvent::Rejection)
}

fn parse_end(json: &str) -> Result<AllowanceEvent> {
    let wire: EndWire = parse_wire(json)?;
    validate_wire_version_kind(
        wire.version,
        &wire.kind,
        PrivateMessageKind::AllowanceEnd,
        WIRE_LABEL,
    )?;
    let event_id = parse_canonical(wire.event_id, EventId::new)?;
    let allowance_id = parse_canonical(wire.allowance_id, AllowanceId::new)?;
    let proposal_event_id = parse_canonical(wire.proposal_event_id, EventId::new)?;
    let acceptance_event_id = wire
        .acceptance_event_id
        .into_inner()
        .map(|id| parse_canonical(id, EventId::new))
        .transpose()?;
    require_distinct_causal_ids(&event_id, &proposal_event_id, acceptance_event_id.as_ref())?;
    Ok(AllowanceEvent::End(AllowanceEnd::new(
        event_id,
        allowance_id,
        proposal_event_id,
        acceptance_event_id,
    )))
}

/// Parse the shared Acceptance/Rejection wire shape into `build`'s event type.
fn parse_response<T>(
    json: &str,
    expected: PrivateMessageKind,
    build: fn(EventId, AllowanceId, EventId) -> T,
) -> Result<T> {
    let wire: ResponseWire = parse_wire(json)?;
    validate_wire_version_kind(wire.version, &wire.kind, expected, WIRE_LABEL)?;
    let event_id = parse_canonical(wire.event_id, EventId::new)?;
    let allowance_id = parse_canonical(wire.allowance_id, AllowanceId::new)?;
    let proposal_event_id = parse_canonical(wire.proposal_event_id, EventId::new)?;
    require_distinct_causal_ids(&event_id, &proposal_event_id, None)?;
    Ok(build(event_id, allowance_id, proposal_event_id))
}

/// Enforce the spec rule that an event's own Event ID and every causal Event
/// ID it references are pairwise distinct.
///
/// The message is a fixed string so the check is safe on both the outbound
/// (caller input) and inbound (decrypted plaintext) paths.
fn require_distinct_causal_ids(
    event_id: &EventId,
    proposal_event_id: &EventId,
    acceptance_event_id: Option<&EventId>,
) -> Result<()> {
    let reused = event_id == proposal_event_id
        || acceptance_event_id.is_some_and(|acceptance_event_id| {
            acceptance_event_id == event_id || acceptance_event_id == proposal_event_id
        });
    if reused {
        return Err(PaykitError::Validation(
            "Allowance causal Event IDs must be pairwise distinct".into(),
        ));
    }
    Ok(())
}

fn parse_wire<W: DeserializeOwned>(json: &str) -> Result<W> {
    serde_json::from_str(json).map_err(|_| invalid_allowance_message())
}

/// Construct an ID with `new` and reject any spelling `new` had to canonicalize.
fn parse_canonical<T: AsRef<str>>(
    value: String,
    new: impl FnOnce(String) -> Result<T>,
) -> Result<T> {
    let id = new(value.clone())?;
    if id.as_ref() != value {
        return Err(PaykitError::Validation(
            "Allowance IDs must use canonical UUID-v4 spelling".into(),
        ));
    }
    Ok(id)
}

fn validate_received_size(json: &str) -> Result<()> {
    if json.len() > ALLOWANCE_V1_MESSAGE_MAX_LEN {
        return Err(invalid_allowance_message());
    }
    Ok(())
}

fn invalid_allowance_message() -> PaykitError {
    invalid_plaintext_json("invalid Allowance Event Message")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowance::{
        test_fixtures::{event_id, proposal_with_terms, ALLOWANCE_ID, EVENT_ID},
        types::AllowanceTermsBuilder,
    };

    /// Parse with a kind this module is known to route.
    fn parse_json(kind: PrivateMessageKind, json: &str) -> Result<AllowanceEvent> {
        parse_allowance_json(kind, json).expect("Allowance kind is routed")
    }

    fn parse_proposal_json(json: &str) -> Result<AllowanceEvent> {
        parse_json(PrivateMessageKind::AllowanceProposal, json)
    }

    fn full_proposal() -> AllowanceEvent {
        let period =
            AllowancePeriod::anchored(1, AllowancePeriodUnit::Month, "2026-01-31T00:00:00Z")
                .unwrap();
        let terms = AllowanceTermsBuilder::new("btc")
            .per_payment_amount(AllowanceAmountRange::new("0.0001", "0.01").unwrap())
            .period_limits(vec![AllowancePeriodLimit::new(
                Some("0.03".into()),
                Some(5),
                period,
            )
            .unwrap()])
            .lifetime_amount_limit("0.10")
            .active_from("2026-06-01T00:00:00Z")
            .expires_at("2027-06-01T00:00:00Z")
            .allowed_payment_endpoint_identifiers(vec![PaymentEndpointIdentifier::new(
                "btc-lightning-bolt12",
            )
            .unwrap()])
            .build()
            .unwrap();
        AllowanceEvent::Proposal(proposal_with_terms(terms))
    }

    #[test]
    fn test_non_allowance_kinds_are_not_routed() {
        assert!(parse_allowance_json(PrivateMessageKind::PaymentRequest, "{}").is_none());
    }

    #[test]
    fn test_all_event_shapes_round_trip() {
        let proposal = full_proposal();
        let acceptance_id = event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202");
        let events = vec![
            proposal,
            AllowanceEvent::Acceptance(AllowanceAcceptance::new(
                acceptance_id.clone(),
                AllowanceId::new(ALLOWANCE_ID).unwrap(),
                event_id(EVENT_ID),
            )),
            AllowanceEvent::Rejection(AllowanceRejection::new(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203"),
                AllowanceId::new(ALLOWANCE_ID).unwrap(),
                event_id(EVENT_ID),
            )),
            AllowanceEvent::End(AllowanceEnd::withdrawal(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
                AllowanceId::new(ALLOWANCE_ID).unwrap(),
                event_id(EVENT_ID),
            )),
            AllowanceEvent::End(AllowanceEnd::accepted(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d205"),
                AllowanceId::new(ALLOWANCE_ID).unwrap(),
                event_id(EVENT_ID),
                acceptance_id,
            )),
        ];

        for event in events {
            let json = serialize_allowance_json(&event).unwrap();
            let parsed = parse_json(event.kind(), &json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn test_noncompact_json_parses_and_required_closed_fields_are_enforced() {
        let compact = serialize_allowance_json(&full_proposal()).unwrap();
        let value: JsonValue = serde_json::from_str(&compact).unwrap();
        let pretty = serde_json::to_string_pretty(&value).unwrap();
        assert_eq!(parse_proposal_json(&pretty).unwrap(), full_proposal());

        let mut missing_value = value.clone();
        missing_value
            .get_mut("terms")
            .and_then(JsonValue::as_object_mut)
            .unwrap()
            .remove("expires_at");
        let missing = serde_json::to_string(&missing_value).unwrap();
        assert!(parse_proposal_json(&missing).is_err());
        let unknown = compact.replacen("{", "{\"unknown\":true,", 1);
        assert!(parse_proposal_json(&unknown).is_err());
        let duplicate = compact.replacen("{", "{\"version\":1,", 1);
        assert!(parse_proposal_json(&duplicate).is_err());

        let mut nested_unknowns = Vec::new();
        for pointer in [
            "/terms",
            "/terms/per_payment_amount",
            "/terms/period_limits/0",
            "/terms/period_limits/0/period",
        ] {
            let mut invalid = value.clone();
            invalid
                .pointer_mut(pointer)
                .and_then(JsonValue::as_object_mut)
                .unwrap()
                .insert("unknown".into(), JsonValue::Bool(true));
            nested_unknowns.push(invalid);
        }

        let mut missing_period_nullable = value.clone();
        missing_period_nullable["terms"]["period_limits"][0]
            .as_object_mut()
            .unwrap()
            .remove("amount_limit");
        nested_unknowns.push(missing_period_nullable);

        let mut rolling_null_anchor = value.clone();
        rolling_null_anchor["terms"]["period_limits"][0]["period"]["kind"] =
            JsonValue::from("rolling");
        rolling_null_anchor["terms"]["period_limits"][0]["period"]["unit"] = JsonValue::from("day");
        rolling_null_anchor["terms"]["period_limits"][0]["period"]["anchor"] = JsonValue::Null;
        nested_unknowns.push(rolling_null_anchor);

        let mut unsupported_version = value;
        unsupported_version["version"] = JsonValue::from(2);
        nested_unknowns.push(unsupported_version);

        for invalid in nested_unknowns {
            assert!(parse_proposal_json(&serde_json::to_string(&invalid).unwrap()).is_err());
        }

        let end = AllowanceEvent::End(AllowanceEnd::withdrawal(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d204"),
            AllowanceId::new(ALLOWANCE_ID).unwrap(),
            event_id(EVENT_ID),
        ));
        let mut end_value: JsonValue =
            serde_json::from_str(&serialize_allowance_json(&end).unwrap()).unwrap();
        end_value
            .as_object_mut()
            .unwrap()
            .remove("acceptance_event_id");
        assert!(parse_json(
            PrivateMessageKind::AllowanceEnd,
            &serde_json::to_string(&end_value).unwrap(),
        )
        .is_err());
    }

    #[test]
    fn test_wire_rejects_noncanonical_uuid_spelling() {
        let json = serialize_allowance_json(&full_proposal()).unwrap();
        let uppercase = json.replace(EVENT_ID, &EVENT_ID.to_uppercase());
        assert!(parse_proposal_json(&uppercase).is_err());
        let simple = json.replace(EVENT_ID, &EVENT_ID.replace('-', ""));
        assert!(parse_proposal_json(&simple).is_err());
        let uppercase_allowance_id = json.replace(ALLOWANCE_ID, &ALLOWANCE_ID.to_uppercase());
        assert!(parse_proposal_json(&uppercase_allowance_id).is_err());
    }

    /// Valid End for accepted authority with the fixture IDs.
    fn accepted_end() -> AllowanceEvent {
        AllowanceEvent::End(AllowanceEnd::accepted(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d205"),
            AllowanceId::new(ALLOWANCE_ID).unwrap(),
            event_id(EVENT_ID),
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202"),
        ))
    }

    /// Re-serialize `event` with the `target` field copied from `source`.
    fn json_with_reused_id(event: &AllowanceEvent, target: &str, source: &str) -> String {
        let mut value: JsonValue =
            serde_json::from_str(&serialize_allowance_json(event).unwrap()).unwrap();
        value[target] = value[source].clone();
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn test_wire_rejects_reused_causal_event_ids() {
        let acceptance = AllowanceEvent::Acceptance(AllowanceAcceptance::new(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202"),
            AllowanceId::new(ALLOWANCE_ID).unwrap(),
            event_id(EVENT_ID),
        ));
        let rejection = AllowanceEvent::Rejection(AllowanceRejection::new(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d203"),
            AllowanceId::new(ALLOWANCE_ID).unwrap(),
            event_id(EVENT_ID),
        ));
        let end = accepted_end();

        for (event, target, source) in [
            (&acceptance, "proposal_event_id", "event_id"),
            (&rejection, "proposal_event_id", "event_id"),
            (&end, "proposal_event_id", "event_id"),
            (&end, "acceptance_event_id", "event_id"),
            (&end, "acceptance_event_id", "proposal_event_id"),
        ] {
            let raw = json_with_reused_id(event, target, source);
            assert!(matches!(
                parse_json(event.kind(), &raw),
                Err(PaykitError::InvalidData { .. })
            ));
        }
    }

    #[test]
    fn test_serialize_rejects_reused_causal_event_ids() {
        let allowance_id = AllowanceId::new(ALLOWANCE_ID).unwrap();
        let proposal_event_id = event_id(EVENT_ID);
        let acceptance_event_id = event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202");

        for event in [
            AllowanceEvent::Acceptance(AllowanceAcceptance::new(
                proposal_event_id.clone(),
                allowance_id.clone(),
                proposal_event_id.clone(),
            )),
            AllowanceEvent::Rejection(AllowanceRejection::new(
                proposal_event_id.clone(),
                allowance_id.clone(),
                proposal_event_id.clone(),
            )),
            AllowanceEvent::End(AllowanceEnd::withdrawal(
                proposal_event_id.clone(),
                allowance_id.clone(),
                proposal_event_id.clone(),
            )),
            AllowanceEvent::End(AllowanceEnd::accepted(
                acceptance_event_id.clone(),
                allowance_id.clone(),
                proposal_event_id.clone(),
                acceptance_event_id.clone(),
            )),
            AllowanceEvent::End(AllowanceEnd::accepted(
                event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d205"),
                allowance_id.clone(),
                acceptance_event_id.clone(),
                acceptance_event_id.clone(),
            )),
        ] {
            assert!(matches!(
                serialize_allowance_json(&event),
                Err(PaykitError::Validation(_))
            ));
        }
        assert!(serialize_allowance_json(&accepted_end()).is_ok());
    }

    #[test]
    fn test_wire_rejects_invalid_response_shapes() {
        let acceptance = AllowanceEvent::Acceptance(AllowanceAcceptance::new(
            event_id("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202"),
            AllowanceId::new(ALLOWANCE_ID).unwrap(),
            event_id(EVENT_ID),
        ));
        let json = serialize_allowance_json(&acceptance).unwrap();

        // JSON kind says acceptance but the message was routed as a rejection.
        assert!(parse_json(PrivateMessageKind::AllowanceRejection, &json).is_err());

        // Missing required proposal_event_id on both response kinds.
        let mut value: JsonValue = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("proposal_event_id");
        let missing = serde_json::to_string(&value).unwrap();
        assert!(parse_json(PrivateMessageKind::AllowanceAcceptance, &missing).is_err());
        let rejection = missing.replace(
            PrivateMessageKind::AllowanceAcceptance.as_str(),
            PrivateMessageKind::AllowanceRejection.as_str(),
        );
        assert!(parse_json(PrivateMessageKind::AllowanceRejection, &rejection).is_err());
    }

    #[test]
    fn test_wire_rejects_invalid_term_boundaries() {
        let json = serialize_allowance_json(&full_proposal()).unwrap();
        let value: JsonValue = serde_json::from_str(&json).unwrap();

        let mut reversed_range = value.clone();
        reversed_range["terms"]["per_payment_amount"]["minimum"] = JsonValue::from("2");
        reversed_range["terms"]["per_payment_amount"]["maximum"] = JsonValue::from("1");

        let mut empty_period_limit = value.clone();
        empty_period_limit["terms"]["period_limits"][0]["amount_limit"] = JsonValue::Null;
        empty_period_limit["terms"]["period_limits"][0]["payment_count_limit"] = JsonValue::Null;

        let mut rolling_month = value.clone();
        rolling_month["terms"]["period_limits"][0]["period"]["kind"] = JsonValue::from("rolling");
        rolling_month["terms"]["period_limits"][0]["period"]["unit"] = JsonValue::from("month");
        rolling_month["terms"]["period_limits"][0]["period"]
            .as_object_mut()
            .unwrap()
            .remove("anchor");

        let mut reversed_time = value.clone();
        reversed_time["terms"]["active_from"] = JsonValue::from("2027-01-01T00:00:00Z");
        reversed_time["terms"]["expires_at"] = JsonValue::from("2026-01-01T00:00:00Z");

        let mut empty_allowlist = value;
        empty_allowlist["terms"]["allowed_payment_endpoint_identifiers"] =
            JsonValue::Array(Vec::new());

        for invalid in [
            reversed_range,
            empty_period_limit,
            rolling_month,
            reversed_time,
            empty_allowlist,
        ] {
            let raw = serde_json::to_string(&invalid).unwrap();
            assert!(matches!(
                parse_proposal_json(&raw),
                Err(PaykitError::InvalidData { .. })
            ));
        }
    }

    #[test]
    fn test_wire_accepts_u64_count_boundaries_and_distinct_decimal_spellings() {
        let period = AllowancePeriod::rolling(1, AllowancePeriodUnit::Day).unwrap();
        for count in [0, u64::MAX] {
            let terms = AllowanceTerms::builder("btc")
                .period_limits(vec![AllowancePeriodLimit::new(
                    None,
                    Some(count),
                    period.clone(),
                )
                .unwrap()])
                .build()
                .unwrap();
            let event = AllowanceEvent::Proposal(proposal_with_terms(terms));
            let raw = serialize_allowance_json(&event).unwrap();
            assert_eq!(parse_proposal_json(&raw).unwrap(), event);
        }

        let terms = AllowanceTerms::builder("btc")
            .period_limits(vec![
                AllowancePeriodLimit::new(Some("1".into()), None, period.clone()).unwrap(),
                AllowancePeriodLimit::new(Some("1.0".into()), None, period).unwrap(),
            ])
            .build()
            .unwrap();
        assert_eq!(terms.period_limits().len(), 2);
    }

    #[test]
    fn test_message_size_accepts_1000_bytes_and_rejects_1001() {
        fn proposal_with_lifetime_digits(digits: usize) -> AllowanceProposal {
            proposal_with_terms(
                AllowanceTerms::builder("btc")
                    .lifetime_amount_limit("1".repeat(digits))
                    .build()
                    .unwrap(),
            )
        }
        fn unbounded_json(proposal: &AllowanceProposal) -> String {
            serialize_wire_json_unbounded(&ProposalWire::from(proposal)).unwrap()
        }

        let base = unbounded_json(&proposal_with_lifetime_digits(1));
        let exact = proposal_with_lifetime_digits(1 + ALLOWANCE_V1_MESSAGE_MAX_LEN - base.len());
        let exact_json = serialize_proposal_json(&exact).unwrap();
        assert_eq!(exact_json.len(), ALLOWANCE_V1_MESSAGE_MAX_LEN);
        assert!(parse_proposal_json(&exact_json).is_ok());

        let too_large =
            proposal_with_lifetime_digits(2 + ALLOWANCE_V1_MESSAGE_MAX_LEN - base.len());
        let raw = unbounded_json(&too_large);
        assert_eq!(raw.len(), ALLOWANCE_V1_MESSAGE_MAX_LEN + 1);
        assert!(serialize_proposal_json(&too_large).is_err());
        assert!(parse_proposal_json(&raw).is_err());

        let multibyte = proposal_with_terms(
            AllowanceTerms::builder("\u{e9}".repeat(450))
                .lifetime_amount_limit("1")
                .build()
                .unwrap(),
        );
        let multibyte_raw = unbounded_json(&multibyte);
        assert!(multibyte_raw.chars().count() <= ALLOWANCE_V1_MESSAGE_MAX_LEN);
        assert!(multibyte_raw.len() > ALLOWANCE_V1_MESSAGE_MAX_LEN);
        assert!(serialize_proposal_json(&multibyte).is_err());
        assert!(parse_proposal_json(&multibyte_raw).is_err());
    }

    #[test]
    fn test_invalid_error_does_not_expose_plaintext() {
        let sentinel = "SENTINEL_PRIVATE_TERMS";
        let raw = format!(
            "{{\"version\":1,\"kind\":\"paykit.allowance_proposal\",\"event_id\":\"{EVENT_ID}\",\"allowance_id\":\"{ALLOWANCE_ID}\",\"proposer_role\":\"allower\",\"terms\":\"{sentinel}\"}}"
        );
        let error = parse_proposal_json(&raw).unwrap_err();
        assert!(!error.to_string().contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));
    }
}
