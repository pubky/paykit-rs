//! Asymmetric path derivation for per-peer-pair private storage.
//!
//! This module provides a utility function that derives two distinct storage
//! paths from a Diffie-Hellman shared secret. Each party in a communication
//! pair writes to one path and reads from the other, preventing third parties
//! from enumerating who is communicating with whom.
//!
//! ## Derivation Formula
//!
//! ```text
//! dh_secret   = X25519(to_scalar_bytes(ed25519_seed), to_montgomery(remote_ed25519_pk))
//! write_path  = "{base_path}/{hex(SHA-256(domain || dh_secret || local_ed25519_pk))}"
//! read_path   = "{base_path}/{hex(SHA-256(domain || dh_secret || remote_ed25519_pk))}"
//! ```
//!
//! ## Crypto Primitives
//!
//! | Primitive | Crate | Rationale |
//! |---|---|---|
//! | Ed25519 → X25519 scalar | `ed25519-dalek` | Standard birational map via `to_scalar_bytes()` |
//! | Ed25519 → Montgomery point | `ed25519-dalek` | Standard conversion via `to_montgomery()` |
//! | X25519 DH | `curve25519-dalek` | Same DH used by the Noise `_25519_` suite |
//! | SHA-256 | `sha2` | Same hash used by the Noise `_SHA256` suite |
//! | Hex encoding | `hex` | Safe for URL/path segments |

use curve25519_dalek::montgomery::MontgomeryPoint;
use ed25519_dalek::SigningKey;
use pubky::PublicKey;
use sha2::{Digest, Sha256};

/// Derives asymmetric write/read storage paths from a DH shared secret.
///
/// Both parties in a communication pair can compute both paths, but each
/// party writes to a different path than they read from. This prevents
/// third parties from enumerating communication relationships.
///
/// # Parameters
///
/// - `local_secret_key` — 32-byte Ed25519 secret key (seed) of the local party.
/// - `remote_pubkey` — Ed25519 public key of the remote party.
/// - `domain` — Domain separation bytes (e.g., `b"paykit-path-v0"`). Prevents
///   cross-protocol attacks by ensuring different applications derive different
///   paths from the same key pair.
/// - `base_path` — Storage path prefix (e.g., `"/pub/paykit.app/v0/private"`).
///   The derived hex component is appended as a child segment.
///
/// # Returns
///
/// A tuple `(write_path, read_path)` where:
/// - `write_path` — the path the local party writes to on their own homeserver.
/// - `read_path` — the path the local party reads from on the remote homeserver.
///
/// # Correctness
///
/// For parties Alice (secret `a`, public `A`) and Bob (secret `b`, public `B`):
/// - `derive(a, B, ...).write_path == derive(b, A, ...).read_path`
/// - `derive(a, B, ...).read_path == derive(b, A, ...).write_path`
///
/// This holds because `X25519(a, B) == X25519(b, A)` (DH commutativity).
///
/// # Example
///
/// ```ignore
/// let (write_path, read_path) = derive_asymmetric_paths(
///     &my_secret_key,
///     &their_pubkey,
///     b"paykit-path-v0",
///     "/pub/paykit.app/v0/private",
/// );
/// // write_path = "/pub/paykit.app/v0/private/a1b2c3d4...64 hex chars"
/// // read_path  = "/pub/paykit.app/v0/private/e5f6a7b8...64 hex chars"
/// ```
pub fn derive_asymmetric_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
    domain: &[u8],
    base_path: &str,
) -> (String, String) {
    let dh_secret = compute_dh_shared_secret(local_secret_key, remote_pubkey);

    // Derive the local Ed25519 public key from the secret key.
    let signing_key = SigningKey::from_bytes(local_secret_key);
    let local_pubkey_bytes = signing_key.verifying_key().to_bytes();

    let remote_pubkey_bytes = remote_pubkey.to_bytes();

    let write_component = derive_path_hash(domain, &dh_secret, &local_pubkey_bytes);
    let read_component = derive_path_hash(domain, &dh_secret, &remote_pubkey_bytes);

    let write_path = format!("{base_path}/{write_component}");
    let read_path = format!("{base_path}/{read_component}");

    (write_path, read_path)
}

/// Performs X25519 Diffie-Hellman using Ed25519 keys.
///
/// Converts the Ed25519 secret key to an X25519 scalar via `to_scalar_bytes()`
/// (first 32 bytes of `SHA-512(seed)`) and the Ed25519 public key to a
/// Montgomery point via `to_montgomery()`.
fn compute_dh_shared_secret(local_secret_key: &[u8; 32], remote_pubkey: &PublicKey) -> [u8; 32] {
    // Ed25519 seed → X25519 scalar (unclamped; mul_clamped applies clamping)
    let signing_key = SigningKey::from_bytes(local_secret_key);
    let x25519_scalar = signing_key.to_scalar_bytes();

    // Ed25519 public key → X25519 Montgomery point
    let remote_montgomery: MontgomeryPoint = remote_pubkey.verifying_key().to_montgomery();

    // X25519 DH: shared_secret = remote_montgomery * clamp(x25519_scalar)
    remote_montgomery.mul_clamped(x25519_scalar).to_bytes()
}

