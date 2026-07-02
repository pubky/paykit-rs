use serde::{Deserialize, Serialize};

use crate::{PaykitError, PaykitReceiverId, PublicKey, Result};

const SNAPSHOT_WIRE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct SnapshotWire {
    version: u32,
    local_receiver_id: PaykitReceiverId,
    remote_receiver_id: PaykitReceiverId,
    state: Vec<u8>,
}

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
    /// Local receiver folder used by this link.
    local_receiver_id: PaykitReceiverId,
    /// Counterparty receiver folder used by this link.
    remote_receiver_id: PaykitReceiverId,
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
            .field("local_receiver_id", &self.local_receiver_id)
            .field("remote_receiver_id", &self.remote_receiver_id)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
        local_receiver_id: PaykitReceiverId,
        remote_receiver_id: PaykitReceiverId,
    ) -> Self {
        Self {
            state,
            recipient,
            local_receiver_id,
            remote_receiver_id,
        }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to the receiver-scoped snapshot wire format for durable storage.
    ///
    /// The output contains the Noise session state plus the local and remote
    /// receiver ids needed to restore it into the same Paykit folders.
    pub fn serialize(&self) -> Vec<u8> {
        let wire = SnapshotWire {
            version: SNAPSHOT_WIRE_VERSION,
            local_receiver_id: self.local_receiver_id.clone(),
            remote_receiver_id: self.remote_receiver_id.clone(),
            state: self.state.serialize(),
        };
        serde_json::to_vec(&wire).expect("Encrypted Link snapshot wire serialization is infallible")
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let wire = deserialize_snapshot_wire(bytes, "Encrypted Link snapshot")?;
        let state = deserialize_noise_state(&wire.state, "Encrypted Link snapshot")?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link snapshot")?;

        Ok(Self {
            state,
            recipient,
            local_receiver_id: wire.local_receiver_id,
            remote_receiver_id: wire.remote_receiver_id,
        })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }

    /// Access the local receiver id embedded in the snapshot.
    pub fn local_receiver_id(&self) -> &PaykitReceiverId {
        &self.local_receiver_id
    }

    /// Access the remote receiver id embedded in the snapshot.
    pub fn remote_receiver_id(&self) -> &PaykitReceiverId {
        &self.remote_receiver_id
    }

    pub(super) fn validate_receiver_scope(
        &self,
        local_receiver_id: &PaykitReceiverId,
        remote_receiver_id: &PaykitReceiverId,
    ) -> Result<()> {
        validate_receiver_scope(
            "Encrypted Link snapshot",
            &self.local_receiver_id,
            &self.remote_receiver_id,
            local_receiver_id,
            remote_receiver_id,
        )
    }
}

fn deserialize_snapshot_wire(bytes: &[u8], snapshot_kind: &'static str) -> Result<SnapshotWire> {
    let wire: SnapshotWire =
        serde_json::from_slice(bytes).map_err(|err| PaykitError::InvalidData {
            context: format!("failed to deserialize {snapshot_kind}: {err}"),
            source: Some(err.into()),
        })?;
    if wire.version != SNAPSHOT_WIRE_VERSION {
        return Err(PaykitError::InvalidData {
            context: format!(
                "unsupported {snapshot_kind} version {}, expected {SNAPSHOT_WIRE_VERSION}",
                wire.version
            ),
            source: None,
        });
    }
    Ok(wire)
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
    /// Local receiver folder used by this handshake.
    local_receiver_id: PaykitReceiverId,
    /// Counterparty receiver folder used by this handshake.
    remote_receiver_id: PaykitReceiverId,
}

impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .field("local_receiver_id", &self.local_receiver_id)
            .field("remote_receiver_id", &self.remote_receiver_id)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkHandshakeSnapshot {
    pub(super) fn from_state(
        state: pubky_noise::serializer::PubkyNoiseSessionState,
        recipient: PublicKey,
        local_receiver_id: PaykitReceiverId,
        remote_receiver_id: PaykitReceiverId,
    ) -> Self {
        Self {
            state,
            recipient,
            local_receiver_id,
            remote_receiver_id,
        }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> pubky_noise::serializer::PubkyNoiseSessionState {
        self.state
    }

    /// Serialize to the receiver-scoped snapshot wire format for durable storage.
    ///
    /// The output contains the Noise session state plus the local and remote
    /// receiver ids needed to restore it into the same Paykit folders.
    pub fn serialize(&self) -> Vec<u8> {
        let wire = SnapshotWire {
            version: SNAPSHOT_WIRE_VERSION,
            local_receiver_id: self.local_receiver_id.clone(),
            remote_receiver_id: self.remote_receiver_id.clone(),
            state: self.state.serialize(),
        };
        serde_json::to_vec(&wire)
            .expect("Encrypted Link Handshake snapshot wire serialization is infallible")
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        let wire = deserialize_snapshot_wire(bytes, "Encrypted Link Handshake snapshot")?;
        let state = deserialize_noise_state(&wire.state, "Encrypted Link Handshake snapshot")?;

        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;

        Ok(Self {
            state,
            recipient,
            local_receiver_id: wire.local_receiver_id,
            remote_receiver_id: wire.remote_receiver_id,
        })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }

    /// Access the local receiver id embedded in the snapshot.
    pub fn local_receiver_id(&self) -> &PaykitReceiverId {
        &self.local_receiver_id
    }

    /// Access the remote receiver id embedded in the snapshot.
    pub fn remote_receiver_id(&self) -> &PaykitReceiverId {
        &self.remote_receiver_id
    }

    pub(super) fn validate_receiver_scope(
        &self,
        local_receiver_id: &PaykitReceiverId,
        remote_receiver_id: &PaykitReceiverId,
    ) -> Result<()> {
        validate_receiver_scope(
            "Encrypted Link Handshake snapshot",
            &self.local_receiver_id,
            &self.remote_receiver_id,
            local_receiver_id,
            remote_receiver_id,
        )
    }
}

fn validate_receiver_scope(
    snapshot_kind: &'static str,
    snapshot_local: &PaykitReceiverId,
    snapshot_remote: &PaykitReceiverId,
    restore_local: &PaykitReceiverId,
    restore_remote: &PaykitReceiverId,
) -> Result<()> {
    if snapshot_local != restore_local || snapshot_remote != restore_remote {
        return Err(PaykitError::Validation(format!(
            "{snapshot_kind} receiver scope mismatch (restore local={restore_local}, remote={restore_remote}; snapshot local={snapshot_local}, remote={snapshot_remote})"
        )));
    }
    Ok(())
}
