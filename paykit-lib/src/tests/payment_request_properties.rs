//! Property-based tests for the Payment Request wire format.
//!
//! These are the workspace's first `proptest` properties. They target the
//! hand-rolled parse/serialize logic in `payment_request::wire` and the public
//! `parse_payment_request_event_message` entry point, which is the reachable
//! surface for untrusted, network-delivered plaintext. `parse_payment_request_json`
//! itself is `pub(super)` and unreachable from here, so it is exercised
//! indirectly through the public parser.
//!
//! Two properties are covered:
//!
//! * Constrained-valid round-trip: generate construction-valid
//!   `PaymentRequestEvent` values, serialize them, parse them back, and assert
//!   structural equality.
//! * Never-panic: feed arbitrary strings and arbitrary JSON into the public
//!   parser and assert it returns Some/None without panicking. This also
//!   regression-guards the `unreachable!()` invariant in
//!   `payment_request::api::parse_event`, which is only reachable for kinds
//!   accepted by `PrivateMessageKind::is_payment_request_event`.
//!
//! Case count: each property runs 256 cases by default. Override with the
//! standard proptest knob, e.g.
//! `PROPTEST_CASES=2048 cargo test -p paykit-lib --lib payment_request_properties`.
//!
//! Adding strategies for future wire types: write a strategy that emits only
//! construction-valid typed values (build the domain type, not raw JSON), then
//! assert `parse(serialize(value)) == value`. Pair it with a never-panic
//! property that pushes arbitrary bytes/JSON through the public parser. Keep the
//! valid-value strategies narrow at first (see the NARROWER notes below) and
//! widen them as the parser's guarantees are pinned down.

use proptest::prelude::*;
use serde_json::{Map as JsonMap, Value as JsonValue};

use super::*;

/// Canonical Payment Request event kind strings routed by the public parser.
const PAYMENT_REQUEST_EVENT_KINDS: &[&str] = &[
    "paykit.payment_request",
    "paykit.payment_request_acceptance",
    "paykit.payment_request_rejection",
    "paykit.payment_request_cancellation",
    "paykit.payment_proof",
];

/// Every `PrivateMessageKind` variant, listed once.
///
/// This is the fixed input for
/// `test_payment_request_routing_covers_all_private_message_kinds`, which pushes
/// each kind through the public Payment Request parser. It intentionally spans
/// the whole enum, not just the routed kinds in `PAYMENT_REQUEST_EVENT_KINDS`,
/// so a kind with no `parse_event` arm is still exercised against the
/// `unreachable!()` invariant. Keep it in sync with
/// `payment_request_routing_expectation`; the length cross-check in that test
/// fails if a routed kind is added here without updating
/// `PAYMENT_REQUEST_EVENT_KINDS`.
const ALL_PRIVATE_MESSAGE_KINDS: &[PrivateMessageKind] = &[
    PrivateMessageKind::PrivatePaymentList,
    PrivateMessageKind::ReceiptAccess,
    PrivateMessageKind::PaymentRequest,
    PrivateMessageKind::PaymentRequestAcceptance,
    PrivateMessageKind::PaymentRequestRejection,
    PrivateMessageKind::PaymentRequestCancellation,
    PrivateMessageKind::PaymentProof,
];

/// Hand-maintained spec for which private message kinds the Payment Request
/// parser is expected to route (return `Some`) versus ignore (return `None`).
///
/// Compile-time exhaustiveness guard: this match has no wildcard arm, so adding
/// a `PrivateMessageKind` variant fails to compile here until the new variant is
/// deliberately classified. A variant classified `true` must also gain a
/// `parse_event` arm in `payment_request::api`; otherwise the dispatcher reaches
/// `unreachable!()` at runtime, a panic
/// `test_payment_request_routing_covers_all_private_message_kinds` then catches.
fn payment_request_routing_expectation(kind: PrivateMessageKind) -> bool {
    match kind {
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => true,
        PrivateMessageKind::PrivatePaymentList | PrivateMessageKind::ReceiptAccess => false,
    }
}

// ---- strategies emitting construction-valid values ----------------------

