use super::*;
use crate::{
    parse_payment_request_event_message, parse_receipt_access_event_message,
    PrivateApplicationMessage,
};
use serde_json::{json, Value};

const EVENT_ID: &str = "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201";
const PAYLOAD_HASH: &str =
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn confirmation() -> DeliveryConfirmation {
    DeliveryConfirmation::new(
        PaykitAppId::new("confirming-app").unwrap(),
        EventId::new(EVENT_ID).unwrap(),
        PAYLOAD_HASH.into(),
    )
    .unwrap()
}

fn wire_value() -> Value {
    serde_json::from_str(&serialize_delivery_confirmation(&confirmation()).unwrap()).unwrap()
}

#[test]
fn test_delivery_confirmation_wire_roundtrip() {
    let confirmation = confirmation();
    let json = serialize_delivery_confirmation(&confirmation).unwrap();
    let expected = json!({
        "version": 1,
        "kind": "paykit.delivery_confirmation",
        "app_id": "confirming-app",
        "event_id": EVENT_ID,
        "payload_hash": PAYLOAD_HASH,
    });
    assert_eq!(serde_json::from_str::<Value>(&json).unwrap(), expected);
    assert_eq!(
        parse_delivery_confirmation_json(&json).unwrap(),
        confirmation
    );
    assert_eq!(confirmation.app_id().as_str(), "confirming-app");
    assert_eq!(confirmation.event_id().as_str(), EVENT_ID);
    assert_eq!(confirmation.payload_hash(), PAYLOAD_HASH);
    assert!(!PrivateMessageKind::DeliveryConfirmation.is_event());
}

#[test]
fn test_delivery_confirmation_rejects_invalid_hash_format() {
    for hash in [
        String::new(),
        "0".repeat(64),
        format!("SHA256:{}", "0".repeat(64)),
        format!("sha256:{}", "0".repeat(63)),
        format!("sha256:{}", "0".repeat(65)),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha256:{}", "g".repeat(64)),
        format!("sha256:{}", "\u{e9}".repeat(32)),
        format!("{PAYLOAD_HASH}\n"),
    ] {
        assert!(matches!(
            DeliveryConfirmation::new(
                PaykitAppId::new("confirming-app").unwrap(),
                EventId::new(EVENT_ID).unwrap(),
                hash.clone(),
            ),
            Err(PaykitError::Validation(_))
        ));
        let mut wire = wire_value();
        wire["payload_hash"] = json!(hash);
        assert!(matches!(
            parse_delivery_confirmation_json(&wire.to_string()),
            Err(PaykitError::InvalidData { .. })
        ));
    }
}

#[test]
fn test_delivery_confirmation_rejects_invalid_wire_fields() {
    for (field, value) in [
        ("version", json!(0)),
        ("version", json!(2)),
        ("version", json!(256)),
        ("version", json!("1")),
        ("kind", json!("paykit.payment_request")),
        ("kind", json!("paykit.future_kind")),
        ("app_id", json!("")),
        ("app_id", json!("remote/app")),
        ("app_id", json!("a".repeat(65))),
        ("event_id", json!("not-a-uuid")),
        ("event_id", json!("8a0d8b4c-913f-1e31-9f2c-2a6f5bb4d201")),
        ("event_id", json!("8a0d8b4c-913f-4e31-0f2c-2a6f5bb4d201")),
        ("payload_hash", json!(42)),
    ] {
        let mut wire = wire_value();
        wire[field] = value;
        assert!(
            matches!(
                parse_delivery_confirmation_json(&wire.to_string()),
                Err(PaykitError::InvalidData { .. })
            ),
            "accepted invalid {field}"
        );
    }
}

#[test]
fn test_delivery_confirmation_rejects_missing_null_unknown_and_duplicate_fields() {
    for field in ["version", "kind", "app_id", "event_id", "payload_hash"] {
        let mut wire = wire_value();
        wire.as_object_mut().unwrap().remove(field);
        assert!(parse_delivery_confirmation_json(&wire.to_string()).is_err());
        wire[field] = Value::Null;
        assert!(parse_delivery_confirmation_json(&wire.to_string()).is_err());
    }
    let mut wire = wire_value();
    wire["confirmation_id"] = json!(EVENT_ID);
    assert!(parse_delivery_confirmation_json(&wire.to_string()).is_err());

    let json = serialize_delivery_confirmation(&confirmation()).unwrap();
    let duplicate = json.replacen("{", "{\"event_id\":\"duplicate\",", 1);
    assert!(parse_delivery_confirmation_json(&duplicate).is_err());
    for malformed in ["", "{", "[]", "null"] {
        assert!(parse_delivery_confirmation_json(malformed).is_err());
    }
}

#[test]
fn test_delivery_confirmation_errors_redact_plaintext() {
    const SECRET: &str = "SENTINEL_DECRYPTED_PLAINTEXT";
    for field in ["version", "kind", "app_id", "event_id", "payload_hash"] {
        let mut wire = wire_value();
        wire[field] = json!(SECRET);
        let error = parse_delivery_confirmation_json(&wire.to_string()).unwrap_err();
        assert!(!format!("{error:?}").contains(SECRET));
        assert!(!error.to_string().contains(SECRET));
        assert!(matches!(
            error,
            PaykitError::InvalidData { source: None, .. }
        ));
    }
}

#[test]
fn test_delivery_confirmation_fits_unchanged_application_payload_limit() {
    let confirmation = DeliveryConfirmation::new(
        PaykitAppId::new("a".repeat(64)).unwrap(),
        EventId::new(EVENT_ID).unwrap(),
        PAYLOAD_HASH.into(),
    )
    .unwrap();
    let json = serialize_delivery_confirmation(&confirmation).unwrap();
    assert_eq!(pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN, 1000);
    assert!(json.len() <= pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN);
}

#[test]
fn test_delivery_confirmation_raw_kind_is_authoritative() {
    let mut message = PrivateApplicationMessage {
        version: Some(2),
        kind: Some(PrivateMessageKind::PaymentRequest.as_str().into()),
        app_id: Some("untrusted-cached-app".into()),
        raw_json: serialize_delivery_confirmation(&confirmation()).unwrap(),
    };
    assert_eq!(
        message.known_kind(),
        Some(PrivateMessageKind::DeliveryConfirmation)
    );
    assert!(parse_payment_request_event_message(&message).is_none());
    assert!(parse_receipt_access_event_message(&message).is_none());
    assert_eq!(
        parse_delivery_confirmation_json(&message.raw_json)
            .unwrap()
            .app_id()
            .as_str(),
        "confirming-app"
    );

    message.kind = Some(PrivateMessageKind::DeliveryConfirmation.as_str().into());
    for raw in [
        r#"{"kind":"paykit.future_kind"}"#,
        r#"{"kind":42}"#,
        "{}",
        "not json",
    ] {
        message.raw_json = raw.into();
        assert_eq!(message.known_kind(), None);
        assert!(parse_delivery_confirmation_json(raw).is_err());
    }
    message.raw_json = r#"{"kind":"paykit.payment_request"}"#.into();
    assert_eq!(
        message.known_kind(),
        Some(PrivateMessageKind::PaymentRequest)
    );
    assert!(parse_delivery_confirmation_json(&message.raw_json).is_err());
}
