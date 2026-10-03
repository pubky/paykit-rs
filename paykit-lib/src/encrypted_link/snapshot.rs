use super::EncryptedLinkRecoveryContext;
use crate::{PaykitError, PublicKey, Result};

/// Serializable snapshot of an established [`EncryptedLink`](crate::EncryptedLink).
///
/// Serialize with [`serialize`](Self::serialize) and restore with
/// [`restore_encrypted_link`](crate::restore_encrypted_link). Snapshot bytes
/// include sensitive key material and must be stored as secrets.
pub struct EncryptedLinkSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
    /// The counterparty's identity-wide Noise public key.
    remote_noise_public_key: PublicKey,
    recovery_context: EncryptedLinkRecoveryContext,
}

fn recipient_from_snapshot_state(
    state: &pubky_noise::serializer::PubkyNoiseSessionState,
    snapshot_kind: &'static str,
) -> Result<PublicKey> {
    let pkarr_pk =
        pubky::pkarr::PublicKey::try_from(state.endpoint_pubkey.as_slice()).map_err(|err| {
            PaykitError::InvalidData {
                context: format!(
                    "failed to reconstruct recipient public key from {snapshot_kind}: {err}"
                ),
                source: Some(err.into()),
            }
        })?;
    Ok(PublicKey::from(pkarr_pk))
}

fn public_key_from_bytes(bytes: &[u8], context: &'static str) -> Result<PublicKey> {
    let pkarr_pk =
        pubky::pkarr::PublicKey::try_from(bytes).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to reconstruct {context}: {err}"),
            source: Some(err.into()),
        })?;
    Ok(PublicKey::from(pkarr_pk))
}

fn serialize_snapshot(
    state: &pubky_noise::serializer::PubkyNoiseSessionState,
    remote_noise_public_key: &PublicKey,
    recovery_context: &EncryptedLinkRecoveryContext,
) -> Vec<u8> {
    let mut bytes = state.serialize();
    bytes.extend_from_slice(&remote_noise_public_key.to_bytes());
    recovery_context.append_bytes(&mut bytes, true);
    bytes
}

fn deserialize_snapshot(
    bytes: &[u8],
    snapshot_kind: &'static str,
) -> Result<(
    pubky_noise::serializer::PubkyNoiseSessionState,
    PublicKey,
    EncryptedLinkRecoveryContext,
)> {
    let Some(state_len) = bytes
        .len()
        .checked_sub(32 + EncryptedLinkRecoveryContext::ENCODED_LEN)
    else {
        return Err(PaykitError::InvalidData {
            context: format!("{snapshot_kind} is too short"),
            source: None,
        });
    };
    let state = deserialize_noise_state(&bytes[..state_len], snapshot_kind)?;
    if state.serialize().len() != state_len {
        return Err(PaykitError::InvalidData {
            context: format!("{snapshot_kind} has an invalid length"),
            source: None,
        });
    }
    let remote_noise_public_key =
        public_key_from_bytes(&bytes[state_len..state_len + 32], "remote Noise public key")?;
    let recovery_context = EncryptedLinkRecoveryContext::from_bytes(&bytes[state_len + 32..])?;
    Ok((state, remote_noise_public_key, recovery_context))
}

