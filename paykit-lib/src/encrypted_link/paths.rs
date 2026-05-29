use crate::{PublicKey, PAYKIT_PRIVATE_PATH_PREFIX};

/// Domain separation string for Paykit private payment path derivation.
///
/// Ensures that different applications using the same key pairs derive
/// different storage paths, preventing cross-protocol path collisions.
pub(super) const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-counterparty-pair paths from a DH shared secret. The derivation formula is:
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_ed25519_seed), to_montgomery(remote_ed25519_pk))
/// write_path = "{base}/{hex(SHA-256(domain || dh_secret || local_pk))}"
/// read_path  = "{base}/{hex(SHA-256(domain || dh_secret || remote_pk))}"
/// ```
///
/// # Returns
///
/// A tuple `(write_path, read_path)` where:
/// - `write_path` — the full path the local party writes to on their own homeserver.
/// - `read_path` — the full path the local party reads from on the remote homeserver.
///
/// # Correctness
///
/// For parties Alice and Bob:
/// - `compute_private_paths(alice_sk, bob_pk).0 == compute_private_paths(bob_sk, alice_pk).1`
/// - `compute_private_paths(alice_sk, bob_pk).1 == compute_private_paths(bob_sk, alice_pk).0`
pub(super) fn compute_private_payment_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_PATH_DOMAIN,
        PAYKIT_PRIVATE_PATH_PREFIX,
    )
}
