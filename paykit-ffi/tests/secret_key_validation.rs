//! Rejection tests for the FFI 32-byte Pubky secret-key length gate.
//!
//! `pubky_public_key_from_secret` is the nearest public entry point that
//! exercises the internal `local_secret_from_bytes` length gate. That gate is
//! the single guard protecting every session-bootstrap call site in
//! `paykit-ffi`, so these tests pin its rejection behavior. They live in a
//! standalone integration-test crate to keep the hot `session.rs` and
//! `tests.rs` files untouched.
//!
//! The gate rejects any non-32-byte input with `PaykitFfiError::Protocol`
//! carrying `code == "validation"`. That is the FFI error taxonomy, which is
//! deliberately distinct from the library's four-variant `PaykitError`: the FFI
//! layer has no `Validation` variant, so the assertions match on
//! `Protocol { code, .. }` rather than a dedicated variant.

use std::sync::Arc;

use paykit::{pubky_public_key_from_secret, FfiPubkyLocalSecretKey, PaykitFfiError};

/// Wrap raw bytes in the FFI secret-key type without any length check.
///
/// The constructor performs no validation, so this faithfully reproduces the
/// malformed input a caller could hand across the FFI boundary.
fn secret_from(bytes: Vec<u8>) -> Arc<FfiPubkyLocalSecretKey> {
    Arc::new(FfiPubkyLocalSecretKey::new(bytes))
}

/// Assert the result is the length-gate rejection: `Protocol { code: "validation" }`.
fn assert_validation_rejection(result: Result<String, PaykitFfiError>) {
    match result {
        Ok(key) => panic!("expected a validation rejection, got Ok({key})"),
        Err(PaykitFfiError::Protocol { code, .. }) => {
            assert_eq!(
                code, "validation",
                "expected the length-gate validation code"
            );
        }
        Err(other) => {
            panic!("expected PaykitFfiError::Protocol {{ code: \"validation\" }}, got {other:?}")
        }
    }
}

#[test]
fn test_secret_key_length_gate_empty_rejected() {
    assert_validation_rejection(pubky_public_key_from_secret(secret_from(Vec::new())));
}

#[test]
fn test_secret_key_length_gate_thirty_one_bytes_rejected() {
    assert_validation_rejection(pubky_public_key_from_secret(secret_from(vec![7u8; 31])));
}

#[test]
fn test_secret_key_length_gate_thirty_three_bytes_rejected() {
    assert_validation_rejection(pubky_public_key_from_secret(secret_from(vec![7u8; 33])));
}

#[test]
fn test_secret_key_length_gate_thirty_two_bytes_accepted() {
    let result = pubky_public_key_from_secret(secret_from(vec![7u8; 32]));
    assert!(
        result.is_ok(),
        "expected a valid 32-byte secret key to be accepted, got {result:?}"
    );
}
