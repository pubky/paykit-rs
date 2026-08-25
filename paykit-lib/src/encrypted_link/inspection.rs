//! Shared inspection entry point for raw Private Application Message JSON.
//!
//! SDK intake, outbound validation, and backup validation each need the same
//! answers about one raw private-message payload: which Private Message Kind
//! it carries, whether its body is structurally valid, which Latest-State
//! Message versus Event Message semantics apply, whether an Event ID is
//! recoverable, and which stable redacted parse category describes a failure.
//! This module computes all of that in one place so those security boundaries
//! cannot drift apart.
//!
//! SECURITY / REDACTION: inputs are decrypted private-message plaintext. The
//! inspection result carries no free-form error text; failures surface only as
//! a [`PrivateMessageParseCategory`], and [`PrivateMessageInspection`]'s
//! `Debug` output redacts unrecognized kind strings.

use super::private_application_message::{
    PrivateApplicationMessage, PrivateMessageKind, PrivateMessageParseCategory,
    PrivateMessageSemantics, INVALID_UTF8_PRIVATE_MESSAGE_PREFIX,
};
use crate::{
    parse_payment_request_event_message, parse_private_payment_list_json,
    parse_receipt_access_event_message,
};

/// Structural classification of one raw Private Application Message payload.
///
/// This enum is deliberately exhaustive (no `#[non_exhaustive]`), following
/// the [`PrivateMessageKind`] exhaustive-enum precedent: SDK boundary matches
/// over it must break at compile time when a variant is added, so every
/// security boundary classifies new structural outcomes explicitly instead of
/// falling through a wildcard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateMessageStructure {
    /// A recognized kind whose body passed structural validation.
    Valid,
    /// A recognized kind whose body failed structural validation.
    MalformedRecognized,
    /// A well-formed envelope (header `version` and `kind` both present)
    /// declaring a kind this library does not recognize.
    UnknownKind,
    /// Not a well-formed Private Application Message envelope: invalid JSON,
    /// a missing header field, or the local invalid-UTF-8 receive marker.
    InvalidJson,
}

/// Result of inspecting one raw Private Application Message payload.
///
/// See [`inspect_private_application_message`] for the classification rules.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateMessageInspection {
    /// Header `version` field, when present and representable as a `u8`.
    pub parsed_version: Option<u8>,
    /// Header `kind` field, when present and a string. May be an arbitrary
    /// unrecognized value from decrypted plaintext; `Debug` redacts it unless
    /// it equals the recognized kind's canonical string.
    pub parsed_kind: Option<String>,
    /// The recognized Private Message Kind, parsed from the message body.
    pub known_kind: Option<PrivateMessageKind>,
    /// Latest-State Message versus Event Message semantics of the recognized
    /// kind; `None` when the kind is not recognized.
    pub semantics: Option<PrivateMessageSemantics>,
    /// Structural classification of the payload.
    pub structure: PrivateMessageStructure,
    /// Recoverable Event ID for recognized Event Message kinds, when the
    /// top-level `event_id` field parses as a valid Event ID. Present even
    /// when the body is otherwise invalid, matching what intake persists.
    pub event_id: Option<String>,
    /// Stable redacted parse category when the payload is not `Valid` and a
    /// category is defined for the failure; `None` for a `Valid` payload and
    /// for the unrecognized outcomes that persist no parse summary today.
    pub error_category: Option<PrivateMessageParseCategory>,
}