impl std::fmt::Debug for EncryptedLinkSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkSnapshot")
            .field("recipient", &self.recipient)
            .field("remote_noise_public_key", &self.remote_noise_public_key)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
        remote_noise_public_key: PublicKey,
        recovery_context: EncryptedLinkRecoveryContext,
    ) -> Self {
        Self {
            state,
            recipient,
            remote_noise_public_key,
            recovery_context,
        }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output contains the `pubky-noise` session state followed by the
    /// counterparty's 32-byte Noise public key and 72-byte recovery context.
    pub fn serialize(&self) -> Vec<u8> {
        serialize_snapshot(
            &self.state,
            &self.remote_noise_public_key,
            &self.recovery_context,
        )
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let (state, remote_noise_public_key, recovery_context) =
            deserialize_snapshot(bytes, "Encrypted Link snapshot")?;
        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link snapshot")?;

        Ok(Self {
            state,
            recipient,
            remote_noise_public_key,
            recovery_context,
        })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }

    /// Access the counterparty's identity-wide Noise public key.
    pub fn remote_noise_public_key(&self) -> &PublicKey {
        &self.remote_noise_public_key
    }

    /// Recovery attempts used to derive this connection's stream paths.
    pub fn recovery_context(&self) -> &EncryptedLinkRecoveryContext {
        &self.recovery_context
    }

    /// Stable identifier of the completed Noise handshake, unchanged by transport messages.
    pub fn link_id(&self) -> Option<[u8; 32]> {
        self.state.link_id
    }

    /// Check whether the next private message slot exists without restoring Noise.
    ///
    /// This is an advisory HEAD request. It does not authenticate a message or
    /// advance the snapshot. Before receiving, reload the authoritative state
    /// and use the ordinary locked receive workflow. Session authorization and
    /// key rotation remain the caller's responsibility.
    pub async fn has_pending_private_application_message(
        &self,
        public_storage: &pubky::PublicStorage,
        local_identity_public_key: &PublicKey,
        local_noise_secret_key: &[u8; 32],
    ) -> Result<bool> {
        if self.state.phase != pubky_noise::snow_crypto::NoisePhase::Transport
            || self.state.static_secret.as_ref() != Some(local_noise_secret_key)
            || self.state.read_counter >= u32::MAX - 1
            || self.state.receiving_nonce >= u64::MAX - 1
        {
            return Err(PaykitError::Validation(
                "snapshot cannot receive with the supplied Noise key".into(),
            ));
        }
        let (_, read_path) = super::paths::compute_private_payment_paths(
            local_noise_secret_key,
            local_identity_public_key,
            &self.recipient,
            &self.remote_noise_public_key,
            &self.recovery_context,
        );
        public_storage
            .exists(format!(
                "{}/{}/{}",
                self.recipient, read_path, self.state.read_counter
            ))
            .await
            .map_err(|err| PaykitError::Transport {
                context: "failed to check private message availability".into(),
                source: err.into(),
            })
    }
}

fn deserialize_noise_state(
    bytes: &[u8],
    snapshot_kind: &'static str,
) -> Result<pubky_noise::serializer::PubkyNoiseSessionState> {
    pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
        PaykitError::InvalidData {
            context: format!("failed to deserialize {snapshot_kind}: {err:?}"),
            source: None,
        }
    })
}

/// Serializable snapshot of an in-progress
/// [`EncryptedLinkHandshake`](crate::EncryptedLinkHandshake).
///
/// Serialize with [`serialize`](Self::serialize) and restore with
/// [`restore_encrypted_link_handshake`](crate::restore_encrypted_link_handshake).
/// Snapshot bytes include sensitive key material and must be stored as secrets.
pub struct EncryptedLinkHandshakeSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
    /// The counterparty's identity-wide Noise public key.
    remote_noise_public_key: PublicKey,
    recovery_context: EncryptedLinkRecoveryContext,
}

impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .field("remote_noise_public_key", &self.remote_noise_public_key)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkHandshakeSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
        remote_noise_public_key: PublicKey,
        recovery_context: EncryptedLinkRecoveryContext,
    ) -> Self {
        Self {
            state,
            recipient,
            remote_noise_public_key,
            recovery_context,
        }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output contains the `pubky-noise` session state followed by the
    /// counterparty's 32-byte Noise public key and 72-byte recovery context.
    pub fn serialize(&self) -> Vec<u8> {
        serialize_snapshot(
            &self.state,
            &self.remote_noise_public_key,
            &self.recovery_context,
        )
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let (state, remote_noise_public_key, recovery_context) =
            deserialize_snapshot(bytes, "Encrypted Link Handshake snapshot")?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;

        Ok(Self {
            state,
            recipient,
            remote_noise_public_key,
            recovery_context,
        })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }

    /// Access the counterparty's identity-wide Noise public key.
    pub fn remote_noise_public_key(&self) -> &PublicKey {
        &self.remote_noise_public_key
    }

    /// Recovery attempts used to derive this handshake's stream paths.
    pub fn recovery_context(&self) -> &EncryptedLinkRecoveryContext {
        &self.recovery_context
    }
}
