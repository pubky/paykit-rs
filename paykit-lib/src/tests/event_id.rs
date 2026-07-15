//! Unit tests for the shared UUID-v4 validator (`validate_uuid_v4`), exercised
//! through the public `EventId`, `PaymentRequestId`, and `ReceiptId`
//! constructors.
//!
//! These pin the version/variant rejection branch (nil, v1, v3, v5) that no
//! other test covers, plus the acceptance and lowercase-canonicalization paths.

use super::*;

/// A valid, lowercase, RFC4122 version-4 UUID.
const VALID_V4: &str = "550e8400-e29b-41d4-a716-446655440000";

/// The same UUID as `VALID_V4`, spelled in uppercase.
const VALID_V4_UPPERCASE: &str = "550E8400-E29B-41D4-A716-446655440000";

/// Nil UUID: version 0, non-RFC4122 variant.
const NIL_UUID: &str = "00000000-0000-0000-0000-000000000000";

/// Time-based (version 1) UUID.
const V1_UUID: &str = "a8098c1a-f86e-11da-bd1a-00112444be1e";

/// Name-based MD5 (version 3) UUID.
const V3_UUID: &str = "6fa459ea-ee8a-3ca4-894e-db77e160355e";

/// Name-based SHA-1 (version 5) UUID.
const V5_UUID: &str = "886313e1-3b8a-5372-9b90-0c9aee199e5d";

#[test]
fn test_event_id_accepts_valid_v4() {
    let id = EventId::new(VALID_V4).expect("a valid v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);
}

#[test]
fn test_event_id_canonicalizes_uppercase_to_lowercase() {
    // PartialEq/Hash derive on the raw String, so two case-variant spellings of
    // the same UUID must canonicalize to one stored value; otherwise they would
    // compare and hash as distinct ids.
    let id = EventId::new(VALID_V4_UPPERCASE).expect("an uppercase v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);
    assert_eq!(id, EventId::new(VALID_V4).unwrap());
}

#[test]
fn test_event_id_rejects_nil_uuid() {
    let err = EventId::new(NIL_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_event_id_rejects_v1_uuid() {
    let err = EventId::new(V1_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_event_id_rejects_v3_uuid() {
    let err = EventId::new(V3_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_event_id_rejects_v5_uuid() {
    let err = EventId::new(V5_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_payment_request_id_routes_through_validator() {
    // Accept a valid v4 and reject a well-formed non-v4 (version 1) UUID, which
    // pins that the newtype validates rather than storing arbitrary strings.
    let id = PaymentRequestId::new(VALID_V4).expect("a valid v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);

    let err = PaymentRequestId::new(V1_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}

#[test]
fn test_receipt_id_routes_through_validator() {
    // Accept a valid v4 and reject a well-formed non-v4 (version 5) UUID.
    let id = ReceiptId::new(VALID_V4).expect("a valid v4 UUID must be accepted");
    assert_eq!(id.as_str(), VALID_V4);

    let err = ReceiptId::new(V5_UUID).unwrap_err();
    assert!(matches!(err, PaykitError::Validation(_)));
}
