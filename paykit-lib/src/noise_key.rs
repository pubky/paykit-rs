use crate::PublicKey;

const PAYKIT_NOISE_KEY_CONTEXT: &str = "paykit/noise";

/// Derive the identity-wide Paykit Noise secret key from a Paykit identity secret.
///
/// Every application authorized for the same key generation derives the same
/// Noise key. Application attribution is handled separately by the Paykit App
/// Registry and private message metadata.
pub fn derive_paykit_noise_secret_key(paykit_identity_secret_key: &[u8; 32]) -> [u8; 32] {
    blake3::derive_key(PAYKIT_NOISE_KEY_CONTEXT, paykit_identity_secret_key)
}

/// Derive the identity-wide Paykit Noise public key from a Paykit identity secret.
pub fn derive_paykit_noise_public_key(paykit_identity_secret_key: &[u8; 32]) -> PublicKey {
    pubky::Keypair::from_secret(&derive_paykit_noise_secret_key(paykit_identity_secret_key))
        .public_key()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noise_key_derivation_is_deterministic_and_domain_separated() {
        let paykit_identity_secret_key = [7; 32];

        let first = derive_paykit_noise_secret_key(&paykit_identity_secret_key);
        let second = derive_paykit_noise_secret_key(&paykit_identity_secret_key);

        assert_eq!(first, second);
        assert_eq!(
            first,
            [
                139, 233, 237, 224, 3, 115, 2, 131, 96, 220, 144, 73, 253, 178, 176, 121, 197, 120,
                203, 97, 209, 193, 110, 208, 137, 44, 197, 5, 171, 202, 52, 164,
            ]
        );
        assert_ne!(first, paykit_identity_secret_key);
        assert_eq!(
            derive_paykit_noise_public_key(&paykit_identity_secret_key),
            pubky::Keypair::from_secret(&first).public_key()
        );
    }
}
