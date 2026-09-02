use super::*;
use crate::{
    serialize_private_payment_list_json, PaykitAppId, PaymentEndpointIdentifier,
    PaymentEndpointPayload, PrivatePaymentList,
};
use std::collections::HashMap;

fn app_id() -> PaykitAppId {
    PaykitAppId::new("test-app").unwrap()
}

#[test]
fn test_send_attempts_from_retries_bounds() {
    assert_eq!(send_attempts_from_retries(0), 1);
    assert_eq!(send_attempts_from_retries(3), 4);
    assert_eq!(send_attempts_from_retries(u32::MAX), u32::MAX);
}

#[test]
fn test_private_send_retry_classification() {
    assert!(is_retryable_private_send_error(
        &pubky_noise::PubkyNoiseError::HomeserverWriteError,
    ));

    for err in [
        pubky_noise::PubkyNoiseError::IsHandshake,
        pubky_noise::PubkyNoiseError::EncryptionError,
        pubky_noise::PubkyNoiseError::CounterOverflow,
        pubky_noise::PubkyNoiseError::NonceOverflow,
    ] {
        assert!(
            !is_retryable_private_send_error(&err),
            "{err:?} should not be retried"
        );
    }
}

#[test]
fn test_non_retryable_private_send_error_is_classified() {
    let err = PaykitError::Transport {
        context: "failed to send Private Application Message".into(),
        source: anyhow::Error::new(NonRetryablePrivateSendError(
            pubky_noise::PubkyNoiseError::EncryptionError,
        )),
    };

    assert!(err.is_non_retryable_private_send_error());
}

#[test]
fn test_private_receive_retry_classification() {
    assert!(is_retryable_private_receive_error(
        &pubky_noise::PubkyNoiseError::HomeserverResponseError,
    ));

    for err in [
        pubky_noise::PubkyNoiseError::BadLengthCiphertext,
        pubky_noise::PubkyNoiseError::IsHandshake,
        pubky_noise::PubkyNoiseError::DecryptionError,
        pubky_noise::PubkyNoiseError::CounterOverflow,
        pubky_noise::PubkyNoiseError::NonceOverflow,
    ] {
        assert!(
            !is_retryable_private_receive_error(&err),
            "{err:?} should not be retried"
        );
    }
}

#[test]
fn test_non_retryable_private_receive_error_is_classified() {
    let err = PaykitError::Transport {
        context: "failed to receive Private Application Messages".into(),
        source: anyhow::Error::new(NonRetryablePrivateReceiveError(
            pubky_noise::PubkyNoiseError::DecryptionError,
        )),
    };

    assert!(err.is_non_retryable_private_receive_error());
}

#[test]
fn test_private_application_message_size_validation_rejects_oversized_payload() {
    let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN + 1];
    let err = validate_private_application_message_size(&payload, "Payment Request").unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
        "expected oversize validation error, got: {err}"
    );
}

#[test]
fn test_private_application_message_size_validation_accepts_payload_at_limit() {
    // Guards against a `>` -> `>=` regression in the send-time size check:
    // a payload of exactly PUBKY_NOISE_MSG_LEN bytes must be accepted.
    let payload = vec![b'x'; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN];
    validate_private_application_message_size(&payload, "Payment Request")
        .expect("payload of exactly PUBKY_NOISE_MSG_LEN bytes should be accepted");
}

#[test]
fn test_private_application_message_size_validation_rejects_oversized_private_payment_list() {
    // A Private Payment List can cross the pubky-noise message ceiling via
    // many small Payment Endpoints rather than one oversized payload.
    // Send-time validation is the documented enforcement point; there is
    // no construction-time size limit. The context string matches the real
    // send path (EncryptedLink::send_private_payment_list_message).
    let mut payment_endpoints = HashMap::new();
    for i in 0..20 {
        payment_endpoints.insert(
            PaymentEndpointIdentifier::new(format!("endpoint-{i:02}")).unwrap(),
            PaymentEndpointPayload::new(format!("payload-{i:02}-{}", "x".repeat(40))),
        );
    }
    let list = PrivatePaymentList::new(app_id(), payment_endpoints);
    let json = serialize_private_payment_list_json(&list).unwrap();
    assert!(
        json.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        "fixture must exceed the pubky-noise message ceiling, got {} bytes",
        json.len()
    );

    let err = validate_private_application_message_size(json.as_bytes(), "Private Payment List")
        .unwrap_err();
    assert!(
        matches!(err, PaykitError::Validation(ref msg) if msg.contains("exceeds")),
        "expected oversize validation error, got: {err}"
    );
}

