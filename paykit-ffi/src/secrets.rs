use std::fmt;

use zeroize::Zeroize;

/// SDK state blob owned by platform storage.
#[derive(uniffi::Object)]
pub struct FfiSdkStateBlob {
    pub(crate) bytes: Vec<u8>,
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

/// Receiver-scoped Noise secret key bytes supplied by platform secure storage.
#[derive(uniffi::Object)]
pub struct FfiReceiverNoiseSecretKey {
    pub(crate) bytes: Vec<u8>,
}

impl Drop for FfiReceiverNoiseSecretKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for FfiReceiverNoiseSecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "FfiReceiverNoiseSecretKey(<redacted:{} bytes>)",
            self.bytes.len()
        )
    }
}

#[uniffi::export]
impl FfiReceiverNoiseSecretKey {
    /// Create a receiver Noise secret key from platform secure storage bytes.
    #[uniffi::constructor]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Generate a fresh receiver Noise secret key.
    #[uniffi::constructor]
    pub fn random() -> Self {
        Self {
            bytes: paykit_sdk::ReceiverNoiseSecretKey::random()
                .as_bytes()
                .to_vec(),
        }
    }

    /// Export the raw bytes for platform secure storage.
    pub fn export_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
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
}
