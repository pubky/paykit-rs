use crate::{
    pubky_routing::{private_message_path_prefix, receiver_pair_path_domain},
    PaykitError, PaykitReceiverPath, PublicKey, Result,
};

/// Domain separation string for Paykit private payment path derivation.
///
/// Keeps Paykit private paths separate from other pubky-noise users that share
/// the same key material.
pub(super) const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-counterparty-receiver-pair paths from a DH shared secret. The path
/// domain includes both `(Pubky identity key, receiver path)` endpoints in
/// canonical order, so two receiver paths under the same Pubky identity do
/// not share private message folders. The DH exchange uses the independent
/// receiver Noise keys published in Receiver Markers.
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_ed25519_seed), to_montgomery(remote_ed25519_pk))
/// path_domain = domain || canonical(local_identity, local_receiver, remote_identity, remote_receiver)
/// write_path  = "{base}/{hex(SHA-256(path_domain || dh_secret || local_noise_pk))}"
/// read_path   = "{base}/{hex(SHA-256(path_domain || dh_secret || remote_noise_pk))}"
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
    local_noise_secret_key: &[u8; 32],
    local_identity_public_key: &PublicKey,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> (String, String) {
    let path_domain = receiver_pair_path_domain(
        PAYKIT_PATH_DOMAIN,
        local_identity_public_key,
        local_receiver_path,
        remote_identity_public_key,
        remote_receiver_path,
    );
    let local_base = private_message_path_prefix(local_receiver_path);
    let remote_base = private_message_path_prefix(remote_receiver_path);
    let (write_path, _) = pubky_noise::path_derivation::derive_asymmetric_paths(
        local_noise_secret_key,
        remote_noise_public_key,
        &path_domain,
        &local_base,
    );
    let (_, read_path) = pubky_noise::path_derivation::derive_asymmetric_paths(
        local_noise_secret_key,
        remote_noise_public_key,
        &path_domain,
        &remote_base,
    );
    (write_path, read_path)
}

pub(super) fn validate_private_payment_paths(
    config: &pubky_noise::PubkyNoiseConfig,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    let local_secret_key = config.pubky_root_keypair.secret_key();
    let local_identity_public_key = config.local_session.info().public_key().clone();
    let (expected_write_path, expected_read_path) = compute_private_payment_paths(
        &local_secret_key,
        &local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );

    if config.write_path != expected_write_path || config.read_path != expected_read_path {
        return Err(PaykitError::Validation(format!(
            "Noise config paths do not match receiver scope (local={local_receiver_path}, remote={remote_receiver_path})"
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky::Keypair;

    #[test]
    fn test_receiver_scoped_private_paths_are_pairwise_symmetric() {
        let alice_noise_secret = [1u8; 32];
        let bob_noise_secret = [2u8; 32];
        let alice_noise_public = Keypair::from_secret(&alice_noise_secret).public_key();
        let bob_noise_public = Keypair::from_secret(&bob_noise_secret).public_key();
        let alice_identity = Keypair::from_secret(&[11u8; 32]).public_key();
        let bob_identity = Keypair::from_secret(&[12u8; 32]).public_key();
        let alice_receiver = PaykitReceiverPath::new("bitkit/wallet").unwrap();
        let bob_receiver = PaykitReceiverPath::new("tether/wallet").unwrap();

        let (alice_write, alice_read) = compute_private_payment_paths(
            &alice_noise_secret,
            &alice_identity,
            &bob_identity,
            &bob_noise_public,
            &alice_receiver,
            &bob_receiver,
        );
        let (bob_write, bob_read) = compute_private_payment_paths(
            &bob_noise_secret,
            &bob_identity,
            &alice_identity,
            &alice_noise_public,
            &bob_receiver,
            &alice_receiver,
        );

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert!(alice_write.starts_with("/pub/paykit/v0/private/bitkit/wallet/messages/"));
        assert!(bob_write.starts_with("/pub/paykit/v0/private/tether/wallet/messages/"));
    }

    #[test]
    fn test_private_paths_include_both_receiver_paths() {
        let alice_noise_secret = [1u8; 32];
        let bob_noise_public = Keypair::from_secret(&[2u8; 32]).public_key();
        let alice_identity = Keypair::from_secret(&[11u8; 32]).public_key();
        let bob_identity = Keypair::from_secret(&[12u8; 32]).public_key();
        let alice_receiver = PaykitReceiverPath::new("bitkit/wallet").unwrap();
        let bob_receiver = PaykitReceiverPath::new("tether/wallet").unwrap();
        let bob_other_receiver = PaykitReceiverPath::new("bitkit/server").unwrap();

        let (write_to_bob_receiver, _) = compute_private_payment_paths(
            &alice_noise_secret,
            &alice_identity,
            &bob_identity,
            &bob_noise_public,
            &alice_receiver,
            &bob_receiver,
        );
        let (write_to_bob_other_receiver, _) = compute_private_payment_paths(
            &alice_noise_secret,
            &alice_identity,
            &bob_identity,
            &bob_noise_public,
            &alice_receiver,
            &bob_other_receiver,
        );

        assert_ne!(write_to_bob_receiver, write_to_bob_other_receiver);
    }
}