#[test]
fn test_private_application_message_keeps_malformed_header() {
    let raw = r#"{"version":"bad","kind":"paykit.payment_request","event_id":"not-a-uuid"}"#;
    let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

    assert_eq!(message.version, None);
    assert_eq!(
        message.known_kind(),
        Some(PrivateMessageKind::PaymentRequest)
    );
    assert_eq!(message.raw_json, raw);
}

#[test]
fn test_private_application_message_keeps_json_without_kind() {
    let raw = r#"{"version":1,"payload":"unknown"}"#;
    let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

    assert_eq!(message.version, Some(1));
    assert_eq!(message.kind, None);
    assert_eq!(message.known_kind(), None);
    assert_eq!(message.raw_json, raw);
}

#[test]
fn test_private_application_message_known_kind_uses_raw_json() {
    let raw = r#"{"version":1,"kind":"paykit.receipt_access"}"#;
    let message = PrivateApplicationMessage {
        version: Some(1),
        kind: Some("paykit.payment_request".to_string()),
        app_id: Some(app_id().as_str().to_string()),
        raw_json: raw.to_string(),
    };

    assert_eq!(
        message.known_kind(),
        Some(PrivateMessageKind::ReceiptAccess)
    );
}

#[test]
fn test_private_application_message_debug_redacts_raw_json() {
    let raw = r#"{"version":1,"kind":"paykit.receipt_access","key":"secret"}"#;
    let message = PrivateApplicationMessage::from_plaintext(raw.to_string());
    let debug = format!("{message:?}");

    assert!(!debug.contains("secret"));
    assert!(debug.contains("<redacted:"));
}

#[test]
fn test_private_application_message_keeps_invalid_json() {
    let raw = "not json";
    let message = PrivateApplicationMessage::from_plaintext(raw.to_string());

    assert_eq!(message.version, None);
    assert_eq!(message.kind, None);
    assert_eq!(message.known_kind(), None);
    assert_eq!(message.raw_json, raw);
}

#[test]
fn test_decode_private_application_message_retains_invalid_utf8_marker() {
    let mut raw = [0u8; pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN];
    raw[0] = 0xff;

    let message = decode_private_application_message(&raw).unwrap();

    assert_eq!(message.version, None);
    assert_eq!(message.kind, None);
    assert_eq!(message.known_kind(), None);
    assert_eq!(
        message.invalid_utf8_error(),
        Some("Private Application Message plaintext is not valid UTF-8")
    );
    assert!(message
        .raw_json
        .starts_with(INVALID_UTF8_PRIVATE_MESSAGE_PREFIX));
}

// CLAUDE.md contract: public reads treat 404/GONE as absence, never as errors.
fn server_error(status: StatusCode) -> PubkyError {
    PubkyError::Request(RequestError::Server {
        status,
        message: "test response".into(),
    })
}

#[test]
fn test_is_not_found_matches_not_found_and_gone() {
    assert!(is_not_found(&server_error(StatusCode::NOT_FOUND)));
    assert!(is_not_found(&server_error(StatusCode::GONE)));
}

#[test]
fn test_is_not_found_rejects_other_statuses_and_variants() {
    assert!(!is_not_found(&server_error(
        StatusCode::INTERNAL_SERVER_ERROR
    )));
    assert!(!is_not_found(&server_error(StatusCode::FORBIDDEN)));

    let validation_error = PubkyError::Request(RequestError::Validation {
        message: "invalid request".into(),
    });
    assert!(!is_not_found(&validation_error));
}
