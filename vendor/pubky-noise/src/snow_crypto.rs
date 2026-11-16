use ed25519_dalek::SecretKey;
use pubky::PublicKey;

use snow::Builder;
use snow::{HandshakeState, StatelessTransportState};

use crate::snow_crypto_resolver::ReplayResolver;
use crate::PubkyNoiseError;

pub const PUBKY_NOISE_MSG_LEN: usize = 1000;
/// Length of the authenticated transport plaintext: body length, body, and padding.
pub(crate) const PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN: usize = PUBKY_NOISE_MSG_LEN + 2;
/// ChaChaPoly AEAD authentication tag size (Poly1305).
pub const PUBKY_NOISE_TAG_LEN: usize = 16;
/// Maximum ciphertext size for a [`PUBKY_NOISE_MSG_LEN`]-byte Noise message.
pub const PUBKY_NOISE_CIPHERTEXT_LEN: usize = PUBKY_NOISE_MSG_LEN + PUBKY_NOISE_TAG_LEN;
/// Fixed stored transport packet size: authenticated plaintext plus AEAD tag.
pub const PUBKY_NOISE_TRANSPORT_PACKET_LEN: usize =
    PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN + PUBKY_NOISE_TAG_LEN;
/// Noise reserves `u64::MAX`. The preceding value is the exhausted cursor,
/// making `u64::MAX - 2` the final usable nonce.
const EXHAUSTED_NOISE_NONCE: u64 = u64::MAX - 1;

#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum NoisePhase {
    HandShake,
    Transport,
}

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum NoiseStep {
    StepOne,
    StepTwo,
    Final,
}

impl NoiseStep {
    pub fn next_step(&self) -> NoiseStep {
        match self {
            NoiseStep::StepOne => NoiseStep::StepTwo,
            NoiseStep::StepTwo => NoiseStep::Final,
            // if final echo final
            NoiseStep::Final => NoiseStep::Final,
        }
    }

    /// Convert to a u8 for serialization.
    pub fn to_u8(&self) -> u8 {
        match self {
            NoiseStep::StepOne => 0,
            NoiseStep::StepTwo => 1,
            NoiseStep::Final => 2,
        }
    }

