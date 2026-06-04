use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Policy for SDK-managed public Payment Endpoint cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the full local Paykit public namespace.
    FullPaykitNamespace,
}

/// Policy for private Paykit message sharing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivateSharingPolicy {
    /// Private Paykit messages are enabled when the identity is private-link-capable.
    Enabled,
    /// Private Paykit messages are disabled by local policy.
    Disabled,
}

/// Policy for falling back to public Payment Endpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PublicFallbackPolicy {
    /// Never fall back to public endpoints.
    Disabled,
    /// Use public endpoints when no private endpoint is available.
    WhenPrivateUnavailable,
    /// Try bounded private recovery before falling back to public endpoints.
    AfterPrivateRecoveryTimeout,
}

/// Runtime configuration for Paykit SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitSdkConfig {
    /// Public endpoint management scope.
    pub endpoint_management_scope: EndpointManagementScope,
    /// Private Paykit message sharing policy.
    pub private_sharing: PrivateSharingPolicy,
    /// Public fallback behavior.
    pub public_fallback: PublicFallbackPolicy,
    /// Maximum time to spend on private recovery before returning/falling back.
    pub private_recovery_timeout: Duration,
    /// Time after which an in-progress peer link operation can be retried.
    pub peer_link_operation_lease_timeout: Duration,
    /// Time after which an in-progress outbound private send can be retried.
    pub outbound_private_send_lease_timeout: Duration,
}

impl Default for PaykitSdkConfig {
    fn default() -> Self {
        Self {
            endpoint_management_scope: EndpointManagementScope::ManagedOnly,
            private_sharing: PrivateSharingPolicy::Enabled,
            public_fallback: PublicFallbackPolicy::AfterPrivateRecoveryTimeout,
            private_recovery_timeout: Duration::from_secs(3),
            peer_link_operation_lease_timeout: Duration::from_secs(60),
            outbound_private_send_lease_timeout: Duration::from_secs(60),
        }
    }
}
