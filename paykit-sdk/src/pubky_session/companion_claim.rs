//! Bitkit watch-only account companion claims for Pubky Auth approval.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use crypto_secretbox::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    XSalsa20Poly1305,
};
use percent_encoding::percent_decode_str;
use pubky::HttpRelayInboxChannel;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use super::{
    parse_capabilities, validate_auth_url_capabilities, validate_sign_in_or_sign_up_auth_url,
    PubkySessionBootstrap,
};
use crate::PubkyLocalSecretKey;

/// Query value identifying the Bitkit watch-only account companion claim.
pub const BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE: &str = "watch-only-account-v1";

/// Exact Pubky capability required by the Bitkit watch-only account setup flow.
pub const BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY: &str = "/pub/paykit/v0/bitkit/server/:rw";

/// Binary protocol version for a Bitkit watch-only account companion claim.
pub const BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_VERSION: u8 = 1;

const BITKIT_CLAIM_QUERY_PARAMETER: &str = "x-bitkit-claim";
const SIGNATURE_DOMAIN: &[u8] = b"x-bitkit-claim|watch-only-account-v1|";
const SERIALIZED_ACCOUNT_XPUB_LEN: usize = 78;
const ED25519_SIGNATURE_LEN: usize = 64;

/// Bitcoin address type represented by a watch-only account claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchOnlyAccountAddressType {
    /// BIP84 native SegWit account (`P2WPKH`).
    NativeSegwit,
}

impl WatchOnlyAccountAddressType {
    const fn wire_code(self) -> u8 {
        match self {
            Self::NativeSegwit => 0,
        }
    }
}

/// App-owned account information delivered with a Bitkit Pubky Auth approval.
///
/// The SDK validates and Base58Check-decodes `account_xpub`, creates the
/// request-bound Ed25519 signature, encrypts the resulting binary claim, and
/// sends it to the companion relay channel. Callers do not handle claim
/// signatures, nonces, relay channel identifiers, or ciphertext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchOnlyAccountClaim {
    /// Companion claim protocol version. Version 1 is currently supported.
    pub version: u8,
    /// BIP account index represented by the account xpub.
    pub account_index: u32,
    /// Address type used to derive addresses from the account xpub.
    pub address_type: WatchOnlyAccountAddressType,
    /// Base58Check-encoded 78-byte serialized account extended public key.
    pub account_xpub: String,
}

impl WatchOnlyAccountClaim {
    /// Create structured watch-only account claim input.
    pub fn new(
        version: u8,
        account_index: u32,
        address_type: WatchOnlyAccountAddressType,
        account_xpub: impl Into<String>,
    ) -> Self {
        Self {
            version,
            account_index,
            address_type,
            account_xpub: account_xpub.into(),
        }
    }
}

/// Failure returned while approving Pubky Auth with a companion claim.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WatchOnlyAccountClaimApprovalError {
    /// The URL, claim type, secret, relay, or capability request is invalid.
    #[error("invalid Pubky Auth companion request: {reason}")]
    InvalidAuthUrl {
        /// Redacted validation detail.
        reason: String,
    },
    /// The structured watch-only account claim is invalid.
    #[error("invalid watch-only account claim: {reason}")]
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
    /// Normal Pubky Auth approval failed after companion delivery succeeded.
    #[error("Pubky Auth approval failed after companion delivery: {reason}")]
    AuthorizationFailure {
        /// Pubky authorization failure detail.
        reason: String,
    },
}

struct CompanionAuthRequest {
    relay: Url,
    secret: [u8; 32],
    secret_text: String,
}

impl PubkySessionBootstrap {
    /// Deliver a signed Bitkit watch-only account claim, then approve Pubky Auth.
    ///
    /// The URL must contain exactly one
    /// `x-bitkit-claim=watch-only-account-v1` parameter and request exactly
    /// [`BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY`]. `expected_capabilities` must
    /// independently name that same capability.
    ///
    /// The claim is delivered before the normal `AuthToken`. A relay delivery
    /// or encryption failure therefore leaves the requesting server
    /// unauthorized. Pubky client timeout configuration remains the caller's
    /// responsibility.
    pub async fn approve_auth_with_companion_claim(
        &self,
        auth_url: &str,
        expected_capabilities: &str,
        secret_key: &PubkyLocalSecretKey,
        claim: &WatchOnlyAccountClaim,
    ) -> Result<(), WatchOnlyAccountClaimApprovalError> {
        let request = parse_companion_auth_request(auth_url, expected_capabilities)?;
        let signed_claim = encode_signed_claim(claim, &request.secret_text, secret_key)?;
        let encrypted_claim = encrypt_claim(&signed_claim, &request.secret)?;
        self.deliver_companion_claim(&request, &encrypted_claim)
            .await?;
        self.approve_auth(auth_url, expected_capabilities, secret_key)
            .await
            .map_err(
                |err| WatchOnlyAccountClaimApprovalError::AuthorizationFailure {
                    reason: err.to_string(),
                },
            )
    }