    /// Convert from a u8 for deserialization.
    pub fn from_u8(val: u8) -> Option<NoiseStep> {
        match val {
            0 => Some(NoiseStep::StepOne),
            1 => Some(NoiseStep::StepTwo),
            2 => Some(NoiseStep::Final),
            _ => None,
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum HandshakeAction {
    Read,
    Write,
    Pending,
    Terminal,
}

/// The source properties are:
///
/// 0. **No authentication**. This payload may have been sent by any party, including an active attacker.
///
/// 1. **Sender authentication vulnerable to key-compromise impersonation (KCI)**. The sender authentication
///    is based on a static-static DH ("ss") involving both parties' static key pairs. If the recipient's
///    long-term private key has been compromised, this authentication can be forged. Note that a future
///    version of Noise might include signatures, which could improve this security property, but brings
///    other trade-offs.
///
/// 2. **Sender authentication resistant to key-compromise impersonation (KCI)**. The sender authentication
///    is based on an ephemeral-static DH ("es" or "se") between the sender's static key pair and the recipient's
///    ephemeral key pair. Assuming the corresponding private keys are secure, this authentication cannot be
///    forged.
///
/// The destination properties are:
///
/// 0. **No confidentiality**. This payload is sent in cleartext.
///
/// 1. **Encryption to an ephemeral recipient**. This payload has forward secrecy, since encryption involves
///    an ephemeral-ephemeral DH ("ee"). However, the sender has not authenticated the recipient, so this payload
///    might be sent to any party, including an active attacker.
///
/// 2. **Encryption to a known recipient, forward secrecy for sender compromise only, vulnerable to replay**.
///    This payload is encrypted based only on DHs involving the recipient's static key pair. If the recipient's
///    static private key is compromised, even at a later date, this payload can be decrypted. This message can
///    also be replayed, since there's no ephemeral contribution from the recipient.
///
/// 3. **Encryption to a known recipient, weak forward secrecy**. This payload is encrypted based on an
///    ephemeral-ephemeral DH and also an ephemeral-static DH involving the recipient's static key pair. However,
///    the binding between the recipient's alleged ephemeral public key and the recipient's static public key
///    hasn't been verified by the sender, so the recipient's alleged ephemeral public key may have been forged
///    by an active attacker. In this case, the attacker could later compromise the recipient's static private
///    key to decrypt the payload. Note that a future version of Noise might include signatures, which could
///    improve this security property, but brings other trade-offs.
///
/// 4. **Encryption to a known recipient, weak forward secrecy if the sender's private key has been compromised**.
///    This payload is encrypted based on an ephemeral-ephemeral DH, and also based on an ephemeral-static DH
///    involving the recipient's static key pair. However, the binding between the recipient's alleged ephemeral
///    public and the recipient's static public key has only been verified based on DHs involving both those public
///    keys and the sender's static private key. Thus, if the sender's static private key was previously compromised,
///    the recipient's alleged ephemeral public key may have been forged by an active attacker. In this case, the
///    attacker could later compromise the intended recipient's static private key to decrypt the payload (this is
///    a variant of a "KCI" attack enabling a "weak forward secrecy" attack). Note that a future version of Noise
///    might include signatures, which could improve this security property, but brings other trade-offs.
///
/// 5. **Encryption to a known recipient, strong forward secrecy**. This payload is encrypted based on an
///    ephemeral-ephemeral DH as well as an ephemeral-static DH with the recipient's static key pair. Assuming
///    the ephemeral private keys are secure, and the recipient is not being actively impersonated by an attacker
///    that has stolen its static private key, this payload cannot be decrypted.
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum HandshakePattern {
    /// <- s                      0                0
    /// ...
    /// -> e, es                  0                2
    PatternN,
    /// -> e                      0                0
    /// <- e, ee                  0                1
    /// ->                        0                1
    PatternNN,
    /// -> e                      0                0
    /// <- e, ee, s, es           2                1
    /// -> s, se                  2                5
    /// <-                        2                5
    PatternXX,
    /// <- s
    /// ...
    /// -> e, es, s, ss           1                2
    /// <- e, ee, se              2                4
    /// ->                        2                5
    /// <-                        2                5
    PatternIK,
    /// <- s
    /// ...
    /// -> e, es                  0                2
    /// <- e, ee                  2                1
    /// ->                        0                5
    PatternNK,

    // TODO ???:
    //   -> s
    //   <- s
    //   ...
    //   -> e, es, ss              1                2
    //   <- e, ee, se              2                4
    //   ->                        2                5
    //   <-                        2                5
    // KK
    //
    //   -> s
    //   ...
    //   -> e                      0                0
    //   <- e, ee, se, s, es       2                3
    //   ->                        2                5
    //   <-                        2                5
    // KX
    //
    //   -> e, s                   0                0
    //   <- e, ee, se              0                3
    //   ->                        2                1
    //   <-                        0                5
    // IN
    //
    //   <- s
    //   ...
    //   -> e, es, s, ss           1                2
    //   <- e, ee, se              2                4
    //   ->                        2                5
    //   <-                        2                5
    // IK
    //
    //   -> e, s                   0                0
    //   <- e, ee, se, s, es       2                3
    //   ->                        2                5
    //   <-                        2                5
    // IX
    //
    /// Test-only pattern to inject build fault.
    #[cfg(feature = "test-utils")]
    TestOnlyPatternAA,
}

pub enum PatternError {
    UnknownLiteralPattern,
}

impl std::str::FromStr for HandshakePattern {
    type Err = PatternError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "N" => Ok(HandshakePattern::PatternN),
            "NN" => Ok(HandshakePattern::PatternNN),
            "XX" => Ok(HandshakePattern::PatternXX),
            "IK" => Ok(HandshakePattern::PatternIK),
            "NK" => Ok(HandshakePattern::PatternNK),
            _ => Err(PatternError::UnknownLiteralPattern),
        }
    }
}

impl HandshakePattern {
    pub fn as_str(&self) -> &'static str {
        match self {
            HandshakePattern::PatternN => "N",
            HandshakePattern::PatternNN => "NN",
            HandshakePattern::PatternXX => "XX",
            HandshakePattern::PatternIK => "IK",
            HandshakePattern::PatternNK => "NK",
            #[cfg(feature = "test-utils")]
            HandshakePattern::TestOnlyPatternAA => "AA",
        }
    }

    pub fn needs_local_key(&self) -> bool {
        match self {
            HandshakePattern::PatternN => false,
            HandshakePattern::PatternNN => false,
            HandshakePattern::PatternXX => true,
            HandshakePattern::PatternIK => true,
            HandshakePattern::PatternNK => true,
            #[cfg(feature = "test-utils")]
            HandshakePattern::TestOnlyPatternAA => false,
        }
    }

    /// Convert to a u8 for serialization.
    pub fn to_u8(&self) -> u8 {
        match self {
            HandshakePattern::PatternN => 0,
            HandshakePattern::PatternNN => 1,
            HandshakePattern::PatternXX => 2,
            HandshakePattern::PatternIK => 3,
            HandshakePattern::PatternNK => 4,
            #[cfg(feature = "test-utils")]
            HandshakePattern::TestOnlyPatternAA => 255,
        }
    }

    /// Convert from a u8 for deserialization.
    pub fn from_u8(val: u8) -> Option<HandshakePattern> {
        match val {
            0 => Some(HandshakePattern::PatternN),
            1 => Some(HandshakePattern::PatternNN),
            2 => Some(HandshakePattern::PatternXX),
            3 => Some(HandshakePattern::PatternIK),
            4 => Some(HandshakePattern::PatternNK),
            #[cfg(feature = "test-utils")]
            255 => Some(HandshakePattern::TestOnlyPatternAA),
            _ => None,
        }
    }
}

pub fn resolve_pattern_nn(noise_step: NoiseStep, initiator: bool) -> Vec<HandshakeAction> {
    if initiator {
        match noise_step {
            NoiseStep::StepOne => vec![HandshakeAction::Write, HandshakeAction::Pending],
            NoiseStep::StepTwo => vec![HandshakeAction::Read],
            NoiseStep::Final => vec![HandshakeAction::Terminal],
        }
    } else {
        match noise_step {
            NoiseStep::StepOne => vec![HandshakeAction::Read, HandshakeAction::Write],
            NoiseStep::StepTwo => vec![HandshakeAction::Terminal],
            NoiseStep::Final => vec![HandshakeAction::Terminal],
        }
    }
}

