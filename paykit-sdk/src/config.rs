use serde::{Deserialize, Serialize};

use paykit_lib::PaykitAppId;

/// Policy for SDK-managed public Payment Endpoint cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the configured application's full public endpoint namespace.
    FullAppEndpointNamespace,
}

/// Policy for publishing saved contacts to public Pubky storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PublicContactSharingPolicy {
    /// Keep saved contacts only in private SDK state. This is the default.
    PrivateOnly,
    /// Allow explicit public contact marker publication.
    Enabled,
}

/// Runtime configuration for Paykit SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitSdkConfig {
    /// Application using this identity-wide Paykit runtime.
    pub app_id: PaykitAppId,
    /// Public endpoint management scope.
    pub endpoint_management_scope: EndpointManagementScope,
    /// Public contact marker behavior.
    #[serde(default = "default_public_contact_sharing_policy")]
    pub public_contact_sharing: PublicContactSharingPolicy,
}

impl PaykitSdkConfig {
    /// Create SDK configuration for one Paykit application.
    pub fn new(app_id: impl Into<String>) -> crate::Result<Self> {
        Ok(Self {
            app_id: PaykitAppId::new(app_id.into()).map_err(crate::PaykitSdkError::from)?,
            endpoint_management_scope: EndpointManagementScope::ManagedOnly,
            public_contact_sharing: PublicContactSharingPolicy::PrivateOnly,
        })
    }
}

fn default_public_contact_sharing_policy() -> PublicContactSharingPolicy {
    PublicContactSharingPolicy::PrivateOnly
}

#[cfg(test)]
mod tests;