/// Build a valid RFC3339 UTC timestamp (always `Z`-suffixed).
///
/// NARROWER THAN THE DOMAIN: day is capped at 28 and no fractional seconds are
/// generated, so every emitted value parses. The validator accepts a wider
/// RFC3339 grammar.
fn utc_timestamp() -> impl Strategy<Value = String> {
    (
        2000u32..2100,
        1u32..=12,
        1u32..=28,
        0u32..=23,
        0u32..=59,
        0u32..=59,
    )
        .prop_map(|(y, mo, d, h, mi, s)| format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z"))
}

/// Build a valid UUID-v4 string from 16 proptest-controlled bytes.
///
/// `Builder::from_random_bytes` pins version 4 and the RFC4122 variant, so
/// `validate_uuid_v4` always accepts the result.
fn uuid_v4() -> impl Strategy<Value = String> {
    any::<[u8; 16]>().prop_map(|bytes| {
        uuid::Builder::from_random_bytes(bytes)
            .into_uuid()
            .to_string()
    })
}

fn event_id() -> impl Strategy<Value = EventId> {
    uuid_v4().prop_map(|id| EventId::new(id).expect("uuid v4 is a valid Event ID"))
}

fn payment_request_id() -> impl Strategy<Value = PaymentRequestId> {
    uuid_v4()
        .prop_map(|id| PaymentRequestId::new(id).expect("uuid v4 is a valid Payment Request ID"))
}

/// Build a valid `PaymentAmount`.
///
/// NARROWER THAN THE DOMAIN: values are ASCII decimals and assets are short
/// alphanumeric codes; the validator also accepts other non-control assets.
fn payment_amount() -> impl Strategy<Value = PaymentAmount> {
    ("[0-9]{1,12}(\\.[0-9]{1,8})?", "[a-zA-Z0-9]{1,10}")
        .prop_map(|(value, asset)| PaymentAmount { value, asset })
}

/// Build a valid `PaymentReference`.
///
/// NARROWER THAN THE DOMAIN: printable ASCII only; the validator accepts any
/// non-control string up to 256 characters.
fn payment_reference() -> impl Strategy<Value = PaymentReference> {
    "[a-zA-Z0-9 ._:/-]{1,32}"
        .prop_map(|s| PaymentReference::new(s).expect("generated reference is valid"))
}

/// Build a valid `PaymentEndpointIdentifier`.
///
/// The `ep-` prefix guarantees the value never collides with a reserved
/// identifier (`private`, `encrypted-link-recovery`) and is never a pure-dot
/// path-traversal component.
fn payment_endpoint_identifier() -> impl Strategy<Value = PaymentEndpointIdentifier> {
    "ep-[a-zA-Z0-9]{1,12}(-[a-zA-Z0-9]{1,8}){0,2}"
        .prop_map(|s| PaymentEndpointIdentifier::new(s).expect("generated identifier is valid"))
}

fn recurrence_unit() -> impl Strategy<Value = RecurrenceUnit> {
    prop_oneof![
        Just(RecurrenceUnit::Minute),
        Just(RecurrenceUnit::Hour),
        Just(RecurrenceUnit::Day),
        Just(RecurrenceUnit::Week),
        Just(RecurrenceUnit::Month),
        Just(RecurrenceUnit::Year),
    ]
}

/// Build a valid `Recurrence`.
///
/// NARROWER THAN THE DOMAIN: `every` is capped at 1_000; the type allows any
/// positive `u32`.
fn recurrence() -> impl Strategy<Value = Recurrence> {
    (
        1u32..=1000,
        recurrence_unit(),
        utc_timestamp(),
        utc_timestamp(),
        proptest::option::of(utc_timestamp()),
    )
        .prop_map(|(every, unit, starts_at, anchor, ends_at)| Recurrence {
            every,
            unit,
            starts_at,
            anchor,
            ends_at,
        })
}

/// Build valid metadata / proof objects.
///
/// NARROWER THAN THE DOMAIN: string to string only; these maps accept arbitrary
/// JSON values.
fn json_string_map() -> impl Strategy<Value = JsonMap<String, JsonValue>> {
    proptest::collection::hash_map("[a-zA-Z0-9_]{1,12}", "[a-zA-Z0-9 ]{0,16}", 0..4).prop_map(
        |entries| {
            entries
                .into_iter()
                .map(|(k, v)| (k, JsonValue::String(v)))
                .collect()
        },
    )
}

fn payment_request_terms() -> impl Strategy<Value = PaymentRequestTerms> {
    (
        payment_amount(),
        payment_reference(),
        proptest::option::of(utc_timestamp()),
        proptest::option::of(recurrence()),
        proptest::collection::vec(payment_endpoint_identifier(), 1..=4),
        json_string_map(),
    )
        .prop_map(
            |(amount, payment_reference, proposal_expires_at, recurrence, ids, metadata)| {
                PaymentRequestTerms {
                    amount,
                    payment_reference,
                    proposal_expires_at,
                    recurrence,
                    accepted_payment_endpoint_identifiers: ids,
                    metadata,
                }
            },
        )
}

fn payment_request() -> impl Strategy<Value = PaymentRequest> {
    (event_id(), payment_request_id(), payment_request_terms())
        .prop_map(|(event_id, id, terms)| PaymentRequest::new(event_id, id, terms))
}

/// Build a valid `BillingPeriod` whose `ends_at` is strictly after `starts_at`.
fn billing_period() -> impl Strategy<Value = BillingPeriod> {
    (2000u32..2050, 1u32..=12, 1u32..=28).prop_map(|(y, mo, d)| BillingPeriod {
        starts_at: format!("{y:04}-{mo:02}-{d:02}T00:00:00Z"),
        ends_at: format!("{:04}-{mo:02}-{d:02}T00:00:00Z", y + 1),
    })
}

fn payment_proof() -> impl Strategy<Value = PaymentProof> {
    (
        event_id(),
        payment_request_id(),
        payment_reference(),
        proptest::option::of(billing_period()),
        payment_endpoint_identifier(),
        json_string_map(),
    )
        .prop_map(
            |(event_id, id, reference, billing_period, endpoint, proof)| {
                PaymentProof::new(event_id, id, reference, billing_period, endpoint, proof)
            },
        )
}

fn optional_reason() -> impl Strategy<Value = Option<String>> {
    proptest::option::of("[a-zA-Z0-9 _-]{1,24}")
}

/// Build any construction-valid `PaymentRequestEvent`.
fn payment_request_event() -> impl Strategy<Value = PaymentRequestEvent> {
    prop_oneof![
        payment_request().prop_map(PaymentRequestEvent::Request),
        (event_id(), payment_request_id()).prop_map(|(e, p)| PaymentRequestEvent::Acceptance(
            PaymentRequestAcceptance::new(e, p)
        )),
        (event_id(), payment_request_id(), optional_reason()).prop_map(|(e, p, r)| {
            PaymentRequestEvent::Rejection(PaymentRequestRejection::new(e, p, r))
        }),
        (event_id(), payment_request_id(), optional_reason()).prop_map(|(e, p, r)| {
            PaymentRequestEvent::Cancellation(PaymentRequestCancellation::new(e, p, r))
        }),
        payment_proof().prop_map(PaymentRequestEvent::Proof),
    ]
}

// ---- strategies for the never-panic properties --------------------------

/// A small, bounded arbitrary JSON value strategy.
fn arb_json() -> impl Strategy<Value = JsonValue> {
    let leaf = prop_oneof![
        Just(JsonValue::Null),
        any::<bool>().prop_map(JsonValue::Bool),
        any::<i64>().prop_map(|n| JsonValue::Number(n.into())),
        ".*".prop_map(JsonValue::String),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(JsonValue::Array),
            proptest::collection::hash_map("[a-zA-Z0-9_]{0,12}", inner, 0..6)
                .prop_map(|m| JsonValue::Object(m.into_iter().collect())),
        ]
    })
}