/// Computes `hex(SHA-256(domain || dh_secret || writer_pubkey))`.
fn derive_path_hash(domain: &[u8], dh_secret: &[u8; 32], writer_pubkey: &[u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(dh_secret);
    hasher.update(writer_pubkey);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky::prelude::Keypair;
    use rand::RngCore;

    /// Helper: create a random Ed25519 keypair and return (secret_bytes, PublicKey).
    fn random_keypair() -> ([u8; 32], PublicKey) {
        let mut secret = [0u8; 32];
        rand::rng().fill_bytes(&mut secret);
        let keypair = Keypair::from_secret(&secret);
        let pubkey = keypair.public_key();
        (secret, pubkey)
    }

    #[test]
    fn test_symmetry_alice_write_equals_bob_read() {
        let (alice_sk, alice_pk) = random_keypair();
        let (bob_sk, bob_pk) = random_keypair();
        let domain = b"test-domain";
        let base = "/test/path";

        let (alice_write, alice_read) = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);
        let (bob_write, bob_read) = derive_asymmetric_paths(&bob_sk, &alice_pk, domain, base);

        // Alice's write path == Bob's read path (and vice versa)
        assert_eq!(alice_write, bob_read, "Alice write != Bob read");
        assert_eq!(alice_read, bob_write, "Alice read != Bob write");
    }

    #[test]
    fn test_asymmetry_write_differs_from_read() {
        let (alice_sk, _alice_pk) = random_keypair();
        let (_bob_sk, bob_pk) = random_keypair();
        let domain = b"test-domain";
        let base = "/test/path";

        let (write_path, read_path) = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);

        assert_ne!(write_path, read_path, "write and read paths must differ");
    }

    #[test]
    fn test_determinism() {
        let (alice_sk, _alice_pk) = random_keypair();
        let (_bob_sk, bob_pk) = random_keypair();
        let domain = b"test-domain";
        let base = "/test/path";

        let result1 = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);
        let result2 = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);

        assert_eq!(result1, result2, "same inputs must produce same outputs");
    }

    #[test]
    fn test_output_format() {
        let (alice_sk, _alice_pk) = random_keypair();
        let (_bob_sk, bob_pk) = random_keypair();
        let domain = b"test-domain";
        let base = "/pub/paykit.app/v0/private";

        let (write_path, read_path) = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);

        // Path format: "{base}/{64 hex chars}"
        let write_suffix = write_path.strip_prefix(&format!("{base}/")).unwrap();
        let read_suffix = read_path.strip_prefix(&format!("{base}/")).unwrap();

        assert_eq!(write_suffix.len(), 64, "SHA-256 hex should be 64 chars");
        assert_eq!(read_suffix.len(), 64, "SHA-256 hex should be 64 chars");

        // All lowercase hex
        assert!(
            write_suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "expected lowercase hex, got: {write_suffix}"
        );
        assert!(
            read_suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "expected lowercase hex, got: {read_suffix}"
        );
    }

    #[test]
    fn test_different_domains_produce_different_paths() {
        let (alice_sk, _alice_pk) = random_keypair();
        let (_bob_sk, bob_pk) = random_keypair();
        let base = "/test/path";

        let (write_a, read_a) = derive_asymmetric_paths(&alice_sk, &bob_pk, b"domain-a", base);
        let (write_b, read_b) = derive_asymmetric_paths(&alice_sk, &bob_pk, b"domain-b", base);

        assert_ne!(
            write_a, write_b,
            "different domains must produce different write paths"
        );
        assert_ne!(
            read_a, read_b,
            "different domains must produce different read paths"
        );
    }

    #[test]
    fn test_different_key_pairs_produce_different_paths() {
        let (alice_sk, _alice_pk) = random_keypair();
        let (_bob_sk, bob_pk) = random_keypair();
        let (_charlie_sk, charlie_pk) = random_keypair();
        let domain = b"test-domain";
        let base = "/test/path";

        let (write_ab, read_ab) = derive_asymmetric_paths(&alice_sk, &bob_pk, domain, base);
        let (write_ac, read_ac) = derive_asymmetric_paths(&alice_sk, &charlie_pk, domain, base);

        assert_ne!(
            write_ab, write_ac,
            "different peers must produce different write paths"
        );
        assert_ne!(
            read_ab, read_ac,
            "different peers must produce different read paths"
        );
    }
}
