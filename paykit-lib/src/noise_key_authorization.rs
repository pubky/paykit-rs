//! Identity-signed authorization of the current Paykit Noise key.

use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};

use crate::{pubky_routing, validation::invalid_data, PaykitError, PublicKey, Result};

const KIND: &str = "paykit.noise_key_authorization";
const VERSION: u8 = 1;
const MAX_BYTES: usize = 4096;
const SIGNING_DOMAIN: &[u8] = b"paykit.noise_key_authorization/v1\0";

/// Pubky identity approval of Noise routing and handshake keys at one generation.
///
/// Deserialization verifies the signature. Network readers must additionally
/// check the expected owner. Freshness depends on the owner's homeserver;
/// retain the highest observed generation to reject subsequent rollbacks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "AuthorizationWire", into = "AuthorizationWire")]
pub struct PaykitNoiseKeyAuthorization {
    owner: PublicKey,
    noise_public_key: PublicKey,
    noise_static_public_key: [u8; 32],
    key_generation: u64,
    signature: String,
}

impl PaykitNoiseKeyAuthorization {
    /// Sign with the Pubky identity key, never with delegated Paykit key material.
    /// Both public keys are derived from the supplied Noise static secret.
    pub fn sign(
        identity: &pubky::Keypair,
        noise_secret_key: &[u8; 32],
        key_generation: u64,
    ) -> Result<Self> {
        if key_generation == 0 {
            return Err(PaykitError::Validation(
                "Paykit key generation must be positive".into(),
            ));
        }
        let owner = identity.public_key();
        let noise_public_key = pubky::Keypair::from_secret(noise_secret_key).public_key();
        let noise_static_public_key = pubky_noise::derive_static_public_key(noise_secret_key);
        let signature = identity.sign(&signing_bytes(
            &owner,
            &noise_public_key,
            &noise_static_public_key,
            key_generation,
        ));
        Ok(Self {
            owner,
            noise_public_key,
            noise_static_public_key,
            key_generation,
            signature: STANDARD.encode(signature.to_bytes()),
        })
    }

    /// Identity that authorized this key.
    pub fn owner(&self) -> &PublicKey {
        &self.owner
    }

    /// Ed25519 routing key approved for this identity.
    pub fn noise_public_key(&self) -> &PublicKey {
        &self.noise_public_key
    }

    /// X25519 static key that the Noise handshake peer must present.
    pub fn noise_static_public_key(&self) -> &[u8; 32] {
        &self.noise_static_public_key
    }

    /// Monotonically increasing Paykit key generation.
    pub fn key_generation(&self) -> u64 {
        self.key_generation
    }

    /// Reject another owner, a lower generation, or a different key at the same generation.
    /// A reader may skip generations while offline.
    pub fn validate_against(&self, previous: &Self) -> Result<()> {
        if self.owner != previous.owner
            || self.key_generation < previous.key_generation
            || (self.key_generation == previous.key_generation
                && (self.noise_public_key != previous.noise_public_key
                    || self.noise_static_public_key != previous.noise_static_public_key))
        {
            return Err(invalid_data(
                "Paykit Noise key authorization conflicts with the previously verified key",
                None,
            ));
        }
        Ok(())
    }
}

