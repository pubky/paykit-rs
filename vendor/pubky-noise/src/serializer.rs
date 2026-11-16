//! Session state serialization for backup and restore.
//!
//! [`PubkyNoiseSessionState`] captures all the information needed to restore a
//! `PubkyNoiseEncryptor` session, whether it was interrupted during the handshake
//! or is already in transport mode.

use crate::snow_crypto::{
    full_handshake_actions, resolve_pattern, HandshakeAction, HandshakePattern, NoisePhase,
    NoiseStep, PUBKY_NOISE_CIPHERTEXT_LEN,
};

/// Current serialization format version.
pub const SESSION_STATE_VERSION: u8 = 1;
/// Minimum serialized state size, before any incoming handshake messages.
pub const MIN_SESSION_STATE_LEN: usize = 198;
/// Maximum serialized state size, including the two incoming XX handshake messages.
pub const MAX_SESSION_STATE_LEN: usize =
    MIN_SESSION_STATE_LEN + 2 * (2 + PUBKY_NOISE_CIPHERTEXT_LEN);
/// Exhausted nonce cursor sentinel.
const EXHAUSTED_NOISE_NONCE: u64 = u64::MAX - 1;

/// Serializable snapshot of a `PubkyNoiseEncryptor` session.
///
/// This struct contains everything needed to reconstruct the Noise session
/// by replaying persisted handshake messages through a fresh `HandshakeState`
/// built with the same ephemeral key material.
///
/// This state contains secret key material. Serialized snapshots must be
/// encrypted, authenticated, access-controlled, and deleted when superseded.
/// Retaining a restorable snapshot extends the lifetime of the session's
/// ephemeral material and therefore its exposure window.
#[derive(Clone)]
pub struct PubkyNoiseSessionState {
    /// Format version for forward compatibility.
    pub version: u8,
    /// Current phase: Handshake or Transport.
    pub phase: NoisePhase,
    /// The Noise handshake pattern (NN, XX, etc.).
    pub pattern: HandshakePattern,
    /// Whether this side is the initiator.
    pub initiator: bool,
    /// The local ephemeral secret key seed (32 bytes).
    /// This is the critical piece that allows replay to re-derive
    /// the same transport keys.
    pub ephemeral_secret: [u8; 32],
    /// The local static secret key (32 bytes), if the pattern requires one.
    pub static_secret: Option<[u8; 32]>,
    /// Handshake message slot counter, or transport base slot after handshake.
    pub counter: u32,
    /// Which handshake step we're at.
    pub noise_step: NoiseStep,
    /// Progress within the current step's action list.
    pub sub_step_index: u8,
    /// The handshake transcript hash at the saved handshake position.
    /// Transport snapshots retain the completed hash in `link_id` instead.
    pub handshake_hash: Option<[u8; 32]>,
    /// The link ID (available after transition_transport).
    pub link_id: Option<[u8; 32]>,
    /// Transport sending nonce.
    pub sending_nonce: u64,
    /// Transport receiving nonce.
    pub receiving_nonce: u64,
    /// Next outbound homeserver slot in transport mode.
    pub write_counter: u32,
    /// Next remote outbound homeserver slot to read in transport mode.
    pub read_counter: u32,
    /// The remote peer's public key (endpoint).
    pub endpoint_pubkey: [u8; 32],
    /// Incoming handshake messages, in read order, without packet framing or padding.
    /// Retains every completed read so restoration needs no downloads.
    pub handshake_messages: Vec<Vec<u8>>,
}

/// Redacted `Debug`: the ephemeral and static secrets are never rendered.
/// The sending/receiving nonce values are redacted as a logging policy as
/// well: they are not secret (Noise nonce counters are public values), but
/// hiding live cipher-state cursors keeps debug output free of session
/// internals and consistent with `DataLinkContext`'s redaction.
impl std::fmt::Debug for PubkyNoiseSessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubkyNoiseSessionState")
            .field("version", &self.version)
            .field("phase", &self.phase)
            .field("pattern", &self.pattern)
            .field("initiator", &self.initiator)
            .field("ephemeral_secret", &"[redacted]")
            .field(
                "static_secret",
                &self.static_secret.as_ref().map(|_| "[redacted]"),
            )
            .field("counter", &self.counter)
            .field("noise_step", &self.noise_step)
            .field("sub_step_index", &self.sub_step_index)
            .field("handshake_hash", &self.handshake_hash)
            .field("link_id", &self.link_id)
            .field("sending_nonce", &"[redacted]")
            .field("receiving_nonce", &"[redacted]")
            .field("write_counter", &self.write_counter)
            .field("read_counter", &self.read_counter)
            .field("endpoint_pubkey", &self.endpoint_pubkey)
            .field("handshake_messages", &"[redacted]")
            .finish()
    }
}