    async fn deliver_companion_claim(
        &self,
        request: &CompanionAuthRequest,
        encrypted_claim: &[u8],
    ) -> Result<(), WatchOnlyAccountClaimApprovalError> {
        let channel = HttpRelayInboxChannel::new(
            request.relay.clone(),
            derive_companion_channel_id(&request.secret),
        )
        .map_err(invalid_auth_url)?;
        channel
            .produce(self.pubky.client(), encrypted_claim)
            .await
            .map_err(
                |err| WatchOnlyAccountClaimApprovalError::RelayDeliveryFailure {
                    reason: err.to_string(),
                },
            )
    }
}

fn parse_companion_auth_request(
    auth_url: &str,
    expected_capabilities: &str,
) -> Result<CompanionAuthRequest, WatchOnlyAccountClaimApprovalError> {
    let url = Url::parse(auth_url).map_err(invalid_auth_url)?;
    if url.scheme() != "pubkyauth" {
        return Err(invalid_auth_url("URL scheme must be pubkyauth"));
    }
    validate_sign_in_or_sign_up_auth_url(auth_url).map_err(invalid_auth_url)?;
    validate_companion_capabilities(auth_url, expected_capabilities)?;

    let claim_type = unique_query_value(&url, BITKIT_CLAIM_QUERY_PARAMETER)?;
    if claim_type != BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE {
        return Err(invalid_auth_url(format!(
            "unsupported {BITKIT_CLAIM_QUERY_PARAMETER} value"
        )));
    }

    let secret_text = unique_query_value(&url, "secret")?;
    let secret = decode_auth_secret(&secret_text)?;
    let relay_text = unique_query_value(&url, "relay")?;
    let relay = validate_relay_url(&relay_text)?;
    Ok(CompanionAuthRequest {
        relay,
        secret,
        secret_text,
    })
}

fn validate_companion_capabilities(
    auth_url: &str,
    expected_capabilities: &str,
) -> Result<(), WatchOnlyAccountClaimApprovalError> {
    let expected = parse_capabilities(expected_capabilities).map_err(invalid_auth_url)?;
    let required =
        parse_capabilities(BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY).map_err(invalid_auth_url)?;
    if expected != required {
        return Err(invalid_auth_url(format!(
            "expected capabilities must be {BITKIT_WATCH_ONLY_ACCOUNT_CAPABILITY}"
        )));
    }
    validate_auth_url_capabilities(auth_url, expected_capabilities).map_err(invalid_auth_url)
}

fn unique_query_value(
    url: &Url,
    name: &'static str,
) -> Result<String, WatchOnlyAccountClaimApprovalError> {
    let mut value = None;
    for pair in url.query().unwrap_or_default().split('&') {
        let (raw_key, raw_value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = decode_query_component(raw_key, name)?;
        if key != name {
            continue;
        }
        if value.is_some() {
            return Err(invalid_auth_url(format!(
                "duplicate {name} query parameter"
            )));
        }
        value = Some(decode_query_component(raw_value, name)?);
    }
    let value = value.ok_or_else(|| invalid_auth_url(format!("missing {name} query parameter")))?;
    if value.is_empty() {
        return Err(invalid_auth_url(format!("empty {name} query parameter")));
    }
    Ok(value)
}

fn decode_query_component(
    value: &str,
    parameter_name: &'static str,
) -> Result<String, WatchOnlyAccountClaimApprovalError> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len()
            || !bytes[index + 1].is_ascii_hexdigit()
            || !bytes[index + 2].is_ascii_hexdigit()
        {
            return Err(invalid_auth_url(format!(
                "invalid percent encoding in {parameter_name} query parameter"
            )));
        }
        index += 3;
    }
    percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .map_err(|_| {
            invalid_auth_url(format!(
                "{parameter_name} query parameter must be valid UTF-8"
            ))
        })
}

fn decode_auth_secret(secret_text: &str) -> Result<[u8; 32], WatchOnlyAccountClaimApprovalError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(secret_text)
        .map_err(invalid_auth_url)?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        invalid_auth_url(format!("auth secret must be 32 bytes, got {}", bytes.len()))
    })
}

