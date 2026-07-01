use serde::{Deserialize, Serialize};
use std::time::Duration;

use paykit_lib::{receipt_path_prefix, PaykitReceiverId};

use crate::identity::PubkyPublicKey;

/// Default namespace segment for SDK profile/contact public data.
///
/// This is SDK-level Paykit app data, not a core Paykit Protocol route.
pub const DEFAULT_PROFILE_NAMESPACE: &str = "paykit";

/// Policy for SDK-managed public Payment Endpoint cleanup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EndpointManagementScope {
    /// Manage only endpoints previously published by the SDK.
    ManagedOnly,
    /// Manage the full local Paykit public namespace.
    FullPaykitNamespace,
}

/// Policy for public Encrypted Link recovery markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EncryptedLinkRecoveryMarkerPolicy {
    /// Publish and observe minimal public recovery markers.
    Enabled,
    /// Do not use public recovery markers.
    Disabled,
}

/// Policy for publishing saved contacts to public Pubky storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PublicContactSharingPolicy {
    /// Keep saved contacts only in local SDK storage. This is the default.
    LocalOnly,
    /// Allow explicit public contact marker publication in the configured namespace.
    ConfiguredPublicNamespace,
}

/// Runtime configuration for Paykit SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaykitSdkConfig {
    /// Receiver folder for this app/runtime under `/pub/paykit/v0/receivers`.
    pub receiver_id: PaykitReceiverId,
    /// Namespace segment for SDK profile/contact public data under `/pub/`.
    ///
    /// This is a local SDK convention for Paykit-facing profile, blob, and
    /// public contact marker helpers. Pubky session permissions remain the
    /// authority for what a caller can actually write. This does not change
    /// core Paykit Protocol paths or split private runtime state.
    #[serde(default = "default_profile_namespace")]
    pub profile_namespace: String,
    /// Public endpoint management scope.
    pub endpoint_management_scope: EndpointManagementScope,
    /// Public recovery marker behavior.
    #[serde(default = "default_encrypted_link_recovery_marker_policy")]
    pub encrypted_link_recovery_markers: EncryptedLinkRecoveryMarkerPolicy,
    /// Public contact marker behavior.
    #[serde(default = "default_public_contact_sharing_policy")]
    pub public_contact_sharing: PublicContactSharingPolicy,
    /// Time after which an in-progress peer link operation can be retried.
    pub peer_link_operation_lease_timeout: Duration,
    /// Time after which an in-progress outbound private send is considered stale.
    ///
    /// Stale `Sending` records can be reclaimed for retry by another worker.
    pub outbound_private_send_lease_timeout: Duration,
    /// Minimum delay before retrying a failed outbound private send.
    #[serde(default = "default_outbound_private_retry_backoff")]
    pub outbound_private_retry_backoff: Duration,
}

impl PaykitSdkConfig {
    /// Build the default SDK policy for an explicit Paykit receiver id.
    pub fn new(receiver_id: PaykitReceiverId) -> Self {
        Self {
            receiver_id,
            profile_namespace: DEFAULT_PROFILE_NAMESPACE.into(),
            endpoint_management_scope: EndpointManagementScope::ManagedOnly,
            encrypted_link_recovery_markers: EncryptedLinkRecoveryMarkerPolicy::Enabled,
            public_contact_sharing: PublicContactSharingPolicy::LocalOnly,
            peer_link_operation_lease_timeout: Duration::from_secs(60),
            outbound_private_send_lease_timeout: Duration::from_secs(60),
            outbound_private_retry_backoff: Duration::from_secs(30),
        }
    }

    /// Validate runtime configuration values.
    pub fn validate(&self) -> crate::Result<()> {
        PaykitReceiverId::new(self.receiver_id.as_str()).map_err(|err| {
            crate::PaykitSdkError::Policy(format!("invalid Paykit receiver id: {err}"))
        })?;
        validate_profile_namespace(&self.profile_namespace)?;
        validate_runtime_duration(
            "peer link operation lease timeout",
            self.peer_link_operation_lease_timeout,
        )?;
        validate_runtime_duration(
            "outbound private send lease timeout",
            self.outbound_private_send_lease_timeout,
        )?;
        validate_runtime_duration(
            "outbound private retry backoff",
            self.outbound_private_retry_backoff,
        )?;
        Ok(())
    }

