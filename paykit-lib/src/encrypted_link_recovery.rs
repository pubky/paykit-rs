//! Stateless Encrypted Link Recovery Marker helpers.
//!
//! Recovery markers are public Pubky records used when a runtime decides an
//! Encrypted Link with one counterparty can no longer be used safely. They are
//! not sent over the broken link. Instead, each peer derives a pairwise marker
//! path from its local Noise secret key and the counterparty Noise public key, writes a
//! minimal marker to its own homeserver, and polls the counterparty's derived
//! marker path.
//!
//! Marker payloads intentionally contain only `version`, `kind`, `attempt_id`,
//! and `created_at`. They do not contain payment data, endpoint data, message
//! counters, peer labels, or recovery transcripts.
//!
//! This module only provides the wire shape and Pubky publish/fetch/remove
//! helpers. A higher-level runtime or SDK decides when a link is
//! recovery-required, whether public markers are allowed by policy, and how to
//! relink after observing a marker.

use pubky::{
    errors::RequestError, Error as PubkyError, PubkySession, PublicKey, PublicStorage, StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::{
    pubky_routing::fetch_text,
    validation::{invalid_data, invalid_wire, parse_utc_timestamp, validate_uuid_v4},
    PaykitError, Result, PAYKIT_ENCRYPTED_LINK_RECOVERY_PATH_PREFIX,
};

const RECOVERY_MARKER_KIND: &str = "paykit.encrypted_link_recovery";
const RECOVERY_MARKER_PATH_DOMAIN: &[u8] = b"paykit-link-recovery-v0";
const RECOVERY_MARKER_MAX_BYTES: usize = 4 * 1024;

/// Minimal public marker that asks a counterparty to relink an Encrypted Link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptedLinkRecoveryMarker {
    attempt_id: String,
    created_at: String,
}

impl EncryptedLinkRecoveryMarker {
    /// Create a marker from a UUID-v4 attempt id and RFC3339 UTC timestamp.
    pub fn new(attempt_id: impl Into<String>, created_at: impl Into<String>) -> Result<Self> {
        let attempt_id = validate_uuid_v4(attempt_id.into(), "Encrypted Link recovery attempt ID")?;
        let created_at = created_at.into();
        parse_utc_timestamp(&created_at, "Encrypted Link recovery marker created_at")?;
        Ok(Self {
            attempt_id,
            created_at,
        })
    }

    /// Generate a marker with a fresh UUID-v4 attempt id.
    pub fn new_v4(created_at: impl Into<String>) -> Result<Self> {
        Self::new(uuid::Uuid::new_v4().to_string(), created_at)
    }

