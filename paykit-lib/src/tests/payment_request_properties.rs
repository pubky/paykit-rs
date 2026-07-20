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
//!   parser and assert it returns Some/None without panicking.
//!
//! Kind routing itself is covered by
//! `test_payment_request_routing_covers_all_private_message_kinds`, which
//! drives every `PrivateMessageKind` variant through the parser from the single
//! macro-generated routing declaration below.
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

/// Single source of truth for how each `PrivateMessageKind` routes through the
/// public Payment Request parser: `true` means
/// `parse_payment_request_event_message` returns `Some`, `false` means the kind
/// is ignored and the parser returns `None`.
///
/// The macro expands the one declaration below into both the `(kind, routed)`
/// case list (`PRIVATE_MESSAGE_KIND_ROUTING`) and a wildcard-free `match` the
/// compiler checks for exhaustiveness. Adding a `PrivateMessageKind` variant
/// therefore fails to compile until the declaration classifies it, and the new
/// entry automatically flows into
/// `test_payment_request_routing_covers_all_private_message_kinds` (cases and
/// expected routing) and, via `routed_kind_strings`, into the
/// `kinded_json_never_panics` proptest. There is no second list to keep in
/// sync.
macro_rules! declare_private_message_kind_routing {
    ($($variant:ident => $routed:literal),+ $(,)?) => {
        const PRIVATE_MESSAGE_KIND_ROUTING: &[(PrivateMessageKind, bool)] =
            &[$((PrivateMessageKind::$variant, $routed)),+];

        /// Compile-time exhaustiveness guard for
        /// `PRIVATE_MESSAGE_KIND_ROUTING`; never called at runtime.
        #[allow(dead_code)]
        fn private_message_kind_routing_is_exhaustive(kind: PrivateMessageKind) -> bool {
            match kind {
                $(PrivateMessageKind::$variant => $routed,)+
            }
        }
    };
}

declare_private_message_kind_routing! {
    PrivatePaymentList => false,
    ReceiptAccess => false,
    PaymentRequest => true,
    PaymentRequestAcceptance => true,
    PaymentRequestRejection => true,
    PaymentRequestCancellation => true,
    PaymentProof => true,
}

/// Canonical kind strings the parser routes, derived from
/// `PRIVATE_MESSAGE_KIND_ROUTING` and `PrivateMessageKind::as_str` so the
/// proptest strategies cannot drift from the routing declaration.
fn routed_kind_strings() -> Vec<&'static str> {
    PRIVATE_MESSAGE_KIND_ROUTING
        .iter()
        .filter(|&&(_, routed)| routed)
        .map(|&(kind, _)| kind.as_str())
        .collect()
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

/// Build a valid Recurrence window: the optional `ends_at` is the `starts_at`
/// instant shifted a whole number of years forward, so it is always strictly
/// after `starts_at` as the validator requires.
///
/// NARROWER THAN THE DOMAIN: `ends_at` shares the month/day/time of
/// `starts_at`; the validator accepts any strictly later timestamp.
fn recurrence_window() -> impl Strategy<Value = (String, Option<String>)> {
    (
        2000u32..2050,
        1u32..=12,
        1u32..=28,
        0u32..=23,
        0u32..=59,
        0u32..=59,
        proptest::option::of(1u32..=50),
    )
        .prop_map(|(y, mo, d, h, mi, s, end_year_offset)| {
            let starts_at = format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z");
            let ends_at = end_year_offset
                .map(|offset| format!("{:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z", y + offset));
            (starts_at, ends_at)
        })
}

/// Build a valid `Recurrence`.
///
/// NARROWER THAN THE DOMAIN: `every` is capped at 1_000; the type allows any
/// positive `u32`.
fn recurrence() -> impl Strategy<Value = Recurrence> {
    (
        1u32..=1000,
        recurrence_unit(),
        recurrence_window(),
        utc_timestamp(),
    )
        .prop_map(|(every, unit, (starts_at, ends_at), anchor)| Recurrence {
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
/// deepest parse paths).
fn arb_kinded_json() -> impl Strategy<Value = String> {
    (prop::sample::select(routed_kind_strings()), arb_json()).prop_map(|(kind, body)| {
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
    /// parsers and must never panic.
    #[test]
    fn kinded_json_never_panics(raw in arb_kinded_json()) {
        exercise(raw);
    }
}

/// Every `PrivateMessageKind` variant routes through the public Payment Request
/// parser exactly as `PRIVATE_MESSAGE_KIND_ROUTING` declares, without panicking.
///
/// The proptests above only sample routed kind strings, so this test is what
/// exercises the ignored kinds. Cases, expected routing, and the proptest kind
/// strings all derive from the single macro-generated declaration, and
/// `parse_event` in `payment_request::api` is itself an exhaustive match, so a
/// new `PrivateMessageKind` variant cannot reach the parser unclassified or
/// slip past this test unlisted.
#[test]
fn test_payment_request_routing_covers_all_private_message_kinds() {
    for &(kind, expected_routed) in PRIVATE_MESSAGE_KIND_ROUTING {
        // The parser reads the kind from the raw JSON body via `known_kind`, so
        // carry it there rather than in the struct's `kind` header field.
        let message = PrivateApplicationMessage {
            version: None,
            kind: None,
            raw_json: format!(r#"{{"kind":"{}"}}"#, kind.as_str()),
        };

        // Pin the as_str -> parse round-trip before checking routing. Without
        // this, an ignored kind missing from `PrivateMessageKind::parse` also
        // makes the parser return `None`, and the routing assertion below
        // passes without ever exercising that variant.
        assert_eq!(
            message.known_kind(),
            Some(kind),
            "PrivateMessageKind::parse does not round-trip {kind}"
        );

        // Touch every accessor so a panic anywhere downstream fails this test.
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
            "parser routing for {kind} disagreed with PRIVATE_MESSAGE_KIND_ROUTING"
        );
    }
}
