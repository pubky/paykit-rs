use super::*;

const TEST_PUBLIC_KEY: &str = "5jsjx1o6fzu6aeeo697r3i5rx15zq41kikcye8wtwdqm4nb4tryo";
const TEST_CLIENT_ID: &str = "paykit.test";
const TEST_AUTH_SECRET: &str = "e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s";

fn scoped_capabilities() -> String {
    crate::PaykitSdkConfig::new(crate::PaykitReceiverPath::new("bitkit/wallet").unwrap())
        .required_session_capabilities()
}

fn sign_in_auth_url(capabilities: &str) -> String {
    format!(
        "pubkyauth://signin_grant?caps={capabilities}&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&cid={TEST_CLIENT_ID}&cpk={TEST_PUBLIC_KEY}"
    )
}

fn sign_in_auth_state_url(capabilities: &str, client_key_secret: &[u8; 32]) -> String {
    let client_public_key = pubky::Keypair::from_secret(client_key_secret).public_key();
    format!(
        "pubkyauth://signin_grant?caps={capabilities}&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&cid={TEST_CLIENT_ID}&cpk={}",
        client_public_key.z32()
    )
}

#[test]
fn test_parse_pubky_auth_url_sign_in_grant() {
    let details = parse_pubky_auth_url(&sign_in_auth_url("/:rw")).unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignIn);
    assert_eq!(details.capabilities, "/:rw");
    assert_eq!(details.relay_url, "https://httprelay.pubky.app/inbox/");
    assert_eq!(details.client_id, TEST_CLIENT_ID);
    assert!(details.homeserver_public_key.is_none());
}

#[test]
fn test_parse_pubky_auth_url_sign_up_grant() {
    let homeserver = PubkyPublicKey::new(TEST_PUBLIC_KEY).unwrap();
    let details = parse_pubky_auth_url(&format!(
        "pubkyauth://signup_grant?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}&hs={TEST_PUBLIC_KEY}&st=invite&cid={TEST_CLIENT_ID}&cpk={TEST_PUBLIC_KEY}"
    ))
    .unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignUp);
    assert_eq!(details.capabilities, "/:rw");
    assert_eq!(details.client_id, TEST_CLIENT_ID);
    assert_eq!(details.homeserver_public_key.as_ref(), Some(&homeserver));
    assert!(!format!("{details:?}").contains("invite"));
    assert!(!serde_json::to_string(&details).unwrap().contains("invite"));
}

#[test]
fn test_parse_pubky_auth_url_rejects_invalid_url() {
    assert!(parse_pubky_auth_url("not-an-auth-url").is_err());
    assert!(parse_pubky_auth_url(&sign_in_auth_url("/:rw,not-a-capability")).is_err());
}

#[test]
fn test_parse_pubky_auth_url_requires_grant_auth() {
    assert!(parse_pubky_auth_url(&format!(
        "pubkyauth://signin?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret={TEST_AUTH_SECRET}"
    ))
    .is_err());
}

#[test]
fn test_parse_pubky_auth_url_rejects_duplicate_grant_parameters() {
    let auth_url = format!("{}&cid=other.test", sign_in_auth_url("/:rw"));

    assert!(parse_pubky_auth_url(&auth_url).is_err());
}

#[test]
fn test_parse_pubky_resource_normalizes_uri() {
    let public_key = PubkyLocalSecretKey::new([4; 32]).public_key();
    let resource = parse_pubky_resource(&format!(
        "pubky://{}/pub/paykit/v0/paykit/wallet/profile.json",
        public_key.as_str()
    ))
    .unwrap();

    assert_eq!(resource.public_key, public_key);
    assert_eq!(resource.path, "/pub/paykit/v0/paykit/wallet/profile.json");
    assert!(resource.transport_url.starts_with("https://"));
}

#[test]
fn test_parse_pubky_resource_accepts_pubky_identifier_form() {
    let public_key = PubkyLocalSecretKey::new([5; 32]).public_key();
    let resource = parse_pubky_resource(&format!(
        "pubky{}/pub/paykit/v0/paykit/wallet/profile.json",
        public_key.as_str()
    ))
    .unwrap();

    assert_eq!(resource.public_key, public_key);
    assert_eq!(resource.path, "/pub/paykit/v0/paykit/wallet/profile.json");
}

#[test]
fn test_parse_pubky_resource_rejects_missing_path() {
    let public_key = PubkyLocalSecretKey::new([6; 32]).public_key();

    assert!(parse_pubky_resource(public_key.as_str()).is_err());
}

#[test]
fn test_parse_capabilities_rejects_invalid_entries() {
    let capabilities = scoped_capabilities();
    assert_eq!(
        parse_capabilities(&capabilities).unwrap().to_string(),
        capabilities
    );
    assert_eq!(
        capabilities,
        "/pub/paykit/v0/bitkit/wallet/:rw,/pub/paykit/v0/private/bitkit/wallet/:rw"
    );
    assert!(parse_capabilities("/:rw,not-a-capability").is_err());
    assert!(parse_capabilities("/:rw,").is_err());
    assert!(parse_capabilities(",/:rw").is_err());
    assert!(parse_capabilities("").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_requires_exact_match() {
    let capabilities = scoped_capabilities();
    let auth_url = sign_in_auth_url(&capabilities);

    assert!(validate_auth_url_capabilities(&auth_url, &capabilities).is_ok());
    assert!(validate_auth_url_capabilities(&auth_url, "/:rw").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_rejects_broad_capabilities() {
    let capabilities = scoped_capabilities();
    let auth_url = sign_in_auth_url("/:rw");

    assert!(validate_auth_url_capabilities(&auth_url, &capabilities).is_err());
}

#[test]
fn test_validate_local_secret_for_public_key_rejects_mismatch() {
    let session_key = PubkyLocalSecretKey::new([7; 32]).public_key();
    let matching_secret = PubkyLocalSecretKey::new([7; 32]);
    let wrong_secret = PubkyLocalSecretKey::new([8; 32]);

    assert!(validate_local_secret_for_public_key(&session_key, matching_secret).is_ok());
    assert!(validate_local_secret_for_public_key(&session_key, wrong_secret).is_err());
}

#[test]
fn test_pubky_session_secret_debug_is_redacted() {
    let secret = PubkySessionSecret::new("pubky-session-secret".into());

    assert_eq!(format!("{secret:?}"), "PubkySessionSecret(<redacted>)");
    assert_eq!(secret.as_str(), "pubky-session-secret");
    assert_eq!(secret.into_inner(), "pubky-session-secret");
}

#[test]
fn test_pubky_auth_request_state_redacts_secrets() {
    let state =
        PubkyAuthRequestState::new(sign_in_auth_state_url("/:rw", &[42; 32]), [42; 32]).unwrap();
    let debug = format!("{state:?}");

    assert!(!debug.contains(TEST_AUTH_SECRET));
    assert!(!debug.contains("42"));
    assert_eq!(state.client_key_secret(), &[42; 32]);
}

#[test]
fn test_pubky_auth_request_state_rejects_mismatched_client_key() {
    let auth_url = sign_in_auth_state_url("/:rw", &[42; 32]);

    assert!(PubkyAuthRequestState::new(auth_url, [43; 32]).is_err());
}
