use crate::{PaykitError, PublicKey, Result};

/// Serializable snapshot of an established [`EncryptedLink`](crate::EncryptedLink).
///
/// Created by [`EncryptedLink::snapshot`](crate::EncryptedLink::snapshot). Can
/// be serialized to a compact binary format via [`serialize`](Self::serialize)
/// for durable storage, and deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to [`restore_encrypted_link`](crate::restore_encrypted_link) or
/// [`restore_encrypted_link_from_config`](crate::restore_encrypted_link_from_config)
/// to resume the session after an app restart without re-doing the Noise handshake.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The counterparty public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
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

impl std::fmt::Debug for EncryptedLinkSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
    ) -> Self {
        Self { state, recipient }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 197 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed. Snapshots using the
    /// older 189-byte `pubky-noise` `0.1.0-rc3` format are rejected.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!("failed to deserialize Encrypted Link snapshot: {err:?}"),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Serializable snapshot of an in-progress
/// [`EncryptedLinkHandshake`](crate::EncryptedLinkHandshake).
///
/// Created by
/// [`EncryptedLinkHandshake::snapshot`](crate::EncryptedLinkHandshake::snapshot).
/// Can be serialized to a compact binary format via [`serialize`](Self::serialize)
/// for durable storage, and deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to
/// [`restore_encrypted_link_handshake`](crate::restore_encrypted_link_handshake)
/// or
/// [`restore_encrypted_link_handshake_from_config`](crate::restore_encrypted_link_handshake_from_config)
/// to resume the handshake after an app restart without starting over.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The counterparty public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkHandshakeSnapshot {
    /// The underlying pubky-noise session state.
    state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    recipient: PublicKey,
}

impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkHandshakeSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
    ) -> Self {
        Self { state, recipient }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to a compact binary format for durable storage.
    ///
    /// The output is 197 bytes and can be passed to
    /// [`deserialize`](Self::deserialize) to reconstruct the snapshot.
    pub fn serialize(&self) -> Vec<u8> {
        self.state.serialize()
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// # Errors
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or
    /// the embedded public key cannot be reconstructed. Snapshots using the
    /// older 189-byte `pubky-noise` `0.1.0-rc3` format are rejected.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let state =
            pubky_noise::serializer::PubkyNoiseSessionState::deserialize(bytes).map_err(|err| {
                PaykitError::InvalidData {
                    context: format!(
                        "failed to deserialize Encrypted Link Handshake snapshot: {err:?}"
                    ),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}
