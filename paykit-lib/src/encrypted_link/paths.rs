use crate::{
    pubky_routing::identity_pair_path_domain, PaykitError, PublicKey, Result,
    PAYKIT_PRIVATE_PATH_PREFIX,
};

/// Domain separation string for Paykit private payment path derivation.
///
/// Prevents Paykit private paths from colliding with paths derived by other
/// protocols from the same key pairs.
pub(super) const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-counterparty-pair paths from a DH shared secret. The domain binds both
/// Pubky identities, so copying a published Noise key cannot alias another
/// identity's folders. App IDs do not participate in path derivation.
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_noise_seed), to_montgomery(remote_noise_pk))
/// path_domain = domain || sorted(local_identity_bytes, remote_identity_bytes)
/// write_path = "{base}/{hex(SHA-256(path_domain || dh_secret || local_noise_pk))}"
/// read_path  = "{base}/{hex(SHA-256(path_domain || dh_secret || remote_noise_pk))}"
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
    local_identity_public_key: &PublicKey,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
) -> (String, String) {
    let path_domain = identity_pair_path_domain(
        PAYKIT_PATH_DOMAIN,
        local_identity_public_key,
        remote_identity_public_key,
    );
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_noise_public_key,
        &path_domain,
        PAYKIT_PRIVATE_PATH_PREFIX,
    )
}

pub(super) fn validate_private_payment_paths(
    config: &pubky_noise::PubkyNoiseConfig,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
) -> Result<()> {
    let (write_path, read_path) = compute_private_payment_paths(
        &config.pubky_root_keypair.secret_key(),
        config.local_session.info().public_key(),
        remote_identity_public_key,
        remote_noise_public_key,
    );
    if config.write_path != write_path || config.read_path != read_path {
        return Err(PaykitError::Validation(
            "Noise config paths do not match Pubky identity pair".into(),
        ));
    }
    Ok(())
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

        let (alice_write, alice_read) = compute_private_payment_paths(
            &alice_noise_secret,
            &alice.public_key(),
            &bob.public_key(),
            &bob_noise_public,
        );
        let (bob_write, bob_read) = compute_private_payment_paths(
            &bob_noise_secret,
            &bob.public_key(),
            &alice.public_key(),
            &alice_noise_public,
        );

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert_ne!(alice_write, alice_read);
    }

    #[test]
    fn test_private_payment_paths_bind_both_identities_with_shared_noise_keys() {
        let alice = pubky::Keypair::from_secret(&[1; 32]).public_key();
        let bob = pubky::Keypair::from_secret(&[2; 32]).public_key();
        let other = pubky::Keypair::from_secret(&[3; 32]).public_key();
        let secret = derive_paykit_noise_secret_key(&[4; 32]);
        let remote_noise = derive_paykit_noise_public_key(&[5; 32]);
        let original = compute_private_payment_paths(&secret, &alice, &bob, &remote_noise);

        for (local, remote) in [(&alice, &other), (&other, &bob)] {
            let changed = compute_private_payment_paths(&secret, local, remote, &remote_noise);
            assert_ne!(original.0, changed.0);
            assert_ne!(original.1, changed.1);
        }
    }
}