pub fn resolve_pattern_xx(noise_step: NoiseStep, initiator: bool) -> Vec<HandshakeAction> {
    if initiator {
        match noise_step {
            NoiseStep::StepOne => vec![HandshakeAction::Write, HandshakeAction::Pending],
            NoiseStep::StepTwo => vec![HandshakeAction::Read, HandshakeAction::Write],
            NoiseStep::Final => vec![HandshakeAction::Terminal],
        }
    } else {
        match noise_step {
            NoiseStep::StepOne => {
                vec![
                    HandshakeAction::Read,
                    HandshakeAction::Write,
                    HandshakeAction::Pending,
                ]
            }
            NoiseStep::StepTwo => vec![HandshakeAction::Read],
            NoiseStep::Final => vec![HandshakeAction::Terminal],
        }
    }
}

/// Resolve the handshake actions for a given pattern, step, and role.
///
/// Only NN and XX patterns are supported.
pub(crate) fn resolve_pattern(
    pattern: HandshakePattern,
    noise_step: NoiseStep,
    initiator: bool,
) -> Result<Vec<HandshakeAction>, PubkyNoiseError> {
    match pattern {
        HandshakePattern::PatternNN => Ok(resolve_pattern_nn(noise_step, initiator)),
        HandshakePattern::PatternXX => Ok(resolve_pattern_xx(noise_step, initiator)),
        HandshakePattern::PatternN | HandshakePattern::PatternIK | HandshakePattern::PatternNK => {
            Err(PubkyNoiseError::UnsupportedNoisePattern)
        }
        #[cfg(feature = "test-utils")]
        HandshakePattern::TestOnlyPatternAA => {
            panic!("TestOnlyPatternAA cannot be resolved to handshake actions")
        }
    }
}

/// Return the flat sequence of Write/Read actions for a complete handshake,
/// given the pattern and role (initiator/respnder). Used by the replay logic
/// to know which operations to perform when re-feeding persisted messages.
/// # Parameters:
/// - `pattern`: a handshake pattern
/// - `initiator`: boolean parameter which indicates who is initiator and who is responder
///
/// # Errors
/// Returns [`PubkyNoiseError::UnsupportedNoisePattern`] for unsupported patterns.
pub fn full_handshake_actions(
    pattern: HandshakePattern,
    initiator: bool,
) -> Result<Vec<HandshakeAction>, PubkyNoiseError> {
    let mut actions = Vec::new();
    let steps = [NoiseStep::StepOne, NoiseStep::StepTwo, NoiseStep::Final];
    for step in &steps {
        let step_actions = resolve_pattern(pattern, *step, initiator)?;
        for action in step_actions {
            match action {
                HandshakeAction::Write | HandshakeAction::Read => actions.push(action),
                HandshakeAction::Pending | HandshakeAction::Terminal => {
                    // Skip control actions; we only care about Write/Read for replay
                }
            }
        }
    }
    Ok(actions)
}

/// Internal Noise state-machine errors.
#[derive(Debug)]
pub(crate) enum ContextError {
    Init,
    OngoingHandshake,
    InvalidPhase,
    InternalSnowTransitionErr,
    InternalSnowWriteErr,
    InternalSnowReadErr,
    CounterOverflow,
    NonceOverflow,
}

fn ensure_counter_can_increment(counter: u32) -> Result<(), ContextError> {
    // Keep u32::MAX - 1 as the exhausted serialized cursor.
    if counter >= u32::MAX - 1 {
        Err(ContextError::CounterOverflow)
    } else {
        Ok(())
    }
}

fn ensure_nonce_can_increment(nonce: u64) -> Result<(), ContextError> {
    if nonce >= EXHAUSTED_NOISE_NONCE {
        Err(ContextError::NonceOverflow)
    } else {
        Ok(())
    }
}

/// A Noise state machine
pub(crate) struct DataLinkContext {
    initiator: bool,
    message_patterns: HandshakePattern,

    // ephemeral key always handled by the snow
    local_static_seckey: Option<SecretKey>,

    /// The local ephemeral secret key used for this session.
    /// Captured at construction time so it can be persisted for restore.
    local_ephemeral_seckey: [u8; 32],

    noise_step: NoiseStep,
    noise_phase: NoisePhase,

    noise_handshake: Option<HandshakeState>,
    noise_transport: Option<StatelessTransportState>,
    /// Successfully processed incoming handshake messages for local replay.
    handshake_messages: Vec<Vec<u8>>,

    /// Explicit nonce for outbound transport messages.
    sending_nonce: u64,
    /// Explicit nonce for decrypting remote outbound transport messages.
    receiving_nonce: u64,

    endpoint_pubkey: PublicKey,

    /// Handshake slot counter, or transport base slot after handshake.
    counter: u32,
    /// Homeserver slot for the next outbound transport message.
    write_counter: u32,
    /// Homeserver slot for the next remote outbound transport message.
    read_counter: u32,

    // Tracks progress within the current step's action list for polling-safe handshake.
    // When a Read fails (peer hasn't written yet), we return Pending without advancing
    // sub_step_index, so the next poll retries from the same action.
    sub_step_index: usize,
}

