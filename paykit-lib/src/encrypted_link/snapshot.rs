use super::EncryptedLinkRecoveryContext;
use crate::{PaykitError, PublicKey, Result};

const HANDSHAKE_CHECKPOINT_VERSION: u8 = 1;
const HANDSHAKE_PACKET_LEN: usize = pubky_noise::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN + 2;
const MAX_HANDSHAKE_PATH_LEN: usize = crate::PAYKIT_PRIVATE_PATH_PREFIX.len() + 1 + 64 + 1 + 10;
const MAX_HANDSHAKE_SNAPSHOT_LEN: usize =
    pubky_noise::serializer::MAX_SESSION_STATE_LEN + 32 + EncryptedLinkRecoveryContext::ENCODED_LEN;

struct PendingHandshakePacket {
    destination_path: String,
    packet: Vec<u8>,
}

fn invalid_handshake_checkpoint() -> PaykitError {
    PaykitError::InvalidData {
        context: "invalid Encrypted Link Handshake checkpoint".into(),
        source: None,
    }
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
    pending: Option<PendingHandshakePacket>,
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
            pending: None,
        }
    }

    pub(super) fn phase(&self) -> pubky_noise::snow_crypto::NoisePhase {
        self.state.phase
    }

    pub(super) fn into_state(self) -> Result<pubky_noise::serializer::PubkyNoiseSessionState> {
        if self.pending.is_some() {
            return Err(PaykitError::Validation(
                "handshake publication must be acknowledged before restoration".into(),
            ));
        }
        Ok(self.state)
    }

    /// Serialize to a compact binary format for durable storage.
    ///
    /// The versioned envelope contains a length-delimited snapshot and at most
    /// one destination path with its exact, complete handshake packet.
    pub fn serialize(&self) -> Vec<u8> {
        let snapshot = serialize_snapshot(
            &self.state,
            &self.remote_noise_public_key,
            &self.recovery_context,
        );
        let mut bytes = vec![HANDSHAKE_CHECKPOINT_VERSION];
        bytes.extend_from_slice(&(snapshot.len() as u16).to_be_bytes());
        bytes.extend_from_slice(&snapshot);
        let path = self
            .pending
            .as_ref()
            .map_or("", |pending| pending.destination_path.as_str());
        bytes.extend_from_slice(&(path.len() as u16).to_be_bytes());
        bytes.extend_from_slice(path.as_bytes());
        if let Some(pending) = &self.pending {
            bytes.extend_from_slice(&pending.packet);
        }
        bytes
    }

    /// Deserialize from bytes previously produced by [`serialize`](Self::serialize).
    ///
    /// Returns [`PaykitError::InvalidData`] if the bytes are malformed or the
    /// embedded public key cannot be reconstructed.
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 5
            || bytes.len()
                > 5 + MAX_HANDSHAKE_SNAPSHOT_LEN + MAX_HANDSHAKE_PATH_LEN + HANDSHAKE_PACKET_LEN
            || bytes[0] != HANDSHAKE_CHECKPOINT_VERSION
        {
            return Err(invalid_handshake_checkpoint());
        }
        let snapshot_len = u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
        if snapshot_len > MAX_HANDSHAKE_SNAPSHOT_LEN || bytes.len() < 5 + snapshot_len {
            return Err(invalid_handshake_checkpoint());
        }
        let path_offset = 3 + snapshot_len;
        let path_len = u16::from_be_bytes([bytes[path_offset], bytes[path_offset + 1]]) as usize;
        let tail = &bytes[path_offset + 2..];
        if path_len > MAX_HANDSHAKE_PATH_LEN
            || tail.len()
                != path_len
                    + if path_len == 0 {
                        0
                    } else {
                        HANDSHAKE_PACKET_LEN
                    }
        {
            return Err(invalid_handshake_checkpoint());
        }
        let (state, remote_noise_public_key, recovery_context) =
            deserialize_snapshot(&bytes[3..path_offset], "Encrypted Link Handshake snapshot")?;
        if state.phase != pubky_noise::snow_crypto::NoisePhase::HandShake {
            return Err(invalid_handshake_checkpoint());
        }
        let recipient = recipient_from_snapshot_state(&state, "Encrypted Link Handshake snapshot")?;
        let mut snapshot = Self {
            state,
            recipient,
            remote_noise_public_key,
            recovery_context,
            pending: None,
        };
        if path_len != 0 {
            let path = std::str::from_utf8(&tail[..path_len])
                .map_err(|_| invalid_handshake_checkpoint())?;
            snapshot.validate_pending_packet(path, &tail[path_len..])?;
            snapshot.pending = Some(PendingHandshakePacket {
                destination_path: path.to_owned(),
                packet: tail[path_len..].to_vec(),
            });
        }
        Ok(snapshot)
    }

    fn validate_pending_packet(&self, path: &str, packet: &[u8]) -> Result<()> {
        use pubky_noise::snow_crypto::{full_handshake_actions, HandshakeAction};
        let slot = self
            .state
            .counter
            .checked_sub(1)
            .ok_or_else(invalid_handshake_checkpoint)?;
        let (directory, suffix) = path
            .rsplit_once('/')
            .ok_or_else(invalid_handshake_checkpoint)?;
        let hash = directory
            .strip_prefix(crate::PAYKIT_PRIVATE_PATH_PREFIX)
            .and_then(|path| path.strip_prefix('/'))
            .ok_or_else(invalid_handshake_checkpoint)?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || suffix != slot.to_string()
            || full_handshake_actions(self.state.pattern, self.state.initiator)
                .map_err(|_| invalid_handshake_checkpoint())?
                .get(slot as usize)
                != Some(&HandshakeAction::Write)
            || packet.len() != HANDSHAKE_PACKET_LEN
        {
            return Err(invalid_handshake_checkpoint());
        }
        let payload_len = u16::from_be_bytes([packet[0], packet[1]]) as usize;
        if payload_len == 0
            || payload_len > HANDSHAKE_PACKET_LEN - 2
            || packet[2 + payload_len..].iter().any(|byte| *byte != 0)
        {
            return Err(invalid_handshake_checkpoint());
        }
        Ok(())
    }

    /// Attach one exact Noise-prepared packet to its post-write snapshot.
    ///
    /// Persist the returned checkpoint before publication. Session authorization
    /// and key rotation remain the caller's responsibility.
    pub(super) fn with_pending_publication(
        mut self,
        local_identity: &PublicKey,
        destination_path: String,
        packet: Vec<u8>,
    ) -> Result<Self> {
        if self.pending.is_some() {
            return Err(invalid_handshake_checkpoint());
        }
        self.validate_pending_packet(&destination_path, &packet)?;
        self.pending = Some(PendingHandshakePacket {
            destination_path,
            packet,
        });
        let secret = self
            .state
            .static_secret
            .ok_or_else(invalid_handshake_checkpoint)?;
        self.pending_publication(local_identity, &secret)?;
        Ok(self)
    }

    /// Read pending bytes only after checking the local key and identity-bound path.
    ///
    /// This does not authorize publication: the caller must validate current
    /// authorization, recovery context, source checkpoint and lease atomically.
    pub fn pending_publication(
        &self,
        local_identity: &PublicKey,
        local_noise_secret: &[u8; 32],
    ) -> Result<Option<(&str, &[u8])>> {
        if self.state.static_secret.as_ref() != Some(local_noise_secret) {
            return Err(PaykitError::Validation(
                "handshake checkpoint Noise key mismatch".into(),
            ));
        }
        let Some(pending) = &self.pending else {
            return Ok(None);
        };
        let (write_path, _) = super::paths::compute_private_payment_paths(
            local_noise_secret,
            local_identity,
            &self.recipient,
            &self.remote_noise_public_key,
            &self.recovery_context,
        );
        if pending.destination_path != format!("{write_path}/{}", self.state.counter - 1) {
            return Err(PaykitError::Validation(
                "handshake publication identity path mismatch".into(),
            ));
        }
        Ok(Some((&pending.destination_path, &pending.packet)))
    }

    /// Clear only the exact published packet; conditionally persist this result
    /// against the original pending checkpoint before advancing or exposing a link.
    pub fn acknowledge_publication(
        mut self,
        destination_path: &str,
        packet: &[u8],
    ) -> Result<Self> {
        if !self.pending.as_ref().is_some_and(|pending| {
            pending.destination_path == destination_path && pending.packet == packet
        }) {
            return Err(PaykitError::Validation(
                "handshake publication acknowledgement mismatch".into(),
            ));
        }
        self.pending = None;
        Ok(self)
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

    /// Return the next remote handshake resource without restoring Noise.
    ///
    /// `None` means local work is required, not that the handshake is idle. The
    /// returned path includes the counterparty identity and can be checked with
    /// `PublicStorage::exists` after validating authorization and recovery state.
    /// This advisory lookup neither authenticates a transcript nor advances it.
    /// Before advancing, reload authoritative state under the ordinary lease.
    /// Session authorization and key rotation remain the caller's responsibility.
    pub fn next_handshake_read_path(
        &self,
        local_identity_public_key: &PublicKey,
        local_noise_secret_key: &[u8; 32],
    ) -> Result<Option<String>> {
        if self.state.phase != pubky_noise::snow_crypto::NoisePhase::HandShake
            || self.state.static_secret.as_ref() != Some(local_noise_secret_key)
        {
            return Err(PaykitError::Validation(
                "snapshot cannot advance with the supplied Noise key".into(),
            ));
        }
        if self.pending.is_some() {
            return Ok(None);
        }
        let slot =
            self.state
                .next_handshake_read_slot()
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("invalid Encrypted Link Handshake cursor: {err:?}"),
                    source: None,
                })?;
        let Some(slot) = slot else {
            return Ok(None);
        };
        let (_, read_path) = super::paths::compute_private_payment_paths(
            local_noise_secret_key,
            local_identity_public_key,
            &self.recipient,
            &self.remote_noise_public_key,
            &self.recovery_context,
        );
        Ok(Some(format!("{}/{read_path}/{slot}", self.recipient)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pubky_noise::{
        serializer::PubkyNoiseSessionState,
        snow_crypto::{HandshakePattern, NoisePhase, NoiseStep},
    };

    fn checkpoint(final_write: bool) -> EncryptedLinkHandshakeSnapshot {
        let recipient = pubky::Keypair::from_secret(&[4; 32]).public_key();
        EncryptedLinkHandshakeSnapshot::from_state(
            PubkyNoiseSessionState {
                version: pubky_noise::serializer::SESSION_STATE_VERSION,
                phase: NoisePhase::HandShake,
                pattern: HandshakePattern::PatternXX,
                initiator: true,
                ephemeral_secret: [1; 32],
                static_secret: Some([2; 32]),
                counter: if final_write { 3 } else { 1 },
                noise_step: if final_write {
                    NoiseStep::Final
                } else {
                    NoiseStep::StepTwo
                },
                sub_step_index: 0,
                handshake_hash: Some([3; 32]),
                link_id: None,
                sending_nonce: 0,
                receiving_nonce: 0,
                write_counter: 0,
                read_counter: 0,
                endpoint_pubkey: recipient.to_bytes(),
                handshake_messages: if final_write {
                    vec![vec![5; 96]]
                } else {
                    Vec::new()
                },
            },
            recipient,
            pubky::Keypair::from_secret(&[5; 32]).public_key(),
            EncryptedLinkRecoveryContext::default(),
        )
    }

    fn pending(final_write: bool) -> (EncryptedLinkHandshakeSnapshot, PublicKey, String, Vec<u8>) {
        let snapshot = checkpoint(final_write);
        let local = pubky::Keypair::from_secret(&[6; 32]).public_key();
        let (path, _) = super::super::paths::compute_private_payment_paths(
            &[2; 32],
            &local,
            snapshot.recipient(),
            snapshot.remote_noise_public_key(),
            snapshot.recovery_context(),
        );
        let path = format!("{path}/{}", snapshot.state.counter - 1);
        let mut packet = vec![0; HANDSHAKE_PACKET_LEN];
        packet[1] = 32;
        packet[2..34].fill(7);
        (
            snapshot
                .with_pending_publication(&local, path.clone(), packet.clone())
                .unwrap(),
            local,
            path,
            packet,
        )
    }

    #[test]
    fn test_handshake_checkpoint_keeps_exact_publication_until_acknowledged() {
        for final_write in [false, true] {
            let plain = checkpoint(final_write).serialize();
            let (snapshot, local, path, packet) = pending(final_write);
            let bytes = snapshot.serialize();
            let load = || EncryptedLinkHandshakeSnapshot::deserialize(&bytes).unwrap();
            assert_eq!(load().serialize(), bytes);
            assert_eq!(
                snapshot.pending_publication(&local, &[2; 32]).unwrap(),
                Some((path.as_str(), packet.as_slice()))
            );
            assert!(snapshot
                .next_handshake_read_path(&local, &[2; 32])
                .unwrap()
                .is_none());
            assert!(load().into_state().is_err());
            for (wrong_path, wrong_packet) in [
                (path.as_str(), &[0; HANDSHAKE_PACKET_LEN][..]),
                ("wrong-path", packet.as_slice()),
            ] {
                assert!(load()
                    .acknowledge_publication(wrong_path, wrong_packet)
                    .is_err());
            }
            assert!(load()
                .with_pending_publication(&local, path.clone(), packet.clone())
                .is_err());
            assert!(snapshot.pending_publication(&local, &[9; 32]).is_err());
            assert!(snapshot
                .pending_publication(
                    &pubky::Keypair::from_secret(&[9; 32]).public_key(),
                    &[2; 32]
                )
                .is_err());
            let acknowledged = load().acknowledge_publication(&path, &packet).unwrap();
            assert_eq!(acknowledged.serialize(), plain);
            assert_eq!(
                acknowledged.into_state().unwrap().phase,
                NoisePhase::HandShake
            );
        }
    }

    #[test]
    fn test_handshake_checkpoint_rejects_malformed_and_unbounded_envelopes() {
        let (mut snapshot, _, path, packet) = pending(false);
        let bytes = snapshot.serialize();
        let path_offset = 3 + u16::from_be_bytes([bytes[1], bytes[2]]) as usize;
        for end in [0, 1, 2, 3, path_offset, path_offset + 1, bytes.len() - 1] {
            assert!(EncryptedLinkHandshakeSnapshot::deserialize(&bytes[..end]).is_err());
        }
        for mutation in [
            "version",
            "state-length",
            "path-length",
            "path",
            "hash",
            "slot",
            "length",
            "empty",
            "padding",
            "trailing",
            "oversized",
            "transport",
        ] {
            let mut malformed = bytes.clone();
            let packet_offset = bytes.len() - HANDSHAKE_PACKET_LEN;
            match mutation {
                "version" => malformed[0] += 1,
                "state-length" => malformed[1..3].fill(255),
                "path-length" => malformed[path_offset..path_offset + 2].fill(255),
                "path" => malformed[path_offset + 2] = 255,
                "hash" => {
                    malformed[path_offset + 3 + crate::PAYKIT_PRIVATE_PATH_PREFIX.len()] = b'/'
                }
                "slot" => malformed[packet_offset - 1] = b'1',
                "length" => malformed[packet_offset..packet_offset + 2].fill(255),
                "empty" => malformed[packet_offset..packet_offset + 2].fill(0),
                "padding" => *malformed.last_mut().unwrap() = 1,
                "trailing" => malformed.push(0),
                "oversized" => malformed.resize(
                    6 + MAX_HANDSHAKE_SNAPSHOT_LEN + MAX_HANDSHAKE_PATH_LEN + HANDSHAKE_PACKET_LEN,
                    0,
                ),
                "transport" => malformed[4] = 1,
                _ => unreachable!(),
            }
            assert!(
                EncryptedLinkHandshakeSnapshot::deserialize(&malformed).is_err(),
                "{mutation}"
            );
        }
        snapshot.state.counter = 2;
        snapshot.state.sub_step_index = 1;
        snapshot.state.handshake_messages = vec![vec![5; 96]];
        let directory = path.rsplit_once('/').unwrap().0;
        assert!(snapshot
            .validate_pending_packet(&format!("{directory}/1"), &packet)
            .is_err());
    }
}
