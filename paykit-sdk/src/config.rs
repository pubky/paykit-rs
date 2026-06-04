use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Policy for SDK-managed public Payment Endpoint cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the full local Paykit public namespace.
    FullPaykitNamespace,
}

/// Policy for private Payment List sharing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrivateSharingPolicy {
    /// Private sharing is enabled when the identity is private-link-capable.
    Enabled,
    /// Private sharing is disabled by local policy.
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

/// Retention limits for unknown Private Application Messages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownMessageRetentionPolicy {
    /// Maximum unknown messages retained per counterparty.
    pub max_messages_per_counterparty: usize,
    /// Maximum unknown message bytes retained per counterparty.
    pub max_bytes_per_counterparty: usize,
    /// Maximum retention duration.
    pub retention_duration: Duration,
    /// Whether unknown messages may be discarded when limits are reached.
    pub allow_discard: bool,
}

impl Default for UnknownMessageRetentionPolicy {
    fn default() -> Self {
        Self {
            max_messages_per_counterparty: 1_000,
            max_bytes_per_counterparty: 512 * 1024,
            retention_duration: Duration::from_secs(60 * 60 * 24 * 30),
            allow_discard: true,
        }
    }
}

/// Runtime configuration for Paykit SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitSdkConfig {
    /// Public endpoint management scope.
    pub endpoint_management_scope: EndpointManagementScope,
    /// Private endpoint sharing policy.
    pub private_sharing: PrivateSharingPolicy,
    /// Public fallback behavior.
    pub public_fallback: PublicFallbackPolicy,
    /// Maximum time to spend on private recovery before returning/falling back.
    pub private_recovery_timeout: Duration,
    /// Time after which an in-progress peer link operation can be retried.
    pub peer_link_operation_lease_timeout: Duration,
    /// Time after which an in-progress outbound private send can be retried.
    pub outbound_private_send_lease_timeout: Duration,
    /// Unknown private message retention limits.
    pub unknown_message_retention: UnknownMessageRetentionPolicy,
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
            unknown_message_retention: UnknownMessageRetentionPolicy::default(),
        }
    }
}
