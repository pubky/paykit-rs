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

fn registry() -> PaykitAppRegistry {
    let mut registry = PaykitAppRegistry::new(pubky::Keypair::from_secret(&[7; 32]).public_key());
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

#[test]
fn test_app_registry_json_round_trips() {
    let registry = registry();
    let json = serialize_paykit_app_registry(&registry).unwrap();
    let parsed = parse_paykit_app_registry_json(&json).unwrap();

    assert_eq!(parsed, registry);
    let value = serde_json::from_str::<serde_json::Value>(&json).unwrap();
    assert_eq!(value["version"], 1);
    assert_eq!(value["kind"], "paykit.app_registry");
    assert_eq!(value["apps"]["bitkit"]["display_name"], "Bitkit");
    assert_eq!(
        value["default_apps_by_endpoint"]["btc-lightning-bolt11"],
        "bitkit"
    );
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
        r#"{{"version":1,"kind":"paykit.app_registry","noise_public_key":"{public_key}","apps":{{"bitkit":{{"display_name":"Bitkit","capabilities":{{"private_payments":true,"payment_requests":true,"receipts":true,"outgoing_payments":true}}}},"bitkit":{{"display_name":"Other","capabilities":{{"private_payments":true,"payment_requests":true,"receipts":true,"outgoing_payments":true}}}}}},"default_app_id":null,"default_apps_by_endpoint":{{}}}}"#
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
    let mut registry = PaykitAppRegistry::new(pubky::Keypair::from_secret(&[7; 32]).public_key());
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
fn test_app_registry_rejects_oversized_json() {
    let raw = " ".repeat(PAYKIT_APP_REGISTRY_MAX_BYTES + 1);

    assert!(matches!(
        parse_paykit_app_registry_json(&raw),
        Err(PaykitError::InvalidData { .. })
    ));
}
