use std::fmt;
use std::sync::Arc;

use paykit_sdk::{PaykitIdentitySecretKey, PubkyLocalSecretKey};
use zeroize::Zeroize;

use crate::errors::{validation_error, PaykitFfiError};

/// Identity-wide SDK state blob owned by the configured storage boundary.
///
/// Android storage callbacks should export callback-supplied blob bytes and
/// close the generated native wrapper before returning.
#[derive(uniffi::Object)]
pub struct FfiSdkStateBlob {
    pub(crate) bytes: Vec<u8>,
}

impl Drop for FfiSdkStateBlob {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for FfiSdkStateBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FfiSdkStateBlob(<redacted:{} bytes>)", self.bytes.len())
    }
}

#[uniffi::export]
impl FfiSdkStateBlob {
    /// Create an SDK state blob from platform storage bytes.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Export the raw bytes for platform storage.
    pub fn export_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// SDK backup blob owned by the app.
#[derive(uniffi::Object)]
pub struct FfiSdkBackupBlob {
    bytes: Vec<u8>,
}

impl Drop for FfiSdkBackupBlob {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for FfiSdkBackupBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FfiSdkBackupBlob(<redacted:{} bytes>)", self.bytes.len())
    }
}

#[uniffi::export]
impl FfiSdkBackupBlob {
    /// Create an SDK backup blob from app-owned bytes.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Export the raw bytes for app-controlled backup storage.
    pub fn export_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}

/// Local Pubky secret key bytes supplied by platform secure storage.
#[derive(uniffi::Object)]
pub struct FfiPubkyLocalSecretKey {
    pub(crate) bytes: Vec<u8>,
}

impl Drop for FfiPubkyLocalSecretKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for FfiPubkyLocalSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FfiPubkyLocalSecretKey(<redacted:{} bytes>)",
            self.bytes.len()
        )
    }
}

#[uniffi::export]
impl FfiPubkyLocalSecretKey {
    /// Create a local Pubky secret key from platform secure storage bytes.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Export the raw bytes for platform secure storage.
    pub fn export_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// Derive one generation of the identity-wide Paykit secret.
    pub fn derive_paykit_identity_secret_key(
        &self,
        key_generation: u64,
    ) -> Result<Arc<FfiPaykitIdentitySecretKey>, PaykitFfiError> {
        let bytes: [u8; 32] = self.bytes.clone().try_into().map_err(|bytes: Vec<u8>| {
            validation_error(format!(
                "Pubky local secret key must be 32 bytes, got {}",
                bytes.len()
            ))
        })?;
        let secret =
            PubkyLocalSecretKey::new(bytes).derive_paykit_identity_secret_key(key_generation)?;
        Ok(Arc::new(FfiPaykitIdentitySecretKey::from_sdk(&secret)))
    }
}

/// Rotatable identity-wide Paykit secret supplied by secure platform storage.
#[derive(uniffi::Object)]
pub struct FfiPaykitIdentitySecretKey {
    bytes: Vec<u8>,
    key_generation: u64,
}

impl Drop for FfiPaykitIdentitySecretKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for FfiPaykitIdentitySecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FfiPaykitIdentitySecretKey")
            .field(
                "bytes",
                &format_args!("<redacted:{} bytes>", self.bytes.len()),
            )
            .field("key_generation", &self.key_generation)
            .finish()
    }
}

#[uniffi::export]
impl FfiPaykitIdentitySecretKey {
    /// Create Paykit key material from secure platform storage.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>, key_generation: u64) -> Result<Self, PaykitFfiError> {
        let secret = paykit_identity_secret_from_bytes(bytes, key_generation)?;
        Ok(Self::from_sdk(&secret))
    }

    /// Export the secret bytes for secure platform storage or delegation.
    pub fn export_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    /// Return the key generation.
    pub fn key_generation(&self) -> u64 {
        self.key_generation
    }
}

impl FfiPaykitIdentitySecretKey {
    pub(crate) fn to_sdk(&self) -> Result<PaykitIdentitySecretKey, PaykitFfiError> {
        paykit_identity_secret_from_bytes(self.bytes.clone(), self.key_generation)
    }

    fn from_sdk(secret: &PaykitIdentitySecretKey) -> Self {
        Self {
            bytes: secret.as_bytes().to_vec(),
            key_generation: secret.key_generation(),
        }
    }
}

fn paykit_identity_secret_from_bytes(
    bytes: Vec<u8>,
    key_generation: u64,
) -> Result<PaykitIdentitySecretKey, PaykitFfiError> {
    let bytes: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        validation_error(format!(
            "Paykit identity secret key must be 32 bytes, got {}",
            bytes.len()
        ))
    })?;
    PaykitIdentitySecretKey::new(bytes, key_generation).map_err(Into::into)
}
