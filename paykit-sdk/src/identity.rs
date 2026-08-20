use std::fmt;

use bip39::{Language, Mnemonic};
use chrono::{DateTime, Utc};
use paykit_lib::PublicKey;
use pubky::{Capabilities, Capability};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const PUBKY_APP_KEY_PREFIX: &str = "pubky";
const PUBKY_PUBLIC_KEY_Z32_LEN: usize = 52;
const BIP39_SEED_BYTES: usize = 64;
const PUBKY_SECRET_BYTES: usize = 32;

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

    /// Create a public key wrapper from canonical z-base32 text or `pubky...` app-key text.
    pub fn from_raw_or_app_key(value: impl AsRef<str>) -> crate::Result<Self> {
        let value = value.as_ref().trim();
        let raw = value
            .strip_prefix(PUBKY_APP_KEY_PREFIX)
            .filter(|_| value.len() == PUBKY_APP_KEY_PREFIX.len() + PUBKY_PUBLIC_KEY_Z32_LEN)
            .unwrap_or(value);
        Self::new(raw.to_owned())
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

    /// Return the `pubky...` app-key representation used by product surfaces.
    pub fn to_app_key(&self) -> String {
        format!("{PUBKY_APP_KEY_PREFIX}{}", self.0)
    }

    /// Return a shortened app-key string for diagnostic displays.
    pub fn redacted_app_key(&self) -> String {
        let app_key = self.to_app_key();
        let prefix = &app_key[..PUBKY_APP_KEY_PREFIX.len() + 6];
        let suffix = &app_key[app_key.len() - 6..];
        format!("{prefix}...{suffix}")
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

/// Pubky capability state for an identity-wide Paykit runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum PubkyIdentityCapability {
    /// No live Pubky session is available.
    SignedOut,
    /// Public Pubky operations may work, but private links cannot be established.
    ///
    /// Private Link workflows require `PrivateLinkCapable`.
    PublicOnly,
    /// Public operations and Encrypted Links can work.
    PrivateLinkCapable,
}

/// Local Pubky secret key used for Pubky sessions and Encrypted Links.
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

    /// Parse a 32-byte secret key from hex text.
    pub fn from_hex(value: &str) -> crate::Result<Self> {
        let mut bytes = [0; 32];
        hex::decode_to_slice(value, &mut bytes).map_err(|err| {
            let context = if value.len() != 64 {
                "Pubky secret key hex must decode to 32 bytes".into()
            } else {
                format!("invalid Pubky secret key hex: {err}")
            };
            crate::PaykitSdkError::Identity {
                context,
                source: None,
            }
        })?;
        Ok(Self::new(bytes))
    }

    /// Derive a local Pubky secret key from a 64-byte BIP39 seed.
    ///
    /// This matches Pubky Core and Pubky Ring: the Pubky secret key is the
    /// first 32 bytes of the BIP39 seed produced with an empty passphrase.
    pub fn from_bip39_seed(seed: &[u8]) -> crate::Result<Self> {
        if seed.len() != BIP39_SEED_BYTES {
            return Err(crate::PaykitSdkError::Identity {
                context: format!(
                    "Pubky BIP39 seed must be {BIP39_SEED_BYTES} bytes, got {}",
                    seed.len()
                ),
                source: None,
            });
        }
        let bytes: [u8; PUBKY_SECRET_BYTES] = seed[..PUBKY_SECRET_BYTES]
            .try_into()
            .expect("BIP39 seed length checked above");
        Ok(Self::new(bytes))
    }

    /// Derive a local Pubky secret key from a BIP39 English mnemonic phrase.
    ///
    /// The mnemonic is converted to a BIP39 seed with an empty passphrase, then
    /// handled the same way as [`Self::from_bip39_seed`].
    pub fn from_bip39_mnemonic(mnemonic_phrase: &str) -> crate::Result<Self> {
        let mnemonic = Mnemonic::parse_in(Language::English, mnemonic_phrase).map_err(|err| {
            crate::PaykitSdkError::Identity {
                context: format!("invalid BIP39 mnemonic phrase: {err}"),
                source: None,
            }
        })?;
        let mut seed = mnemonic.to_seed("");
        let key = Self::from_bip39_seed(&seed);
        seed.zeroize();
        key
    }

    /// Return the Pubky public key for this secret key.
    pub fn public_key(&self) -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&self.keypair().public_key())
    }

    /// Derive the identity-wide secret used for Paykit Encrypted Links.
    pub(crate) fn paykit_noise_secret_key(&self) -> [u8; 32] {
        paykit_lib::derive_paykit_noise_secret_key(&self.0)
    }

    pub(crate) fn keypair(&self) -> pubky::Keypair {
        pubky::Keypair::from_secret(&self.0)
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

/// Live Pubky access used by the SDK for one Paykit identity.
///
/// The SDK validates that a present local secret key belongs to the session
/// public key before using it for private-link capability.
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

    /// Validate that the local secret key, when present, belongs to the session.
    pub fn validate(&self) -> crate::Result<()> {
        let Some(local_secret_key) = &self.local_secret_key else {
            return Ok(());
        };

        let session_public_key = self.public_key()?;
        if local_secret_key.public_key() != session_public_key {
            return Err(crate::PaykitSdkError::Identity {
                context: "local Pubky secret key does not match session public key".into(),
                source: None,
            });
        }

        Ok(())
    }

    /// Validate local secret ownership and required Pubky write capabilities.
    pub fn validate_for_capabilities(&self, required_capabilities: &str) -> crate::Result<()> {
        self.validate()?;
        validate_session_capabilities(self.session.info().capabilities(), required_capabilities)
    }

    /// Return the Paykit capability implied by this access and capability scope.
    pub fn capability_for_capabilities(
        &self,
        required_capabilities: &str,
    ) -> crate::Result<PubkyIdentityCapability> {
        self.validate_for_capabilities(required_capabilities)?;
        Ok(if self.private_link_capable_unchecked() {
            PubkyIdentityCapability::PrivateLinkCapable
        } else {
            PubkyIdentityCapability::PublicOnly
        })
    }

    /// Report whether this validated access can establish Encrypted Links.
    pub fn private_link_capable_for_capabilities(
        &self,
        required_capabilities: &str,
    ) -> crate::Result<bool> {
        self.validate_for_capabilities(required_capabilities)?;
        Ok(self.private_link_capable_unchecked())
    }

    fn private_link_capable_unchecked(&self) -> bool {
        self.local_secret_key.is_some()
    }
}