/// Redacted `Debug`: secret key material (`local_static_seckey`,
/// `local_ephemeral_seckey`) and the Snow handshake/transport internals are
/// never rendered. Transport nonce values are hidden as a logging policy
/// (consistent with `PubkyNoiseSessionState`), not because they are secret —
/// Noise nonce counters are public values.
impl std::fmt::Debug for DataLinkContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataLinkContext")
            .field("initiator", &self.initiator)
            .field("message_patterns", &self.message_patterns)
            .field("has_static_secret", &self.local_static_seckey.is_some())
            .field("noise_step", &self.noise_step)
            .field("noise_phase", &self.noise_phase)
            .field("sending_nonce", &"[redacted]")
            .field("receiving_nonce", &"[redacted]")
            .field("endpoint_pubkey", &self.endpoint_pubkey)
            .field("counter", &self.counter)
            .field("write_counter", &self.write_counter)
            .field("read_counter", &self.read_counter)
            .field("sub_step_index", &self.sub_step_index)
            .finish_non_exhaustive()
    }
}

impl DataLinkContext {
    /// Build the Snow protocol name string for the given pattern.
    /// We're using ChaCha as the stream cipher. Poly1305 as the MAC and SHA256 as a hash function.
    fn build_protocol_name(handshake_pattern: &HandshakePattern) -> String {
        format!(
            "Noise_{}_25519_ChaChaPoly_SHA256",
            handshake_pattern.as_str()
        )
    }

    /// Build a Snow HandshakeState using the ReplayResolver with the given ephemeral seed.
    fn build_handshake_state(
        protocol_name: &str,
        handshake_pattern: &HandshakePattern,
        initiator: bool,
        local_static_key: &Option<SecretKey>,
        ephemeral_seed: &[u8; 32],
    ) -> Result<HandshakeState, ContextError> {
        let params = protocol_name.parse().map_err(|_| ContextError::Init)?;

        let resolver = ReplayResolver::new(*ephemeral_seed);
        let builder = Builder::with_resolver(params, resolver);

        let noise_stack = if handshake_pattern.needs_local_key() {
            let key = local_static_key.as_ref().ok_or(ContextError::Init)?;
            if initiator {
                builder
                    .local_private_key(key)
                    .map_err(|_| ContextError::Init)?
                    .build_initiator()
            } else {
                builder
                    .local_private_key(key)
                    .map_err(|_| ContextError::Init)?
                    .build_responder()
            }
        } else if initiator {
            builder.build_initiator()
        } else {
            builder.build_responder()
        };

        noise_stack.map_err(|_| ContextError::Init)
    }

    pub fn new(
        handshake_pattern: HandshakePattern,
        initiator: bool,
        local_static_key: Option<SecretKey>,
        endpoint_pubkey: PublicKey,
    ) -> Result<DataLinkContext, ContextError> {
        Self::new_with_ephemeral(
            handshake_pattern,
            initiator,
            local_static_key,
            endpoint_pubkey,
            None,
        )
    }

    /// Create a new DataLinkContext, optionally with a pre-set ephemeral key.
    ///
    /// When `ephemeral_secret` is `None`, a fresh 32-byte random seed is generated.
    /// When `Some`, the provided seed is used (for session restore / replay).
    pub fn new_with_ephemeral(
        handshake_pattern: HandshakePattern,
        initiator: bool,
        local_static_key: Option<SecretKey>,
        endpoint_pubkey: PublicKey,
        ephemeral_secret: Option<[u8; 32]>,
    ) -> Result<DataLinkContext, ContextError> {
        // Section 5.3 The Handshake Object
        //
        // Perform the following steps:
        // - Derives a protocol_name byte sequence by combining the names for
        //   the handshake pattern and crypto functions, as specified in Section 8.
        //   Calls InitializeSymmetric(protocol_name).

        let protocol_name = Self::build_protocol_name(&handshake_pattern);

        // Generate or use provided ephemeral seed
        let ephemeral_seed = match ephemeral_secret {
            Some(seed) => seed,
            None => {
                let mut seed = [0u8; 32];
                getrandom::fill(&mut seed).map_err(|_| ContextError::Init)?;
                seed
            }
        };

        let handshake_state = Self::build_handshake_state(
            &protocol_name,
            &handshake_pattern,
            initiator,
            &local_static_key,
            &ephemeral_seed,
        )?;

        // - Sets the initator s, e, rs and re variables to the corresponding
        //   arguments.

        // - Calls MixHash() once for each public key listed in the pre-messages
        //   from handshake_pattern, with the specified public key as input (see
        //   Section 7 for an explanation of pre-messages). If both initiator and
        //   responder have pre-messages, the initiator's public key are hashed
        //   first. If multiple public keys are listed in either party's pre-message,
        //   the public keys are hashed in the order they are listed.

        Ok(DataLinkContext {
            initiator,
            message_patterns: handshake_pattern,

            //TODO: for now keep keys separated from the state.
            local_static_seckey: local_static_key,

            local_ephemeral_seckey: ephemeral_seed,

            noise_step: NoiseStep::StepOne,
            noise_phase: NoisePhase::HandShake,

            noise_handshake: Some(handshake_state),
            noise_transport: None,
            handshake_messages: Vec::new(),

            sending_nonce: 0,
            receiving_nonce: 0,

            endpoint_pubkey,

            counter: 0,
            write_counter: 0,
            read_counter: 0,

            sub_step_index: 0,
        })
    }

