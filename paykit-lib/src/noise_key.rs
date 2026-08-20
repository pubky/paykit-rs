use crate::PublicKey;

const PAYKIT_NOISE_KEY_CONTEXT: &str = "paykit/noise";

/// Derive the identity-wide Paykit Noise secret key from a Pubky secret key.
///
/// Every application using the same Pubky identity derives the same Noise key.
/// Application attribution and state ownership are handled separately by the
/// Paykit App Registry and private message metadata.
pub fn derive_paykit_noise_secret_key(pubky_secret_key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key(PAYKIT_NOISE_KEY_CONTEXT, pubky_secret_key)
}

/// Derive the identity-wide Paykit Noise public key from a Pubky secret key.
pub fn derive_paykit_noise_public_key(pubky_secret_key: &[u8; 32]) -> PublicKey {
    pubky::Keypair::from_secret(&derive_paykit_noise_secret_key(pubky_secret_key)).public_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noise_key_derivation_is_deterministic_and_domain_separated() {
        let pubky_secret_key = [7; 32];

        let first = derive_paykit_noise_secret_key(&pubky_secret_key);
        let second = derive_paykit_noise_secret_key(&pubky_secret_key);

        assert_eq!(first, second);
        assert_ne!(first, pubky_secret_key);
        assert_eq!(
            derive_paykit_noise_public_key(&pubky_secret_key),
            pubky::Keypair::from_secret(&first).public_key()
        );
    }
}
