//! Signed companion claims for Pubky Auth approval.

use std::fmt;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crypto_secretbox::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XSalsa20Poly1305,
};
use pubky::{
    deep_links::DeepLink, errors::RequestError, Error as PubkyError, HttpRelayInboxChannel,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::{
    validate_auth_url_capabilities, validate_auth_url_client_id, validate_grant_auth_url,
    PubkySessionBootstrap,
};
use crate::PubkyLocalSecretKey;

const ED25519_SIGNATURE_LEN: usize = 64;
const MAX_QUERY_PARAMETER_LEN: usize = 64;
const MAX_CLAIM_TYPE_LEN: usize = 128;

/// App-defined data delivered alongside one Pubky Auth approval.
///
/// The integrating application owns the claim's payload schema and supplies
/// its unsigned binary representation. Paykit owns request validation,
/// request-bound identity signing, encryption, relay delivery, and approval
/// ordering. It does not expose the underlying cryptographic primitives.
#[derive(Clone, PartialEq, Eq)]
pub struct PubkyAuthCompanionClaim {
    query_parameter: String,
    claim_type: String,
    unsigned_payload: Vec<u8>,
}

impl fmt::Debug for PubkyAuthCompanionClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PubkyAuthCompanionClaim")
            .field("query_parameter", &self.query_parameter)
            .field("claim_type", &self.claim_type)
            .field(
                "unsigned_payload",
                &format_args!("<redacted:{} bytes>", self.unsigned_payload.len()),
            )
            .finish()
    }
}

impl PubkyAuthCompanionClaim {
    /// Create a validated companion claim description.
    ///
    /// `query_parameter` and `claim_type` are protocol identifiers supplied by
    /// the integrating application. They may contain ASCII letters, digits,
    /// hyphens, underscores, and dots. `unsigned_payload` is app-defined and is
    /// not interpreted by Paykit.
    pub fn new(
        query_parameter: impl Into<String>,
        claim_type: impl Into<String>,
        unsigned_payload: Vec<u8>,
    ) -> Result<Self, PubkyAuthCompanionClaimApprovalError> {
        let query_parameter = query_parameter.into();
        validate_protocol_identifier(
            &query_parameter,
            "companion query parameter",
            MAX_QUERY_PARAMETER_LEN,
        )?;
        let claim_type = claim_type.into();
        validate_protocol_identifier(&claim_type, "companion claim type", MAX_CLAIM_TYPE_LEN)?;

        Ok(Self {
            query_parameter,
            claim_type,
            unsigned_payload,
        })
    }

    /// Return the auth URL query parameter that announces this claim.
    pub fn query_parameter(&self) -> &str {
        &self.query_parameter
    }

    /// Return the claim type used for URL validation and relay derivation.
    pub fn claim_type(&self) -> &str {
        &self.claim_type
    }

    /// Return the app-defined bytes signed by the approving identity.
    pub fn unsigned_payload(&self) -> &[u8] {
        &self.unsigned_payload
    }
}

/// Failure returned while approving Pubky Auth with a companion claim.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PubkyAuthCompanionClaimApprovalError {
    /// The URL, claim identifier, secret, relay, or capability request is invalid.
    #[error("invalid Pubky Auth companion request: {reason}")]
    InvalidAuthUrl {
        /// Redacted validation detail.
        reason: String,
    },
    /// The companion claim description is invalid.
    #[error("invalid Pubky Auth companion claim: {reason}")]
    InvalidClaim {
        /// Claim validation detail.
        reason: String,
    },
    /// XSalsa20-Poly1305 encryption failed before relay delivery.
    #[error("companion claim encryption failed: {reason}")]
    EncryptionFailure {
        /// Encryption failure detail.
        reason: String,
    },
    /// The encrypted companion claim could not be delivered to its relay channel.
    #[error("companion claim relay delivery failed: {reason}")]
    RelayDeliveryFailure {
        /// Relay failure detail.
        reason: String,
    },
    /// Pubky grant approval failed after companion delivery succeeded.
    #[error("Pubky Auth approval failed after companion delivery: {reason}")]
    AuthorizationFailure {
        /// Pubky authorization failure detail.
        reason: String,
    },
}

struct CompanionAuthRequest {
    relay: Url,
    secret: [u8; 32],
}