    pub fn get_endpoint(&self) -> &PublicKey {
        &self.endpoint_pubkey
    }

    pub fn get_counter(&self) -> u32 {
        self.counter
    }

    pub fn ensure_can_increment_counter(&self) -> Result<(), ContextError> {
        ensure_counter_can_increment(self.counter)
    }

    pub fn increment_counter(&mut self) -> Result<(), ContextError> {
        self.counter = self
            .counter
            .checked_add(1)
            .ok_or(ContextError::CounterOverflow)?;
        Ok(())
    }

    pub fn get_write_slot(&self) -> u32 {
        self.write_counter
    }

    pub fn get_read_slot(&self) -> u32 {
        self.read_counter
    }

    pub fn ensure_can_advance_write_slot(&self) -> Result<(), ContextError> {
        ensure_counter_can_increment(self.write_counter)
    }

    pub fn ensure_can_advance_read_slot(&self) -> Result<(), ContextError> {
        ensure_counter_can_increment(self.read_counter)
    }

    pub fn ensure_can_advance_sending_nonce(&self) -> Result<(), ContextError> {
        ensure_nonce_can_increment(self.sending_nonce)
    }

    pub fn ensure_can_advance_receiving_nonce(&self) -> Result<(), ContextError> {
        ensure_nonce_can_increment(self.receiving_nonce)
    }

    pub fn delete(&mut self) {
        if let Some(seckey) = &mut self.local_static_seckey {
            seckey.as_mut_slice().copy_from_slice(&[0; 32][..]);
        }
        self.local_static_seckey = None;
        self.local_ephemeral_seckey.copy_from_slice(&[0; 32][..]);
    }

    pub fn is_handshake(&self) -> bool {
        match &self.noise_handshake {
            Some(hs) => !hs.is_handshake_finished(),
            None => false,
        }
    }

    /// Check if this context is in transport phase.
    pub fn is_transport(&self) -> bool {
        self.noise_phase == NoisePhase::Transport
    }

