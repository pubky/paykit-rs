use serde_json::json;

use super::*;

fn capabilities() -> PaykitAppCapabilities {
    PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    }
}

fn public_only_capabilities() -> PaykitAppCapabilities {
    PaykitAppCapabilities {
        private_payments: false,
        payment_requests: false,
        receipts: false,
        outgoing_payments: true,
    }
}

fn registry() -> PaykitAppRegistry {
    let mut registry =
        PaykitAppRegistry::new(Some(pubky::Keypair::from_secret(&[7; 32]).public_key()));
    registry
        .register_app(
            PaykitAppId::new("bitkit").unwrap(),
            PaykitApp::new("Bitkit", capabilities()).unwrap(),
        )
        .unwrap();
    registry
        .set_default_app(Some(PaykitAppId::new("bitkit").unwrap()))
        .unwrap();
    registry
        .set_default_app_for_endpoint(
            PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
            PaykitAppId::new("bitkit").unwrap(),
        )
        .unwrap();
    registry
}

fn registry_json_with(edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut document: serde_json::Value =
        serde_json::from_str(&serialize_paykit_app_registry(&registry()).unwrap()).unwrap();
    edit(&mut document);
    document.to_string()
}

fn assert_registry_error_redacts(raw: &str, expected_context: &str, sentinel: &str) -> PaykitError {
    let error = parse_paykit_app_registry_json(raw).unwrap_err();
    assert!(
        matches!(error, PaykitError::InvalidData { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains(expected_context), "{message:?}");
    assert!(!message.contains(sentinel), "{message:?}");
    assert!(!message.chars().any(char::is_control), "{message:?}");
    assert!(!format!("{error:?}").contains(sentinel), "{error:?}");
    error
}

#[test]
fn test_app_registry_json_round_trips() {
    let registry = registry();
    let json = serialize_paykit_app_registry(&registry).unwrap();
    let parsed = parse_paykit_app_registry_json(&json).unwrap();

    assert_eq!(parsed, registry);
    let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["kind"], "paykit.app_registry");
    assert_eq!(value["key_generation"], 1);
    assert_eq!(value["apps"]["bitkit"]["display_name"], "Bitkit");
    assert_eq!(
        value["default_apps_by_endpoint"]["btc-lightning-bolt11"],
        "bitkit"
    );
}

#[test]
fn test_public_only_registry_round_trips_without_noise_key() {
    let mut registry = PaykitAppRegistry::new(None);
    registry
        .register_app(
            PaykitAppId::new("public-wallet").unwrap(),
            PaykitApp::new("Public Wallet", public_only_capabilities()).unwrap(),
        )
        .unwrap();

    let json = serialize_paykit_app_registry(&registry).unwrap();
    let parsed = parse_paykit_app_registry_json(&json).unwrap();

    assert!(parsed.noise_public_key().is_none());
    assert_eq!(parsed, registry);
}

#[test]
fn test_registry_requires_noise_key_for_private_capabilities() {
    let mut registry = PaykitAppRegistry::new(None);

    let error = registry
        .register_app(
            PaykitAppId::new("bitkit").unwrap(),
            PaykitApp::new("Bitkit", capabilities()).unwrap(),
        )
        .unwrap_err();

    assert!(matches!(error, PaykitError::Validation(_)));
}

#[test]
fn test_registry_initializes_later_key_generation() {
    let mut registry = PaykitAppRegistry::new(None);
    let key = pubky::Keypair::random().public_key();
    assert!(registry.set_noise_public_key(key.clone(), 0).is_err());
    registry.set_noise_public_key(key.clone(), 3).unwrap();
    registry.set_noise_public_key(key.clone(), 3).unwrap();
    assert!(registry.set_noise_public_key(key, 1).is_err());
    assert_eq!(registry.key_generation(), 3);
    let json = serialize_paykit_app_registry(&registry).unwrap();
    assert_eq!(parse_paykit_app_registry_json(&json).unwrap(), registry);
    registry
        .rotate_noise_public_key(pubky::Keypair::random().public_key(), 4)
        .unwrap();
}

#[test]
fn test_remote_registry_reports_invalid_app_without_noise_key() {
    let raw = json!({
        "version": APP_REGISTRY_VERSION,
        "kind": APP_REGISTRY_KIND,
        "key_generation": INITIAL_PAYKIT_KEY_GENERATION,
        "noise_public_key": null,
        "apps": {
            "bitkit": {
                "display_name": "Bitkit",
                "capabilities": capabilities(),
            }
        },
        "default_app_id": null,
        "default_apps_by_endpoint": {},
    })
    .to_string();

    let error = parse_paykit_app_registry_json(&raw).unwrap_err();
    let message = error.to_string();

    assert!(message.contains("invalid application"));
    assert!(!message.contains("too many applications"));
}

#[test]
fn test_app_registry_parse_errors_redact_remote_payload() {
    let sentinel = "SENTINEL_REMOTE_REGISTRY";
    let payload = format!("{sentinel}\n\u{1b}");
    let app = json!({ "display_name": "Bitkit", "capabilities": capabilities() });
    let redacted_cases = [
        (
            "JSON is invalid",
            registry_json_with(|doc| doc["version"] = json!(payload)),
        ),
        (
            "JSON is invalid",
            registry_json_with(|doc| doc[payload.as_str()] = json!(1)),
        ),
        (
            "version/kind",
            registry_json_with(|doc| doc["kind"] = json!(payload)),
        ),
        (
            "invalid App ID",
            registry_json_with(|doc| doc["apps"][payload.as_str()] = app),
        ),
        (
            "invalid App ID",
            registry_json_with(|doc| doc["default_app_id"] = json!(payload)),
        ),
        (
            "invalid Payment Endpoint Identifier",
            registry_json_with(|doc| {
                doc["default_apps_by_endpoint"][payload.as_str()] = json!("bitkit")
            }),
        ),
        (
            "invalid App ID",
            registry_json_with(|doc| {
                doc["default_apps_by_endpoint"]["btc-lightning-bolt11"] = json!(payload)
            }),
        ),
        (
            "unregistered App ID",
            registry_json_with(|doc| doc["default_apps_by_endpoint"][sentinel] = json!("ghost")),
        ),
    ];
    // These checks keep an upstream cause whose Display is static text.
    let static_cause_cases = [
        (
            "Noise public key is invalid",
            registry_json_with(|doc| doc["noise_public_key"] = json!(payload)),
        ),
        (
            "display name",
            registry_json_with(|doc| doc["apps"]["bitkit"]["display_name"] = json!(payload)),
        ),
    ];

    for (expected_context, raw) in redacted_cases {
        let error = assert_registry_error_redacts(&raw, expected_context, sentinel);
        assert!(
            matches!(error, PaykitError::InvalidData { source: None, .. }),
            "{error:?}"
        );
    }
    for (expected_context, raw) in static_cause_cases {
        assert_registry_error_redacts(&raw, expected_context, sentinel);
    }
}

#[test]
fn test_app_registry_json_uses_stable_key_order() {
    let mut registry = registry();
    registry
        .register_app(
            PaykitAppId::new("alpha").unwrap(),
            PaykitApp::new("Alpha", capabilities()).unwrap(),
        )
        .unwrap();

    let first = serialize_paykit_app_registry(&registry).unwrap();
    let second = serialize_paykit_app_registry(&registry).unwrap();

    assert_eq!(first, second);
    assert!(first.find("\"alpha\"").unwrap() < first.find("\"bitkit\"").unwrap());
}

#[test]
fn test_app_registry_rejects_unregistered_defaults() {
    let raw = json!({
        "version": 1,
        "kind": "paykit.app_registry",
        "key_generation": INITIAL_PAYKIT_KEY_GENERATION,
        "noise_public_key": pubky::Keypair::from_secret(&[7; 32]).public_key().z32(),
        "apps": {},
        "default_app_id": "missing",
        "default_apps_by_endpoint": {}
    })
    .to_string();

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_app_registry_rejects_zero_key_generation() {
    let raw = json!({
        "version": APP_REGISTRY_VERSION,
        "kind": APP_REGISTRY_KIND,
        "key_generation": 0,
        "noise_public_key": null,
        "apps": {},
        "default_app_id": null,
        "default_apps_by_endpoint": {},
    })
    .to_string();

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_app_registry_prefers_endpoint_default_over_identity_default() {
    let mut registry = registry();
    registry
        .register_app(
            PaykitAppId::new("server").unwrap(),
            PaykitApp::new("Server", capabilities()).unwrap(),
        )
        .unwrap();
    let lightning = PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap();
    let onchain = PaymentEndpointIdentifier::new("btc-bitcoin-p2tr").unwrap();
    registry
        .set_default_app_for_endpoint(lightning.clone(), PaykitAppId::new("server").unwrap())
        .unwrap();

    assert_eq!(
        registry
            .preferred_app_for_endpoint(&lightning)
            .map(PaykitAppId::as_str),
        Some("server")
    );
    assert_eq!(
        registry
            .preferred_app_for_endpoint(&onchain)
            .map(PaykitAppId::as_str),
        Some("bitkit")
    );
}

#[test]
fn test_app_registry_rejects_duplicate_apps() {
    let public_key = pubky::Keypair::from_secret(&[7; 32]).public_key().z32();
    let raw = format!(
        r#"{{"version":1,"kind":"paykit.app_registry","key_generation":1,"noise_public_key":"{public_key}","apps":{{"bitkit":{{"display_name":"Bitkit","capabilities":{{"private_payments":true,"payment_requests":true,"receipts":true,"outgoing_payments":true}}}},"bitkit":{{"display_name":"Other","capabilities":{{"private_payments":true,"payment_requests":true,"receipts":true,"outgoing_payments":true}}}}}},"default_app_id":null,"default_apps_by_endpoint":{{}}}}"#
    );

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_remove_app_clears_defaults() {
    let mut registry = registry();
    let app_id = PaykitAppId::new("bitkit").unwrap();
    registry.remove_app(&app_id).unwrap();

    assert!(registry.default_app_id().is_none());
    assert!(registry.default_apps_by_endpoint().is_empty());
}

#[test]
fn test_app_registry_rejects_too_many_local_apps() {
    let mut registry =
        PaykitAppRegistry::new(Some(pubky::Keypair::from_secret(&[7; 32]).public_key()));
    for index in 0..PAYKIT_APP_REGISTRY_MAX_APPS {
        registry
            .register_app(
                PaykitAppId::new(format!("app-{index}")).unwrap(),
                PaykitApp::new(format!("App {index}"), capabilities()).unwrap(),
            )
            .unwrap();
    }

    let result = registry.register_app(
        PaykitAppId::new("one-too-many").unwrap(),
        PaykitApp::new("One Too Many", capabilities()).unwrap(),
    );

    assert!(matches!(result, Err(PaykitError::Validation(_))));
}

#[test]
fn test_app_registry_rejects_too_many_remote_apps() {
    let mut apps = serde_json::Map::new();
    for index in 0..=PAYKIT_APP_REGISTRY_MAX_APPS {
        apps.insert(
            format!("app-{index}"),
            json!({
                "display_name": format!("App {index}"),
                "capabilities": capabilities(),
            }),
        );
    }
    let raw = json!({
        "version": APP_REGISTRY_VERSION,
        "kind": APP_REGISTRY_KIND,
        "key_generation": INITIAL_PAYKIT_KEY_GENERATION,
        "noise_public_key": pubky::Keypair::from_secret(&[7; 32]).public_key().z32(),
        "apps": apps,
        "default_app_id": null,
        "default_apps_by_endpoint": {},
    })
    .to_string();

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}

#[test]
fn test_app_registry_rotates_noise_key_only_to_next_generation() {
    let mut registry = registry();
    let replacement = pubky::Keypair::from_secret(&[8; 32]).public_key();

    registry
        .rotate_noise_public_key(replacement.clone(), 2)
        .unwrap();

    assert_eq!(registry.key_generation(), 2);
    assert_eq!(registry.noise_public_key(), Some(&replacement));
    assert!(registry
        .rotate_noise_public_key(pubky::Keypair::from_secret(&[9; 32]).public_key(), 4)
        .is_err());
}

#[test]
fn test_app_registry_rejects_oversized_json() {
    let raw = " ".repeat(PAYKIT_APP_REGISTRY_MAX_BYTES + 1);

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}
