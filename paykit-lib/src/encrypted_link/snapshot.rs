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
) -> Vec<u8> {
    let mut bytes = state.serialize();
    bytes.extend_from_slice(&remote_noise_public_key.to_bytes());
    bytes
}

fn deserialize_snapshot(
    bytes: &[u8],
    snapshot_kind: &'static str,
) -> Result<(pubky_noise::serializer::PubkyNoiseSessionState, PublicKey)> {
    let Some(state_len) = bytes.len().checked_sub(32) else {
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
        public_key_from_bytes(&bytes[state_len..], "remote Noise public key")?;
    Ok((state, remote_noise_public_key))
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
    ) -> Self {
        Self {
            state,
            recipient,
            remote_noise_public_key,
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
    /// counterparty's 32-byte identity-wide Noise public key.
    pub fn serialize(&self) -> Vec<u8> {
        serialize_snapshot(&self.state, &self.remote_noise_public_key)
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let (state, remote_noise_public_key) =
            deserialize_snapshot(bytes, "Encrypted Link snapshot")?;
        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link snapshot")?;

        Ok(Self {
            state,
            recipient,
            remote_noise_public_key,
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
    ) -> Self {
        Self {
            state,
            recipient,
            remote_noise_public_key,
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
    /// counterparty's 32-byte identity-wide Noise public key.
    pub fn serialize(&self) -> Vec<u8> {
        serialize_snapshot(&self.state, &self.remote_noise_public_key)
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let (state, remote_noise_public_key) =
            deserialize_snapshot(bytes, "Encrypted Link Handshake snapshot")?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;

        Ok(Self {
            state,
            recipient,
            remote_noise_public_key,
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
}
