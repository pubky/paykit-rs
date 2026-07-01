use crate::{private_message_path_prefix, PaykitReceiverId, PublicKey};

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
    local_receiver_id: &PaykitReceiverId,
    remote_receiver_id: &PaykitReceiverId,
) -> (String, String) {
    let local_base = private_message_path_prefix(local_receiver_id);
    let remote_base = private_message_path_prefix(remote_receiver_id);
    let (write_path, _) = pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_PATH_DOMAIN,
        &local_base,
    );
    let (_, read_path) = pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_PATH_DOMAIN,
        &remote_base,
    );
    (write_path, read_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky::Keypair;

    #[test]
    fn test_receiver_scoped_private_paths_are_pairwise_symmetric() {
        let alice_secret = [1u8; 32];
        let bob_secret = [2u8; 32];
        let alice_public = Keypair::from_secret(&alice_secret).public_key();
        let bob_public = Keypair::from_secret(&bob_secret).public_key();
        let alice_receiver = PaykitReceiverId::new("bitkit").unwrap();
        let bob_receiver = PaykitReceiverId::new("tether").unwrap();

        let (alice_write, alice_read) = compute_private_payment_paths(
            &alice_secret,
            &bob_public,
            &alice_receiver,
            &bob_receiver,
        );
        let (bob_write, bob_read) = compute_private_payment_paths(
            &bob_secret,
            &alice_public,
            &bob_receiver,
            &alice_receiver,
        );

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert!(alice_write.starts_with("/pub/paykit/v0/private/bitkit/messages/"));
        assert!(bob_write.starts_with("/pub/paykit/v0/private/tether/messages/"));
    }
}