// Fixed-width fields avoid JSON serialization or separator ambiguities.
fn signing_bytes(
    owner: &PublicKey,
    noise: &PublicKey,
    noise_static: &[u8; 32],
    generation: u64,
) -> Vec<u8> {
    [
        SIGNING_DOMAIN,
        owner.as_bytes(),
        noise.as_bytes(),
        noise_static,
        &generation.to_be_bytes(),
    ]
    .concat()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthorizationWire {
    version: u8,
    kind: String,
    owner: PublicKey,
    noise_public_key: PublicKey,
    noise_static_public_key: String,
    key_generation: u64,
    signature: String,
}

impl From<PaykitNoiseKeyAuthorization> for AuthorizationWire {
    fn from(record: PaykitNoiseKeyAuthorization) -> Self {
        Self {
            version: VERSION,
            kind: KIND.into(),
            owner: record.owner,
            noise_public_key: record.noise_public_key,
            noise_static_public_key: hex::encode(record.noise_static_public_key),
            key_generation: record.key_generation,
            signature: record.signature,
        }
    }
}

impl TryFrom<AuthorizationWire> for PaykitNoiseKeyAuthorization {
    type Error = PaykitError;

    fn try_from(wire: AuthorizationWire) -> Result<Self> {
        if wire.version != VERSION || wire.kind != KIND || wire.key_generation == 0 {
            return Err(invalid_data(
                "invalid Paykit Noise key authorization header",
                None,
            ));
        }
        let mut noise_static_public_key = [0; 32];
        hex::decode_to_slice(&wire.noise_static_public_key, &mut noise_static_public_key)
            .map_err(|err| invalid_data("invalid Noise static public key", Some(err.into())))?;
        let bytes = STANDARD.decode(&wire.signature).map_err(|err| {
            invalid_data(
                "invalid Paykit Noise key authorization signature",
                Some(err.into()),
            )
        })?;
        let signature = bytes.as_slice().try_into().map_err(|_| {
            invalid_data(
                "invalid Paykit Noise key authorization signature length",
                None,
            )
        })?;
        wire.owner
            .verify(
                &signing_bytes(
                    &wire.owner,
                    &wire.noise_public_key,
                    &noise_static_public_key,
                    wire.key_generation,
                ),
                &signature,
            )
            .map_err(|err| {
                invalid_data(
                    "Paykit Noise key authorization signature verification failed",
                    Some(err.into()),
                )
            })?;
        Ok(Self {
            owner: wire.owner,
            noise_public_key: wire.noise_public_key,
            noise_static_public_key,
            key_generation: wire.key_generation,
            signature: wire.signature,
        })
    }
}

fn parse(bytes: &[u8], owner: &PublicKey) -> Result<PaykitNoiseKeyAuthorization> {
    let record: PaykitNoiseKeyAuthorization = serde_json::from_slice(bytes)
        .map_err(|err| invalid_data("invalid Paykit Noise key authorization", Some(err.into())))?;
    if record.owner() != owner {
        return Err(invalid_data(
            "Paykit Noise key authorization belongs to another identity",
            None,
        ));
    }
    Ok(record)
}

/// Read and verify the identity's current Noise key. Missing records return `None`.
///
/// The caller is responsible for session creation, capability scope, key rotation,
/// and Pubky request timeouts. This record has no expiry: the homeserver must
/// return its current contents, and peers should retain verified generations.
pub async fn get_paykit_noise_key_authorization(
    storage: &pubky::PublicStorage,
    owner: &PublicKey,
) -> Result<Option<PaykitNoiseKeyAuthorization>> {
    pubky_routing::fetch_text(
        storage,
        format!(
            "{owner}{}",
            pubky_routing::PAYKIT_NOISE_KEY_AUTHORIZATION_PATH
        ),
        "fetch Paykit Noise key authorization",
        Some(MAX_BYTES),
    )
    .await?
    .map(|body| parse(body.as_bytes(), owner))
    .transpose()
}

/// Publish a signed Noise key authorization under an exclusive homeserver lock.
///
/// Requires separate write access to `/pub/paykit-authority/v0/current-key.json`.
/// Never delegate that access to ordinary Paykit apps. Replacement advances
/// exactly one generation. Repeating the same publication is harmless.
/// Persist the rotated shared state before publishing.
/// Session creation, capability scope, key rotation, and timeouts are the caller's
/// responsibility. The homeserver must fence writes after lock ownership is lost.
pub async fn publish_paykit_noise_key_authorization(
    session: &pubky::PubkySession,
    record: &PaykitNoiseKeyAuthorization,
) -> Result<()> {
    if session.info().public_key() != record.owner() {
        return Err(PaykitError::Validation(
            "Noise key authorizer session belongs to another identity".into(),
        ));
    }
    let path = pubky_routing::PAYKIT_NOISE_KEY_AUTHORIZATION_PATH;
    pubky_routing::with_write_lock(session, path, |lock| async move {
        let previous = match session.storage().get(path).await {
            Ok(response) => {
                let bytes = pubky_routing::read_bounded_body(
                    response,
                    MAX_BYTES,
                    "read current Noise key authorization",
                )
                .await?;
                Some(parse(&bytes, record.owner())?)
            }
            Err(err) if pubky_routing::is_not_found(&err) => None,
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "read current Noise key authorization".into(),
                    source: err.into(),
                })
            }
        };
        if let Some(previous) = previous {
            record.validate_against(&previous)?;
            if record.key_generation == previous.key_generation {
                return Ok(());
            }
            if previous.key_generation.checked_add(1) != Some(record.key_generation) {
                return Err(PaykitError::Validation(
                    "Noise key authorization must advance one generation".into(),
                ));
            }
        }
        let body =
            serde_json::to_vec(record).map_err(|err| PaykitError::Validation(err.to_string()))?;
        session
            .storage()
            .put_locked(&lock, body)
            .await
            .map_err(|err| PaykitError::Transport {
                context: "publish Paykit Noise key authorization".into(),
                source: err.into(),
            })?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests;