    /// Return the peer's static key only after the handshake completes.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        match &self.noise_handshake {
            Some(handshake) if handshake.is_handshake_finished() => handshake.get_remote_static(),
            Some(_) => None,
            None => self.noise_transport.as_ref()?.get_remote_static(),
        }
    }

    pub fn get_handshake_hash(&self) -> Option<[u8; 32]> {
        if let Some(handshake_state) = &self.noise_handshake {
            let mut buf = [0; 32];
            let hash = handshake_state.get_handshake_hash();
            assert_eq!(hash.len(), 32);
            buf[0..32].copy_from_slice(&hash[0..32]);
            return Some(buf);
        }
        None
    }

    pub(crate) fn transition_to_transport(&mut self) -> Result<(), ContextError> {
        let hs = self
            .noise_handshake
            .as_ref()
            .ok_or(ContextError::OngoingHandshake)?;
        if !hs.is_handshake_finished() {
            return Err(ContextError::OngoingHandshake);
        }
        let transport = self
            .noise_handshake
            .take()
            .unwrap()
            .into_stateless_transport_mode()
            .map_err(|_| ContextError::InternalSnowTransitionErr)?;
        self.noise_transport = Some(transport);
        self.noise_phase = NoisePhase::Transport;
        self.sending_nonce = 0;
        self.receiving_nonce = 0;
        self.write_counter = self.counter;
        self.read_counter = self.counter;
        Ok(())
    }

    /// Returns the remaining actions for the current step, starting from sub_step_index.
    /// Uses the stored `initiator` flag -- callers no longer need to pass it.
    /// Does NOT advance noise_step -- call `complete_step()` for that.
    pub fn remaining_handshake_actions(&self) -> Result<Vec<HandshakeAction>, PubkyNoiseError> {
        let all_actions = resolve_pattern(self.message_patterns, self.noise_step, self.initiator)?;
        Ok(all_actions.into_iter().skip(self.sub_step_index).collect())
    }

    /// Mark one sub-step action as completed, advancing the sub-step index.
    pub fn advance_sub_step(&mut self) {
        self.sub_step_index += 1;
    }

    /// Complete the current step and move to the next one, resetting sub-step index.
    pub fn complete_step(&mut self) {
        self.noise_step = self.noise_step.next_step();
        self.sub_step_index = 0;
    }

    pub(crate) fn write_handshake_message(
        &mut self,
        payload: &[u8],
        message: &mut [u8; PUBKY_NOISE_CIPHERTEXT_LEN],
    ) -> Result<usize, ContextError> {
        if self.noise_phase != NoisePhase::HandShake {
            return Err(ContextError::InvalidPhase);
        }

        self.noise_handshake
            .as_mut()
            .ok_or(ContextError::InvalidPhase)?
            .write_message(payload, message.as_mut())
            .map_err(|_| ContextError::InternalSnowWriteErr)
    }

    /// Encrypt a transport message without advancing the sending nonce.
    pub(crate) fn prepare_transport_write(
        &self,
        payload: &[u8; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN],
        message: &mut [u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN],
    ) -> Result<usize, ContextError> {
        if !self.is_transport() {
            return Err(ContextError::OngoingHandshake);
        }

        self.noise_transport
            .as_ref()
            .ok_or(ContextError::OngoingHandshake)?
            .write_message(self.sending_nonce, payload, message.as_mut())
            .map_err(|_| ContextError::InternalSnowWriteErr)
    }

    /// Decrypt a transport message without advancing the receiving nonce.
    pub(crate) fn prepare_transport_read(
        &self,
        message: &[u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN],
        payload: &mut [u8; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN],
    ) -> Result<usize, ContextError> {
        if !self.is_transport() {
            return Err(ContextError::OngoingHandshake);
        }

        self.noise_transport
            .as_ref()
            .ok_or(ContextError::OngoingHandshake)?
            .read_message(self.receiving_nonce, message, payload)
            .map_err(|_| ContextError::InternalSnowReadErr)
    }

    pub(crate) fn read_handshake_message(
        &mut self,
        message: &mut [u8; PUBKY_NOISE_CIPHERTEXT_LEN],
        payload: &mut [u8; PUBKY_NOISE_MSG_LEN],
        index: usize,
    ) -> Result<(), ContextError> {
        if self.noise_phase != NoisePhase::HandShake {
            return Err(ContextError::InvalidPhase);
        }

        self.noise_handshake
            .as_mut()
            .ok_or(ContextError::InvalidPhase)?
            .read_message(&message[..index], payload)
            .map_err(|_| ContextError::InternalSnowReadErr)?;
        self.handshake_messages.push(message[..index].to_vec());
        Ok(())
    }

    // --- Snapshot / restore accessors ---

    pub(crate) fn handshake_messages(&self) -> &[Vec<u8>] {
        &self.handshake_messages
    }

    /// Get the current noise phase.
    pub fn get_phase(&self) -> NoisePhase {
        self.noise_phase
    }

    /// Get the handshake pattern.
    pub fn get_pattern(&self) -> HandshakePattern {
        self.message_patterns
    }

    /// Whether this side is the initiator.
    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    /// Get the local ephemeral secret key (for backup/restore).
    pub fn get_ephemeral_secret(&self) -> &[u8; 32] {
        &self.local_ephemeral_seckey
    }

    /// Get the local static secret key (for backup/restore).
    pub fn get_static_secret(&self) -> Option<&SecretKey> {
        self.local_static_seckey.as_ref()
    }

    /// Get the current noise step.
    pub fn get_noise_step(&self) -> NoiseStep {
        self.noise_step
    }

    /// Get the current sub-step index.
    pub fn get_sub_step_index(&self) -> usize {
        self.sub_step_index
    }

    /// Get the sending nonce (transport phase).
    pub fn get_sending_nonce(&self) -> u64 {
        self.sending_nonce
    }

    /// Get the receiving nonce (transport phase).
    pub fn get_receiving_nonce(&self) -> u64 {
        self.receiving_nonce
    }

    /// Get the next outbound homeserver slot (transport phase).
    pub fn get_write_counter(&self) -> u32 {
        self.write_counter
    }

    /// Get the next remote outbound homeserver slot to read (transport phase).
    pub fn get_read_counter(&self) -> u32 {
        self.read_counter
    }

    /// Set the counter (used during restore).
    pub fn set_counter(&mut self, counter: u32) {
        self.counter = counter;
    }

    /// Set the outbound homeserver slot counter (used during restore).
    pub fn set_write_counter(&mut self, counter: u32) {
        self.write_counter = counter;
    }

    /// Set the remote outbound homeserver slot counter (used during restore).
    pub fn set_read_counter(&mut self, counter: u32) {
        self.read_counter = counter;
    }

    /// Set the noise step (used during restore).
    pub fn set_noise_step(&mut self, step: NoiseStep) {
        self.noise_step = step;
    }

    /// Set the sub-step index (used during restore).
    pub fn set_sub_step_index(&mut self, index: usize) {
        self.sub_step_index = index;
    }

    /// Set the sending nonce (used during restore after transport transition).
    pub fn set_sending_nonce(&mut self, nonce: u64) {
        self.sending_nonce = nonce;
    }

    /// Set the receiving nonce (used during restore after transport transition).
    pub fn set_receiving_nonce(&mut self, nonce: u64) {
        self.receiving_nonce = nonce;
    }
}

#[cfg(test)]
mod tests {
    use pubky::prelude::Keypair;
    use sha2::{Digest, Sha256};

    use super::*;

    #[test]
    fn test_handshake_actions_reject_unsupported_patterns() {
        for pattern in [
            HandshakePattern::PatternN,
            HandshakePattern::PatternIK,
            HandshakePattern::PatternNK,
        ] {
            for initiator in [true, false] {
                assert!(matches!(
                    full_handshake_actions(pattern, initiator),
                    Err(PubkyNoiseError::UnsupportedNoisePattern)
                ));
                for step in [NoiseStep::StepOne, NoiseStep::StepTwo, NoiseStep::Final] {
                    assert!(matches!(
                        resolve_pattern(pattern, step, initiator),
                        Err(PubkyNoiseError::UnsupportedNoisePattern)
                    ));
                }
            }
        }
    }

