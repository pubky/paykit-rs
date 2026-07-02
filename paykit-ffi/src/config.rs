use std::time::Duration;

use paykit_sdk::{
    EncryptedLinkRecoveryMarkerPolicy, EndpointManagementScope, PaykitReceiverId, PaykitSdkConfig,
    PublicContactSharingPolicy,
};

use crate::errors::{validation_error, PaykitFfiError};
use crate::DEFAULT_PUBKY_REQUEST_TIMEOUT_SECS;

/// SDK policy for public Payment Endpoint cleanup.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiEndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the full local Paykit public namespace.
    FullPaykitNamespace,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// SDK policy for public Encrypted Link recovery markers.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiEncryptedLinkRecoveryMarkerPolicy {
    /// Publish and observe recovery markers.
    Enabled,
    /// Do not use recovery markers.
    Disabled,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// SDK policy for public contact marker publication.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiPublicContactSharingPolicy {
    /// Keep saved contacts only in local SDK storage.
    LocalOnly,
    /// Allow explicit public contact marker publication in the configured namespace.
    ConfiguredPublicNamespace,
    /// SDK returned a value this binding version does not understand.
    Unknown,
}

/// Runtime configuration for Paykit SDK bindings.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPaykitSdkConfig {
    /// Receiver folder for this app/runtime under `/pub/paykit/v0/receivers`.
    pub receiver_id: String,
    /// Namespace segment for SDK profile/contact public data under `/pub/`.
    pub profile_namespace: String,
    /// Public endpoint management scope.
    pub endpoint_management_scope: FfiEndpointManagementScope,
    /// Public recovery marker behavior.
    pub encrypted_link_recovery_markers: FfiEncryptedLinkRecoveryMarkerPolicy,
    /// Public contact marker behavior.
    pub public_contact_sharing: FfiPublicContactSharingPolicy,
    /// Peer link operation lease timeout in seconds.
    pub peer_link_operation_lease_timeout_secs: u64,
    /// Outbound private send lease timeout in seconds.
    pub outbound_private_send_lease_timeout_secs: u64,
    /// Minimum delay before retrying a failed outbound private send in seconds.
    pub outbound_private_retry_backoff_secs: u64,
}

/// Pubky client configuration owned by the binding layer.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyClientConfig {
    /// Request timeout for Pubky HTTP operations in seconds.
    pub request_timeout_secs: u64,
}

/// Return the default SDK policy for an explicit Paykit receiver id.
#[uniffi::export]
pub fn default_config(receiver_id: String) -> Result<FfiPaykitSdkConfig, PaykitFfiError> {
    Ok(PaykitSdkConfig::new(
        PaykitReceiverId::new(receiver_id).map_err(|err| validation_error(err.to_string()))?,
    )
    .into())
}

/// Return the default Pubky client configuration.
#[uniffi::export]
pub fn default_pubky_client_config() -> FfiPubkyClientConfig {
    FfiPubkyClientConfig {
        request_timeout_secs: DEFAULT_PUBKY_REQUEST_TIMEOUT_SECS,
    }
}

/// Return Pubky capabilities required by this SDK configuration.
#[uniffi::export]
pub fn required_session_capabilities(config: FfiPaykitSdkConfig) -> Result<String, PaykitFfiError> {
    let config = PaykitSdkConfig::try_from(config)?;
    config
        .validate()
        .map_err(|err| validation_error(err.to_string()))?;
    Ok(config.required_session_capabilities())
}

impl TryFrom<FfiPaykitSdkConfig> for PaykitSdkConfig {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPaykitSdkConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            receiver_id: PaykitReceiverId::new(value.receiver_id)
                .map_err(|err| validation_error(err.to_string()))?,
            profile_namespace: value.profile_namespace,
            endpoint_management_scope: value.endpoint_management_scope.try_into()?,
            encrypted_link_recovery_markers: value.encrypted_link_recovery_markers.try_into()?,
            public_contact_sharing: value.public_contact_sharing.try_into()?,
            peer_link_operation_lease_timeout: Duration::from_secs(
                value.peer_link_operation_lease_timeout_secs,
            ),
            outbound_private_send_lease_timeout: Duration::from_secs(
                value.outbound_private_send_lease_timeout_secs,
            ),
            outbound_private_retry_backoff: Duration::from_secs(
                value.outbound_private_retry_backoff_secs,
            ),
        })
    }
}