impl PubkyNoiseSessionState {
    /// Return the next handshake read slot, without restoring or advancing the session.
    ///
    /// `Some(slot)` identifies the remote resource at
    /// `{endpoint_pubkey}/{read_path}/{slot}`, using the peer's encoded public key
    /// and the same read path as the session configuration. `None` means the next
    /// action is a write, control action, step completion, or transport operation:
    /// callers must continue normal advancement, not treat the session as idle.
    ///
    /// This is an advisory scheduling probe with no network I/O. It checks snapshot
    /// version and cursor consistency, not authenticity or transcript validity.
    /// Message existence does not authorize advancement. Restore, authentication,
    /// recovery, and advancement must still run under the caller's existing leases.
    ///
    /// # Errors
    /// Returns [`SerializerError`] for an unsupported version or handshake pattern,
    /// an out-of-range sub-step, or inconsistent counters.
    pub fn next_handshake_read_slot(&self) -> Result<Option<u32>, SerializerError> {
        if self.version != SESSION_STATE_VERSION {
            return Err(SerializerError::UnsupportedVersion(self.version));
        }
        validate_counters(
            self.phase,
            self.counter,
            self.write_counter,
            self.read_counter,
            self.sending_nonce,
            self.receiving_nonce,
        )?;
        if !matches!(
            self.pattern,
            HandshakePattern::PatternNN | HandshakePattern::PatternXX
        ) {
            return Err(SerializerError::InvalidField(
                "pattern",
                self.pattern.to_u8(),
            ));
        }
        if self.phase == NoisePhase::Transport {
            let actions = full_handshake_actions(self.pattern, self.initiator)
                .map_err(|_| SerializerError::InvalidField("pattern", self.pattern.to_u8()))?;
            if self.counter as usize != actions.len() {
                return Err(SerializerError::InvalidCounter);
            }
            return Ok(None);
        }

        let actions = resolve_pattern(self.pattern, self.noise_step, self.initiator)
            .map_err(|_| SerializerError::InvalidField("pattern", self.pattern.to_u8()))?;
        let sub_step = usize::from(self.sub_step_index);
        if sub_step > actions.len() {
            return Err(SerializerError::InvalidField(
                "sub_step_index",
                self.sub_step_index,
            ));
        }
        let next_is_read = actions.get(sub_step) == Some(&HandshakeAction::Read);
        let mut completed_messages = actions
            .into_iter()
            .take(sub_step)
            .filter(|action| matches!(action, HandshakeAction::Read | HandshakeAction::Write))
            .count();
        for step in [NoiseStep::StepOne, NoiseStep::StepTwo, NoiseStep::Final]
            .into_iter()
            .take_while(|step| *step != self.noise_step)
        {
            completed_messages += resolve_pattern(self.pattern, step, self.initiator)
                .map_err(|_| SerializerError::InvalidField("pattern", self.pattern.to_u8()))?
                .into_iter()
                .filter(|action| matches!(action, HandshakeAction::Read | HandshakeAction::Write))
                .count();
        }
        if self.counter as usize != completed_messages {
            return Err(SerializerError::InvalidCounter);
        }

        Ok(next_is_read.then_some(self.counter))
    }