    /// Return the configured Paykit Profile path.
    pub fn paykit_profile_path(&self) -> String {
        if self.profile_namespace == DEFAULT_PROFILE_NAMESPACE {
            return format!("/pub/paykit/v0/receivers/{}/profile.json", self.receiver_id);
        }
        format!("/pub/{}/profile.json", self.profile_namespace)
    }

    /// Return the configured Paykit Profile blob path prefix.
    pub fn paykit_profile_blob_path_prefix(&self) -> String {
        if self.profile_namespace == DEFAULT_PROFILE_NAMESPACE {
            return format!("/pub/paykit/v0/receivers/{}/blobs/", self.receiver_id);
        }
        format!("/pub/{}/blobs/", self.profile_namespace)
    }

    /// Return the configured public contact marker path prefix.
    pub fn public_contact_path_prefix(&self) -> String {
        if self.profile_namespace == DEFAULT_PROFILE_NAMESPACE {
            return format!("/pub/paykit/v0/receivers/{}/contacts/", self.receiver_id);
        }
        format!("/pub/{}/contacts/", self.profile_namespace)
    }

    /// Return the configured public contact marker path for one contact.
    pub fn public_contact_path(&self, public_key: &PubkyPublicKey) -> String {
        format!(
            "{}{}.json",
            self.public_contact_path_prefix(),
            public_key.as_str()
        )
    }

    /// Return the Pubky session capabilities required by this SDK configuration.
    pub fn required_session_capabilities(&self) -> String {
        let mut capabilities = vec![
            format!("/pub/paykit/v0/receivers/{}/:rw", self.receiver_id),
            format!("/pub/paykit/v0/private/{}/:rw", self.receiver_id),
        ];
        if self.profile_namespace != DEFAULT_PROFILE_NAMESPACE {
            capabilities.push(format!("/pub/{}:rw", self.profile_namespace));
        }
        capabilities.join(",")
    }

    /// Return the receiver-scoped Receipt Location prefix.
    pub fn receipt_path_prefix(&self) -> String {
        receipt_path_prefix(&self.receiver_id)
    }
}

fn validate_profile_namespace(namespace: &str) -> crate::Result<()> {
    if namespace.is_empty() {
        return Err(crate::PaykitSdkError::Policy(
            "profile namespace must not be empty".into(),
        ));
    }
    if namespace.len() > 128 {
        return Err(crate::PaykitSdkError::Policy(
            "profile namespace must not exceed 128 bytes".into(),
        ));
    }
    if namespace.starts_with('.') || namespace.ends_with('.') {
        return Err(crate::PaykitSdkError::Policy(
            "profile namespace must not start or end with '.'".into(),
        ));
    }
    if namespace == "pubky.app" {
        return Err(crate::PaykitSdkError::Policy(
            "profile namespace 'pubky.app' is reserved for Pubky app data".into(),
        ));
    }
    if namespace
        .chars()
        .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_'))
    {
        return Err(crate::PaykitSdkError::Policy(
            "profile namespace may only contain ASCII letters, digits, '.', '-' and '_'".into(),
        ));
    }
    Ok(())
}

fn validate_runtime_duration(label: &str, duration: Duration) -> crate::Result<()> {
    if duration.is_zero() {
        return Err(crate::PaykitSdkError::Policy(format!(
            "{label} must be greater than zero"
        )));
    }
    chrono::Duration::from_std(duration).map_err(|err| {
        crate::PaykitSdkError::Policy(format!("{label} must fit SDK runtime duration: {err}"))
    })?;
    Ok(())
}

fn default_encrypted_link_recovery_marker_policy() -> EncryptedLinkRecoveryMarkerPolicy {
    EncryptedLinkRecoveryMarkerPolicy::Enabled
}

fn default_public_contact_sharing_policy() -> PublicContactSharingPolicy {
    PublicContactSharingPolicy::LocalOnly
}

fn default_profile_namespace() -> String {
    DEFAULT_PROFILE_NAMESPACE.into()
}

fn default_outbound_private_retry_backoff() -> Duration {
    Duration::from_secs(30)
}

#[cfg(test)]
impl Default for PaykitSdkConfig {
    fn default() -> Self {
        Self::new(PaykitReceiverId::new("test").expect("valid test receiver id"))
    }
}

#[cfg(test)]
mod tests;
