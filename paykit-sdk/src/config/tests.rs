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