    pub(crate) fn validate(&self) -> Result<(), SerializerError> {
        self.next_handshake_read_slot()?;
        let expected = full_handshake_actions(self.pattern, self.initiator)
            .map_err(|_| SerializerError::InvalidField("pattern", self.pattern.to_u8()))?
            .into_iter()
            .take(self.counter as usize)
            .filter(|action| *action == HandshakeAction::Read)
            .count();
        if self.handshake_messages.len() != expected
            || self
                .handshake_messages
                .iter()
                .any(|message| message.is_empty() || message.len() > PUBKY_NOISE_CIPHERTEXT_LEN)
        {
            return Err(SerializerError::InvalidTranscript);
        }
        Ok(())
    }

    /// Serialize to a compact binary format.
    ///
    /// Layout:
    /// ```text
    /// [0]       version (u8)
    /// [1]       phase (u8: 0=Handshake, 1=Transport)
    /// [2]       pattern (u8)
    /// [3]       initiator (u8: 0 or 1)
    /// [4..36]   ephemeral_secret (32 bytes)
    /// [36]      has_static_secret (u8: 0 or 1)
    /// [37..69]  static_secret (32 bytes, zeros if absent)
    /// [69..73]  counter (u32 big-endian)
    /// [73]      noise_step (u8)
    /// [74]      sub_step_index (u8)
    /// [75]      has_handshake_hash (u8: 0 or 1)
    /// [76..108] handshake_hash (32 bytes, zeros if absent)
    /// [108]     has_link_id (u8: 0 or 1)
    /// [109..141] link_id (32 bytes, zeros if absent)
    /// [141..149] sending_nonce (u64 big-endian)
    /// [149..157] receiving_nonce (u64 big-endian)
    /// [157..161] write_counter (u32 big-endian)
    /// [161..165] read_counter (u32 big-endian)
    /// [165..197] endpoint_pubkey (32 bytes)
    /// [197]     incoming handshake message count (u8)
    /// [198..]   repeated: message length (u16 big-endian), message bytes
    /// ```
    /// The size is bounded by [`MIN_SESSION_STATE_LEN`] and [`MAX_SESSION_STATE_LEN`].
    pub fn serialize(&self) -> Vec<u8> {
        let transcript_len = self
            .handshake_messages
            .iter()
            .map(|message| 2 + message.len())
            .sum::<usize>();
        let mut buf = Vec::with_capacity(MIN_SESSION_STATE_LEN + transcript_len);

        // [0] version
        buf.push(self.version);

        // [1] phase
        buf.push(match self.phase {
            NoisePhase::HandShake => 0,
            NoisePhase::Transport => 1,
        });

        // [2] pattern
        buf.push(self.pattern.to_u8());

        // [3] initiator
        buf.push(if self.initiator { 1 } else { 0 });

        // [4..36] ephemeral_secret
        buf.extend_from_slice(&self.ephemeral_secret);

        // [36] has_static_secret
        if let Some(ref key) = self.static_secret {
            buf.push(1);
            buf.extend_from_slice(key);
        } else {
            buf.push(0);
            buf.extend_from_slice(&[0u8; 32]);
        }

        // [69..73] counter
        buf.extend_from_slice(&self.counter.to_be_bytes());

        // [73] noise_step
        buf.push(self.noise_step.to_u8());

        // [74] sub_step_index
        buf.push(self.sub_step_index);

        // [75] has_handshake_hash
        if let Some(ref hash) = self.handshake_hash {
            buf.push(1);
            buf.extend_from_slice(hash);
        } else {
            buf.push(0);
            buf.extend_from_slice(&[0u8; 32]);
        }

        // [108] has_link_id
        if let Some(ref id) = self.link_id {
            buf.push(1);
            buf.extend_from_slice(id);
        } else {
            buf.push(0);
            buf.extend_from_slice(&[0u8; 32]);
        }

        // [141..149] sending_nonce
        buf.extend_from_slice(&self.sending_nonce.to_be_bytes());

        // [149..157] receiving_nonce
        buf.extend_from_slice(&self.receiving_nonce.to_be_bytes());

        // [157..161] write_counter
        buf.extend_from_slice(&self.write_counter.to_be_bytes());

        // [161..165] read_counter
        buf.extend_from_slice(&self.read_counter.to_be_bytes());

        // [165..197] endpoint_pubkey
        buf.extend_from_slice(&self.endpoint_pubkey);

        buf.push(self.handshake_messages.len() as u8);
        debug_assert_eq!(buf.len(), MIN_SESSION_STATE_LEN);
        for message in &self.handshake_messages {
            buf.extend_from_slice(&(message.len() as u16).to_be_bytes());
            buf.extend_from_slice(message);
        }
        buf
    }

