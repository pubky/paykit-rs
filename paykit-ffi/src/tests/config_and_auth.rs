use std::sync::Arc;

use crate::*;
use paykit_sdk::PaykitSdkConfig;

const TEST_CLIENT_ID: &str = "paykit.test";
const TEST_AUTH_SECRET: &str = "e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s";

fn grant_auth_url(client_key_secret: &[u8; 32]) -> String {
    let client_public_key = pubky::Keypair::from_secret(client_key_secret).public_key();
    format!(
        "pubkyauth://signin_grant?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&cid={TEST_CLIENT_ID}&cpk={}",
        client_public_key.z32()
    )
}

#[test]
fn test_default_config_round_trips_to_sdk_config() {
    let ffi = default_config("bitkit".into()).unwrap();
    let sdk = PaykitSdkConfig::try_from(ffi.clone()).unwrap();
    let round_trip = FfiPaykitSdkConfig::from(sdk);

    assert_eq!(ffi, round_trip);
}

#[test]
fn test_config_rejects_invalid_app_ids_as_validation_errors() {
    for app_id in ["", "bitkit/wallet", "..", "UPPERCASE"] {
        let mut explicit = default_config("bitkit".into()).unwrap();
        explicit.app_id = app_id.into();

        for error in [
            default_config(app_id.into()).unwrap_err(),
            PaykitSdkConfig::try_from(explicit).unwrap_err(),
        ] {
            assert!(matches!(error, PaykitFfiError::Protocol { code, .. } if code == "validation"));
        }
    }
}

#[test]
fn test_default_pubky_client_config_uses_production() {
    let config = default_pubky_client_config();

    assert!(config.local_testnet_host.is_none());
    assert!(config.auth_relay_url.is_none());
    assert!(pubky_from_config(&config).is_ok());
}

#[test]
fn test_pubky_client_config_validates_auth_relay_url() {
    let mut config = default_pubky_client_config();
    config.auth_relay_url = Some("http://127.0.0.1:15412/inbox".into());

    assert!(FfiPubkySessionBootstrap::with_pubky_client_config(
        TEST_CLIENT_ID.into(),
        config.clone(),
    )
    .is_ok());

    config.auth_relay_url = Some("file:///tmp/relay".into());
    let error = FfiPubkySessionBootstrap::with_pubky_client_config(TEST_CLIENT_ID.into(), config)
        .err()
        .expect("non-HTTP auth relay must be rejected");
    assert!(error.to_string().contains("auth relay"));
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

#[tokio::test]
async fn test_pubky_client_config_routes_auth_to_local_testnet_relay() {
    let mut config = default_pubky_client_config();
    config.local_testnet_host = Some("10.0.2.2".into());
    let bootstrap =
        FfiPubkySessionBootstrap::with_pubky_client_config(TEST_CLIENT_ID.into(), config).unwrap();

    let request = bootstrap.start_sign_in_auth("/:rw".into()).await.unwrap();
    let details = parse_pubky_auth_url(request.authorization_url().await.unwrap()).unwrap();

    assert_eq!(details.relay_url, "http://10.0.2.2:15412/inbox/");
}

#[tokio::test]
async fn test_pubky_client_config_explicit_auth_relay_overrides_local_testnet() {
    let mut config = default_pubky_client_config();
    config.local_testnet_host = Some("10.0.2.2".into());
    config.auth_relay_url = Some("http://relay.example:19000/inbox/".into());
    let bootstrap =
        FfiPubkySessionBootstrap::with_pubky_client_config(TEST_CLIENT_ID.into(), config).unwrap();

    let request = bootstrap.start_sign_in_auth("/:rw".into()).await.unwrap();
    let details = parse_pubky_auth_url(request.authorization_url().await.unwrap()).unwrap();

    assert_eq!(details.relay_url, "http://relay.example:19000/inbox/");
}

#[tokio::test]
async fn test_republish_identity_rejects_invalid_public_key() {
    let bootstrap = FfiPubkySessionBootstrap::new("paykit.test".into()).unwrap();

    assert!(bootstrap
        .republish_identity("not-a-public-key".into())
        .await
        .is_err());
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
            Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
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
            Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
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
fn test_pubky_auth_request_state_round_trips_and_redacts_secrets() {
    let authorization_url = grant_auth_url(&[42; 32]);
    let state = FfiPubkyAuthRequestState::new(authorization_url.clone(), vec![42; 32]).unwrap();

    assert_eq!(state.authorization_url(), authorization_url);
    assert_eq!(state.export_client_key_secret(), vec![42; 32]);
    assert_eq!(format!("{state:?}"), "FfiPubkyAuthRequestState(<redacted>)");
}

#[test]
fn test_pubky_auth_request_state_rejects_invalid_or_mismatched_secrets() {
    let authorization_url = grant_auth_url(&[42; 32]);

    assert!(FfiPubkyAuthRequestState::new(authorization_url.clone(), vec![42; 31]).is_err());
    assert!(FfiPubkyAuthRequestState::new(authorization_url, vec![43; 32]).is_err());
}

#[tokio::test]
async fn test_pubky_auth_request_complete_consumes_request_before_key_validation() {
    let bootstrap = FfiPubkySessionBootstrap::new(TEST_CLIENT_ID.into()).unwrap();
    let request = bootstrap.start_sign_in_auth("/:rw".into()).await.unwrap();

    assert!(request
        .complete(
            Some(Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 31]))),
            "/:rw".into(),
        )
        .await
        .is_err());
    assert!(request.authorization_url().await.is_err());
}

#[test]
fn test_session_access_rejects_invalid_client_id() {
    assert!(
        FfiPubkySessionAccess::new(String::new(), "session-secret".into(), None, None).is_err()
    );
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
