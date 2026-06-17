use super::*;

#[test]
fn test_config_validate_rejects_zero_timeouts() {
    let config = PaykitSdkConfig {
        peer_link_operation_lease_timeout: Duration::ZERO,
        ..PaykitSdkConfig::default()
    };

    assert!(config.validate().is_err());
}

#[test]
fn test_config_validate_rejects_oversized_timeouts() {
    let config = PaykitSdkConfig {
        outbound_private_send_lease_timeout: Duration::MAX,
        ..PaykitSdkConfig::default()
    };

    assert!(config.validate().is_err());
}

#[test]
fn test_config_builds_default_profile_namespace_paths() {
    let config = PaykitSdkConfig::default();
    let public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    assert_eq!(config.paykit_profile_path(), "/pub/paykit/profile.json");
    assert_eq!(
        config.paykit_profile_blob_path_prefix(),
        "/pub/paykit/blobs/"
    );
    assert_eq!(
        config.public_contact_path(&public_key),
        format!("/pub/paykit/contacts/{}.json", public_key.as_str())
    );
    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/:rw,/pub/paykit/profile.json:rw,/pub/paykit/blobs/:rw"
    );
}

#[test]
fn test_config_allows_custom_profile_namespace_segment() {
    let config = PaykitSdkConfig {
        profile_namespace: "bitkit.to".into(),
        ..PaykitSdkConfig::default()
    };

    config.validate().unwrap();
    assert_eq!(config.paykit_profile_path(), "/pub/bitkit.to/profile.json");
    assert_eq!(
        config.paykit_profile_blob_path_prefix(),
        "/pub/bitkit.to/blobs/"
    );
    assert_eq!(
        config.public_contact_path_prefix(),
        "/pub/bitkit.to/contacts/"
    );
    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/:rw,/pub/bitkit.to/profile.json:rw,/pub/bitkit.to/blobs/:rw"
    );
}

#[test]
fn test_config_adds_contact_capability_when_public_contact_sharing_enabled() {
    let config = PaykitSdkConfig {
        public_contact_sharing: PublicContactSharingPolicy::ConfiguredPublicNamespace,
        ..PaykitSdkConfig::default()
    };

    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/:rw,/pub/paykit/profile.json:rw,/pub/paykit/blobs/:rw,/pub/paykit/contacts/:rw"
    );
}

#[test]
fn test_config_rejects_profile_namespace_path_segments() {
    let config = PaykitSdkConfig {
        profile_namespace: "bitkit/to".into(),
        ..PaykitSdkConfig::default()
    };

    assert!(config.validate().is_err());
}

#[test]
fn test_config_rejects_pubky_app_profile_namespace() {
    let config = PaykitSdkConfig {
        profile_namespace: "pubky.app".into(),
        ..PaykitSdkConfig::default()
    };

    assert!(config.validate().is_err());
}