impl From<PaykitSdkConfig> for FfiPaykitSdkConfig {
    fn from(value: PaykitSdkConfig) -> Self {
        Self {
            receiver_id: value.receiver_id.to_string(),
            profile_namespace: value.profile_namespace,
            endpoint_management_scope: value.endpoint_management_scope.into(),
            encrypted_link_recovery_markers: value.encrypted_link_recovery_markers.into(),
            public_contact_sharing: value.public_contact_sharing.into(),
            peer_link_operation_lease_timeout_secs: value
                .peer_link_operation_lease_timeout
                .as_secs(),
            outbound_private_send_lease_timeout_secs: value
                .outbound_private_send_lease_timeout
                .as_secs(),
            outbound_private_retry_backoff_secs: value.outbound_private_retry_backoff.as_secs(),
        }
    }
}

impl TryFrom<FfiEndpointManagementScope> for EndpointManagementScope {
    type Error = PaykitFfiError;

    fn try_from(value: FfiEndpointManagementScope) -> Result<Self, Self::Error> {
        match value {
            FfiEndpointManagementScope::ManagedOnly => Ok(Self::ManagedOnly),
            FfiEndpointManagementScope::FullPaykitNamespace => Ok(Self::FullPaykitNamespace),
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
            EndpointManagementScope::FullPaykitNamespace => Self::FullPaykitNamespace,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiEncryptedLinkRecoveryMarkerPolicy> for EncryptedLinkRecoveryMarkerPolicy {
    type Error = PaykitFfiError;

    fn try_from(value: FfiEncryptedLinkRecoveryMarkerPolicy) -> Result<Self, Self::Error> {
        match value {
            FfiEncryptedLinkRecoveryMarkerPolicy::Enabled => Ok(Self::Enabled),
            FfiEncryptedLinkRecoveryMarkerPolicy::Disabled => Ok(Self::Disabled),
            FfiEncryptedLinkRecoveryMarkerPolicy::Unknown => Err(validation_error(
                "encrypted_link_recovery_markers cannot be unknown",
            )),
        }
    }
}

impl From<EncryptedLinkRecoveryMarkerPolicy> for FfiEncryptedLinkRecoveryMarkerPolicy {
    fn from(value: EncryptedLinkRecoveryMarkerPolicy) -> Self {
        match value {
            EncryptedLinkRecoveryMarkerPolicy::Enabled => Self::Enabled,
            EncryptedLinkRecoveryMarkerPolicy::Disabled => Self::Disabled,
            _ => Self::Unknown,
        }
    }
}

impl TryFrom<FfiPublicContactSharingPolicy> for PublicContactSharingPolicy {
    type Error = PaykitFfiError;

    fn try_from(value: FfiPublicContactSharingPolicy) -> Result<Self, Self::Error> {
        match value {
            FfiPublicContactSharingPolicy::LocalOnly => Ok(Self::LocalOnly),
            FfiPublicContactSharingPolicy::ConfiguredPublicNamespace => {
                Ok(Self::ConfiguredPublicNamespace)
            }
            FfiPublicContactSharingPolicy::Unknown => {
                Err(validation_error("public_contact_sharing cannot be unknown"))
            }
        }
    }
}

impl From<PublicContactSharingPolicy> for FfiPublicContactSharingPolicy {
    fn from(value: PublicContactSharingPolicy) -> Self {
        match value {
            PublicContactSharingPolicy::LocalOnly => Self::LocalOnly,
            PublicContactSharingPolicy::ConfiguredPublicNamespace => {
                Self::ConfiguredPublicNamespace
            }
            _ => Self::Unknown,
        }
    }
}
