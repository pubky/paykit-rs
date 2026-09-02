use crate::{PublicKey, PAYKIT_PRIVATE_PATH_PREFIX};

/// Domain separation string for Paykit private payment path derivation.
///
/// Prevents Paykit private paths from colliding with paths derived by other
/// protocols from the same key pairs.
pub(super) const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-counterparty-pair paths from a DH shared secret. The derivation formula is:
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_noise_seed), to_montgomery(remote_noise_pk))
/// write_path = "{base}/{hex(SHA-256(domain || dh_secret || local_noise_pk))}"
/// read_path  = "{base}/{hex(SHA-256(domain || dh_secret || remote_noise_pk))}"
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
/// - Alice's write path equals Bob's read path.
/// - Alice's read path equals Bob's write path.
pub(super) fn compute_private_payment_paths(
    local_secret_key: &[u8; 32],
    remote_noise_public_key: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_noise_public_key,
        PAYKIT_PATH_DOMAIN,
        PAYKIT_PRIVATE_PATH_PREFIX,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{derive_paykit_noise_public_key, derive_paykit_noise_secret_key};

    #[test]
    fn test_private_payment_paths_match_for_derived_identity_noise_keys() {
        let alice = pubky::Keypair::random();
        let bob = pubky::Keypair::random();
        let alice_noise_secret = derive_paykit_noise_secret_key(&alice.secret_key());
        let bob_noise_secret = derive_paykit_noise_secret_key(&bob.secret_key());
        let alice_noise_public = derive_paykit_noise_public_key(&alice.secret_key());
        let bob_noise_public = derive_paykit_noise_public_key(&bob.secret_key());

        let (alice_write, alice_read) =
            compute_private_payment_paths(&alice_noise_secret, &bob_noise_public);
        let (bob_write, bob_read) =
            compute_private_payment_paths(&bob_noise_secret, &alice_noise_public);

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
    }
}
