use paykit_sdk::{EndpointManagementScope, PaykitSdkConfig, PublicContactSharingPolicy};

use crate::errors::{validation_error, PaykitFfiError};
use crate::DEFAULT_PUBKY_REQUEST_TIMEOUT_SECS;

/// SDK policy for public Payment Endpoint cleanup.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiEndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the configured application's full public endpoint namespace.
    FullAppEndpointNamespace,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// SDK policy for public contact marker publication.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPublicContactSharingPolicy {
    /// Keep saved contacts only in private SDK state.
    PrivateOnly,
    /// Allow explicit public contact marker publication.
    Enabled,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Runtime configuration for Paykit SDK bindings.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitSdkConfig {
    /// Stable identifier for this application within the Paykit identity.
    pub app_id: String,
    /// Public endpoint management scope.
    pub endpoint_management_scope: FfiEndpointManagementScope,
    /// Public contact marker behavior.
    pub public_contact_sharing: FfiPublicContactSharingPolicy,
}

/// Pubky client configuration owned by the binding layer.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyClientConfig {
    /// Request timeout for Pubky HTTP operations in seconds.
    pub request_timeout_secs: u64,
    /// Host running local testnet services, or `None` to use the public Pubky network.
    pub local_testnet_host: Option<String>,
}

/// Return SDK configuration defaults for one application.
#[uniffi::export]
pub fn default_config(app_id: String) -> Result<FfiPaykitSdkConfig, PaykitFfiError> {
    Ok(PaykitSdkConfig::new(app_id)?.into())
}

/// Return the default Pubky client configuration.
#[uniffi::export]
pub fn default_pubky_client_config() -> FfiPubkyClientConfig {
    FfiPubkyClientConfig {
        request_timeout_secs: DEFAULT_PUBKY_REQUEST_TIMEOUT_SECS,
        local_testnet_host: None,
    }
}

/// Return Pubky capabilities required by Paykit SDK.
#[uniffi::export]
pub fn required_session_capabilities() -> String {
    paykit_sdk::PAYKIT_SESSION_CAPABILITIES.to_string()
}

impl TryFrom<FfiPaykitSdkConfig> for PaykitSdkConfig {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaykitSdkConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            app_id: paykit_sdk::PaykitAppId::new(value.app_id)
                .map_err(|err| validation_error(err.to_string()))?,
            endpoint_management_scope: value.endpoint_management_scope.try_into()?,
            public_contact_sharing: value.public_contact_sharing.try_into()?,
        })
    }
}

impl From<PaykitSdkConfig> for FfiPaykitSdkConfig {
    fn from(value: PaykitSdkConfig) -> Self {
        Self {
            app_id: value.app_id.to_string(),
            endpoint_management_scope: value.endpoint_management_scope.into(),
            public_contact_sharing: value.public_contact_sharing.into(),
        }
    }
}

impl TryFrom<FfiEndpointManagementScope> for EndpointManagementScope {
    type Error = PaykitFfiError;

    fn try_from(value: FfiEndpointManagementScope) -> Result<Self, Self::Error> {
        match value {
            FfiEndpointManagementScope::ManagedOnly => Ok(Self::ManagedOnly),
            FfiEndpointManagementScope::FullAppEndpointNamespace => {
                Ok(Self::FullAppEndpointNamespace)
            }
            FfiEndpointManagementScope::Unknown => Err(validation_error(
                "endpoint_management_scope cannot be unknown",
            )),
        }
    }
}

impl From<EndpointManagementScope> for FfiEndpointManagementScope {
    fn from(value: EndpointManagementScope) -> Self {
        match value {
            EndpointManagementScope::ManagedOnly => Self::ManagedOnly,
            EndpointManagementScope::FullAppEndpointNamespace => Self::FullAppEndpointNamespace,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiPublicContactSharingPolicy> for PublicContactSharingPolicy {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPublicContactSharingPolicy) -> Result<Self, Self::Error> {
        match value {
            FfiPublicContactSharingPolicy::PrivateOnly => Ok(Self::PrivateOnly),
            FfiPublicContactSharingPolicy::Enabled => Ok(Self::Enabled),
            FfiPublicContactSharingPolicy::Unknown => {
                Err(validation_error("public_contact_sharing cannot be unknown"))
            }
        }
    }
}

impl From<PublicContactSharingPolicy> for FfiPublicContactSharingPolicy {
    fn from(value: PublicContactSharingPolicy) -> Self {
        match value {
            PublicContactSharingPolicy::PrivateOnly => Self::PrivateOnly,
            PublicContactSharingPolicy::Enabled => Self::Enabled,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_app_endpoint_namespace_round_trips() {
        let ffi =
            FfiEndpointManagementScope::from(EndpointManagementScope::FullAppEndpointNamespace);

        assert_eq!(ffi, FfiEndpointManagementScope::FullAppEndpointNamespace);
        assert_eq!(
            EndpointManagementScope::try_from(ffi).unwrap(),
            EndpointManagementScope::FullAppEndpointNamespace
        );
    }
}
