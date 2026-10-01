use super::*;

#[test]
fn test_config_defaults_are_conservative() {
    let config = PaykitSdkConfig::new("test-app").unwrap();

    assert_eq!(config.app_id.as_str(), "test-app");
    assert_eq!(
        config.endpoint_management_scope,
        EndpointManagementScope::ManagedOnly
    );
    assert_eq!(
        config.public_contact_sharing,
        PublicContactSharingPolicy::PrivateOnly
    );
}