fn validate_session_capabilities(
    actual_capabilities: &[Capability],
    required_capabilities: &str,
) -> crate::Result<()> {
    let actual = Capabilities::from(actual_capabilities.to_vec()).normalize();
    let required = crate::pubky_session::parse_capabilities(required_capabilities)?;
    let missing = required
        .as_slice()
        .iter()
        .filter(|required| {
            !actual
                .as_slice()
                .iter()
                .any(|actual| capability_covers(actual, required))
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    if missing.is_empty() {
        Ok(())
    } else {
        Err(crate::PaykitSdkError::Identity {
            context: format!(
                "Pubky session is missing required Paykit capabilities: {}",
                missing.join(",")
            ),
            source: None,
        })
    }
}

fn capability_covers(actual: &Capability, required: &Capability) -> bool {
    scope_covers(&actual.scope, &required.scope)
        && required
            .actions
            .iter()
            .all(|required_action| actual.actions.contains(required_action))
}

fn scope_covers(parent: &str, child: &str) -> bool {
    parent == child || (parent.ends_with('/') && child.starts_with(parent))
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

/// Durable identity state tracked in shared SDK storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityState {
    /// Last initialized public key, when known.
    pub public_key: Option<PubkyPublicKey>,
    /// Last successful initialization time.
    pub initialized_at: DateTime<Utc>,
}

/// Current identity status returned to apps.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityStatus {
    /// Last initialized public key, when known.
    pub public_key: Option<PubkyPublicKey>,
    /// Current Pubky capability.
    pub capability: PubkyIdentityCapability,
}

impl IdentityStatus {
    pub(crate) fn from_state(
        state: &IdentityState,
        live_session_available: bool,
        private_link_capable: bool,
    ) -> Self {
        Self {
            public_key: state.public_key.clone(),
            capability: if private_link_capable {
                PubkyIdentityCapability::PrivateLinkCapable
            } else if live_session_available {
                PubkyIdentityCapability::PublicOnly
            } else {
                PubkyIdentityCapability::SignedOut
            },
        }
    }
}

#[cfg(test)]
mod tests;