impl std::fmt::Debug for PrivateMessageInspection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("PrivateMessageInspection");
        debug.field("parsed_version", &self.parsed_version);
        // An unrecognized kind string is arbitrary decrypted plaintext, so it
        // may only appear in Debug output when it equals the recognized
        // kind's canonical (public, closed-vocabulary) string.
        match (
            self.parsed_kind.as_deref(),
            self.known_kind.map(PrivateMessageKind::as_str),
        ) {
            (Some(parsed), Some(canonical)) if parsed == canonical => {
                debug.field("parsed_kind", &self.parsed_kind);
            }
            (Some(parsed), _) => {
                debug.field(
                    "parsed_kind",
                    &format_args!("<redacted:{} bytes>", parsed.len()),
                );
            }
            (None, _) => {
                debug.field("parsed_kind", &self.parsed_kind);
            }
        }
        debug
            .field("known_kind", &self.known_kind)
            .field("semantics", &self.semantics)
            .field("structure", &self.structure)
            .field("event_id", &self.event_id)
            .field("error_category", &self.error_category)
            .finish()
    }
}

/// Inspect one raw Private Application Message payload.
///
/// Classification rules, shared by every caller:
///
/// - **The body's `kind` is authoritative over the envelope header.** The
///   recognized kind is parsed from `raw_json` itself, exactly like
///   [`PrivateApplicationMessage::known_kind`]; kind metadata carried outside
///   the raw payload (an envelope header, a storage column) has no influence
///   here.
/// - **The invalid-UTF-8 receive marker is special-cased.** Persisted raw
///   JSON is not always the literal wire payload: when decrypted bytes are
///   not valid UTF-8, the receive path persists a local marker string
///   instead. Inspection reports that marker as
///   [`PrivateMessageStructure::InvalidJson`] with
///   [`PrivateMessageParseCategory::InvalidUtf8Plaintext`].
/// - **Receipt Access receiver-scope enforcement is NOT inspection.** Whether
///   a Receipt Access location falls inside the counterparty receiver scope
///   is a separate SDK policy pass applied today at intake and backup
///   validation, not at outbound validation. Inspection reports only
///   payload-intrinsic structure, so all three boundaries can share it.
///
/// A payload whose kind is unrecognized is
/// [`PrivateMessageStructure::UnknownKind`] only when the header `version`
/// and `kind` are both present; anything less is
/// [`PrivateMessageStructure::InvalidJson`]. Neither outcome carries an error
/// category (except the invalid-UTF-8 marker above), matching the parse
/// summary intake persists. For recognized kinds, `error_category` is the
/// stable redacted category of the body parse failure, and `event_id` is the
/// recoverable Event ID even when the body is invalid.
///
/// # Examples
///
/// ```
/// use paykit_lib::{
///     inspect_private_application_message, PrivateMessageKind, PrivateMessageSemantics,
///     PrivateMessageStructure,
/// };
///
/// let raw = r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#;
/// let inspection = inspect_private_application_message(raw);
/// assert_eq!(inspection.known_kind, Some(PrivateMessageKind::PrivatePaymentList));
/// assert_eq!(inspection.semantics, Some(PrivateMessageSemantics::LatestState));
/// assert_eq!(inspection.structure, PrivateMessageStructure::Valid);
/// assert_eq!(inspection.error_category, None);
///
/// let unknown = r#"{"version":1,"kind":"paykit.allowance","body":{}}"#;
/// let inspection = inspect_private_application_message(unknown);
/// assert_eq!(inspection.known_kind, None);
/// assert_eq!(inspection.structure, PrivateMessageStructure::UnknownKind);
/// assert_eq!(inspection.error_category, None);
/// ```
pub fn inspect_private_application_message(raw_json: &str) -> PrivateMessageInspection {
    // The local invalid-UTF-8 receive marker is not a wire payload, so it is
    // classified before any JSON handling. A marker string can never be valid
    // JSON, so this matches the general flow below byte-for-byte while making
    // the special case explicit.
    if raw_json.starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX) {
        return PrivateMessageInspection {
            parsed_version: None,
            parsed_kind: None,
            known_kind: None,
            semantics: None,
            structure: PrivateMessageStructure::InvalidJson,
            event_id: None,
            error_category: Some(PrivateMessageParseCategory::InvalidUtf8Plaintext),
        };
    }

    // Header derivation and body-kind recognition are delegated to the same
    // code intake uses (`from_plaintext` + `known_kind`), so inspection cannot
    // drift from the classification the SDK persists.
    let message = PrivateApplicationMessage::from_plaintext(raw_json.to_owned());
    let parsed_version = message.version;
    let parsed_kind = message.kind.clone();

    let Some(known_kind) = message.known_kind() else {
        // Unrecognized payloads persist no parse summary today, so no error
        // category is reported: `UnknownKind` requires a well-formed header
        // (version and kind both present), anything less is invalid JSON.
        let structure = if message.version.is_some() && message.kind.is_some() {
            PrivateMessageStructure::UnknownKind
        } else {
            PrivateMessageStructure::InvalidJson
        };
        return PrivateMessageInspection {
            parsed_version,
            parsed_kind,
            known_kind: None,
            semantics: None,
            structure,
            event_id: None,
            error_category: None,
        };
    };

    // Recognized kinds dispatch to their body parser with no wildcard arm:
    // adding a `PrivateMessageKind` variant fails to compile until it is
    // routed to a parser explicitly here.
    let (is_valid, error_category, event_id) = match known_kind {
        PrivateMessageKind::PrivatePaymentList => {
            match parse_private_payment_list_json(&message.raw_json) {
                Ok(_) => (true, None, None),
                Err(err) => (
                    false,
                    Some(
                        err.private_message_parse_category()
                            .unwrap_or(PrivateMessageParseCategory::InvalidStructure),
                    ),
                    None,
                ),
            }
        }
        PrivateMessageKind::ReceiptAccess => match parse_receipt_access_event_message(&message) {
            Some(parsed) => (
                parsed.is_valid(),
                parsed.parse_category(),
                parsed.event_id().map(|id| id.as_str().to_owned()),
            ),
            // Unreachable in practice: the parser returns `Some` for every
            // message whose body kind is Receipt Access, which the outer
            // match established. Mirroring intake, an absent parse result
            // counts as invalid with no category and no recoverable Event ID.
            None => (false, None, None),
        },
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => match parse_payment_request_event_message(&message) {
            Some(parsed) => (
                parsed.is_valid(),
                parsed.parse_category(),
                parsed.event_id().map(|id| id.as_str().to_owned()),
            ),
            // Unreachable in practice; see the Receipt Access arm.
            None => (false, None, None),
        },
    };

    PrivateMessageInspection {
        parsed_version,
        parsed_kind,
        known_kind: Some(known_kind),
        semantics: Some(known_kind.semantics()),
        structure: if is_valid {
            PrivateMessageStructure::Valid
        } else {
            PrivateMessageStructure::MalformedRecognized
        },
        event_id,
        error_category,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Per-kind payloads mirroring the frozen SDK classification fixture
    // (paykit-sdk/src/domain/private_stream/fixtures/), so lib inspection and
    // the SDK boundary decisions are pinned against the same inputs.
    struct KindCase {
        kind: PrivateMessageKind,
        valid: &'static str,
        valid_event_id: Option<&'static str>,
        malformed: &'static str,
        malformed_event_id: Option<&'static str>,
        wrong_version: &'static str,
        wrong_version_event_id: Option<&'static str>,
    }

    fn kind_cases() -> [KindCase; 7] {
        [
            KindCase {
                kind: PrivateMessageKind::PrivatePaymentList,
                valid: r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"btc-lightning-bolt11":"lnbc-fixture-endpoint"}}"#,
                valid_event_id: None,
                malformed: r#"{"version":1,"kind":"paykit.private_payment_list"}"#,
                malformed_event_id: None,
                wrong_version: r#"{"version":2,"kind":"paykit.private_payment_list","payment_endpoints":{"btc-lightning-bolt11":"lnbc-fixture-endpoint"}}"#,
                wrong_version_event_id: None,
            },
            KindCase {
                kind: PrivateMessageKind::ReceiptAccess,
                valid: r#"{"version":1,"kind":"paykit.receipt_access","event_id":"11111111-1111-4111-8111-000000000001","receipt_id":"22222222-2222-4222-8222-000000000001","payment_reference":"invoice-2026-0001","location":"/pub/paykit/v0/private/bitkit/wallet/receipts/22222222-2222-4222-8222-000000000001","key":"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY"}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000001"),
                malformed: r#"{"version":1,"kind":"paykit.receipt_access","event_id":"11111111-1111-4111-8111-000000000007","receipt_id":"22222222-2222-4222-8222-000000000002","payment_reference":"invoice-2026-0001","location":"/pub/paykit/v0/private/bitkit/wallet/receipts/22222222-2222-4222-8222-000000000002"}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-000000000007"),
                wrong_version: r#"{"version":2,"kind":"paykit.receipt_access","event_id":"11111111-1111-4111-8111-00000000000d","receipt_id":"22222222-2222-4222-8222-000000000003","payment_reference":"invoice-2026-0001","location":"/pub/paykit/v0/private/bitkit/wallet/receipts/22222222-2222-4222-8222-000000000003","key":"MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY"}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-00000000000d"),
            },
            KindCase {
                kind: PrivateMessageKind::PaymentRequest,
                valid: r#"{"version":1,"kind":"paykit.payment_request","event_id":"11111111-1111-4111-8111-000000000002","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"0.001","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000002"),
                malformed: r#"{"version":1,"kind":"paykit.payment_request","event_id":"11111111-1111-4111-8111-000000000008","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"ten","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-000000000008"),
                wrong_version: r#"{"version":2,"kind":"paykit.payment_request","event_id":"11111111-1111-4111-8111-00000000000e","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","request":{"amount":{"value":"0.001","asset":"btc"},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{}}}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-00000000000e"),
            },
            KindCase {
                kind: PrivateMessageKind::PaymentRequestAcceptance,
                valid: r#"{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"11111111-1111-4111-8111-000000000003","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000003"),
                malformed: r#"{"version":1,"kind":"paykit.payment_request_acceptance","event_id":"11111111-1111-4111-8111-000000000009","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","reason":"accepted"}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-000000000009"),
                wrong_version: r#"{"version":2,"kind":"paykit.payment_request_acceptance","event_id":"11111111-1111-4111-8111-00000000000f","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-00000000000f"),
            },
            KindCase {
                kind: PrivateMessageKind::PaymentRequestRejection,
                valid: r#"{"version":1,"kind":"paykit.payment_request_rejection","event_id":"11111111-1111-4111-8111-000000000004","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","reason":"amount too high"}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000004"),
                malformed: r#"{"version":1,"kind":"paykit.payment_request_rejection","event_id":"11111111-1111-4111-8111-00000000000a","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","reason":null}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-00000000000a"),
                wrong_version: r#"{"version":2,"kind":"paykit.payment_request_rejection","event_id":"11111111-1111-4111-8111-000000000010","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","reason":"amount too high"}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-000000000010"),
            },
            KindCase {
                kind: PrivateMessageKind::PaymentRequestCancellation,
                valid: r#"{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"11111111-1111-4111-8111-000000000005","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","reason":"no longer needed"}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000005"),
                malformed: r#"{"version":1,"kind":"paykit.payment_request_cancellation","event_id":"11111111-1111-4111-8111-00000000000b"}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-00000000000b"),
                wrong_version: r#"{"version":2,"kind":"paykit.payment_request_cancellation","event_id":"11111111-1111-4111-8111-000000000011","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33"}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-000000000011"),
            },
            KindCase {
                kind: PrivateMessageKind::PaymentProof,
                valid: r#"{"version":1,"kind":"paykit.payment_proof","event_id":"11111111-1111-4111-8111-000000000006","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","payment_reference":"invoice-2026-0001","billing_period":null,"payment_endpoint_identifier":"btc-lightning-bolt11","proof":{"preimage":"a1b2c3"}}"#,
                valid_event_id: Some("11111111-1111-4111-8111-000000000006"),
                malformed: r#"{"version":1,"kind":"paykit.payment_proof","event_id":"11111111-1111-4111-8111-00000000000c","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","payment_reference":"invoice-2026-0001","payment_endpoint_identifier":"btc-lightning-bolt11","proof":{"preimage":"a1b2c3"}}"#,
                malformed_event_id: Some("11111111-1111-4111-8111-00000000000c"),
                wrong_version: r#"{"version":2,"kind":"paykit.payment_proof","event_id":"11111111-1111-4111-8111-000000000012","payment_request_id":"b7f9c2a1-6d43-4b0e-a8d4-0fe2c712ab33","payment_reference":"invoice-2026-0001","billing_period":null,"payment_endpoint_identifier":"btc-lightning-bolt11","proof":{"preimage":"a1b2c3"}}"#,
                wrong_version_event_id: Some("11111111-1111-4111-8111-000000000012"),
            },
        ]
    }

    fn assert_inspection(
        label: &str,
        raw: &str,
        expected_kind: PrivateMessageKind,
        expected_structure: PrivateMessageStructure,
        expected_category: Option<PrivateMessageParseCategory>,
        expected_event_id: Option<&str>,
        expected_version: Option<u8>,
    ) {
        let inspection = inspect_private_application_message(raw);
        assert_eq!(
            inspection.parsed_version, expected_version,
            "{label}: parsed_version"
        );
        assert_eq!(
            inspection.parsed_kind.as_deref(),
            Some(expected_kind.as_str()),
            "{label}: parsed_kind"
        );
        assert_eq!(
            inspection.known_kind,
            Some(expected_kind),
            "{label}: known_kind"
        );
        assert_eq!(
            inspection.semantics,
            Some(expected_kind.semantics()),
            "{label}: semantics"
        );
        assert_eq!(
            inspection.structure, expected_structure,
            "{label}: structure"
        );
        assert_eq!(
            inspection.event_id.as_deref(),
            expected_event_id,
            "{label}: event_id"
        );
        assert_eq!(
            inspection.error_category, expected_category,
            "{label}: error_category"
        );
    }

    #[test]
    fn test_inspection_matrix_covers_every_kind_and_failure_class() {
        let cases = kind_cases();
        assert_eq!(cases.len(), PrivateMessageKind::ALL.len());
        for (case, kind) in cases.iter().zip(PrivateMessageKind::ALL) {
            assert_eq!(case.kind, kind, "matrix must follow ALL declaration order");

            assert_inspection(
                &format!("{kind:?} valid"),
                case.valid,
                kind,
                PrivateMessageStructure::Valid,
                None,
                case.valid_event_id,
                Some(1),
            );
            // Recoverable-Event-ID pin: for Event Message kinds the header
            // Event ID stays available even when the body is invalid,
            // matching what intake persists for malformed stream items.
            assert_inspection(
                &format!("{kind:?} malformed"),
                case.malformed,
                kind,
                PrivateMessageStructure::MalformedRecognized,
                Some(PrivateMessageParseCategory::InvalidStructure),
                case.malformed_event_id,
                Some(1),
            );
            assert_inspection(
                &format!("{kind:?} wrong version"),
                case.wrong_version,
                kind,
                PrivateMessageStructure::MalformedRecognized,
                Some(PrivateMessageParseCategory::UnsupportedVersion),
                case.wrong_version_event_id,
                Some(2),
            );
        }

        // Unknown kind with a well-formed header: no error category, no
        // recoverable Event ID (unrecognized payloads persist no summary).
        let unknown = inspect_private_application_message(
            r#"{"version":1,"kind":"paykit.allowance","event_id":"11111111-1111-4111-8111-000000000014","body":{}}"#,
        );
        assert_eq!(unknown.parsed_version, Some(1));
        assert_eq!(unknown.parsed_kind.as_deref(), Some("paykit.allowance"));
        assert_eq!(unknown.known_kind, None);
        assert_eq!(unknown.semantics, None);
        assert_eq!(unknown.structure, PrivateMessageStructure::UnknownKind);
        assert_eq!(unknown.event_id, None);
        assert_eq!(unknown.error_category, None);

        // Unknown kind without a version header is invalid JSON, not
        // UnknownKind: the well-formed-envelope rule requires both fields.
        let headerless = inspect_private_application_message(r#"{"kind":"custom.kind"}"#);
        assert_eq!(headerless.parsed_version, None);
        assert_eq!(headerless.known_kind, None);
        assert_eq!(headerless.structure, PrivateMessageStructure::InvalidJson);
        assert_eq!(headerless.error_category, None);

        // Invalid JSON: nothing recoverable, no category.
        let invalid = inspect_private_application_message("this is not JSON");
        assert_eq!(invalid.parsed_version, None);
        assert_eq!(invalid.parsed_kind, None);
        assert_eq!(invalid.known_kind, None);
        assert_eq!(invalid.semantics, None);
        assert_eq!(invalid.structure, PrivateMessageStructure::InvalidJson);
        assert_eq!(invalid.event_id, None);
        assert_eq!(invalid.error_category, None);

        // Invalid-UTF-8 receive marker: the one unrecognized outcome that
        // carries a category, because intake persists the marker's parse
        // summary.
        let sentinel =
            inspect_private_application_message("paykit.invalid_utf8_private_message:_w");
        assert_eq!(sentinel.parsed_version, None);
        assert_eq!(sentinel.parsed_kind, None);
        assert_eq!(sentinel.known_kind, None);
        assert_eq!(sentinel.semantics, None);
        assert_eq!(sentinel.structure, PrivateMessageStructure::InvalidJson);
        assert_eq!(sentinel.event_id, None);
        assert_eq!(
            sentinel.error_category,
            Some(PrivateMessageParseCategory::InvalidUtf8Plaintext)
        );

        // Body-kind-over-header pin: inspection accepts raw JSON only, so a
        // divergent envelope header cannot influence it. The recognized kind
        // always matches PrivateApplicationMessage::known_kind() on the same
        // raw payload, even when the envelope claims a different kind.
        let raw = cases[1].valid;
        let divergent_envelope = PrivateApplicationMessage {
            version: Some(1),
            kind: Some(PrivateMessageKind::PaymentRequest.as_str().to_owned()),
            raw_json: raw.to_owned(),
        };
        let inspection = inspect_private_application_message(raw);
        assert_eq!(inspection.known_kind, divergent_envelope.known_kind());
        assert_eq!(
            inspection.known_kind,
            Some(PrivateMessageKind::ReceiptAccess)
        );
    }

    #[test]
    fn test_inspection_debug_redacts_unrecognized_parsed_kind() {
        // An unrecognized kind string is decrypted plaintext and must not
        // reach Debug output.
        let inspection = inspect_private_application_message(
            r#"{"version":1,"kind":"paykit.kind-sentinel-secret"}"#,
        );
        let debug = format!("{inspection:?}");
        assert!(
            !debug.contains("kind-sentinel-secret"),
            "unrecognized parsed_kind leaked into Debug: {debug}"
        );
        assert!(
            debug.contains("<redacted:"),
            "expected redaction marker in Debug: {debug}"
        );

        // A recognized kind's canonical string is public vocabulary and stays
        // readable.
        let recognized = inspect_private_application_message(
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#,
        );
        let debug = format!("{recognized:?}");
        assert!(
            debug.contains("paykit.private_payment_list"),
            "canonical parsed_kind should stay readable in Debug: {debug}"
        );
        assert!(
            !debug.contains("<redacted:"),
            "canonical parsed_kind must not be redacted: {debug}"
        );

        // Absent parsed_kind prints as None without a redaction marker.
        let invalid = inspect_private_application_message("this is not JSON");
        let debug = format!("{invalid:?}");
        assert!(
            debug.contains("parsed_kind: None"),
            "unexpected Debug: {debug}"
        );
    }
}
