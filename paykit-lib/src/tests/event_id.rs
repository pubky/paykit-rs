//! Unit tests for the shared UUID-v4 validator (`validate_uuid_v4`), exercised
//! through the public `EventId`, `PaymentRequestId`, and `ReceiptId`
//! constructors.
//!
//! These pin the two behaviors this crate owns (the `uuid` crate's own version
//! detection is not re-tested): lowercase canonicalization of accepted input,
//! and the version/variant rejection branch for parseable-but-non-v4 UUIDs.

use super::*;

/// A valid, lowercase, RFC4122 version-4 UUID.
const VALID_V4: &str = "550e8400-e29b-41d4-a716-446655440000";

/// The same UUID as `VALID_V4`, spelled in uppercase.
const VALID_V4_UPPERCASE: &str = "550E8400-E29B-41D4-A716-446655440000";

/// Well-formed UUIDs that parse successfully but are not RFC4122 version 4, so
/// they must fail the version/variant check rather than the parse step: the nil
/// UUID (version 0) plus versions 1, 3, and 5.
const NON_V4_UUIDS: &[&str] = &[
    "00000000-0000-0000-0000-000000000000", // nil (version 0)
    "a8098c1a-f86e-11da-bd1a-00112444be1e", // version 1 (time-based)
    "6fa459ea-ee8a-3ca4-894e-db77e160355e", // version 3 (name-based MD5)
    "886313e1-3b8a-5372-9b90-0c9aee199e5d", // version 5 (name-based SHA-1)
];

#[test]
fn test_event_id_canonicalizes_uppercase_to_lowercase() {
    // PartialEq/Hash derive on the raw String, so two case-variant spellings of
    // the same UUID must canonicalize to one stored value; otherwise they would
    // compare and hash as distinct ids. Also covers acceptance of a valid v4.
    let id = EventId::new(VALID_V4_UPPERCASE).expect("an uppercase v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);
    assert_eq!(id, EventId::new(VALID_V4).unwrap());
}

#[test]
fn test_event_id_rejects_non_v4_uuids() {
    // The version/variant rejection branch: parseable UUIDs that are not RFC4122
    // v4 (nil, v1, v3, v5) must be rejected rather than stored.
    for uuid in NON_V4_UUIDS {
        let err = EventId::new(*uuid).unwrap_err();
        assert!(
            matches!(err, PaykitError::Validation(_)),
            "expected Validation rejection for non-v4 UUID {uuid}",
        );
    }
}

#[test]
fn test_payment_request_id_routes_through_validator() {
    // Accept a valid v4 and reject a well-formed non-v4 UUID, pinning that the
    // newtype validates rather than storing arbitrary strings.
    let id = PaymentRequestId::new(VALID_V4).expect("a valid v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);

    let err = PaymentRequestId::new(NON_V4_UUIDS[1]).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_receipt_id_routes_through_validator() {
    // Accept a valid v4 and reject a well-formed non-v4 UUID.
    let id = ReceiptId::new(VALID_V4).expect("a valid v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);

    let err = ReceiptId::new(NON_V4_UUIDS[3]).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}
