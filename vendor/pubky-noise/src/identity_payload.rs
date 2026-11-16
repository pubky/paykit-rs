//! Identity payload for Noise protocol authentication.
//!
//! This module provides the identity binding mechanism that links Ed25519 identities
//! to Noise X25519 ephemeral keys, preventing man-in-the-middle attacks.
//!
//! ## Wire Format (PUBKY_CRYPTO_SPEC v2.5)
//!
//! The binding message uses BLAKE3 with a specific input format per spec Section 6.4.

/// Identity payload transmitted during Noise handshake.
///
/// Contains the Ed25519 identity and a signature binding it to the Noise handshake.
///
/// ## Wire Format (PUBKY_CRYPTO_SPEC v2.5 Section 6.3)
#[derive(Debug, Clone)]
pub struct IdentityPayload {
    /// Sender's Ed25519 public key (PKARR identity)
    pub ed25519_pub: [u8; 32],
    /// Noise handshake
    pub noise_handshake: [u8; 32],
    /// Ed25519 signature over binding message (PKARR identity key)
    pub sig: [u8; 64],
}
