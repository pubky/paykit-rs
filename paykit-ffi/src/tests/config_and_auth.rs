use crate::*;
use paykit_sdk::PaykitSdkConfig;

#[test]
fn test_default_config_round_trips_to_sdk_config() {
    let ffi = default_config("bitkit".into()).unwrap();
    let sdk = PaykitSdkConfig::try_from(ffi.clone()).unwrap();
    let round_trip = FfiPaykitSdkConfig::from(sdk);

    assert_eq!(ffi, round_trip);
}

#[test]
fn test_default_pubky_client_config_uses_production() {
    let config = default_pubky_client_config();

    assert!(config.local_testnet_host.is_none());
    assert!(pubky_from_config(&config).is_ok());
}

#[test]
fn test_pubky_client_config_accepts_local_testnet() {
    let mut config = default_pubky_client_config();
    config.local_testnet_host = Some("10.0.2.2".into());

    let result = pubky_from_config(&config);
    assert!(
        result.is_ok(),
        "expected local testnet client, got: {result:?}"
    );
}

#[test]
fn test_pubky_client_config_rejects_invalid_local_testnet_host() {
    for host in ["", " not-a-host", "not a host", "::1"] {
        let mut config = default_pubky_client_config();
        config.local_testnet_host = Some(host.into());

        let err = pubky_from_config(&config).unwrap_err();

        assert!(
            err.to_string().contains("local testnet host is invalid"),
            "expected validation error for {host:?}, got: {err}"
        );
    }
}

#[test]
fn test_required_capabilities_use_identity_wide_paykit_scope() {
    let capabilities = required_session_capabilities();

    assert_eq!(capabilities, "/pub/paykit/:rw");
}

#[tokio::test]
async fn test_pubky_auth_companion_claim_reports_invalid_auth_url() {
    let bootstrap = FfiPubkySessionBootstrap::new("paykit.test".into()).unwrap();
    let error = bootstrap
        .approve_auth_with_companion_claim(
            "https://example.com/not-pubky-auth".into(),
            "/pub/example/account/:rw".into(),
            std::sync::Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
            FfiPubkyAuthCompanionClaim {
                query_parameter: "x-example-claim".into(),
                claim_type: "account-export-v1".into(),
                unsigned_payload: vec![1, 2, 3],
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        FfiPubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. }
    ));
}

#[tokio::test]
async fn test_pubky_auth_companion_claim_reports_invalid_claim() {
    let bootstrap = FfiPubkySessionBootstrap::new("paykit.test".into()).unwrap();
    let error = bootstrap
        .approve_auth_with_companion_claim(
            "pubkyauth://signin".into(),
            "/pub/example/account/:rw".into(),
            std::sync::Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
            FfiPubkyAuthCompanionClaim {
                query_parameter: "x-example|claim".into(),
                claim_type: "account-export-v1".into(),
                unsigned_payload: vec![],
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        FfiPubkyAuthCompanionClaimApprovalError::InvalidClaim { .. }
    ));
}

#[test]
fn test_pubky_auth_companion_claim_debug_redacts_unsigned_payload() {
    let claim = FfiPubkyAuthCompanionClaim {
        query_parameter: "x-example-claim".into(),
        claim_type: "account-export-v1".into(),
        unsigned_payload: vec![222, 173, 190, 239],
    };

    let debug = format!("{claim:?}");

    assert!(debug.contains("x-example-claim"));
    assert!(debug.contains("account-export-v1"));
    assert!(debug.contains("<redacted:4 bytes>"));
    assert!(!debug.contains("[222, 173, 190, 239]"));
}

#[test]
fn test_pubky_auth_companion_claim_unexpected_error_is_delivery_neutral() {
    let error = FfiPubkyAuthCompanionClaimApprovalError::Unexpected {
        reason: "unrecognized SDK companion claim approval failure".into(),
    };

    let display = error.to_string();

    assert!(display.contains("unexpected"));
    assert!(!display.contains("after companion delivery"));
}
