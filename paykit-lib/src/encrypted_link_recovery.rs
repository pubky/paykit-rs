//! Stateless Encrypted Link Recovery Marker helpers.
//!
//! Recovery markers are public Pubky records used when a runtime decides an
//! Encrypted Link with one counterparty can no longer be used safely. They are
//! not sent over the broken link. Instead, each peer derives a pairwise marker
//! path from its receiver Noise secret key and the counterparty receiver Noise
//! public key, writes a minimal marker to its own homeserver, and polls the
//! counterparty's derived marker path. Pubky identity keys remain part of the
//! receiver-pair domain and select the homeserver used for storage.
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
    pubky_routing::{encrypted_link_recovery_path_prefix, receiver_pair_path_domain},
    validation::{invalid_data, invalid_wire, parse_utc_timestamp, validate_uuid_v4},
    PaykitError, PaykitReceiverPath, Result,
};

const RECOVERY_MARKER_KIND: &str = "paykit.encrypted_link_recovery";
const RECOVERY_MARKER_PATH_DOMAIN: &[u8] = b"paykit-link-recovery-v0";

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
    local_noise_secret_key: &[u8; 32],
    local_identity_public_key: &PublicKey,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> (String, String) {
    let local_base = encrypted_link_recovery_path_prefix(local_receiver_path);
    let remote_base = encrypted_link_recovery_path_prefix(remote_receiver_path);
    let path_domain = receiver_pair_path_domain(
        RECOVERY_MARKER_PATH_DOMAIN,
        local_identity_public_key,
        local_receiver_path,
        remote_identity_public_key,
        remote_receiver_path,
    );
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

/// Publish a local recovery marker and return the path written.
pub async fn publish_encrypted_link_recovery_marker(
    session: &PubkySession,
    local_noise_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
    marker: &EncryptedLinkRecoveryMarker,
) -> Result<String> {
    let local_identity_public_key = session.info().public_key().clone();
    let (write_path, _) = encrypted_link_recovery_marker_paths(
        local_noise_secret_key,
        &local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
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
pub async fn remove_encrypted_link_recovery_marker(
    session: &PubkySession,
    local_noise_secret_key: &[u8; 32],
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> Result<String> {
    let local_identity_public_key = session.info().public_key().clone();
    let (write_path, _) = encrypted_link_recovery_marker_paths(
        local_noise_secret_key,
        &local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
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
pub async fn fetch_encrypted_link_recovery_marker(
    storage: &PublicStorage,
    local_noise_secret_key: &[u8; 32],
    local_identity_public_key: &PublicKey,
    remote_identity_public_key: &PublicKey,
    remote_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
) -> Result<Option<EncryptedLinkRecoveryMarker>> {
    let (_, read_path) = encrypted_link_recovery_marker_paths(
        local_noise_secret_key,
        local_identity_public_key,
        remote_identity_public_key,
        remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
    let addr = format!("{remote_identity_public_key}{read_path}");
    match storage.get(&addr).await {
        Ok(resp) => {
            let bytes = resp.bytes().await.map_err(|err| PaykitError::Transport {
                context: "fetch Encrypted Link recovery marker".into(),
                source: err.into(),
            })?;
            if bytes.is_empty() {
                return Ok(None);
            }
            let raw_json = String::from_utf8(bytes.to_vec()).map_err(|err| {
                let pos = err.utf8_error().valid_up_to();
                invalid_data(
                    format!("Encrypted Link recovery marker is invalid UTF-8 at byte {pos}"),
                    Some(err.into()),
                )
            })?;
            parse_encrypted_link_recovery_marker_json(&raw_json).map(Some)
        }
        Err(err) if is_not_found(&err) => Ok(None),
        Err(err) => Err(PaykitError::Transport {
            context: "fetch Encrypted Link recovery marker".into(),
            source: err.into(),
        }),
    }
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

    fn key_pair() -> ([u8; 32], PublicKey) {
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
    fn test_recovery_marker_paths_are_pairwise_symmetric() {
        let (alice_noise_secret, alice_noise_public) = key_pair();
        let (bob_noise_secret, bob_noise_public) = key_pair();
        let (_, alice_identity_public) = key_pair();
        let (_, bob_identity_public) = key_pair();
        let alice_receiver = PaykitReceiverPath::new("bitkit/wallet").unwrap();
        let bob_receiver = PaykitReceiverPath::new("tether/wallet").unwrap();

        let (alice_write, alice_read) = encrypted_link_recovery_marker_paths(
            &alice_noise_secret,
            &alice_identity_public,
            &bob_identity_public,
            &bob_noise_public,
            &alice_receiver,
            &bob_receiver,
        );
        let (bob_write, bob_read) = encrypted_link_recovery_marker_paths(
            &bob_noise_secret,
            &bob_identity_public,
            &alice_identity_public,
            &alice_noise_public,
            &bob_receiver,
            &alice_receiver,
        );

        assert_eq!(alice_write, bob_read);
        assert_eq!(alice_read, bob_write);
        assert!(
            alice_write.starts_with("/pub/paykit/v0/private/bitkit/wallet/encrypted-link-recovery")
        );
        assert!(
            bob_write.starts_with("/pub/paykit/v0/private/tether/wallet/encrypted-link-recovery")
        );
        assert_ne!(alice_write, alice_read);
    }

    #[test]
    fn test_recovery_marker_paths_include_both_receiver_paths() {
        let (alice_noise_secret, _) = key_pair();
        let (_, bob_noise_public) = key_pair();
        let (_, alice_identity_public) = key_pair();
        let (_, bob_identity_public) = key_pair();
        let alice_receiver = PaykitReceiverPath::new("bitkit/wallet").unwrap();
        let bob_receiver = PaykitReceiverPath::new("tether/wallet").unwrap();
        let bob_other_receiver = PaykitReceiverPath::new("bitkit/server").unwrap();

        let (write_to_bob_receiver, _) = encrypted_link_recovery_marker_paths(
            &alice_noise_secret,
            &alice_identity_public,
            &bob_identity_public,
            &bob_noise_public,
            &alice_receiver,
            &bob_receiver,
        );
        let (write_to_bob_other_receiver, _) = encrypted_link_recovery_marker_paths(
            &alice_noise_secret,
            &alice_identity_public,
            &bob_identity_public,
            &bob_noise_public,
            &alice_receiver,
            &bob_other_receiver,
        );

        assert_ne!(write_to_bob_receiver, write_to_bob_other_receiver);
    }

    // CLAUDE.md contract: public reads treat 404/GONE as absence, never as errors.
    fn server_error(status: StatusCode) -> PubkyError {
        PubkyError::Request(RequestError::Server {
            status,
            message: "test response".into(),
        })
    }

    #[test]
    fn test_is_not_found_matches_not_found_and_gone() {
        assert!(is_not_found(&server_error(StatusCode::NOT_FOUND)));
        assert!(is_not_found(&server_error(StatusCode::GONE)));
    }

    #[test]
    fn test_is_not_found_rejects_other_statuses_and_variants() {
        assert!(!is_not_found(&server_error(
            StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_not_found(&server_error(StatusCode::FORBIDDEN)));

        let validation_error = PubkyError::Request(RequestError::Validation {
            message: "invalid request".into(),
        });
        assert!(!is_not_found(&validation_error));
    }
}