    /// Deserialize from the compact binary format.
    ///
    /// Rejects unsupported versions and truncated, oversized, or trailing data.
    pub fn deserialize(data: &[u8]) -> Result<Self, SerializerError> {
        if data.len() < MIN_SESSION_STATE_LEN {
            return Err(SerializerError::TooShort);
        }

        let version = data[0];
        if version != SESSION_STATE_VERSION {
            return Err(SerializerError::UnsupportedVersion(version));
        }

        if data.len() > MAX_SESSION_STATE_LEN {
            return Err(SerializerError::InvalidTranscript);
        }

        let phase = match data[1] {
            0 => NoisePhase::HandShake,
            1 => NoisePhase::Transport,
            v => return Err(SerializerError::InvalidField("phase", v)),
        };

        let pattern = HandshakePattern::from_u8(data[2])
            .ok_or(SerializerError::InvalidField("pattern", data[2]))?;

        let initiator = match data[3] {
            0 => false,
            1 => true,
            v => return Err(SerializerError::InvalidField("initiator", v)),
        };

        let mut ephemeral_secret = [0u8; 32];
        ephemeral_secret.copy_from_slice(&data[4..36]);

        let has_static = data[36] == 1;
        let static_secret = if has_static {
            let mut key = [0u8; 32];
            key.copy_from_slice(&data[37..69]);
            Some(key)
        } else {
            None
        };

        let counter = u32::from_be_bytes([data[69], data[70], data[71], data[72]]);

        let noise_step = NoiseStep::from_u8(data[73])
            .ok_or(SerializerError::InvalidField("noise_step", data[73]))?;

        let sub_step_index = data[74];

        let has_hash = data[75] == 1;
        let handshake_hash = if has_hash {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data[76..108]);
            Some(hash)
        } else {
            None
        };

        let has_link_id = data[108] == 1;
        let link_id = if has_link_id {
            let mut id = [0u8; 32];
            id.copy_from_slice(&data[109..141]);
            Some(id)
        } else {
            None
        };

        let sending_nonce = u64::from_be_bytes([
            data[141], data[142], data[143], data[144], data[145], data[146], data[147], data[148],
        ]);

        let receiving_nonce = u64::from_be_bytes([
            data[149], data[150], data[151], data[152], data[153], data[154], data[155], data[156],
        ]);

        let write_counter = u32::from_be_bytes([data[157], data[158], data[159], data[160]]);

        let read_counter = u32::from_be_bytes([data[161], data[162], data[163], data[164]]);

        let mut endpoint_pubkey = [0u8; 32];
        endpoint_pubkey.copy_from_slice(&data[165..197]);

        let mut handshake_messages = Vec::new();
        let count = data[MIN_SESSION_STATE_LEN - 1];
        if count > 2 {
            return Err(SerializerError::InvalidTranscript);
        }
        let mut remaining = &data[MIN_SESSION_STATE_LEN..];
        for _ in 0..count {
            let length = remaining.get(..2).ok_or(SerializerError::TooShort)?;
            let length = u16::from_be_bytes([length[0], length[1]]) as usize;
            if length == 0 || length > PUBKY_NOISE_CIPHERTEXT_LEN {
                return Err(SerializerError::InvalidTranscript);
            }
            let message = remaining
                .get(2..2 + length)
                .ok_or(SerializerError::TooShort)?;
            handshake_messages.push(message.to_vec());
            remaining = &remaining[2 + length..];
        }
        if !remaining.is_empty() {
            return Err(SerializerError::TrailingBytes);
        }

        let state = PubkyNoiseSessionState {
            version,
            phase,
            pattern,
            initiator,
            ephemeral_secret,
            static_secret,
            counter,
            noise_step,
            sub_step_index,
            handshake_hash,
            link_id,
            sending_nonce,
            receiving_nonce,
            write_counter,
            read_counter,
            endpoint_pubkey,
            handshake_messages,
        };
        state.validate()?;
        Ok(state)
    }
}