/// Build a JSON object carrying a recognized Payment Request `kind` so the
/// parser routes past `known_kind` into the per-kind parsers (exercising the
/// deepest code paths, including the `unreachable!()` invariant).
fn arb_kinded_json() -> impl Strategy<Value = String> {
    (
        prop::sample::select(PAYMENT_REQUEST_EVENT_KINDS),
        arb_json(),
    )
        .prop_map(|(kind, body)| {
            let mut object = match body {
                JsonValue::Object(map) => map,
                other => {
                    let mut map = JsonMap::new();
                    map.insert("body".to_string(), other);
                    map
                }
            };
            object.insert("kind".to_string(), JsonValue::String(kind.to_string()));
            JsonValue::Object(object).to_string()
        })
}

/// Push raw plaintext through the public parser and touch every accessor and
/// Debug path, so a panic anywhere downstream fails the property.
fn exercise(raw_json: String) {
    let message = PrivateApplicationMessage {
        version: None,
        kind: None,
        raw_json,
    };
    let _ = format!("{message:?}");
    if let Some(parsed) = parse_payment_request_event_message(&message) {
        let _ = parsed.is_valid();
        let _ = parsed.kind();
        let _ = parsed.event_id();
        let _ = parsed.payment_request_id();
        let _ = parsed.validation_error();
        let _ = parsed.parsed_event();
        let _ = format!("{parsed:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Every construction-valid event survives serialize -> parse unchanged.
    #[test]
    fn valid_event_round_trips(event in payment_request_event()) {
        let serialized = serialize_payment_request_event(&event)
            .expect("construction-valid events must serialize");
        let message = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(event.kind().as_str().to_string()),
            raw_json: serialized,
        };
        let parsed = parse_payment_request_event_message(&message)
            .expect("a recognized Payment Request kind must be routed");
        prop_assert!(parsed.is_valid(), "validation error: {:?}", parsed.validation_error());
        prop_assert_eq!(parsed.parsed_event(), Some(&event));
    }

    /// Arbitrary strings must never panic the parser.
    #[test]
    fn arbitrary_strings_never_panic(raw in any::<String>()) {
        exercise(raw);
    }

    /// Arbitrary JSON values must never panic the parser.
    #[test]
    fn arbitrary_json_never_panics(value in arb_json()) {
        exercise(value.to_string());
    }

    /// JSON carrying a recognized Payment Request kind drives the per-kind
    /// parsers and must never panic (guards the `unreachable!()` invariant).
    #[test]
    fn kinded_json_never_panics(raw in arb_kinded_json()) {
        exercise(raw);
    }
}

/// Every `PrivateMessageKind` routes through the public Payment Request parser
/// without panicking, and the routing decision agrees with both the
/// hand-maintained spec and `PrivateMessageKind::is_payment_request_event`.
///
/// This complements the proptests above, which only sample the five already
/// routed kind strings in `PAYMENT_REQUEST_EVENT_KINDS`. Because it drives
/// *all* variants of the enum -- including kinds with no `parse_event` arm -- it
/// catches a future variant that `is_payment_request_event` starts accepting
/// while the dispatcher still falls through to `unreachable!()`: that kind would
/// route here and panic instead of returning `Some`.
#[test]
fn test_payment_request_routing_covers_all_private_message_kinds() {
    let mut routed_kinds = 0;

    for &kind in ALL_PRIVATE_MESSAGE_KINDS {
        let expected_routed = payment_request_routing_expectation(kind);

        // The parser reads the kind from the raw JSON body via `known_kind`, so
        // carry it there rather than in the struct's `kind` header field.
        let message = PrivateApplicationMessage {
            version: None,
            kind: None,
            raw_json: format!(r#"{{"kind":"{}"}}"#, kind.as_str()),
        };

        // Never-panic: a routed kind reaches `parse_event` (and, for a mis-wired
        // future variant, its `unreachable!()` arm), so a panic there fails this
        // test. Touch every accessor for the same reason.
        let parsed = parse_payment_request_event_message(&message);
        if let Some(event_message) = &parsed {
            let _ = event_message.is_valid();
            let _ = event_message.kind();
            let _ = event_message.event_id();
            let _ = event_message.payment_request_id();
            let _ = event_message.validation_error();
            let _ = event_message.parsed_event();
            let _ = format!("{event_message:?}");
        }

        assert_eq!(
            parsed.is_some(),
            expected_routed,
            "parser routing for {kind} disagreed with the spec"
        );
        assert_eq!(
            kind.is_payment_request_event(),
            expected_routed,
            "is_payment_request_event for {kind} disagreed with the spec"
        );

        if expected_routed {
            routed_kinds += 1;
        }
    }

    // Cross-check the routed set against the proptest kind list, so a newly
    // routed kind must be reflected in `PAYMENT_REQUEST_EVENT_KINDS` too.
    assert_eq!(
        routed_kinds,
        PAYMENT_REQUEST_EVENT_KINDS.len(),
        "routed kind count disagreed with PAYMENT_REQUEST_EVENT_KINDS"
    );
}
