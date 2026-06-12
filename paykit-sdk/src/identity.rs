use std::fmt;

use chrono::{DateTime, Utc};
use paykit_lib::PublicKey;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

/// Pubky public key string used by SDK records.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PubkyPublicKey(String);

impl PubkyPublicKey {
    /// Create a public key wrapper from canonical z-base32 text.
    pub fn new(value: impl Into<String>) -> crate::Result<Self> {
        let value = value.into();
        let public_key =
            PublicKey::try_from_z32(&value).map_err(|err| crate::PaykitSdkError::Identity {
                context: "invalid Pubky public key".into(),
                source: Some(err.into()),
            })?;
        Ok(Self::from_public_key(&public_key))
    }

    /// Create a wrapper from a parsed Pubky public key.
    pub fn from_public_key(public_key: &PublicKey) -> Self {
        Self(public_key.z32())
    }

    /// Parse this wrapper back into a Pubky public key.
    pub fn to_public_key(&self) -> crate::Result<PublicKey> {
        PublicKey::try_from_z32(&self.0).map_err(|err| crate::PaykitSdkError::Identity {
            context: "invalid Pubky public key".into(),
            source: Some(err.into()),
        })
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

impl TryFrom<String> for PubkyPublicKey {
    type Error = crate::PaykitSdkError;

    fn try_from(value: String) -> crate::Result<Self> {
        Self::new(value)
    }
}

impl From<PubkyPublicKey> for String {
    fn from(value: PubkyPublicKey) -> Self {
        value.0
    }
}

/// Pubky capability state visible to Paykit workflows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PubkyIdentityCapability {
    /// No Pubky identity is initialized, or explicit sign-out completed.
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
    pub fn into_inner(mut self) -> [u8; 32] {
        let bytes = self.0;
        self.0.zeroize();
        bytes
    }
}

impl fmt::Debug for PubkyLocalSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PubkyLocalSecretKey(<redacted>)")
    }
}

impl Drop for PubkyLocalSecretKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<[u8; 32]> for PubkyLocalSecretKey {
    fn from(bytes: [u8; 32]) -> Self {
        Self::new(bytes)
    }
}

/// Live Pubky access used by SDK workflows that touch Pubky storage or links.
///
/// Providers must ensure the optional local secret key belongs to the local
/// session public key. The SDK treats a present secret key as private-link
/// capability.
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
        Ok(PubkyPublicKey::from_public_key(
            self.session.info().public_key(),
        ))
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
    /// Whether live Pubky session access is available for this identity.
    pub live_session_available: bool,
    /// Whether private Paykit workflows can run with the live session.
    pub private_link_capable: bool,
}

impl IdentityStatus {
    pub(crate) fn from_state(
        state: &IdentityState,
        live_session_available: bool,
        private_link_capable: bool,
    ) -> Self {
        Self {
            public_key: state.public_key.clone(),
            capability: state.capability,
            live_session_available,
            private_link_capable,
        }
    }
}