impl PubkySessionBootstrap {
    /// Deliver a signed application-defined claim, then approve Pubky Auth.
    ///
    /// The URL must contain exactly one query parameter matching the claim's
    /// `query_parameter` and `claim_type`. Its requested capabilities must
    /// exactly match `expected_capabilities`, and its client ID must match this
    /// bootstrap's client ID.
    ///
    /// The claim is delivered before the Pubky grant. A claim
    /// validation, encryption, or relay delivery failure therefore leaves the
    /// requesting server unauthorized. Pubky client timeout configuration
    /// remains the caller's responsibility. For a signup request, approval
    /// creates the identity on its requested homeserver after claim delivery
    /// and before issuing the application grant.
    pub async fn approve_auth_with_companion_claim(
        &self,
        auth_url: &str,
        expected_capabilities: &str,
        secret_key: &PubkyLocalSecretKey,
        claim: &PubkyAuthCompanionClaim,
    ) -> Result<(), PubkyAuthCompanionClaimApprovalError> {
        validate_auth_url_client_id(auth_url, &self.client_id).map_err(invalid_auth_url)?;
        let request = parse_companion_auth_request(auth_url, expected_capabilities, claim)?;
        let signed_claim = encode_signed_claim(claim, &request.secret, secret_key);
        let encrypted_claim = encrypt_claim(&signed_claim, &request.secret)?;
        self.deliver_companion_claim(&request, claim.claim_type(), &encrypted_claim)
            .await?;
        self.approve_auth(auth_url, expected_capabilities, secret_key)
            .await
            .map_err(
                |err| PubkyAuthCompanionClaimApprovalError::AuthorizationFailure {
                    reason: err.to_string(),
                },
            )
    }

    async fn deliver_companion_claim(
        &self,
        request: &CompanionAuthRequest,
        claim_type: &str,
        encrypted_claim: &[u8],
    ) -> Result<(), PubkyAuthCompanionClaimApprovalError> {
        let channel = HttpRelayInboxChannel::new(
            request.relay.clone(),
            derive_companion_channel_id(claim_type, &request.secret),
        )
        .map_err(invalid_auth_url)?;
        channel
            .produce(self.pubky.client(), encrypted_claim)
            .await
            .map_err(relay_delivery_failure)
    }
}

fn parse_companion_auth_request(
    auth_url: &str,
    expected_capabilities: &str,
    claim: &PubkyAuthCompanionClaim,
) -> Result<CompanionAuthRequest, PubkyAuthCompanionClaimApprovalError> {
    let url = Url::parse(auth_url).map_err(invalid_auth_url)?;
    if url.scheme() != "pubkyauth" {
        return Err(invalid_auth_url("URL scheme must be pubkyauth"));
    }
    validate_grant_auth_url(auth_url).map_err(invalid_auth_url)?;
    let request = parse_pubky_auth_request(auth_url)?;
    validate_auth_url_capabilities(auth_url, expected_capabilities).map_err(invalid_auth_url)?;

    let claim_type = unique_query_value(&url, claim.query_parameter())?;
    if claim_type != claim.claim_type() {
        return Err(invalid_auth_url(format!(
            "{} query parameter does not match the supplied claim type",
            claim.query_parameter()
        )));
    }

    validate_relay_url(&request.relay)?;
    Ok(request)
}

fn parse_pubky_auth_request(
    auth_url: &str,
) -> Result<CompanionAuthRequest, PubkyAuthCompanionClaimApprovalError> {
    let deep_link: DeepLink = auth_url.parse().map_err(invalid_auth_url)?;
    let (relay, secret) = match deep_link {
        DeepLink::SigninGrant(link) => (link.params().relay.clone(), link.params().secret),
        DeepLink::SignupGrant(link) => (link.params().relay.clone(), link.params().secret),
        DeepLink::Signin(_)
        | DeepLink::Signup(_)
        | DeepLink::DirectSignup(_)
        | DeepLink::SeedExport(_) => {
            return Err(invalid_auth_url(
                "only Pubky grant auth URLs can carry companion claims",
            ))
        }
    };
    Ok(CompanionAuthRequest { relay, secret })
}

