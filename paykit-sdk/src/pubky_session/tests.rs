use super::*;

fn scoped_capabilities() -> String {
    crate::PaykitSdkConfig::new(crate::PaykitReceiverId::new("bitkit").unwrap())
        .required_session_capabilities()
}

#[test]
fn test_parse_pubky_auth_url_sign_in() {
    let details = parse_pubky_auth_url(
        "pubkyauth://signin?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s",
    )
    .unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignIn);
    assert_eq!(details.capabilities.as_deref(), Some("/:rw"));
    assert_eq!(
        details.relay_url.as_deref(),
        Some("https://httprelay.pubky.app/inbox/")
    );
    assert!(details.homeserver_public_key.is_none());
}

#[test]
fn test_parse_pubky_auth_url_sign_up() {
    let homeserver =
        PubkyPublicKey::new("5jsjx1o6fzu6aeeo697r3i5rx15zq41kikcye8wtwdqm4nb4tryo").unwrap();
    let details = parse_pubky_auth_url(
        "pubkyauth://signup?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s&hs=5jsjx1o6fzu6aeeo697r3i5rx15zq41kikcye8wtwdqm4nb4tryo&st=invite",
    )
    .unwrap();

    assert_eq!(details.kind, PubkyAuthRequestKind::SignUp);
    assert_eq!(details.capabilities.as_deref(), Some("/:rw"));
    assert_eq!(details.homeserver_public_key.as_ref(), Some(&homeserver));
    assert!(!format!("{details:?}").contains("invite"));
    assert!(!serde_json::to_string(&details).unwrap().contains("invite"));
}

#[test]
fn test_parse_pubky_auth_url_rejects_invalid_url() {
    assert!(parse_pubky_auth_url("not-an-auth-url").is_err());
    assert!(
        parse_pubky_auth_url(
            "pubkyauth://signin?caps=/:rw,not-a-capability&relay=https://httprelay.pubky.app/inbox/&secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s",
        )
        .is_err()
    );
}

#[test]
fn test_parse_pubky_resource_normalizes_uri() {
    let public_key = PubkyLocalSecretKey::new([4; 32]).public_key();
    let resource = parse_pubky_resource(&format!(
        "pubky://{}/pub/paykit/v0/receivers/paykit/profile.json",
        public_key.as_str()
    ))
    .unwrap();

    assert_eq!(resource.public_key, public_key);
    assert_eq!(
        resource.path,
        "/pub/paykit/v0/receivers/paykit/profile.json"
    );
    assert!(resource.transport_url.starts_with("https://"));
}

#[test]
fn test_parse_pubky_resource_accepts_pubky_identifier_form() {
    let public_key = PubkyLocalSecretKey::new([5; 32]).public_key();
    let resource = parse_pubky_resource(&format!(
        "pubky{}/pub/paykit/v0/receivers/paykit/profile.json",
        public_key.as_str()
    ))
    .unwrap();

    assert_eq!(resource.public_key, public_key);
    assert_eq!(
        resource.path,
        "/pub/paykit/v0/receivers/paykit/profile.json"
    );
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
        "/pub/paykit/v0/receivers/bitkit/:rw,/pub/paykit/v0/private/bitkit/:rw"
    );
    assert!(parse_capabilities("/:rw,not-a-capability").is_err());
    assert!(parse_capabilities("/:rw,").is_err());
    assert!(parse_capabilities(",/:rw").is_err());
    assert!(parse_capabilities("").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_requires_exact_match() {
    let capabilities = scoped_capabilities();
    let auth_url =
        "pubkyauth://signin?caps=/pub/paykit/v0/receivers/bitkit/:rw,/pub/paykit/v0/private/bitkit/:rw&relay=https://httprelay.pubky.app/inbox/&secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s";

    assert!(validate_auth_url_capabilities(auth_url, &capabilities).is_ok());
    assert!(validate_auth_url_capabilities(auth_url, "/:rw").is_err());
}

#[test]
fn test_validate_auth_url_capabilities_rejects_broad_capabilities() {
    let capabilities = scoped_capabilities();
    let auth_url =
        "pubkyauth://signin?caps=/:rw&relay=https://httprelay.pubky.app/inbox/&secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s";

    assert!(validate_auth_url_capabilities(auth_url, &capabilities).is_err());
}

#[test]
fn test_validate_session_auth_url_rejects_secret_export() {
    assert!(validate_sign_in_or_sign_up_auth_url(
        "pubkyauth://secret_export?secret=e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3t7e3s",
    )
    .is_err());
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
