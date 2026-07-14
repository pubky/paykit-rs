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

// SECURITY (auditor note): these Drop impls scrub the Rust-owned key buffers on
// drop. Zeroization here is BEST-EFFORT, not a guarantee:
//   * `export_bytes()` returns a plaintext `Vec<u8>` clone the caller owns; that
//     copy is outside Paykit's control.
//   * UniFFI lowering copies the bytes into foreign-managed (Swift/Kotlin) memory
//     that Rust cannot scrub.
// The goal is to shrink the window a resident plaintext copy sits in Rust-owned
// heap for memory-dump / swap scenarios; it does NOT defend against remote
// exploitation. Manual Drop is used deliberately: paykit-ffi declares `zeroize`
// without the `derive` feature, so `#[derive(ZeroizeOnDrop)]` is not available.
impl Drop for FfiSdkStateBlob {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for FfiSdkBackupBlob {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl Drop for FfiPubkyLocalSecretKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Positive regression guard: adding `impl Drop`/`zeroize` must not break
    // construction or byte export for any of the three secret-bearing blobs.
    //
    // Note: we deliberately do NOT try to assert zeroize-on-drop by inspecting
    // memory after the value is dropped. Reading freed memory is undefined
    // behavior and cannot be tested soundly, so this test only exercises the
    // still-live construction/export path.
    #[test]
    fn test_secrets_blobs_export_roundtrip() {
        let state_bytes = vec![1u8, 2, 3, 4];
        assert_eq!(
            FfiSdkStateBlob::new(state_bytes.clone()).export_bytes(),
            state_bytes
        );

        let backup_bytes = vec![5u8, 6, 7, 8];
        assert_eq!(
            FfiSdkBackupBlob::new(backup_bytes.clone()).export_bytes(),
            backup_bytes
        );

        let secret_bytes = vec![9u8, 10, 11, 12];
        assert_eq!(
            FfiPubkyLocalSecretKey::new(secret_bytes.clone()).export_bytes(),
            secret_bytes
        );
    }
}