fn validate_protocol_identifier(
    value: &str,
    label: &str,
    max_len: usize,
) -> Result<(), PubkyAuthCompanionClaimApprovalError> {
    if value.is_empty() {
        return Err(invalid_claim(format!("{label} must not be empty")));
    }
    if value.len() > max_len {
        return Err(invalid_claim(format!(
            "{label} must not exceed {max_len} bytes"
        )));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_claim(format!(
            "{label} may contain only ASCII letters, digits, hyphens, underscores, and dots"
        )));
    }
    Ok(())
}

fn unique_query_value(
    url: &Url,
    name: &str,
) -> Result<String, PubkyAuthCompanionClaimApprovalError> {
    let mut value = None;
    for (key, query_value) in url.query_pairs() {
        if key != name {
            continue;
        }
        if value.is_some() {
            return Err(invalid_auth_url(format!(
                "duplicate {name} query parameter"
            )));
        }
        value = Some(query_value.into_owned());
    }
    let value = value.ok_or_else(|| invalid_auth_url(format!("missing {name} query parameter")))?;
    if value.is_empty() {
        return Err(invalid_auth_url(format!("empty {name} query parameter")));
    }
    Ok(value)
}

fn validate_relay_url(relay: &Url) -> Result<(), PubkyAuthCompanionClaimApprovalError> {
    if !matches!(relay.scheme(), "http" | "https") || relay.host_str().is_none() {
        return Err(invalid_auth_url(
            "relay URL must be an absolute HTTP(S) URL",
        ));
    }
    Ok(())
}

fn encode_signed_claim(
    claim: &PubkyAuthCompanionClaim,
    auth_secret: &[u8; 32],
    secret_key: &PubkyLocalSecretKey,
) -> Vec<u8> {
    let signature_domain = format!("{}|{}|", claim.query_parameter(), claim.claim_type());
    let request_secret_hash = Sha256::digest(auth_secret);
    let mut signable = Vec::with_capacity(
        signature_domain.len() + request_secret_hash.len() + claim.unsigned_payload().len(),
    );
    signable.extend_from_slice(signature_domain.as_bytes());
    signable.extend_from_slice(&request_secret_hash);
    signable.extend_from_slice(claim.unsigned_payload());

    let signature = secret_key.keypair().sign(&signable);
    let mut signed_claim =
        Vec::with_capacity(claim.unsigned_payload().len() + ED25519_SIGNATURE_LEN);
    signed_claim.extend_from_slice(claim.unsigned_payload());
    signed_claim.extend_from_slice(&signature.to_bytes());
    signed_claim
}

fn derive_companion_channel_id(claim_type: &str, secret: &[u8; 32]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(claim_type.as_bytes());
    hasher.update(b"|");
    hasher.update(secret);
    URL_SAFE_NO_PAD.encode(hasher.finalize().as_bytes())
}

fn encrypt_claim(
    plaintext: &[u8],
    secret: &[u8; 32],
) -> Result<Vec<u8>, PubkyAuthCompanionClaimApprovalError> {
    let cipher = XSalsa20Poly1305::new(secret.into());
    let nonce = XSalsa20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext).map_err(|err| {
        PubkyAuthCompanionClaimApprovalError::EncryptionFailure {
            reason: err.to_string(),
        }
    })?;
    let mut encrypted = Vec::with_capacity(nonce.len() + ciphertext.len());
    encrypted.extend_from_slice(&nonce);
    encrypted.extend_from_slice(&ciphertext);
    Ok(encrypted)
}

fn invalid_auth_url(reason: impl std::fmt::Display) -> PubkyAuthCompanionClaimApprovalError {
    PubkyAuthCompanionClaimApprovalError::InvalidAuthUrl {
        reason: reason.to_string(),
    }
}

fn invalid_claim(reason: impl Into<String>) -> PubkyAuthCompanionClaimApprovalError {
    PubkyAuthCompanionClaimApprovalError::InvalidClaim {
        reason: reason.into(),
    }
}

fn relay_delivery_failure(error: PubkyError) -> PubkyAuthCompanionClaimApprovalError {
    let reason = match error {
        PubkyError::Request(RequestError::Server { status, .. }) => {
            format!("relay returned HTTP status {}", status.as_u16())
        }
        PubkyError::Request(RequestError::Transport(_)) => {
            "relay HTTP transport failed".to_string()
        }
        _ => "relay request failed".to_string(),
    };
    PubkyAuthCompanionClaimApprovalError::RelayDeliveryFailure { reason }
}

#[cfg(test)]
mod tests;