    #[test]
    fn test_full_handshake_actions_preserve_message_order() {
        use HandshakeAction::{Read, Write};

        for (pattern, initiator, expected) in [
            (HandshakePattern::PatternNN, true, vec![Write, Read]),
            (HandshakePattern::PatternNN, false, vec![Read, Write]),
            (HandshakePattern::PatternXX, true, vec![Write, Read, Write]),
            (HandshakePattern::PatternXX, false, vec![Read, Write, Read]),
        ] {
            assert!(full_handshake_actions(pattern, initiator).unwrap() == expected);
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let endpoint = Keypair::random().public_key();
        let context = match DataLinkContext::new_with_ephemeral(
            HandshakePattern::PatternXX,
            true,
            Some([0xDD; 32]),
            endpoint,
            Some([0xCC; 32]),
        ) {
            Ok(context) => context,
            Err(_) => panic!("failed to build DataLinkContext"),
        };

        let rendered = format!("{context:?}");

        assert!(
            !rendered.contains(format!("{:?}", [0xCC; 32]).as_str()),
            "ephemeral secret leaked in Debug: {rendered}"
        );
        assert!(
            !rendered.contains(format!("{:?}", [0xDD; 32]).as_str()),
            "static secret leaked in Debug: {rendered}"
        );
        // Non-secret fields remain visible for debugging.
        assert!(rendered.contains("sending_nonce"));
        // Transport nonce values are hidden as a logging policy; the field
        // names remain visible.
        assert!(
            !rendered.contains("sending_nonce: 0"),
            "sending nonce value leaked in Debug: {rendered}"
        );
        assert!(
            !rendered.contains("receiving_nonce: 0"),
            "receiving nonce value leaked in Debug: {rendered}"
        );
    }

    fn transport_contexts() -> (DataLinkContext, DataLinkContext) {
        let initiator_keypair = Keypair::from_secret(&[3; 32]);
        let responder_keypair = Keypair::from_secret(&[4; 32]);
        let mut initiator = DataLinkContext::new_with_ephemeral(
            HandshakePattern::PatternNN,
            true,
            None,
            responder_keypair.public_key(),
            Some([1; 32]),
        )
        .unwrap();
        let mut responder = DataLinkContext::new_with_ephemeral(
            HandshakePattern::PatternNN,
            false,
            None,
            initiator_keypair.public_key(),
            Some([2; 32]),
        )
        .unwrap();

        let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
        let mut payload = [0; PUBKY_NOISE_MSG_LEN];
        let len = initiator
            .write_handshake_message(&[], &mut message)
            .unwrap();
        responder
            .read_handshake_message(&mut message, &mut payload, len)
            .unwrap();

        let len = responder
            .write_handshake_message(&[], &mut message)
            .unwrap();
        initiator
            .read_handshake_message(&mut message, &mut payload, len)
            .unwrap();

        initiator.transition_to_transport().unwrap();
        responder.transition_to_transport().unwrap();
        (initiator, responder)
    }

    #[test]
    fn test_remote_static_public_key() {
        let initiator_secret = [3; 32];
        let responder_secret = [4; 32];
        let mut initiator = DataLinkContext::new(
            HandshakePattern::PatternXX,
            true,
            Some(initiator_secret),
            Keypair::random().public_key(),
        )
        .unwrap();
        let mut responder = DataLinkContext::new(
            HandshakePattern::PatternXX,
            false,
            Some(responder_secret),
            Keypair::random().public_key(),
        )
        .unwrap();
        assert_eq!(initiator.remote_static_public_key(), None);
        assert_eq!(responder.remote_static_public_key(), None);

        let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
        let mut payload = [0; PUBKY_NOISE_MSG_LEN];
        let len = initiator
            .write_handshake_message(&[], &mut message)
            .unwrap();
        responder
            .read_handshake_message(&mut message, &mut payload, len)
            .unwrap();
        assert_eq!(responder.remote_static_public_key(), None);
        let len = responder
            .write_handshake_message(&[], &mut message)
            .unwrap();
        initiator
            .read_handshake_message(&mut message, &mut payload, len)
            .unwrap();
        let responder_key = crate::derive_static_public_key(&responder_secret);
        assert_eq!(initiator.remote_static_public_key(), None);
        let len = initiator
            .write_handshake_message(&[], &mut message)
            .unwrap();
        assert_eq!(
            initiator.remote_static_public_key(),
            Some(responder_key.as_slice())
        );
        responder
            .read_handshake_message(&mut message, &mut payload, len)
            .unwrap();
        let initiator_key = crate::derive_static_public_key(&initiator_secret);
        assert_eq!(
            responder.remote_static_public_key(),
            Some(initiator_key.as_slice())
        );

        initiator.transition_to_transport().unwrap();
        responder.transition_to_transport().unwrap();
        assert_eq!(
            initiator.remote_static_public_key(),
            Some(responder_key.as_slice())
        );
        assert_eq!(
            responder.remote_static_public_key(),
            Some(initiator_key.as_slice())
        );
        let (initiator, responder) = transport_contexts();
        assert_eq!(initiator.remote_static_public_key(), None);
        assert_eq!(responder.remote_static_public_key(), None);
    }

    #[test]
    fn test_remote_static_public_key_rejects_unauthenticated_handshake() {
        for tamper_second_message in [true, false] {
            let mut initiator = DataLinkContext::new(
                HandshakePattern::PatternXX,
                true,
                Some([3; 32]),
                Keypair::random().public_key(),
            )
            .unwrap();
            let mut responder = DataLinkContext::new(
                HandshakePattern::PatternXX,
                false,
                Some([4; 32]),
                Keypair::random().public_key(),
            )
            .unwrap();
            let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
            let mut payload = [0; PUBKY_NOISE_MSG_LEN];
            let len = initiator
                .write_handshake_message(&[], &mut message)
                .unwrap();
            responder
                .read_handshake_message(&mut message, &mut payload, len)
                .unwrap();
            let mut len = responder
                .write_handshake_message(&[], &mut message)
                .unwrap();
            let receiver = if tamper_second_message {
                &mut initiator
            } else {
                initiator
                    .read_handshake_message(&mut message, &mut payload, len)
                    .unwrap();
                len = initiator
                    .write_handshake_message(&[], &mut message)
                    .unwrap();
                &mut responder
            };
            message[len - 1] ^= 1;
            assert!(receiver
                .read_handshake_message(&mut message, &mut payload, len)
                .is_err());
            assert!(receiver.is_handshake());
            assert_eq!(receiver.remote_static_public_key(), None);
        }
    }

    #[test]
    fn test_prepared_transport_operations_do_not_advance_state() {
        let (initiator, responder) = transport_contexts();
        let mut ciphertext = [0; PUBKY_NOISE_TRANSPORT_PACKET_LEN];
        let mut plaintext = [0; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];
        let message = [42; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];

        let ciphertext_len = initiator
            .prepare_transport_write(&message, &mut ciphertext)
            .unwrap();
        let plaintext_len = responder
            .prepare_transport_read(&ciphertext, &mut plaintext)
            .unwrap();

        assert_eq!(ciphertext_len, PUBKY_NOISE_TRANSPORT_PACKET_LEN);
        assert_eq!(plaintext_len, PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN);
        assert_eq!(plaintext, message);
        assert_eq!(initiator.get_sending_nonce(), 0);
        assert_eq!(responder.get_receiving_nonce(), 0);
    }

    #[test]
    fn test_authenticated_transport_wire_vector() {
        let (initiator, _) = transport_contexts();
        let plaintext = crate::encode_transport_plaintext(b"paykit!").unwrap();
        let mut ciphertext = [0; PUBKY_NOISE_TRANSPORT_PACKET_LEN];

        assert_eq!(
            initiator
                .prepare_transport_write(&plaintext, &mut ciphertext)
                .unwrap(),
            PUBKY_NOISE_TRANSPORT_PACKET_LEN
        );
        assert_eq!(
            hex::encode(Sha256::digest(ciphertext)),
            "a1eeedce594a1947e52c950ec6f207b133a93d1f09c92f01f0f66169d26d5c0c"
        );
    }

    #[test]
    fn test_authenticated_transport_rejects_non_zero_padding_without_advancing_state() {
        let (initiator, responder) = transport_contexts();
        let mut plaintext = crate::encode_transport_plaintext(b"message").unwrap();
        *plaintext.last_mut().unwrap() = 1;
        let mut ciphertext = [0; PUBKY_NOISE_TRANSPORT_PACKET_LEN];
        let sender_before = initiator.get_sending_nonce();
        let receiver_before = responder.get_receiving_nonce();

        assert_eq!(
            initiator
                .prepare_transport_write(&plaintext, &mut ciphertext)
                .unwrap(),
            PUBKY_NOISE_TRANSPORT_PACKET_LEN
        );

        let mut decrypted = [0; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];
        assert_eq!(
            responder
                .prepare_transport_read(&ciphertext, &mut decrypted)
                .unwrap(),
            PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN
        );
        assert_eq!(
            crate::decode_transport_plaintext(&decrypted).unwrap_err(),
            crate::PubkyNoiseError::BadLengthCiphertext
        );
        assert_eq!(initiator.get_sending_nonce(), sender_before);
        assert_eq!(responder.get_receiving_nonce(), receiver_before);
    }

    #[test]
    fn test_handshake_message_methods_reject_transport_phase() {
        let (mut initiator, mut responder) = transport_contexts();
        let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
        let mut payload = [0; PUBKY_NOISE_MSG_LEN];
        let initiator_before = (initiator.get_sending_nonce(), initiator.get_write_counter());
        let responder_before = (
            responder.get_receiving_nonce(),
            responder.get_read_counter(),
        );

        assert!(matches!(
            initiator.write_handshake_message(&[], &mut message),
            Err(ContextError::InvalidPhase)
        ));
        assert!(matches!(
            responder.read_handshake_message(&mut message, &mut payload, 0),
            Err(ContextError::InvalidPhase)
        ));
        assert_eq!(
            (initiator.get_sending_nonce(), initiator.get_write_counter()),
            initiator_before
        );
        assert_eq!(
            (
                responder.get_receiving_nonce(),
                responder.get_read_counter()
            ),
            responder_before
        );
    }

    #[test]
    fn test_transport_cursor_boundaries_reserve_restorable_sentinel() {
        assert!(ensure_counter_can_increment(u32::MAX - 2).is_ok());
        assert!(matches!(
            ensure_counter_can_increment(u32::MAX - 1),
            Err(ContextError::CounterOverflow)
        ));
        assert!(ensure_nonce_can_increment(u64::MAX - 2).is_ok());
        assert!(matches!(
            ensure_nonce_can_increment(u64::MAX - 1),
            Err(ContextError::NonceOverflow)
        ));
    }
}