fn validate_relay_url(value: &str) -> Result<Url, WatchOnlyAccountClaimApprovalError> {
    let relay = Url::parse(value).map_err(invalid_auth_url)?;
    if !matches!(relay.scheme(), "http" | "https") || relay.host_str().is_none() {
        return Err(invalid_auth_url(
            "relay URL must be an absolute HTTP(S) URL",
        ));
    }
    Ok(relay)
}

fn encode_signed_claim(
    claim: &WatchOnlyAccountClaim,
    auth_secret_text: &str,
    secret_key: &PubkyLocalSecretKey,
) -> Result<Vec<u8>, WatchOnlyAccountClaimApprovalError> {
    let unsigned_claim = encode_unsigned_claim(claim)?;
    let request_secret_hash = Sha256::digest(auth_secret_text.as_bytes());
    let mut signable = Vec::with_capacity(
        SIGNATURE_DOMAIN.len() + request_secret_hash.len() + unsigned_claim.len(),
    );
    signable.extend_from_slice(SIGNATURE_DOMAIN);
    signable.extend_from_slice(&request_secret_hash);
    signable.extend_from_slice(&unsigned_claim);

    let signature = secret_key.keypair().sign(&signable);
    let mut signed_claim = Vec::with_capacity(unsigned_claim.len() + ED25519_SIGNATURE_LEN);
    signed_claim.extend_from_slice(&unsigned_claim);
    signed_claim.extend_from_slice(&signature.to_bytes());
    Ok(signed_claim)
}

fn encode_unsigned_claim(
    claim: &WatchOnlyAccountClaim,
) -> Result<Vec<u8>, WatchOnlyAccountClaimApprovalError> {
    if claim.version != BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_VERSION {
        return Err(invalid_claim(format!(
            "unsupported protocol version {}",
            claim.version
        )));
    }
    if claim.account_index > 0x7fff_ffff {
        return Err(invalid_claim("BIP account index must be below 2^31"));
    }
    let account_xpub = decode_account_xpub(&claim.account_xpub)?;
    let mut bytes = Vec::with_capacity(1 + 4 + 1 + SERIALIZED_ACCOUNT_XPUB_LEN);
    bytes.push(claim.version);
    bytes.extend_from_slice(&claim.account_index.to_be_bytes());
    bytes.push(claim.address_type.wire_code());
    bytes.extend_from_slice(&account_xpub);
    Ok(bytes)
}

fn decode_account_xpub(value: &str) -> Result<Vec<u8>, WatchOnlyAccountClaimApprovalError> {
    let bytes = bs58::decode(value)
        .with_check(None)
        .into_vec()
        .map_err(|err| invalid_claim(format!("invalid Base58Check account xpub: {err}")))?;
    if bytes.len() != SERIALIZED_ACCOUNT_XPUB_LEN {
        return Err(invalid_claim(format!(
            "serialized account xpub must be {SERIALIZED_ACCOUNT_XPUB_LEN} bytes, got {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

fn derive_companion_channel_id(secret: &[u8; 32]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(BITKIT_WATCH_ONLY_ACCOUNT_CLAIM_TYPE.as_bytes());
    hasher.update(b"|");
    hasher.update(secret);
    URL_SAFE_NO_PAD.encode(hasher.finalize().as_bytes())
}

fn encrypt_claim(
    plaintext: &[u8],
    secret: &[u8; 32],
) -> Result<Vec<u8>, WatchOnlyAccountClaimApprovalError> {
    let cipher = XSalsa20Poly1305::new(secret.into());
    let nonce = XSalsa20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext).map_err(|err| {
        WatchOnlyAccountClaimApprovalError::EncryptionFailure {
            reason: err.to_string(),
        }
    })?;
    let mut encrypted = Vec::with_capacity(nonce.len() + ciphertext.len());
    encrypted.extend_from_slice(&nonce);
    encrypted.extend_from_slice(&ciphertext);
    Ok(encrypted)
}

fn invalid_auth_url(reason: impl std::fmt::Display) -> WatchOnlyAccountClaimApprovalError {
    WatchOnlyAccountClaimApprovalError::InvalidAuthUrl {
        reason: reason.to_string(),
    }
}

fn invalid_claim(reason: impl Into<String>) -> WatchOnlyAccountClaimApprovalError {
    WatchOnlyAccountClaimApprovalError::InvalidClaim {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests;