    /// Stable recovery attempt id.
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    /// Marker creation time as an RFC3339 UTC timestamp.
    pub fn created_at(&self) -> &str {
        &self.created_at
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryMarkerWire {
    version: u8,
    kind: String,
    attempt_id: String,
    created_at: String,
}

impl From<&EncryptedLinkRecoveryMarker> for RecoveryMarkerWire {
    fn from(marker: &EncryptedLinkRecoveryMarker) -> Self {
        Self {
            version: 1,
            kind: RECOVERY_MARKER_KIND.into(),
            attempt_id: marker.attempt_id.clone(),
            created_at: marker.created_at.clone(),
        }
    }
}

impl TryFrom<RecoveryMarkerWire> for EncryptedLinkRecoveryMarker {
    type Error = PaykitError;

    fn try_from(wire: RecoveryMarkerWire) -> Result<Self> {
        if wire.version != 1 || wire.kind != RECOVERY_MARKER_KIND {
            return Err(invalid_data(
                format!(
                    "unsupported Encrypted Link recovery marker version/kind: {}/{}",
                    wire.version, wire.kind
                ),
                None,
            ));
        }
        Self::new(wire.attempt_id, wire.created_at)
            .map_err(|err| invalid_wire(err, "Encrypted Link recovery marker"))
    }
}

/// Serialize an Encrypted Link Recovery Marker to canonical JSON.
pub fn serialize_encrypted_link_recovery_marker(
    marker: &EncryptedLinkRecoveryMarker,
) -> Result<String> {
    serde_json::to_string(&RecoveryMarkerWire::from(marker)).map_err(|err| {
        PaykitError::Validation(format!(
            "failed to serialize Encrypted Link recovery marker: {err}"
        ))
    })
}

/// Parse an Encrypted Link Recovery Marker JSON payload.
pub fn parse_encrypted_link_recovery_marker_json(
    raw_json: &str,
) -> Result<EncryptedLinkRecoveryMarker> {
    if raw_json.len() > RECOVERY_MARKER_MAX_BYTES {
        return Err(invalid_data(
            format!("Encrypted Link recovery marker exceeds {RECOVERY_MARKER_MAX_BYTES} bytes"),
            None,
        ));
    }
    let wire = serde_json::from_str::<RecoveryMarkerWire>(raw_json).map_err(|err| {
        invalid_data(
            format!("Encrypted Link recovery marker JSON is invalid: {err}"),
            Some(err.into()),
        )
    })?;
    wire.try_into()
}

/// Compute local write and remote read paths for recovery markers.
pub fn encrypted_link_recovery_marker_paths(
    local_secret_key: &[u8; 32],
    remote_noise_public_key: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_noise_public_key,
        RECOVERY_MARKER_PATH_DOMAIN,
        PAYKIT_ENCRYPTED_LINK_RECOVERY_PATH_PREFIX,
    )
}

/// Publish a local recovery marker and return the path written.
///
/// The caller must provide a Pubky session with write access to the derived
/// marker path and the same local Noise key used for the peer relationship. The
/// caller owns session creation, capability scope, key rotation, and recovery
/// policy. Request timeouts are configured on the Pubky client.
///
/// # Errors
///
/// Returns [`PaykitError::Validation`] when the marker cannot be serialized and
/// [`PaykitError::Transport`] when Pubky storage rejects the write.
pub async fn publish_encrypted_link_recovery_marker(
    session: &PubkySession,
    local_secret_key: &[u8; 32],
    remote_noise_public_key: &PublicKey,
    marker: &EncryptedLinkRecoveryMarker,
) -> Result<String> {
    let (write_path, _) =
        encrypted_link_recovery_marker_paths(local_secret_key, remote_noise_public_key);
    let payload = serialize_encrypted_link_recovery_marker(marker)?;
    session
        .storage()
        .put(write_path.clone(), payload)
        .await
        .map_err(|err| PaykitError::Transport {
            context: "publish Encrypted Link recovery marker".into(),
            source: err.into(),
        })?;
    Ok(write_path)
}

/// Remove the local recovery marker for a counterparty.
///
/// Missing markers are treated as success. The caller must provide a Pubky
/// session with write access to the derived marker path and owns session
/// creation, capability scope, key rotation, recovery policy, and Pubky-client
/// timeout configuration.
///
/// # Errors
///
/// Returns [`PaykitError::Transport`] when Pubky storage fails for a reason
/// other than the marker already being absent.
pub async fn remove_encrypted_link_recovery_marker(
    session: &PubkySession,
    local_secret_key: &[u8; 32],
    remote_noise_public_key: &PublicKey,
) -> Result<String> {
    let (write_path, _) =
        encrypted_link_recovery_marker_paths(local_secret_key, remote_noise_public_key);
    match session.storage().delete(write_path.clone()).await {
        Ok(_) => Ok(write_path),
        Err(err) if is_not_found(&err) => Ok(write_path),
        Err(err) => Err(PaykitError::Transport {
            context: "remove Encrypted Link recovery marker".into(),
            source: err.into(),
        }),
    }
}

/// Fetch a counterparty's recovery marker, if one is present.
///
/// This unauthenticated read returns `Ok(None)` when the derived marker resource
/// is missing or empty. The caller supplies the peer identity and Noise keys and
/// owns key rotation, recovery policy, and Pubky-client timeout configuration.
///
/// # Errors
///
/// Returns [`PaykitError::InvalidData`] for malformed remote marker data and
/// [`PaykitError::Transport`] when Pubky storage cannot be read.
pub async fn fetch_encrypted_link_recovery_marker(
    storage: &PublicStorage,
    local_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
) -> Result<Option<EncryptedLinkRecoveryMarker>> {
    let (_, read_path) =
        encrypted_link_recovery_marker_paths(local_secret_key, remote_noise_public_key);
    let addr = format!("{remote_identity_public_key}{read_path}");
    fetch_text(
        storage,
        addr,
        "fetch Encrypted Link recovery marker",
        Some(RECOVERY_MARKER_MAX_BYTES),
    )
    .await?
    .map(|raw_json| parse_encrypted_link_recovery_marker_json(&raw_json))
    .transpose()
}

fn is_not_found(err: &PubkyError) -> bool {
    matches!(
        err,
        PubkyError::Request(RequestError::Server { status, .. })
            if *status == StatusCode::NOT_FOUND || *status == StatusCode::GONE
    )
}

#[cfg(test)]
mod tests {
    use pubky::Keypair;

    use super::*;

    fn secret_pair() -> ([u8; 32], PublicKey) {
        let keypair = Keypair::random();
        (keypair.secret_key(), keypair.public_key())
    }

    #[test]
    fn test_recovery_marker_json_round_trips() {
        let marker = EncryptedLinkRecoveryMarker::new(
            "650e8400-e29b-41d4-a716-446655440000",
            "2026-06-03T12:00:00Z",
        )
        .unwrap();

        let raw = serialize_encrypted_link_recovery_marker(&marker).unwrap();
        let parsed = parse_encrypted_link_recovery_marker_json(&raw).unwrap();

        assert_eq!(parsed, marker);
        assert!(raw.contains("\"kind\":\"paykit.encrypted_link_recovery\""));
    }

    #[test]
    fn test_recovery_marker_rejects_extra_fields() {
        let raw = r#"{"version":1,"kind":"paykit.encrypted_link_recovery","attempt_id":"650e8400-e29b-41d4-a716-446655440000","created_at":"2026-06-03T12:00:00Z","peer":"extra"}"#;

        let result = parse_encrypted_link_recovery_marker_json(raw);

        assert!(matches!(result, Err(PaykitError::InvalidData { .. })));
    }

    #[test]
    fn test_recovery_marker_rejects_oversized_payload() {
        let raw = " ".repeat(RECOVERY_MARKER_MAX_BYTES + 1);

        let result = parse_encrypted_link_recovery_marker_json(&raw);

        assert!(matches!(result, Err(PaykitError::InvalidData { .. })));
    }

    #[test]
    fn test_recovery_marker_paths_are_pairwise_symmetric() {
        let (alice_secret, alice_public) = secret_pair();
        let (bob_secret, bob_public) = secret_pair();

        let (alice_write, alice_read) =
            encrypted_link_recovery_marker_paths(&alice_secret, &bob_public);
        let (bob_write, bob_read) =
            encrypted_link_recovery_marker_paths(&bob_secret, &alice_public);

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert!(alice_write.starts_with(PAYKIT_ENCRYPTED_LINK_RECOVERY_PATH_PREFIX));
        assert_ne!(alice_write, alice_read);
    }
}
