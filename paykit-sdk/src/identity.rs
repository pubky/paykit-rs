use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Pubky public key string used by SDK records.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PubkyPublicKey(String);

impl PubkyPublicKey {
    /// Create a public key wrapper from a non-empty string.
    pub fn new(value: impl Into<String>) -> crate::Result<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(crate::PaykitSdkError::Identity {
                context: "Pubky public key must not be empty".into(),
                source: None,
            });
        }
        Ok(Self(value))
    }

    /// Access the inner public key string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PubkyPublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PubkyPublicKey {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Pubky capability state visible to Paykit workflows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PubkyIdentityCapability {
    /// No local Pubky session is available.
    SignedOut,
    /// Public Pubky operations may work, but private links cannot be established.
    PublicOnly,
    /// Public operations and Encrypted Links can work.
    PrivateLinkCapable,
}

/// Local Pubky secret key used for Encrypted Links.
#[derive(Clone, PartialEq, Eq)]
pub struct PubkyLocalSecretKey([u8; 32]);

impl PubkyLocalSecretKey {
    /// Wrap a 32-byte local secret key.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the secret key bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Consume the wrapper and return the secret key bytes.
    pub fn into_inner(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for PubkyLocalSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkyLocalSecretKey(<redacted>)")
    }
}

impl From<[u8; 32]> for PubkyLocalSecretKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self::new(bytes)
    }
}

/// Live Pubky access used by SDK workflows that touch Pubky storage or links.
#[derive(Clone)]
pub struct PubkySessionAccess {
    /// Authenticated Pubky session for local homeserver writes.
    pub session: pubky::PubkySession,
    /// Pubky client used for counterparty homeserver access.
    pub outbox_client: pubky::Pubky,
    /// Local secret key required for Encrypted Links, when available.
    pub local_secret_key: Option<PubkyLocalSecretKey>,
}

impl PubkySessionAccess {
    /// Return the local Pubky public key.
    pub fn public_key(&self) -> crate::Result<PubkyPublicKey> {
        PubkyPublicKey::new(self.session.info().public_key().to_string())
    }

    /// Return the Paykit capability implied by this access.
    pub fn capability(&self) -> PubkyIdentityCapability {
        if self.private_link_capable() {
            PubkyIdentityCapability::PrivateLinkCapable
        } else {
            PubkyIdentityCapability::PublicOnly
        }
    }

    /// Whether this access can establish Encrypted Links.
    pub fn private_link_capable(&self) -> bool {
        self.local_secret_key.is_some()
    }
}

impl fmt::Debug for PubkySessionAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkySessionAccess")
            .field("session", &"<redacted>")
            .field("outbox_client", &self.outbox_client)
            .field("local_secret_key", &self.local_secret_key)
            .finish()
    }
}

/// Durable identity state tracked by the SDK.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityState {
    /// Current local public key, when signed in.
    pub public_key: Option<PubkyPublicKey>,
    /// Current Pubky capability.
    pub capability: PubkyIdentityCapability,
    /// Whether the local secret key is available to the SDK.
    pub local_secret_available: bool,
    /// Last successful initialization time.
    pub initialized_at: DateTime<Utc>,
    /// Monotonic generation used to separate state across sign-outs.
    pub sign_out_generation: u64,
}

/// Current identity status returned to apps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityStatus {
    /// Current local public key, when signed in.
    pub public_key: Option<PubkyPublicKey>,
    /// Current Pubky capability.
    pub capability: PubkyIdentityCapability,
    /// Whether private Paykit workflows can run.
    pub private_link_capable: bool,
}

impl From<&IdentityState> for IdentityStatus {
    fn from(state: &IdentityState) -> Self {
        Self {
            public_key: state.public_key.clone(),
            capability: state.capability,
            private_link_capable: state.capability == PubkyIdentityCapability::PrivateLinkCapable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pubky_local_secret_key_debug_is_redacted() {
        let key = PubkyLocalSecretKey::new([7; 32]);

        assert_eq!(format!("{key:?}"), "PubkyLocalSecretKey(<redacted>)");
    }
}