/// Errors that can occur during session state serialization/deserialization.
#[derive(Debug, PartialEq)]
pub enum SerializerError {
    /// The input data is too short.
    TooShort,
    /// The input data has trailing bytes after the session state.
    TrailingBytes,
    /// Unsupported format version.
    UnsupportedVersion(u8),
    /// An invalid value was found for a field.
    InvalidField(&'static str, u8),
    /// Serialized counter cannot advance in the slot space.
    CounterOverflow,
    /// Serialized nonce cannot be represented in the Noise nonce space.
    NonceOverflow,
    /// Serialized counters are internally inconsistent.
    InvalidCounter,
    /// Saved handshake messages do not match the completed reads or size bounds.
    InvalidTranscript,
}

fn validate_counters(
    phase: NoisePhase,
    counter: u32,
    write_counter: u32,
    read_counter: u32,
    sending_nonce: u64,
    receiving_nonce: u64,
) -> Result<(), SerializerError> {
    if counter == u32::MAX {
        return Err(SerializerError::CounterOverflow);
    }

    if phase == NoisePhase::HandShake {
        if sending_nonce == 0 && receiving_nonce == 0 && write_counter == 0 && read_counter == 0 {
            return Ok(());
        }
        return Err(SerializerError::InvalidCounter);
    }

    if sending_nonce > EXHAUSTED_NOISE_NONCE || receiving_nonce > EXHAUSTED_NOISE_NONCE {
        return Err(SerializerError::NonceOverflow);
    }

    if write_counter == u32::MAX || read_counter == u32::MAX {
        return Err(SerializerError::CounterOverflow);
    }

    if write_counter < counter || read_counter < counter {
        return Err(SerializerError::InvalidCounter);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_state() -> PubkyNoiseSessionState {
        PubkyNoiseSessionState {
            version: SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternNN,
            initiator: true,
            ephemeral_secret: [1; 32],
            static_secret: None,
            counter: 2,
            noise_step: NoiseStep::Final,
            sub_step_index: 0,
            handshake_hash: Some([2; 32]),
            link_id: Some([3; 32]),
            sending_nonce: 2,
            receiving_nonce: 1,
            write_counter: 9,
            read_counter: 7,
            endpoint_pubkey: [4; 32],
            handshake_messages: vec![vec![5; 48]],
        }
    }

    fn handshake_state() -> PubkyNoiseSessionState {
        PubkyNoiseSessionState {
            phase: NoisePhase::HandShake,
            initiator: false,
            counter: 0,
            noise_step: NoiseStep::StepOne,
            sending_nonce: 0,
            receiving_nonce: 0,
            write_counter: 0,
            read_counter: 0,
            handshake_hash: None,
            link_id: None,
            handshake_messages: Vec::new(),
            ..transport_state()
        }
    }

    #[test]
    fn handshake_read_slot_does_not_skip_control_or_completed_steps() {
        for pattern in [HandshakePattern::PatternNN, HandshakePattern::PatternXX] {
            for (initiator, step, sub_step, counter) in [
                (true, NoiseStep::StepOne, 1, 1),
                (true, NoiseStep::StepOne, 2, 1),
                (false, NoiseStep::StepOne, 1, 1),
                (false, NoiseStep::StepOne, 2, 2),
            ] {
                let mut state = handshake_state();
                state.pattern = pattern;
                state.initiator = initiator;
                state.noise_step = step;
                state.sub_step_index = sub_step;
                state.counter = counter;
                assert_eq!(state.next_handshake_read_slot(), Ok(None));
            }
        }
    }

    #[test]
    fn handshake_read_slot_rejects_malformed_cursors_and_unsupported_patterns() {
        for (mutate, expected) in [
            (
                (|s: &mut PubkyNoiseSessionState| s.version = 0) as fn(&mut PubkyNoiseSessionState),
                SerializerError::UnsupportedVersion(0),
            ),
            (
                |s| s.sub_step_index = u8::MAX,
                SerializerError::InvalidField("sub_step_index", u8::MAX),
            ),
            (|s| s.counter = 1, SerializerError::InvalidCounter),
            (|s| s.counter = u32::MAX, SerializerError::CounterOverflow),
            (
                |s| s.counter = u32::MAX - 1,
                SerializerError::InvalidCounter,
            ),
            (
                |s| s.noise_step = NoiseStep::StepTwo,
                SerializerError::InvalidCounter,
            ),
            (|s| s.sending_nonce = 1, SerializerError::InvalidCounter),
            (|s| s.receiving_nonce = 1, SerializerError::InvalidCounter),
            (|s| s.write_counter = 1, SerializerError::InvalidCounter),
            (|s| s.read_counter = 1, SerializerError::InvalidCounter),
        ] {
            let mut state = handshake_state();
            mutate(&mut state);
            assert_eq!(state.next_handshake_read_slot(), Err(expected));
        }
        for pattern in [
            HandshakePattern::PatternN,
            HandshakePattern::PatternIK,
            HandshakePattern::PatternNK,
            #[cfg(feature = "test-utils")]
            HandshakePattern::TestOnlyPatternAA,
        ] {
            for mut state in [handshake_state(), transport_state()] {
                state.pattern = pattern;
                assert_eq!(
                    state.next_handshake_read_slot(),
                    Err(SerializerError::InvalidField("pattern", pattern.to_u8()))
                );
            }
        }
    }

    #[test]
    fn handshake_read_slot_validates_transport_base() {
        for (pattern, completed_messages) in [
            (HandshakePattern::PatternNN, 2),
            (HandshakePattern::PatternXX, 3),
        ] {
            for initiator in [true, false] {
                for counter in [
                    0,
                    completed_messages - 1,
                    completed_messages,
                    completed_messages + 1,
                ] {
                    let state = PubkyNoiseSessionState {
                        pattern,
                        initiator,
                        counter,
                        ..transport_state()
                    };
                    let expected = if counter == completed_messages {
                        Ok(None)
                    } else {
                        Err(SerializerError::InvalidCounter)
                    };
                    assert_eq!(state.next_handshake_read_slot(), expected);
                }
            }
        }
    }

    #[test]
    fn roundtrip_preserves_transport_counters_and_nonces() {
        let state = transport_state();
        let bytes = state.serialize();

        assert_eq!(bytes.len(), MIN_SESSION_STATE_LEN + 2 + 48);

        let restored = PubkyNoiseSessionState::deserialize(&bytes).unwrap();
        assert_eq!(restored.version, SESSION_STATE_VERSION);
        assert_eq!(restored.counter, state.counter);
        assert_eq!(restored.sending_nonce, state.sending_nonce);
        assert_eq!(restored.receiving_nonce, state.receiving_nonce);
        assert_eq!(restored.write_counter, state.write_counter);
        assert_eq!(restored.read_counter, state.read_counter);
        assert_eq!(restored.handshake_messages, state.handshake_messages);
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = transport_state().serialize();
        bytes.push(0);

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::TrailingBytes)
        ));
    }

    #[test]
    fn transcript_serialization_rejects_invalid_lengths_and_counts() {
        let state = transport_state();
        let bytes = state.serialize();
        for end in 0..bytes.len() {
            assert!(PubkyNoiseSessionState::deserialize(&bytes[..end]).is_err());
        }
        for count in [0, 2, u8::MAX] {
            let mut malformed = bytes.clone();
            malformed[MIN_SESSION_STATE_LEN - 1] = count;
            assert!(PubkyNoiseSessionState::deserialize(&malformed).is_err());
        }
        for length in [0u16, PUBKY_NOISE_CIPHERTEXT_LEN as u16 + 1, u16::MAX] {
            let mut malformed = bytes.clone();
            malformed[198..200].copy_from_slice(&length.to_be_bytes());
            assert_eq!(
                PubkyNoiseSessionState::deserialize(&malformed).unwrap_err(),
                SerializerError::InvalidTranscript
            );
        }
        let mut oversized = bytes;
        oversized.resize(MAX_SESSION_STATE_LEN + 1, 0);
        assert_eq!(
            PubkyNoiseSessionState::deserialize(&oversized).unwrap_err(),
            SerializerError::InvalidTranscript
        );
    }

    #[test]
    fn serialization_rejects_unsupported_versions() {
        for version in [0, 2, u8::MAX] {
            let mut state = transport_state();
            state.version = version;
            assert_eq!(
                PubkyNoiseSessionState::deserialize(&state.serialize()).unwrap_err(),
                SerializerError::UnsupportedVersion(version)
            );
        }
    }

    #[test]
    fn deserialization_rejects_unknown_and_unsupported_patterns() {
        for state in [handshake_state(), transport_state()] {
            for pattern in [0, 3, 4, 5, u8::MAX] {
                let mut bytes = state.serialize();
                bytes[2] = pattern;
                assert_eq!(
                    PubkyNoiseSessionState::deserialize(&bytes).unwrap_err(),
                    SerializerError::InvalidField("pattern", pattern)
                );
            }
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut state = transport_state();
        state.ephemeral_secret = [0xAA; 32];
        state.static_secret = Some([0xBB; 32]);

        let rendered = format!("{state:?}");

        assert!(
            !rendered.contains(format!("{:?}", [0xAA; 32]).as_str()),
            "ephemeral secret leaked in Debug: {rendered}"
        );
        assert!(
            !rendered.contains(format!("{:?}", [0xBB; 32]).as_str()),
            "static secret leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("redacted"));
        // Transport nonce values are hidden as a logging policy (they are
        // not secret, but live cipher-state cursors stay out of debug
        // output); the field names remain visible.
        assert!(rendered.contains("sending_nonce"));
        assert!(
            !rendered.contains("sending_nonce: 2"),
            "sending nonce value leaked in Debug: {rendered}"
        );
        assert!(
            !rendered.contains("receiving_nonce: 1"),
            "receiving nonce value leaked in Debug: {rendered}"
        );
    }

    #[test]
    fn transport_snapshot_rejects_exhausted_sending_nonce() {
        let mut bytes = transport_state().serialize();
        bytes[141..149].copy_from_slice(&u64::MAX.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::NonceOverflow)
        ));
    }

    #[test]
    fn transport_snapshot_accepts_exhausted_noise_nonce_sentinel() {
        let mut state = transport_state();
        state.sending_nonce = EXHAUSTED_NOISE_NONCE;
        state.receiving_nonce = EXHAUSTED_NOISE_NONCE;
        let bytes = state.serialize();

        let restored = PubkyNoiseSessionState::deserialize(&bytes).unwrap();
        assert_eq!(restored.sending_nonce, EXHAUSTED_NOISE_NONCE);
        assert_eq!(restored.receiving_nonce, EXHAUSTED_NOISE_NONCE);
    }

    #[test]
    fn transport_snapshot_rejects_exhausted_receiving_nonce() {
        let mut bytes = transport_state().serialize();
        bytes[149..157].copy_from_slice(&u64::MAX.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::NonceOverflow)
        ));
    }

    #[test]
    fn transport_snapshot_rejects_write_counter_before_base() {
        let mut bytes = transport_state().serialize();
        bytes[157..161].copy_from_slice(&1u32.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::InvalidCounter)
        ));
    }

    #[test]
    fn transport_snapshot_rejects_read_counter_before_base() {
        let mut bytes = transport_state().serialize();
        bytes[161..165].copy_from_slice(&1u32.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::InvalidCounter)
        ));
    }

    #[test]
    fn transport_snapshot_rejects_exhausted_write_counter() {
        let mut bytes = transport_state().serialize();
        bytes[157..161].copy_from_slice(&u32::MAX.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::CounterOverflow)
        ));
    }

    #[test]
    fn transport_snapshot_rejects_exhausted_read_counter() {
        let mut bytes = transport_state().serialize();
        bytes[161..165].copy_from_slice(&u32::MAX.to_be_bytes());

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::CounterOverflow)
        ));
    }

    #[test]
    fn handshake_snapshot_rejects_transport_nonces() {
        let mut state = transport_state();
        state.phase = NoisePhase::HandShake;
        let bytes = state.serialize();

        assert!(matches!(
            PubkyNoiseSessionState::deserialize(&bytes),
            Err(SerializerError::InvalidCounter)
        ));
    }
}
