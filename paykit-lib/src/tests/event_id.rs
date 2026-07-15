//! Unit tests for the shared UUID-v4 validator (`validate_uuid_v4`), exercised
//! through the public `EventId`, `PaymentRequestId`, and `ReceiptId`
//! constructors.
//!
//! These pin the two behaviors this crate owns (the `uuid` crate's own version
//! detection is not re-tested): lowercase canonicalization of accepted input,
//! and the version/variant rejection branch for parseable-but-non-v4 UUIDs.
//! The validator rejects on `get_version_num() != 4 || get_variant() !=
//! RFC4122`, so both halves are covered independently: most fixtures trip the
//! version nibble, while [`V4_NON_RFC4122_VARIANT`] carries a real version-4
//! nibble and is rejected specifically on the variant check.

use super::*;

/// A valid, lowercase, RFC4122 version-4 UUID.
const VALID_V4: &str = "550e8400-e29b-41d4-a716-446655440000";

/// The same UUID as `VALID_V4`, spelled in uppercase.
const VALID_V4_UPPERCASE: &str = "550E8400-E29B-41D4-A716-446655440000";

/// A well-formed UUID whose version nibble is 4 but whose variant is NCS
/// (`0xxx`), not RFC4122 (`10xx`): the fourth group starts with `0` instead of
/// the `8`-`b` an RFC4122 UUID would use. This is the only fixture that reaches
/// the `get_variant()` half of the version/variant check; every other entry in
/// [`NON_V4_UUIDS`] is rejected on the version nibble first.
const V4_NON_RFC4122_VARIANT: &str = "550e8400-e29b-41d4-0716-446655440000";

/// Well-formed UUIDs that parse successfully but are not RFC4122 version 4, so
/// they must fail the version/variant check rather than the parse step: the nil
/// UUID (version 0), versions 1, 3, and 5, plus a version-4 nibble carrying a
/// non-RFC4122 variant so the variant half of the check is exercised too.
const NON_V4_UUIDS: &[&str] = &[
    "00000000-0000-0000-0000-000000000000", // nil (version 0)
    "a8098c1a-f86e-11da-bd1a-00112444be1e", // version 1 (time-based)
    "6fa459ea-ee8a-3ca4-894e-db77e160355e", // version 3 (name-based MD5)
    "886313e1-3b8a-5372-9b90-0c9aee199e5d", // version 5 (name-based SHA-1)
    V4_NON_RFC4122_VARIANT,                 // version 4 nibble but NCS variant, not RFC4122
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
fn test_event_id_rejects_v4_with_non_rfc4122_variant() {
    // Guard the variant half of the version/variant check specifically. First
    // confirm the fixture behaves as claimed so the assertion below is meaningful:
    // it parses, its version nibble is 4, and its variant is not RFC4122 (so it
    // slips past `get_version_num() != 4` and can only be caught on the variant).
    let parsed =
        uuid::Uuid::try_parse(V4_NON_RFC4122_VARIANT).expect("fixture must be a parseable UUID");
    assert_eq!(
        parsed.get_version_num(),
        4,
        "fixture must carry a version-4 nibble to bypass the version check",
    );
    assert_ne!(
        parsed.get_variant(),
        uuid::Variant::RFC4122,
        "fixture must carry a non-RFC4122 variant to reach the variant check",
    );

    let err = EventId::new(V4_NON_RFC4122_VARIANT).unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(_)),
        "a version-4 UUID with a non-RFC4122 variant must be rejected",
    );
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
