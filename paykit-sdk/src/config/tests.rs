use super::*;

fn receiver_config(receiver_path: &str) -> PaykitSdkConfig {
    PaykitSdkConfig::new(PaykitReceiverPath::new(receiver_path).unwrap())
}

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
    let config = receiver_config("bitkit/wallet");
    let public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());

    assert_eq!(
        config.paykit_profile_path(),
        "/pub/paykit/v0/bitkit/wallet/profile.json"
    );
    assert_eq!(
        config.paykit_profile_path_for_receiver(&PaykitReceiverPath::new("tether/wallet").unwrap()),
        "/pub/paykit/v0/tether/wallet/profile.json"
    );
    assert_eq!(
        config.paykit_profile_blob_path_prefix(),
        "/pub/paykit/v0/bitkit/wallet/blobs/"
    );
    assert_eq!(
        config.public_contact_path(
            &public_key,
            &PaykitReceiverPath::new("tether/wallet").unwrap()
        ),
        format!(
            "/pub/paykit/v0/bitkit/wallet/contacts/{}/tether/wallet.json",
            public_key.as_str()
        )
    );
    assert_eq!(
        config.receipt_path_prefix(),
        "/pub/paykit/v0/private/bitkit/wallet/receipts"
    );
    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/bitkit/wallet/:rw,/pub/paykit/v0/private/bitkit/wallet/:rw"
    );
}

#[test]
fn test_config_allows_custom_profile_namespace_segment() {
    let config = PaykitSdkConfig {
        profile_namespace: "bitkit.to".into(),
        ..receiver_config("bitkit/wallet")
    };

    config.validate().unwrap();
    assert_eq!(
        config.paykit_profile_path(),
        "/pub/bitkit.to/bitkit/wallet/profile.json"
    );
    assert_eq!(
        config.paykit_profile_path_for_receiver(&PaykitReceiverPath::new("tether/wallet").unwrap()),
        "/pub/bitkit.to/tether/wallet/profile.json"
    );
    assert_eq!(
        config.paykit_profile_blob_path_prefix(),
        "/pub/bitkit.to/bitkit/wallet/blobs/"
    );
    assert_eq!(
        config.public_contact_path_prefix(),
        "/pub/bitkit.to/bitkit/wallet/contacts/"
    );
    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/bitkit/wallet/:rw,/pub/paykit/v0/private/bitkit/wallet/:rw,/pub/bitkit.to/bitkit/wallet/:rw"
    );
}

#[test]
fn test_config_uses_namespace_capability_when_public_contact_sharing_enabled() {
    let config = PaykitSdkConfig {
        public_contact_sharing: PublicContactSharingPolicy::ConfiguredPublicNamespace,
        ..PaykitSdkConfig::default()
    };

    assert_eq!(
        config.required_session_capabilities(),
        "/pub/paykit/v0/test/wallet/:rw,/pub/paykit/v0/private/test/wallet/:rw"
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
