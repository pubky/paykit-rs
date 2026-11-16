pub mod backup_crypto;
pub mod identity_payload;
pub mod path_derivation;
pub mod serializer;
pub mod snow_crypto;
pub mod snow_crypto_resolver;

use std::str::FromStr;
use std::sync::Arc;

use pubky::errors::RequestError;
use pubky::prelude::*;
use pubky::PubkySession;
use sha2::{Digest, Sha256};

use serializer::{PubkyNoiseSessionState, SESSION_STATE_VERSION};
use snow_crypto::{
    full_handshake_actions, ContextError, DataLinkContext, HandshakeAction, HandshakePattern,
    NoisePhase, PUBKY_NOISE_CIPHERTEXT_LEN, PUBKY_NOISE_MSG_LEN, PUBKY_NOISE_TRANSPORT_PACKET_LEN,
    PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN,
};

/// A 32-byte identifier derived from the Noise handshake.
///
/// If ephemeral keys are used by the Noise pattern relied
/// on for the handshake, should change after every handshake.
#[derive(Eq, Hash, PartialEq, Clone, Copy, Debug)]
pub struct LinkId(pub [u8; 32]);

/// Derive the X25519 public key for the static secret passed to
/// [`PubkyNoiseEncryptor::new`]. This is not an Ed25519 public-key conversion.
pub fn derive_static_public_key(secret_key: &[u8; 32]) -> [u8; 32] {
    curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(*secret_key).to_bytes()
}

/// Decode a length-prefixed packet into a message buffer and its length.
///
/// Wire format: `[len_hi, len_lo, payload...]` where len is big-endian u16.
fn decode_handshake_packet(
    ciphertext: &[u8],
) -> Result<([u8; PUBKY_NOISE_CIPHERTEXT_LEN], usize), PubkyNoiseError> {
    if ciphertext.len() < 2 || ciphertext.len() > PUBKY_NOISE_CIPHERTEXT_LEN + 2 {
        return Err(PubkyNoiseError::BadLengthCiphertext);
    }
    let len = u16::from_be_bytes([ciphertext[0], ciphertext[1]]) as usize;
    if len > PUBKY_NOISE_CIPHERTEXT_LEN || len + 2 > ciphertext.len() {
        return Err(PubkyNoiseError::BadLengthCiphertext);
    }
    let mut message = [0u8; PUBKY_NOISE_CIPHERTEXT_LEN];
    message[..len].copy_from_slice(&ciphertext[2..len + 2]);
    Ok((message, len))
}

/// Encode a message into a length-prefixed packet.
///
/// Wire format: `[len_hi, len_lo, payload...]` where len is big-endian u16.
fn encode_handshake_packet(data: &[u8], len: usize) -> [u8; PUBKY_NOISE_CIPHERTEXT_LEN + 2] {
    let mut packet = [0u8; PUBKY_NOISE_CIPHERTEXT_LEN + 2];
    let be_bytes = (len as u16).to_be_bytes();
    packet[0..2].copy_from_slice(&be_bytes);
    packet[2..len + 2].copy_from_slice(&data[..len]);
    packet
}

async fn read_handshake_packet(
    response: pubky::Result<reqwest::Response>,
) -> Result<Option<([u8; PUBKY_NOISE_CIPHERTEXT_LEN], usize)>, PubkyNoiseError> {
    let response = match response {
        Ok(response) => response,
        Err(Error::Request(RequestError::Server { status, .. }))
            if status == StatusCode::NOT_FOUND || status == StatusCode::GONE =>
        {
            return Ok(None);
        }
        Err(error) => return Err(map_homeserver_read_error(error)),
    };
    let ciphertext = read_bounded_packet(response, PUBKY_NOISE_CIPHERTEXT_LEN + 2).await?;
    decode_handshake_packet(&ciphertext).map(Some)
}

/// THROWAWAY (#411): wasm32 reqwest (browser `fetch`) has no
/// `Response::chunk()`. On native this is the original `reqwest::Response`, so
/// the read loops below still call `Response::chunk()` unchanged. On wasm32 it
/// wraps `Response::bytes_stream()` (reqwest `stream` feature, enabled for
/// wasm32 only) behind a `chunk()` method with the same signature, so the byte
/// limit is still enforced chunk by chunk.
#[cfg(not(target_arch = "wasm32"))]
type BoundedBody = reqwest::Response;

#[cfg(not(target_arch = "wasm32"))]
fn bounded_body(response: reqwest::Response) -> BoundedBody {
    response
}

#[cfg(target_arch = "wasm32")]
struct BoundedBody(
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + 'static>>,
);

#[cfg(target_arch = "wasm32")]
impl BoundedBody {
    async fn chunk(&mut self) -> reqwest::Result<Option<bytes::Bytes>> {
        futures_util::StreamExt::next(&mut self.0).await.transpose()
    }
}

#[cfg(target_arch = "wasm32")]
fn bounded_body(response: reqwest::Response) -> BoundedBody {
    BoundedBody(Box::pin(response.bytes_stream()))
}

async fn read_bounded_packet(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, PubkyNoiseError> {
    if response
        .content_length()
        .is_some_and(|len| len > max_bytes as u64)
    {
        return Err(PubkyNoiseError::BadLengthCiphertext);
    }
    let mut body = Vec::new();
    // THROWAWAY (#411): `bounded_body` is the identity on native; see BoundedBody.
    let mut response = bounded_body(response);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| PubkyNoiseError::HomeserverResponseError)?
    {
        // Check before extending; Content-Length may be absent or untrustworthy.
        if chunk.len() > max_bytes - body.len() {
            return Err(PubkyNoiseError::BadLengthCiphertext);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn map_homeserver_read_error(error: Error) -> PubkyNoiseError {
    match error {
        Error::Request(RequestError::Server { status, .. }) => {
            PubkyNoiseError::HomeserverHttpError {
                status: status.as_u16(),
            }
        }
        _ => PubkyNoiseError::HomeserverResponseError,
    }
}

/// Build the fixed-size plaintext frame protected by Noise transport AEAD.
///
/// Format: `[body_len_hi, body_len_lo, body, zero padding]`.
fn encode_transport_plaintext(
    body: &[u8],
) -> Result<[u8; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN], PubkyNoiseError> {
    if body.len() > PUBKY_NOISE_MSG_LEN {
        return Err(PubkyNoiseError::EncryptionError);
    }

    let mut plaintext = [0; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];
    plaintext[..2].copy_from_slice(&(body.len() as u16).to_be_bytes());
    plaintext[2..body.len() + 2].copy_from_slice(body);
    Ok(plaintext)
}

/// Extract a message from an authenticated transport plaintext frame.
fn decode_transport_plaintext(
    plaintext: &[u8; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN],
) -> Result<([u8; PUBKY_NOISE_MSG_LEN], usize), PubkyNoiseError> {
    let body_len = u16::from_be_bytes([plaintext[0], plaintext[1]]) as usize;
    if body_len > PUBKY_NOISE_MSG_LEN {
        return Err(PubkyNoiseError::BadLengthCiphertext);
    }
    if plaintext[body_len + 2..].iter().any(|byte| *byte != 0) {
        return Err(PubkyNoiseError::BadLengthCiphertext);
    }

    let mut body = [0; PUBKY_NOISE_MSG_LEN];
    body[..body_len].copy_from_slice(&plaintext[2..body_len + 2]);
    Ok((body, body_len))
}

fn map_context_overflow(err: ContextError) -> PubkyNoiseError {
    match err {
        ContextError::CounterOverflow => PubkyNoiseError::CounterOverflow,
        ContextError::NonceOverflow => PubkyNoiseError::NonceOverflow,
        _ => PubkyNoiseError::OtherError,
    }
}

fn map_backup_error(err: backup_crypto::BackupCryptoError) -> PubkyNoiseError {
    match err {
        backup_crypto::BackupCryptoError::Rollback { .. } => {
            PubkyNoiseError::RestoreBackupRollbackError
        }
        backup_crypto::BackupCryptoError::DecryptError => {
            PubkyNoiseError::RestoreBackupDecryptError
        }
        _ => PubkyNoiseError::RestoreBackupDeserializeError,
    }
}

// Hash the exact prepared operation so stale or modified values cannot be acknowledged.
fn prepared_transport_digest(
    operation_kind: u8,
    resource: &[u8],
    message: &[u8],
    state: &PubkyNoiseSessionState,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([operation_kind]);
    hasher.update((resource.len() as u64).to_be_bytes());
    hasher.update(resource);
    hasher.update((message.len() as u64).to_be_bytes());
    hasher.update(message);
    hasher.update(state.serialize());
    hasher.finalize().into()
}

/// A session snapshot loaded and decrypted from the homeserver.
///
/// Returned by [`PubkyNoiseEncryptor::load_snapshot`]. The `generation` is
/// the caller-managed monotonic counter bound into the backup; after a
/// successful load, callers should record it in trusted local storage as
/// their new rollback checkpoint (the accepted generation is never lower
/// than the `min_generation` the load was checked against, but may be
/// higher).
#[derive(Clone)]
pub struct LoadedSnapshot {
    /// The generation the backup was persisted with.
    pub generation: u64,
    /// The decrypted session state, ready for
    /// [`PubkyNoiseEncryptor::restore`].
    pub state: PubkyNoiseSessionState,
}

/// Redacted `Debug`: delegates to the redacted `Debug` of
/// [`PubkyNoiseSessionState`], which never renders secret key material.
impl std::fmt::Debug for LoadedSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedSnapshot")
            .field("generation", &self.generation)
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Eq, Hash, PartialEq, Debug)]
pub enum PubkyNoiseError {
    /// The configured pattern name is not recognized.
    UnknownNoisePattern,
    /// The pattern is recognized but its handshake actions are not supported.
    UnsupportedNoisePattern,
    SnowNoiseBuildError,
    BadLengthCiphertext,
    /// Handshake preparation requires an incoming packet, but none was supplied.
    MissingHandshakePacket,
    /// Handshake preparation received a packet when no read was required.
    UnexpectedHandshakePacket,
    /// A homeserver read failed without an HTTP status, a successful (2xx)
    /// response body could not be read, or a snapshot exceeded its size cap.
    /// This does not imply that retrying will help.
    HomeserverResponseError,
    /// A homeserver read returned a non-success HTTP status other than an
    /// absence status handled by that operation. Retry policy belongs to the
    /// caller; authorization or request errors may need correction first.
    HomeserverHttpError {
        /// HTTP response status code. The response body is not retained.
        status: u16,
    },
    /// Handshake write failed to reach the homeserver.
    /// The Noise state has already advanced irreversibly; recovery requires
    /// restoring from [`PubkyNoiseEncryptor::last_good_snapshot()`].
    HomeserverWriteError,
    IsHandshake,
    /// Restore failed: handshake replay error.
    RestoreBackupReplayError,
    /// Restore failed: handshake hash mismatch after replay.
    RestoreBackupHashMismatch,
    /// Restore failed: deserialization error.
    RestoreBackupDeserializeError,
    /// Restore failed: persisted snapshot decryption failed.
    RestoreBackupDecryptError,
    /// Restore failed: persisted snapshot is older than the trusted local
    /// checkpoint (rollback detected).
    RestoreBackupRollbackError,
    /// Restore failed: no backup exists at the backup path on the homeserver.
    ///
    /// Distinct from [`PubkyNoiseError::HomeserverResponseError`] and
    /// [`PubkyNoiseError::HomeserverHttpError`], neither of which confirms
    /// the absence of a backup.
    RestoreBackupNotFoundError,
    /// Handshake or transport message encryption failed.
    EncryptionError,
    /// Handshake or transport message authentication/decryption failed.
    DecryptionError,
    /// Message slot counter space is exhausted.
    CounterOverflow,
    /// Transport nonce space is exhausted.
    NonceOverflow,
    /// A prepared transport operation is awaiting durable acknowledgement.
    UnacknowledgedPreparedTransport,
    /// No prepared transport operation is awaiting acknowledgement.
    NoPreparedTransport,
    /// The prepared value does not match the operation awaiting acknowledgement.
    PreparedTransportMismatch,
    OtherError,
}

#[derive(Eq, Hash, PartialEq, Debug)]
pub enum HandshakeResult {
    Pending,
    Terminal,
}

/// A handshake step prepared for durable storage and later publication.
///
/// Persist the resulting state and optional exact packet/path together before
/// publishing. Keep the packet pending across uncertain writes and retry those
/// same bytes. While a packet is pending, do not restore and advance the state
/// or promote it to transport. The caller must durably acknowledge publication
/// while holding its session lease and checking its application policy for the
/// peer. Noise completion does not grant application access. For XX, compare the
/// learned static key with the independently authenticated expected peer key.
/// NN does not authenticate a peer identity.
#[must_use = "persist the state and exact pending packet before publication"]
pub struct PreparedHandshakeStep {
    resulting_session_state: PubkyNoiseSessionState,
    outgoing: Option<(String, [u8; PUBKY_NOISE_CIPHERTEXT_LEN + 2])>,
    handshake_complete: bool,
    remote_static_public_key: Option<Vec<u8>>,
}

impl PreparedHandshakeStep {
    /// Return the post-step state, which remains in handshake phase.
    pub fn resulting_session_state(&self) -> &PubkyNoiseSessionState {
        &self.resulting_session_state
    }

    /// Return the publication path, present exactly when [`Self::packet`] is.
    pub fn destination_path(&self) -> Option<&str> {
        self.outgoing.as_ref().map(|(path, _)| path.as_str())
    }

    /// Return the exact length-prefixed, padded wire packet to publish or retry.
    /// Every returned slice contains `PUBKY_NOISE_CIPHERTEXT_LEN + 2` bytes,
    /// borrowed from the prepared step's fixed-size packet.
    pub fn packet(&self) -> Option<&[u8]> {
        self.outgoing.as_ref().map(|(_, packet)| packet.as_slice())
    }

    /// Return cryptographic completion, not publication or application readiness.
    /// A completed step can still contain an outgoing packet.
    pub fn is_handshake_complete(&self) -> bool {
        self.handshake_complete
    }

    /// Return the learned static key after cryptographic completion, if any.
    /// Compare it with the independently authenticated expected peer key.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        self.remote_static_public_key.as_deref()
    }
}

impl std::fmt::Debug for PreparedHandshakeStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedHandshakeStep")
            .field("resulting_session_state", &"<redacted>")
            .field("outgoing", &"<redacted>")
            .field("handshake_complete", &self.handshake_complete)
            .field("remote_static_public_key", &"<redacted>")
            .finish()
    }
}

/// A transport message staged for persistent handoff and later publication.
///
/// Preparing a send reserves its nonce and storage slot on the encryptor.
/// Callers must persist this value before publishing its ciphertext.
#[must_use = "persist and acknowledge the prepared send before continuing"]
pub struct PreparedSend {
    /// Pubky storage path for the ciphertext.
    destination_path: String,
    /// Exact ciphertext packet to publish or retry.
    ciphertext: [u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN],
    /// Session state to persist with this prepared ciphertext.
    resulting_session_state: PubkyNoiseSessionState,
    // Binds acknowledgement to this exact prepared operation; it is not a secret.
    acknowledgement_digest: [u8; 32],
}

impl PreparedSend {
    /// Return the Pubky path where the ciphertext must be published.
    pub fn destination_path(&self) -> &str {
        &self.destination_path
    }

    /// Return the exact ciphertext packet to publish or retry.
    pub fn ciphertext(&self) -> &[u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN] {
        &self.ciphertext
    }

    /// Return the session state produced by this send.
    pub fn resulting_session_state(&self) -> &PubkyNoiseSessionState {
        &self.resulting_session_state
    }
}

impl std::fmt::Debug for PreparedSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedSend")
            .field("destination_path", &self.destination_path)
            .field(
                "ciphertext",
                &format_args!("<redacted:{} bytes>", self.ciphertext.len()),
            )
            .field("resulting_session_state", &"<redacted>")
            .finish()
    }
}

/// A decrypted transport message and its resulting session state.
///
/// Preparing a receive advances the encryptor past its ciphertext. Callers
/// should atomically replace the previous persisted session snapshot with
/// `resulting_session_state` while storing the application state produced from
/// `plaintext`, before exposing the event as processed.
///
/// The plaintext is sensitive application data. Avoid logging it, minimize
/// copies, and protect it at rest if the application protocol requires its
/// persistence.
#[must_use = "persist the processing result and acknowledge the receive before continuing"]
pub struct PreparedReceive {
    /// Decrypted private message payload buffer.
    plaintext: [u8; PUBKY_NOISE_MSG_LEN],
    plaintext_len: usize,
    /// Session state to persist with the application's processing result.
    resulting_session_state: PubkyNoiseSessionState,
    // Binds acknowledgement to this exact prepared operation; it is not a secret.
    acknowledgement_digest: [u8; 32],
}

impl PreparedReceive {
    /// Return the exact decrypted private message payload.
    pub fn plaintext(&self) -> &[u8] {
        &self.plaintext[..self.plaintext_len]
    }

    /// Return the session state produced by this receive.
    pub fn resulting_session_state(&self) -> &PubkyNoiseSessionState {
        &self.resulting_session_state
    }
}

impl std::fmt::Debug for PreparedReceive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedReceive")
            .field("plaintext", &"<redacted>")
            .field("resulting_session_state", &"<redacted>")
            .finish()
    }
}

/// Exact in-memory retry state retained by the deprecated convenience send API.
struct PendingConvenienceSend {
    /// Pubky path targeted by the original send attempt.
    destination_path: String,
    /// Ciphertext that must be retried without re-encryption.
    ciphertext: [u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN],
    /// Digest used to acknowledge the matching prepared state transition.
    acknowledgement_digest: [u8; 32],
}

impl std::fmt::Debug for PendingConvenienceSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingConvenienceSend")
            .field("destination_path", &self.destination_path)
            .field(
                "ciphertext",
                &format_args!("<redacted:{} bytes>", self.ciphertext.len()),
            )
            .finish()
    }
}

/// Shared configuration and resources for multiple `PubkyNoiseEncryptor` instances.
///
/// This struct holds the resources that are common across all Noise sessions for
/// a given user: the HTTP client, the authenticated homeserver session, the
/// write/read paths, and the root keypair. Wrap it in `Arc` and share it across
/// multiple single-session encryptors.
///
/// ## Asymmetric paths
///
/// `write_path` is the folder prefix used when writing messages to the local
/// homeserver. `read_path` is the folder prefix used when reading messages from
/// the remote homeserver. For symmetric usage (same path for both), pass the
/// same value to both fields via [`PubkyNoiseConfig::new`].
///
/// For per-peer-pair path privacy, use [`path_derivation::derive_asymmetric_paths`]
/// to compute distinct write/read paths from a DH shared secret.
///
/// ## Request deadlines
///
/// Size limits bound response bytes, not time spent waiting for a body. Noise
/// adds no deadline. On native targets, configure a total
/// [`pubky::PubkyHttpClientBuilder::request_timeout`] on both the outbox client
/// and the client used to create the local session. Pubky's native HTTP timeouts
/// are disabled by default. A read timeout alone resets on successful reads and
/// does not bound a continually active response, including empty body frames.
/// Browser callers must enforce their own operation deadline and cancellation;
/// Pubky's native HTTP timeout settings do not apply to WASM.
///
/// ```no_run
/// # #[cfg(not(target_arch = "wasm32"))]
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use std::time::Duration;
/// use pubky::{Pubky, PubkyHttpClient};
///
/// let client = PubkyHttpClient::builder()
///     .request_timeout(Duration::from_secs(10)) // Choose for the application's workload.
///     .build()?;
/// let pubky = Pubky::with_client(client);
/// // Use this client for peer reads and to create the authenticated session.
/// # let _ = pubky;
/// # Ok(())
/// # }
/// # #[cfg(target_arch = "wasm32")]
/// # fn main() {}
/// ```
pub struct PubkyNoiseConfig {
    pub outbox_client: Pubky,
    pub local_session: PubkySession,
    /// Folder prefix for writing messages to the local homeserver.
    pub write_path: String,
    /// Folder prefix for reading messages from the remote homeserver.
    pub read_path: String,
    pub pubky_root_keypair: Keypair,
    pub pubky_noise_version: u32,
    pub default_pattern: HandshakePattern,
}

impl PubkyNoiseConfig {
    /// Create a new shared configuration with a single symmetric path.
    ///
    /// This is a convenience constructor that uses the same path for both
    /// reading and writing. For per-peer-pair path privacy, use
    /// [`PubkyNoiseConfig::new_with_paths`] instead.
    ///
    /// # Parameters:
    /// - `pubky_root_seckey`: A 32-byte root secret key.
    /// - `pubky_noise_version`: Protocol version identifier.
    /// - `pattern_string`: Default Noise pattern (e.g. "NN", "XX").
    /// - `homeserver_auth_session`: An authenticated PubkySession.
    /// - `destination_path`: Custom destination prefix for message sharing
    ///   (used for both reads and writes).
    /// - `outbox_client`: HTTP Pubky client.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::UnknownNoisePattern`] if the pattern string is invalid.
    pub fn new(
        pubky_root_seckey: [u8; 32],
        pubky_noise_version: u32,
        pattern_string: &str,
        homeserver_auth_session: PubkySession,
        destination_path: String,
        outbox_client: Pubky,
    ) -> Result<Arc<Self>, PubkyNoiseError> {
        Self::new_with_paths(
            pubky_root_seckey,
            pubky_noise_version,
            pattern_string,
            homeserver_auth_session,
            destination_path.clone(),
            destination_path,
            outbox_client,
        )
    }

    /// Create a new shared configuration with separate write and read paths.
    ///
    /// Use this constructor when the local party writes to a different path
    /// than it reads from (e.g., when using
    /// [`path_derivation::derive_asymmetric_paths`] for per-peer-pair privacy).
    ///
    /// # Parameters:
    /// - `pubky_root_seckey`: A 32-byte root secret key.
    /// - `pubky_noise_version`: Protocol version identifier.
    /// - `pattern_string`: Default Noise pattern (e.g. "NN", "XX").
    /// - `homeserver_auth_session`: An authenticated PubkySession.
    /// - `write_path`: Folder prefix for writing messages to the local homeserver.
    /// - `read_path`: Folder prefix for reading messages from the remote homeserver.
    /// - `outbox_client`: HTTP Pubky client.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::UnknownNoisePattern`] if the pattern string is invalid.
    pub fn new_with_paths(
        pubky_root_seckey: [u8; 32],
        pubky_noise_version: u32,
        pattern_string: &str,
        homeserver_auth_session: PubkySession,
        write_path: String,
        read_path: String,
        outbox_client: Pubky,
    ) -> Result<Arc<Self>, PubkyNoiseError> {
        let pubky_root_keypair = Keypair::from_secret(&pubky_root_seckey);
        let default_pattern = HandshakePattern::from_str(pattern_string)
            .map_err(|_| PubkyNoiseError::UnknownNoisePattern)?;

        Ok(Arc::new(PubkyNoiseConfig {
            outbox_client,
            local_session: homeserver_auth_session,
            write_path,
            read_path,
            pubky_root_keypair,
            pubky_noise_version,
            default_pattern,
        }))
    }
}

/// A single-session Noise encryptor for Pubky Noise.
///
/// Each instance manages exactly one Noise session (handshake + transport) with
/// a single remote peer. To manage multiple concurrent sessions, create multiple
/// `PubkyNoiseEncryptor` instances sharing the same `Arc<PubkyNoiseConfig>`.
#[derive(Debug)]
pub struct PubkyNoiseEncryptor {
    config: Arc<PubkyNoiseConfig>,
    context: DataLinkContext,
    link_id: Option<LinkId>,
    endpoint_pubkey: PublicKey,

    /// Snapshot captured automatically at the start of each
    /// [`handle_handshake()`](Self::handle_handshake) call, before any
    /// state-mutating work. See [`last_good_snapshot()`](Self::last_good_snapshot).
    last_good_snapshot: Option<PubkyNoiseSessionState>,
    /// Fences transport operations until the prepared transition is persisted.
    unacknowledged_transport_digest: Option<[u8; 32]>,
    /// Exact retry data retained after an ambiguous convenience-send failure.
    pending_convenience_send: Option<PendingConvenienceSend>,

    // test-only fields — stripped from production builds
    #[cfg(feature = "test-utils")]
    simulate_tampering: bool,
    #[cfg(feature = "test-utils")]
    simulate_write_failure: bool,
    #[cfg(feature = "test-utils")]
    last_ciphertext: Option<[u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN]>,
}

impl PubkyNoiseEncryptor {
    /// Create a new single-session Noise encryptor.
    ///
    /// # Parameters:
    /// - `config`: Shared configuration (wrapped in Arc).
    /// - `holder_skey`: Local static secret key for this session.
    /// - `initiator`: Whether this side initiates the Noise handshake.
    /// - `endpoint_pubkey`: Remote peer's public key used as path suffix.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::SnowNoiseBuildError`] if the Noise stack fails to build.
    pub fn new(
        config: Arc<PubkyNoiseConfig>,
        mut holder_skey: [u8; 32],
        initiator: bool,
        endpoint_pubkey: PublicKey,
    ) -> Result<Self, PubkyNoiseError> {
        let data_link_context = DataLinkContext::new(
            config.default_pattern,
            initiator,
            Some(holder_skey),
            endpoint_pubkey.clone(),
        )
        .map_err(|_| PubkyNoiseError::SnowNoiseBuildError)?;

        // zeroize holder_skey after DataLinkContext load.
        holder_skey.copy_from_slice(&[0; 32][..]);
        //TODO: add assert on holder_skey byte pattern

        Ok(PubkyNoiseEncryptor {
            config,
            context: data_link_context,
            link_id: None,
            endpoint_pubkey,
            last_good_snapshot: None,
            unacknowledged_transport_digest: None,
            pending_convenience_send: None,
            #[cfg(feature = "test-utils")]
            simulate_tampering: false,
            #[cfg(feature = "test-utils")]
            simulate_write_failure: false,
            #[cfg(feature = "test-utils")]
            last_ciphertext: None,
        })
    }

    /// Returns `true` if the Noise handshake has completed.
    pub fn is_handshake_complete(&self) -> bool {
        !self.context.is_handshake()
    }

    /// Return the remote X25519 static key learned from the Noise handshake.
    ///
    /// Returns `None` until the handshake completes, or when the
    /// pattern has no remote static key. Available in transport and after restore.
    /// Callers must compare this with their independently authenticated expected
    /// key before trusting the completed link or exchanging application messages.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        self.context.remote_static_public_key()
    }

    /// Consume this encryptor to prepare one handshake step without network I/O.
    ///
    /// Supports NN and XX. For a handshake-phase snapshot, use
    /// [`PubkyNoiseSessionState::next_handshake_read_slot`] to choose the input:
    /// - `Some(slot)`: fetch that peer slot and pass `Some(packet)` containing
    ///   the complete length-prefixed wire packet. If the slot is absent
    ///   (HTTP 404/410), wait and poll it again instead of calling this method.
    /// - `None`: pass `None` to perform the remaining write or control actions,
    ///   or to return an already-completed handshake step.
    ///
    /// Before applying the result, recheck the session lease, source checkpoint,
    /// and the application's policy permitting use of the peer. These checks
    /// belong to the caller, not the Noise handshake. For XX, compare the learned
    /// static key with the independently authenticated expected peer key before
    /// exchanging application messages. NN does not authenticate a peer identity.
    ///
    /// On failure, discard the consumed state and restore the previous durable
    /// checkpoint. On success, persist the returned state and pending output
    /// atomically before any PUT. The final XX output must be published and
    /// durably acknowledged before promoting the handshake to transport.
    ///
    /// # Errors
    /// Returns [`PubkyNoiseError::MissingHandshakePacket`] if a read is required
    /// but no packet is supplied, or [`PubkyNoiseError::UnexpectedHandshakePacket`]
    /// if a packet is supplied when no read is required. Malformed packet framing
    /// returns [`PubkyNoiseError::BadLengthCiphertext`]. Authentication failures
    /// return [`PubkyNoiseError::DecryptionError`], failed Noise writes return
    /// [`PubkyNoiseError::EncryptionError`], and exhausted message slots return
    /// [`PubkyNoiseError::CounterOverflow`]. Already being in transport phase
    /// returns [`PubkyNoiseError::OtherError`]. Unsupported patterns return
    /// [`PubkyNoiseError::UnsupportedNoisePattern`].
    pub fn prepare_handshake_step(
        mut self,
        incoming_packet: Option<&[u8]>,
    ) -> Result<PreparedHandshakeStep, PubkyNoiseError> {
        if self.context.is_transport() {
            return Err(PubkyNoiseError::OtherError);
        }
        let actions = self.context.remaining_handshake_actions()?;
        match (
            actions.contains(&HandshakeAction::Read),
            incoming_packet.is_some(),
        ) {
            (true, false) => return Err(PubkyNoiseError::MissingHandshakePacket),
            (false, true) => return Err(PubkyNoiseError::UnexpectedHandshakePacket),
            _ => {}
        }
        let mut incoming = incoming_packet.map(decode_handshake_packet).transpose()?;
        let mut outgoing = None;
        for action in actions {
            match action {
                HandshakeAction::Read => {
                    let (message, len) = incoming
                        .take()
                        .ok_or(PubkyNoiseError::MissingHandshakePacket)?;
                    self.apply_handshake_read(message, len)?;
                }
                HandshakeAction::Write => {
                    self.context
                        .ensure_can_increment_counter()
                        .map_err(|_| PubkyNoiseError::CounterOverflow)?;
                    outgoing = Some(self.prepare_handshake_packet()?);
                    self.advance_handshake_message()?;
                }
                HandshakeAction::Pending => {
                    self.context.advance_sub_step();
                    break;
                }
                HandshakeAction::Terminal => break,
            }
        }
        self.context.complete_step();
        Ok(PreparedHandshakeStep {
            resulting_session_state: self.snapshot_unchecked(),
            outgoing,
            handshake_complete: self.is_handshake_complete(),
            remote_static_public_key: self.remote_static_public_key().map(<[u8]>::to_vec),
        })
    }

    fn apply_handshake_read(
        &mut self,
        mut message: [u8; PUBKY_NOISE_CIPHERTEXT_LEN],
        len: usize,
    ) -> Result<(), PubkyNoiseError> {
        self.context
            .ensure_can_increment_counter()
            .map_err(|_| PubkyNoiseError::CounterOverflow)?;
        let mut payload = [0; PUBKY_NOISE_MSG_LEN];
        self.context
            .read_handshake_message(&mut message, &mut payload, len)
            .map_err(|_| PubkyNoiseError::DecryptionError)?;
        self.advance_handshake_message()
    }

    fn prepare_handshake_packet(
        &mut self,
    ) -> Result<(String, [u8; PUBKY_NOISE_CIPHERTEXT_LEN + 2]), PubkyNoiseError> {
        let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
        let len = self
            .context
            .write_handshake_message(&[], &mut message)
            .map_err(|_| PubkyNoiseError::EncryptionError)?;
        let path = format!("{}/{}", self.config.write_path, self.context.get_counter());
        Ok((path, encode_handshake_packet(&message, len)))
    }

    fn advance_handshake_message(&mut self) -> Result<(), PubkyNoiseError> {
        self.context
            .increment_counter()
            .map_err(|_| PubkyNoiseError::CounterOverflow)?;
        self.context.advance_sub_step();
        Ok(())
    }

    /// Handle the forwarding and processing of Noise handshake messages.
    ///
    /// This method is **polling-safe**: it can be called repeatedly by either
    /// the initiator or responder in any order. If the peer's message is not
    /// yet available, returns `HandshakeResult::Pending` without advancing that read.
    ///
    /// # Outbox reliability and recovery
    ///
    /// In the outbox model, two kinds of interruption can occur:
    ///
    /// - **Absent message** (HTTP 404/410 from the peer's outbox):
    ///   The method returns `Pending` without advancing the pending read.
    ///   Other HTTP statuses return [`PubkyNoiseError::HomeserverHttpError`];
    ///   failures without a status and successful (2xx) body-read failures return
    ///   [`PubkyNoiseError::HomeserverResponseError`]. Neither implies that a
    ///   retry will help. Once the cause is addressed, subsequent calls attempt
    ///   the same read; earlier successful actions in this call remain applied.
    ///
    /// - **Write failure** (Initiator fails to write to her outbox):
    ///   If the homeserver `put()` call fails, this method returns
    ///   [`PubkyNoiseError::HomeserverWriteError`]. Because Snow's internal
    ///   `HandshakeState` has already been irreversibly advanced by
    ///   `write_message`, the encryptor is in a corrupted state and **cannot**
    ///   simply retry. The caller must recover by restoring from the
    ///   pre-mutation snapshot captured at the start of this call.
    ///
    ///   **Recovery**: each call to `handle_handshake` automatically captures
    ///   a pre-mutation snapshot accessible via
    ///   [`last_good_snapshot()`](Self::last_good_snapshot). For in-process
    ///   recovery, pass that snapshot directly to [`restore()`](Self::restore);
    ///   the replay mechanism rebuilds the Noise state at that checkpoint,
    ///   allowing the handshake to resume from the right position.
    ///
    ///   Callers that need crash recovery must durably persist the
    ///   pre-mutation snapshot themselves: encrypt it with
    ///   [`backup_crypto::encrypt_backup_with_key`] and store it through
    ///   caller-managed storage *before* the risky call. Note that
    ///   [`persist_snapshot()`](Self::persist_snapshot) snapshots the
    ///   encryptor's state at call time — it cannot persist
    ///   `last_good_snapshot()`, and after a write failure the current state
    ///   may already be advanced and corrupted.
    ///
    ///   Note: if the `put()` succeeds but the data is subsequently lost
    ///   (e.g. homeserver crash after acknowledgment), the same snapshot-based
    ///   recovery path applies — but the failure is not detectable by this
    ///   method.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::UnsupportedNoisePattern`] for unsupported patterns.
    /// - Returns [`PubkyNoiseError::BadLengthCiphertext`] on malformed messages.
    /// - Returns [`PubkyNoiseError::DecryptionError`] if a handshake message fails
    ///   authentication. The failed read does not advance the message counter or
    ///   handshake step. Restore from [`last_good_snapshot()`](Self::last_good_snapshot)
    ///   before reusing the encryptor.
    /// - Returns [`PubkyNoiseError::HomeserverHttpError`] on HTTP statuses other
    ///   than success or 404/410, retaining the status for caller classification.
    /// - Returns [`PubkyNoiseError::HomeserverResponseError`] on GET failures
    ///   without a status, or if a successful (2xx) response body cannot be read.
    /// - Returns [`PubkyNoiseError::HomeserverWriteError`] if the homeserver
    ///   write fails. Recovery via [`last_good_snapshot()`](Self::last_good_snapshot)
    ///   and [`restore()`](Self::restore) is required.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] if message slot space is exhausted.
    ///
    /// Successful response bodies are size-bounded before packet validation.
    /// Non-2xx bodies are handled by the Pubky SDK's configured error-body limit.
    /// Configure [request deadlines](PubkyNoiseConfig#request-deadlines) separately.
    pub async fn handle_handshake(&mut self) -> Result<HandshakeResult, PubkyNoiseError> {
        // Capture pre-mutation snapshot so callers can recover from write failures.
        self.last_good_snapshot = Some(self.snapshot_unchecked());

        let remaining_actions = self.context.remaining_handshake_actions()?;
        for action in remaining_actions {
            match action {
                HandshakeAction::Read => {
                    self.context
                        .ensure_can_increment_counter()
                        .map_err(|_| PubkyNoiseError::CounterOverflow)?;
                    let path = self.config.read_path.as_str();
                    let counter = self.context.get_counter();
                    let public_key = &self.endpoint_pubkey;
                    let formatted_path = format!("{public_key}/{path}/{counter}");

                    let response = self
                        .config
                        .outbox_client
                        .public_storage()
                        .get(formatted_path)
                        .await;
                    let Some((message, len)) = read_handshake_packet(response).await? else {
                        return Ok(HandshakeResult::Pending);
                    };
                    self.apply_handshake_read(message, len)?;
                }
                HandshakeAction::Write => {
                    self.context
                        .ensure_can_increment_counter()
                        .map_err(|_| PubkyNoiseError::CounterOverflow)?;
                    if let Ok((formatted_path, packet)) = self.prepare_handshake_packet() {
                        // Check for simulated write failure (test-only) or
                        // actual homeserver write failure.
                        #[cfg(feature = "test-utils")]
                        let write_failed = if self.simulate_write_failure {
                            true
                        } else {
                            self.config
                                .local_session
                                .storage()
                                .put(formatted_path, packet.to_vec())
                                .await
                                .is_err()
                        };
                        #[cfg(not(feature = "test-utils"))]
                        let write_failed = self
                            .config
                            .local_session
                            .storage()
                            .put(formatted_path, packet.to_vec())
                            .await
                            .is_err();
                        if write_failed {
                            // Snow's HandshakeState has already advanced
                            // irreversibly. The caller must recover via
                            // last_good_snapshot() + restore().
                            return Err(PubkyNoiseError::HomeserverWriteError);
                        }
                        self.advance_handshake_message()?;
                    }
                }
                HandshakeAction::Pending => {
                    self.context.advance_sub_step();
                    self.context.complete_step();
                    return Ok(HandshakeResult::Pending);
                }
                HandshakeAction::Terminal => {
                    self.context.complete_step();
                    return Ok(HandshakeResult::Terminal);
                }
            }
        }
        self.context.complete_step();
        Ok(HandshakeResult::Pending)
    }

    /// Transition from Noise Handshake phase to Transport phase.
    ///
    /// Call this once `is_handshake_complete()` returns `true`.
    /// Returns the `LinkId` derived from the handshake transcript hash.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::IsHandshake`] if still in handshake phase.
    pub fn transition_transport(&mut self) -> Result<LinkId, PubkyNoiseError> {
        if self.context.is_handshake() {
            return Err(PubkyNoiseError::IsHandshake);
        }
        let link_id = LinkId(self.context.get_handshake_hash().unwrap());
        let _ = self.context.transition_to_transport();
        self.link_id = Some(link_id);
        Ok(link_id)
    }

    fn build_prepared_send(&self, plaintext: &[u8]) -> Result<PreparedSend, PubkyNoiseError> {
        if self.unacknowledged_transport_digest.is_some() {
            return Err(PubkyNoiseError::UnacknowledgedPreparedTransport);
        }
        if !self.context.is_transport() {
            return Err(PubkyNoiseError::IsHandshake);
        }

        self.context
            .ensure_can_advance_write_slot()
            .map_err(map_context_overflow)?;
        self.context
            .ensure_can_advance_sending_nonce()
            .map_err(map_context_overflow)?;

        let plaintext = encode_transport_plaintext(plaintext)?;
        let mut ciphertext = [0; PUBKY_NOISE_TRANSPORT_PACKET_LEN];
        let len = self
            .context
            .prepare_transport_write(&plaintext, &mut ciphertext)
            .map_err(|_| PubkyNoiseError::EncryptionError)?;
        if len != PUBKY_NOISE_TRANSPORT_PACKET_LEN {
            return Err(PubkyNoiseError::EncryptionError);
        }
        let counter = self.context.get_write_slot();
        let destination_path = format!("{}/{counter}", self.config.write_path);

        let mut resulting_session_state = self.snapshot_unchecked();
        resulting_session_state.sending_nonce = resulting_session_state
            .sending_nonce
            .checked_add(1)
            .ok_or(PubkyNoiseError::NonceOverflow)?;
        resulting_session_state.write_counter = resulting_session_state
            .write_counter
            .checked_add(1)
            .ok_or(PubkyNoiseError::CounterOverflow)?;

        let acknowledgement_digest = prepared_transport_digest(
            0,
            destination_path.as_bytes(),
            &ciphertext,
            &resulting_session_state,
        );

        Ok(PreparedSend {
            destination_path,
            ciphertext,
            resulting_session_state,
            acknowledgement_digest,
        })
    }

    fn apply_prepared_send(&mut self, prepared: &PreparedSend) {
        self.context
            .set_sending_nonce(prepared.resulting_session_state.sending_nonce);
        self.context
            .set_write_counter(prepared.resulting_session_state.write_counter);
    }

    /// Encrypt and stage a transport message for persistent handoff.
    ///
    /// This method performs the Noise encryption without writing to Pubky. It
    /// reserves the nonce and storage slot on this encryptor so another call
    /// cannot prepare different plaintext with the same nonce. The maximum
    /// payload size is [`PUBKY_NOISE_MSG_LEN`] bytes.
    ///
    /// Persist the returned destination path, exact ciphertext, and
    /// `resulting_session_state` together before publishing the ciphertext. Use
    /// caller-managed persistent storage for this transaction; [`persist_snapshot()`](Self::persist_snapshot)
    /// is rejected while a prepared operation awaits acknowledgement. If
    /// persistence fails, discard this encryptor and restore its previous
    /// persisted state. If publication status is uncertain, retry the exact
    /// prepared ciphertext instead of preparing the plaintext again.
    ///
    /// Callers sharing session state across processes must serialize preparation
    /// and conditionally persist the resulting state against the state they
    /// loaded.
    /// Call [`acknowledge_persisted_send()`](Self::acknowledge_persisted_send) after
    /// persistence succeeds. No other transport operation can be prepared on
    /// this encryptor until then.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if another staged
    ///   transport operation has not been acknowledged.
    /// - Returns [`PubkyNoiseError::IsHandshake`] if not yet in transport phase.
    /// - Returns [`PubkyNoiseError::EncryptionError`] if Noise encryption fails.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] if the outbound transport
    ///   slot space is exhausted.
    /// - Returns [`PubkyNoiseError::NonceOverflow`] if the sending nonce space
    ///   is exhausted.
    pub fn prepare_send(&mut self, plaintext: &[u8]) -> Result<PreparedSend, PubkyNoiseError> {
        let prepared = self.build_prepared_send(plaintext)?;
        #[cfg(feature = "test-utils")]
        if self.simulate_tampering {
            self.last_ciphertext = Some(prepared.ciphertext);
        }
        self.apply_prepared_send(&prepared);
        self.unacknowledged_transport_digest = Some(prepared.acknowledgement_digest);
        Ok(prepared)
    }

    /// Encrypt and send plaintext over the established transport.
    /// The maximum payload size is [`PUBKY_NOISE_MSG_LEN`] bytes.
    ///
    /// This convenience method cannot make the ciphertext and resulting Noise
    /// state durable atomically. Use [`prepare_send()`](Self::prepare_send) for
    /// crash-safe publication and exact-ciphertext retry.
    /// If the homeserver write fails, this encryptor remains fenced;
    /// [`retry_pending_send()`](Self::retry_pending_send) republishes the exact
    /// retained ciphertext. That recovery is in-memory only and does not make
    /// this method crash-safe.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if a staged
    ///   transport operation has not been acknowledged.
    /// - Returns [`PubkyNoiseError::IsHandshake`] if not yet in transport phase.
    /// - Returns [`PubkyNoiseError::EncryptionError`] if Noise encryption fails.
    /// - Returns [`PubkyNoiseError::HomeserverWriteError`] if the homeserver
    ///   write fails.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] if the outbound transport
    ///   slot space is exhausted.
    /// - Returns [`PubkyNoiseError::NonceOverflow`] if the sending nonce space
    ///   is exhausted.
    #[deprecated(
        note = "use prepare_send and acknowledge_persisted_send for crash-safe publication"
    )]
    pub async fn send_message(&mut self, plaintext: &[u8]) -> Result<(), PubkyNoiseError> {
        let prepared = self.prepare_send(plaintext)?;

        self.pending_convenience_send = Some(PendingConvenienceSend {
            destination_path: prepared.destination_path.clone(),
            ciphertext: prepared.ciphertext,
            acknowledgement_digest: prepared.acknowledgement_digest,
        });

        self.publish_pending_convenience_send().await
    }

    async fn publish_pending_convenience_send(&mut self) -> Result<(), PubkyNoiseError> {
        let Some(pending) = self.pending_convenience_send.as_ref() else {
            return Err(PubkyNoiseError::NoPreparedTransport);
        };
        let destination_path = pending.destination_path.clone();
        let ciphertext = pending.ciphertext;
        let acknowledgement_digest = pending.acknowledgement_digest;

        #[cfg(feature = "test-utils")]
        if self.simulate_write_failure {
            return Err(PubkyNoiseError::HomeserverWriteError);
        }

        self.config
            .local_session
            .storage()
            .put(destination_path, ciphertext.to_vec())
            .await
            .map_err(|_| PubkyNoiseError::HomeserverWriteError)?;
        self.acknowledge_prepared_transport(&acknowledgement_digest)?;
        self.pending_convenience_send = None;
        Ok(())
    }

    /// Retry the exact ciphertext retained after an ambiguous `send_message` failure.
    ///
    /// Prefer the staged persistence API for new code. This in-memory retry does not
    /// survive a process crash.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::NoPreparedTransport`] if no convenience send
    ///   is awaiting retry.
    /// - Returns [`PubkyNoiseError::HomeserverWriteError`] if publication fails.
    #[deprecated(note = "use prepare_send with a persisted outbound record")]
    pub async fn retry_pending_send(&mut self) -> Result<(), PubkyNoiseError> {
        self.publish_pending_convenience_send().await
    }

    /// Return the Pubky resource path for the next inbound transport message.
    ///
    /// This method does not perform network I/O or mutate the encryptor.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if a staged
    ///   transport operation has not been acknowledged.
    /// - Returns [`PubkyNoiseError::IsHandshake`] if not yet in transport phase.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] or
    ///   [`PubkyNoiseError::NonceOverflow`] if the inbound transport space is
    ///   exhausted.
    pub fn next_receive_path(&self) -> Result<String, PubkyNoiseError> {
        if self.unacknowledged_transport_digest.is_some() {
            return Err(PubkyNoiseError::UnacknowledgedPreparedTransport);
        }
        if !self.context.is_transport() {
            return Err(PubkyNoiseError::IsHandshake);
        }
        self.context
            .ensure_can_advance_read_slot()
            .map_err(map_context_overflow)?;
        self.context
            .ensure_can_advance_receiving_nonce()
            .map_err(map_context_overflow)?;

        Ok(format!(
            "{}/{}/{}",
            self.context.get_endpoint(),
            self.config.read_path,
            self.context.get_read_slot()
        ))
    }

    fn build_prepared_receive(
        &self,
        ciphertext: &[u8],
    ) -> Result<PreparedReceive, PubkyNoiseError> {
        if self.unacknowledged_transport_digest.is_some() {
            return Err(PubkyNoiseError::UnacknowledgedPreparedTransport);
        }
        if !self.context.is_transport() {
            return Err(PubkyNoiseError::IsHandshake);
        }
        self.context
            .ensure_can_advance_read_slot()
            .map_err(map_context_overflow)?;
        self.context
            .ensure_can_advance_receiving_nonce()
            .map_err(map_context_overflow)?;

        let message: &[u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN] = ciphertext
            .try_into()
            .map_err(|_| PubkyNoiseError::BadLengthCiphertext)?;
        #[cfg(feature = "test-utils")]
        let message = {
            let mut message = *message;
            if self.simulate_tampering {
                message[1] ^= 1;
            }
            message
        };
        #[cfg(feature = "test-utils")]
        let message = &message;
        let mut plaintext = [0; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];

        let plaintext_len = self
            .context
            .prepare_transport_read(message, &mut plaintext)
            .map_err(|err| match err {
                ContextError::NonceOverflow => PubkyNoiseError::NonceOverflow,
                _ => PubkyNoiseError::DecryptionError,
            })?;
        if plaintext_len != PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN {
            return Err(PubkyNoiseError::DecryptionError);
        }
        let (plaintext, plaintext_len) = decode_transport_plaintext(&plaintext)?;

        let mut resulting_session_state = self.snapshot_unchecked();
        resulting_session_state.receiving_nonce = resulting_session_state
            .receiving_nonce
            .checked_add(1)
            .ok_or(PubkyNoiseError::NonceOverflow)?;
        resulting_session_state.read_counter = resulting_session_state
            .read_counter
            .checked_add(1)
            .ok_or(PubkyNoiseError::CounterOverflow)?;

        let acknowledgement_digest = prepared_transport_digest(
            1,
            self.config.read_path.as_bytes(),
            &plaintext[..plaintext_len],
            &resulting_session_state,
        );

        Ok(PreparedReceive {
            plaintext,
            plaintext_len,
            resulting_session_state,
            acknowledgement_digest,
        })
    }

    fn apply_prepared_receive(&mut self, prepared: &PreparedReceive) {
        self.context
            .set_receiving_nonce(prepared.resulting_session_state.receiving_nonce);
        self.context
            .set_read_counter(prepared.resulting_session_state.read_counter);
    }

    /// Prepare an inbound ciphertext for persistent application processing.
    ///
    /// This method decrypts the packet and advances this encryptor past it
    /// without performing network I/O. Atomically replace the previous persisted
    /// session snapshot with `resulting_session_state` while storing the
    /// application state produced from the plaintext, before treating the event
    /// as processed. Use caller-managed persistent storage;
    /// [`persist_snapshot()`](Self::persist_snapshot) is rejected while a
    /// prepared operation awaits acknowledgement. If persistence fails, discard
    /// this encryptor and restore its previous persisted state.
    ///
    /// Treat the plaintext as sensitive application data. Do not log it, and
    /// protect it at rest if the application protocol requires its persistence.
    ///
    /// Callers sharing session state across processes must serialize preparation
    /// and conditionally persist the resulting state against the state they
    /// loaded.
    /// Call [`acknowledge_persisted_receive()`](Self::acknowledge_persisted_receive) after
    /// persistence succeeds. No other transport operation can be prepared on
    /// this encryptor until then.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if another staged
    ///   transport operation has not been acknowledged.
    /// - Returns [`PubkyNoiseError::IsHandshake`] if not yet in transport phase.
    /// - Returns [`PubkyNoiseError::BadLengthCiphertext`] if the packet is malformed.
    /// - Returns [`PubkyNoiseError::DecryptionError`] if Noise decryption fails.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] if the remote outbound
    ///   transport slot space is exhausted.
    /// - Returns [`PubkyNoiseError::NonceOverflow`] if the receiving nonce space
    ///   is exhausted.
    pub fn prepare_receive(
        &mut self,
        ciphertext: &[u8],
    ) -> Result<PreparedReceive, PubkyNoiseError> {
        let prepared = self.build_prepared_receive(ciphertext)?;
        self.apply_prepared_receive(&prepared);
        self.unacknowledged_transport_digest = Some(prepared.acknowledgement_digest);
        Ok(prepared)
    }

    fn acknowledge_prepared_transport(
        &mut self,
        acknowledgement_digest: &[u8; 32],
    ) -> Result<(), PubkyNoiseError> {
        let Some(pending_digest) = self.unacknowledged_transport_digest.as_ref() else {
            return Err(PubkyNoiseError::NoPreparedTransport);
        };
        if pending_digest != acknowledgement_digest {
            return Err(PubkyNoiseError::PreparedTransportMismatch);
        }
        self.unacknowledged_transport_digest = None;
        Ok(())
    }

    /// Acknowledge that a prepared send and its resulting state were durably persisted.
    ///
    /// Call this only after atomically persisting both values from `prepared`.
    /// The prepared handle is consumed so it cannot be acknowledged twice.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::NoPreparedTransport`] if no operation is pending.
    /// - Returns [`PubkyNoiseError::PreparedTransportMismatch`] if `prepared`
    ///   belongs to another encryptor or an earlier operation.
    pub fn acknowledge_persisted_send(
        &mut self,
        prepared: PreparedSend,
    ) -> Result<(), PubkyNoiseError> {
        let acknowledgement_digest = prepared_transport_digest(
            0,
            prepared.destination_path.as_bytes(),
            &prepared.ciphertext,
            &prepared.resulting_session_state,
        );
        if acknowledgement_digest != prepared.acknowledgement_digest {
            return Err(PubkyNoiseError::PreparedTransportMismatch);
        }
        self.acknowledge_prepared_transport(&acknowledgement_digest)
    }

    /// Acknowledge that a prepared receive and its resulting state were durably persisted.
    ///
    /// Call this only after atomically persisting the resulting state with the
    /// application's durable processing result. The prepared handle is consumed
    /// so it cannot be acknowledged twice.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::NoPreparedTransport`] if no operation is pending.
    /// - Returns [`PubkyNoiseError::PreparedTransportMismatch`] if `prepared`
    ///   belongs to another encryptor or an earlier operation.
    pub fn acknowledge_persisted_receive(
        &mut self,
        prepared: PreparedReceive,
    ) -> Result<(), PubkyNoiseError> {
        let acknowledgement_digest = prepared_transport_digest(
            1,
            self.config.read_path.as_bytes(),
            prepared.plaintext(),
            &prepared.resulting_session_state,
        );
        if acknowledgement_digest != prepared.acknowledgement_digest {
            return Err(PubkyNoiseError::PreparedTransportMismatch);
        }
        self.acknowledge_prepared_transport(&acknowledgement_digest)
    }

    /// Receive and decrypt a message from the remote peer.
    ///
    /// Returns `Ok` with an empty vector when no message is available yet
    /// (normal polling behaviour).
    /// Successful response bodies are size-bounded before packet validation.
    /// Non-2xx bodies are handled by the Pubky SDK's configured error-body limit.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if a staged
    ///   transport operation has not been acknowledged.
    /// - Returns [`PubkyNoiseError::IsHandshake`] if not yet in transport phase.
    /// - Returns [`PubkyNoiseError::HomeserverHttpError`] for non-success HTTP
    ///   statuses other than 404. In particular, 410 is not treated as absence here.
    /// - Returns [`PubkyNoiseError::HomeserverResponseError`] if the homeserver read
    ///   fails without a status or if a successful (2xx) body cannot be read, including
    ///   expiry of a caller-configured [deadline](PubkyNoiseConfig#request-deadlines).
    /// - Returns [`PubkyNoiseError::BadLengthCiphertext`] if the packet is malformed.
    /// - Returns [`PubkyNoiseError::DecryptionError`] if Noise decryption fails.
    /// - Returns [`PubkyNoiseError::CounterOverflow`] if the remote outbound
    ///   transport slot space is exhausted.
    /// - Returns [`PubkyNoiseError::NonceOverflow`] if the receiving nonce space
    ///   is exhausted.
    pub async fn receive_message(&mut self) -> Result<Vec<Vec<u8>>, PubkyNoiseError> {
        let mut results = Vec::new();
        let formatted_path = self.next_receive_path()?;
        match self
            .config
            .outbox_client
            .public_storage()
            .get(formatted_path)
            .await
        {
            Ok(response) => {
                // pubky's get() only returns Ok for 2xx responses.
                let ciphertext =
                    read_bounded_packet(response, PUBKY_NOISE_TRANSPORT_PACKET_LEN).await?;
                let prepared = self.build_prepared_receive(&ciphertext)?;
                self.apply_prepared_receive(&prepared);
                results.push(prepared.plaintext().to_vec());
            }
            // 404 = no message available yet (normal polling behaviour)
            Err(Error::Request(RequestError::Server { status, .. }))
                if status == StatusCode::NOT_FOUND => {}
            // Any other error (network, 5xx, 403, etc.) is surfaced.
            Err(error) => {
                return Err(map_homeserver_read_error(error));
            }
        }
        Ok(results)
    }

    /// Close and clean up this encryptor's Noise session.
    pub fn close(&mut self) {
        self.context.delete();
        // when the last atomically reference counted pointer
        // to the config including the pubky_root_seckey, the
        // inner value allocation is destroyed.
        // TODO: verify the Arc implementation to check what
        // is mean by destroyed, it the memory free'd ?
    }

    /// Capture the current session state as a serializable snapshot.
    ///
    /// This snapshot contains everything needed to restore the session later
    /// by replaying persisted handshake messages.
    ///
    /// # Security
    ///
    /// The snapshot contains static and ephemeral secret key material. Encrypt
    /// and authenticate it at rest, restrict access to authorized callers, and
    /// ensure superseded snapshots are no longer recoverable. Retaining
    /// restorable ephemeral material extends its lifetime and can expose messages
    /// from this Noise session if the snapshot is compromised. Starting a fresh
    /// session does not protect old traffic while older snapshots remain
    /// recoverable.
    /// This crate does not authenticate or authorize processes that access
    /// caller-managed snapshots.
    ///
    /// During the handshake phase, a pre-mutation snapshot is captured
    /// automatically by [`handle_handshake()`](Self::handle_handshake) and
    /// is accessible via [`last_good_snapshot()`](Self::last_good_snapshot).
    /// This method remains useful for taking snapshots at arbitrary points
    /// (e.g. after transitioning to transport or after exchanging messages).
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] while a
    ///   prepared operation still needs to be persisted and acknowledged.
    pub fn snapshot(&self) -> Result<PubkyNoiseSessionState, PubkyNoiseError> {
        if self.unacknowledged_transport_digest.is_some() {
            return Err(PubkyNoiseError::UnacknowledgedPreparedTransport);
        }
        Ok(self.snapshot_unchecked())
    }

    fn snapshot_unchecked(&self) -> PubkyNoiseSessionState {
        let phase = self.context.get_phase();
        let handshake_hash = self.context.get_handshake_hash();

        PubkyNoiseSessionState {
            version: SESSION_STATE_VERSION,
            phase,
            pattern: self.context.get_pattern(),
            initiator: self.context.is_initiator(),
            ephemeral_secret: *self.context.get_ephemeral_secret(),
            static_secret: self.context.get_static_secret().copied(),
            counter: self.context.get_counter(),
            noise_step: self.context.get_noise_step(),
            sub_step_index: self.context.get_sub_step_index() as u8,
            handshake_hash,
            link_id: self.link_id.map(|id| id.0),
            sending_nonce: self.context.get_sending_nonce(),
            receiving_nonce: self.context.get_receiving_nonce(),
            write_counter: self.context.get_write_counter(),
            read_counter: self.context.get_read_counter(),
            endpoint_pubkey: self.endpoint_pubkey.to_bytes(),
            handshake_messages: self.context.handshake_messages().to_vec(),
        }
    }

    /// Persist the current session snapshot to the homeserver (encrypted path).
    ///
    /// The serialized snapshot contains the session's ephemeral and static
    /// secrets, so it is encrypted with a caller-provided key
    /// (`XChaCha20Poly1305`, see [`backup_crypto`]) before uploading.
    ///
    /// # Parameters:
    /// - `backup_key`: A 32-byte key used to encrypt the snapshot. For
    ///   apps with access to the Pubky root secret this can be obtained by
    ///   calling [`backup_crypto::derive_backup_key`]; delegated
    ///   apps may supply a key derived from a shared Noise/state key instead.
    /// - `generation`: A caller-managed, monotonically increasing counter that
    ///   is bound into the encrypted backup. Persist the latest value in
    ///   trusted local storage and pass it as `min_generation` to
    ///   [`load_snapshot()`](Self::load_snapshot) to detect a stale or
    ///   malicious homeserver replaying an older backup.
    ///
    /// # Checkpoint update order
    ///
    /// The trusted local checkpoint must be advanced to `generation`
    /// **before** (or atomically with) this upload. If the checkpoint is
    /// advanced only after the upload and the process crashes in between, the
    /// checkpoint still holds `generation - 1`, so a homeserver replaying the
    /// previous backup would be accepted — the exact rollback the checkpoint
    /// exists to prevent. Crashing with the checkpoint already advanced only
    /// loses the new upload: loading then fails (or rejects the older record)
    /// instead of silently accepting stale state.
    ///
    /// # Errors
    /// - Returns [`PubkyNoiseError::UnacknowledgedPreparedTransport`] if a prepared
    ///   message and its resulting state must first be persisted together.
    /// - Returns [`PubkyNoiseError::OtherError`] if the homeserver write fails.
    pub async fn persist_snapshot(
        &self,
        backup_key: &[u8; 32],
        generation: u64,
    ) -> Result<(), PubkyNoiseError> {
        if self.unacknowledged_transport_digest.is_some() {
            return Err(PubkyNoiseError::UnacknowledgedPreparedTransport);
        }
        let state = self.snapshot()?;
        let path = format!("{}/backup", self.config.write_path);
        let encrypted =
            backup_crypto::encrypt_backup_with_key(backup_key, &path, generation, &state);
        self.config
            .local_session
            .storage()
            .put(path, encrypted)
            .await
            .map_err(|_| PubkyNoiseError::OtherError)?;
        Ok(())
    }

    /// Load and decrypt a session snapshot previously stored with
    /// [`persist_snapshot()`](Self::persist_snapshot).
    ///
    /// The returned state can be passed to [`restore()`](Self::restore) to
    /// reconstruct the encryptor.
    ///
    /// Successful (2xx) response bodies are read in chunks under a size cap
    /// ([`backup_crypto::MAX_BACKUP_RESPONSE_BYTES`]), and the record must
    /// match the size bounds of its envelope version. The backup path is
    /// probed with a `HEAD` request first: `HEAD` responses carry no body,
    /// so a malicious homeserver cannot use the probe itself to force a
    /// large allocation, and a confirmed-absent backup can be reported
    /// distinctly from a connectivity or server failure.
    ///
    /// Non-2xx bodies are handled by the Pubky SDK's configured error-body limit.
    /// Size limits do not bound elapsed time: configure a total
    /// [request deadline](PubkyNoiseConfig#request-deadlines) on the client used
    /// to create `config.local_session` for both the HEAD probe and the GET.
    ///
    /// # Parameters:
    /// - `backup_key`: A 32-byte key used to decrypt the snapshot. Must match
    ///   the key used during [`persist_snapshot()`](Self::persist_snapshot).
    ///   For apps with access to the Pubky root secret this can be obtained
    ///   by calling [`backup_crypto::derive_backup_key`]; delegated
    ///   apps may supply a key derived from a shared Noise/state key instead.
    /// - `min_generation`: The highest backup generation observed so far, from
    ///   the caller's trusted local checkpoint. Older backups are rejected
    ///   with [`PubkyNoiseError::RestoreBackupRollbackError`]. Pass `None`
    ///   only when no trusted checkpoint exists (e.g. restoring onto a fresh
    ///   device) — in that case rollback to an older backup cannot be
    ///   detected.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::RestoreBackupNotFoundError`] if no backup
    ///   exists at the backup path.
    /// - Returns [`PubkyNoiseError::HomeserverHttpError`] if the HEAD or GET
    ///   returns a non-success HTTP status other than 404/410.
    /// - Returns [`PubkyNoiseError::HomeserverResponseError`] if the backup
    ///   cannot be fetched without an HTTP status, a successful (2xx) body cannot be read,
    ///   or a 2xx response exceeds the size cap.
    /// - Returns [`PubkyNoiseError::RestoreBackupDecryptError`] if decryption
    ///   fails (wrong key, tampered/corrupted ciphertext, or a record that
    ///   was encrypted for a different backup path — the path is bound into
    ///   the AEAD tag as associated data, defeating cross-path substitution
    ///   by a malicious homeserver).
    /// - Returns [`PubkyNoiseError::RestoreBackupRollbackError`] if the backup
    ///   generation is older than `min_generation`.
    /// - Returns [`PubkyNoiseError::RestoreBackupDeserializeError`] if the record is
    ///   not a valid backup envelope or the decrypted bytes are not a valid
    ///   session state, including an unknown or unsupported snapshot pattern.
    pub async fn load_snapshot(
        config: &PubkyNoiseConfig,
        backup_key: &[u8; 32],
        min_generation: Option<u64>,
    ) -> Result<LoadedSnapshot, PubkyNoiseError> {
        let path = format!("{}/backup", config.write_path);
        let storage = config.local_session.storage();

        // HEAD rejects absent or oversized backups without fetching a body.
        match storage.stats(&path).await {
            // Reject an oversized backup before issuing the GET.
            Ok(Some(stats)) => {
                if let Some(len) = stats.content_length {
                    if len > backup_crypto::MAX_BACKUP_RESPONSE_BYTES as u64 {
                        return Err(PubkyNoiseError::HomeserverResponseError);
                    }
                }
            }
            // 404/410: the backup path is confirmed empty.
            Ok(None) => return Err(PubkyNoiseError::RestoreBackupNotFoundError),
            Err(error) => return Err(map_homeserver_read_error(error)),
        }

        let response = storage.get(&path).await.map_err(|err| match err {
            Error::Request(RequestError::Server { status, .. })
                if status == StatusCode::NOT_FOUND || status == StatusCode::GONE =>
            {
                PubkyNoiseError::RestoreBackupNotFoundError
            }
            error => map_homeserver_read_error(error),
        })?;

        // Bounded body read: reject oversized responses before proportional
        // allocation, regardless of what the Content-Length header claims
        // (the HEAD probe above is only advisory — the GET response may
        // differ from it).
        if let Some(len) = response.content_length() {
            if len > backup_crypto::MAX_BACKUP_RESPONSE_BYTES as u64 {
                return Err(PubkyNoiseError::HomeserverResponseError);
            }
        }
        let mut body = Vec::new();
        // THROWAWAY (#411): `bounded_body` is the identity on native; see BoundedBody.
        let mut response = bounded_body(response);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| PubkyNoiseError::HomeserverResponseError)?
        {
            if body.len() + chunk.len() > backup_crypto::MAX_BACKUP_RESPONSE_BYTES {
                return Err(PubkyNoiseError::HomeserverResponseError);
            }
            body.extend_from_slice(&chunk);
        }

        let (generation, serialized) =
            backup_crypto::decrypt_backup_with_key(backup_key, &path, &body, min_generation)
                .map_err(map_backup_error)?;

        let state = PubkyNoiseSessionState::deserialize(&serialized)
            .map_err(|_| PubkyNoiseError::RestoreBackupDeserializeError)?;

        Ok(LoadedSnapshot { generation, state })
    }

    /// Restore a `PubkyNoiseEncryptor` from a previously saved session state.
    ///
    /// This method:
    /// 1. Builds a fresh `DataLinkContext` with the saved ephemeral key
    /// 2. Uses saved incoming handshake messages
    /// 3. Replays them through the fresh `HandshakeState` to re-derive state
    /// 4. For transport restore: transitions to transport and sets nonces/counters
    /// 5. For handshake restore: stops at the saved step position
    ///
    /// Restoration performs no network I/O. It verifies the saved
    /// transcript by replaying it, not by trusting cached transport keys. Missing
    /// or changed remote handshake files do not invalidate that snapshot. Callers
    /// must still validate current peer authorization and coordinate fresh state.
    ///
    /// # Parameters:
    /// - `config`: Shared configuration (must match the original session).
    /// - `state`: The saved session state snapshot.
    ///
    /// # Errors:
    /// - Returns [`PubkyNoiseError::SnowNoiseBuildError`] if the Noise stack fails to build.
    /// - Returns [`PubkyNoiseError::RestoreBackupDeserializeError`] for unsupported
    ///   snapshot versions or patterns, invalid cursors, message counts, sizes,
    ///   or a mismatched endpoint.
    /// - Returns [`PubkyNoiseError::RestoreBackupReplayError`] for failed cryptographic
    ///   replay.
    /// - Returns [`PubkyNoiseError::RestoreBackupHashMismatch`] if the replayed handshake
    ///   produces a different hash than the saved handshake hash or LinkId, or
    ///   neither is present in a transport snapshot.
    pub async fn restore(
        config: Arc<PubkyNoiseConfig>,
        state: PubkyNoiseSessionState,
        endpoint_pubkey: PublicKey,
    ) -> Result<Self, PubkyNoiseError> {
        state
            .validate()
            .map_err(|_| PubkyNoiseError::RestoreBackupDeserializeError)?;
        // Verify the caller-provided pubkey matches the snapshot (consistency check)
        if endpoint_pubkey.to_bytes() != state.endpoint_pubkey {
            return Err(PubkyNoiseError::RestoreBackupDeserializeError);
        }

        // Build a fresh context with the saved ephemeral key
        let mut context = DataLinkContext::new_with_ephemeral(
            state.pattern,
            state.initiator,
            state.static_secret,
            endpoint_pubkey.clone(),
            Some(state.ephemeral_secret),
        )
        .map_err(|_| PubkyNoiseError::SnowNoiseBuildError)?;

        let mut saved_messages = state.handshake_messages.iter();
        // The validated counter counts completed reads and writes, not control actions.
        for action in full_handshake_actions(state.pattern, state.initiator)
            .map_err(|_| PubkyNoiseError::RestoreBackupDeserializeError)?
            .into_iter()
            .take(state.counter as usize)
        {
            match action {
                HandshakeAction::Write => {
                    // Local writes are deterministic from the saved ephemeral seed.
                    let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
                    context
                        .write_handshake_message(&[], &mut message)
                        .map_err(|_| PubkyNoiseError::RestoreBackupReplayError)?;
                }
                HandshakeAction::Read => {
                    let saved = saved_messages
                        .next()
                        .ok_or(PubkyNoiseError::RestoreBackupDeserializeError)?;
                    let mut message = [0; PUBKY_NOISE_CIPHERTEXT_LEN];
                    message[..saved.len()].copy_from_slice(saved);
                    let mut payload = [0; PUBKY_NOISE_MSG_LEN];

                    context
                        .read_handshake_message(&mut message, &mut payload, saved.len())
                        .map_err(|_| PubkyNoiseError::RestoreBackupReplayError)?;
                }
                HandshakeAction::Pending | HandshakeAction::Terminal => {
                    // These don't correspond to actual messages
                }
            }
        }

        if state.phase == NoisePhase::HandShake
            && state.handshake_hash != context.get_handshake_hash()
        {
            return Err(PubkyNoiseError::RestoreBackupHashMismatch);
        }

        // Now set the context state to match the saved snapshot
        let link_id = if state.phase == NoisePhase::Transport {
            // Verify the handshake completed
            if context.is_handshake() {
                return Err(PubkyNoiseError::RestoreBackupReplayError);
            }

            // Transport snapshots retain the transcript hash in their LinkId.
            let hash = context
                .get_handshake_hash()
                .ok_or(PubkyNoiseError::RestoreBackupReplayError)?;
            let saved_hash = state
                .handshake_hash
                .or(state.link_id)
                .ok_or(PubkyNoiseError::RestoreBackupHashMismatch)?;
            if saved_hash != hash || state.link_id.is_some_and(|id| id != hash) {
                return Err(PubkyNoiseError::RestoreBackupHashMismatch);
            }

            // Transition to transport
            context
                .transition_to_transport()
                .map_err(|_| PubkyNoiseError::RestoreBackupReplayError)?;

            // Set nonces from saved state
            context.set_sending_nonce(state.sending_nonce);
            context.set_receiving_nonce(state.receiving_nonce);

            // Set the base slot and distinct counters used to track the local
            // outbound slot and remote outbound slot.
            context.set_counter(state.counter);
            context.set_write_counter(state.write_counter);
            context.set_read_counter(state.read_counter);

            Some(LinkId(hash))
        } else {
            // Handshake restore: set step/sub_step/counter
            context.set_noise_step(state.noise_step);
            context.set_sub_step_index(state.sub_step_index as usize);
            context.set_counter(state.counter);
            None
        };

        Ok(PubkyNoiseEncryptor {
            config,
            context,
            link_id,
            endpoint_pubkey,
            last_good_snapshot: None,
            unacknowledged_transport_digest: None,
            pending_convenience_send: None,
            #[cfg(feature = "test-utils")]
            simulate_tampering: false,
            #[cfg(feature = "test-utils")]
            simulate_write_failure: false,
            #[cfg(feature = "test-utils")]
            last_ciphertext: None,
        })
    }

    /// Get the LinkId for this session (available after transition_transport).
    pub fn get_link_id(&self) -> Option<LinkId> {
        self.link_id
    }

    /// Returns the snapshot captured at the start of the last
    /// [`handle_handshake()`](Self::handle_handshake) call, before any
    /// state-mutating work.
    ///
    /// The snapshot always reflects the state just before the **most recent**
    /// `handle_handshake` call — each call overwrites the previous snapshot.
    /// Callers that need to preserve a specific pre-failure snapshot should
    /// clone or persist it before the next `handle_handshake` call.
    ///
    /// If a write to the local homeserver was lost during the handshake, the
    /// caller can pass the persisted snapshot to [`restore()`](Self::restore)
    /// to recover the session.
    ///
    /// Returns `None` if `handle_handshake` has never been called.
    pub fn last_good_snapshot(&self) -> Option<&PubkyNoiseSessionState> {
        self.last_good_snapshot.as_ref()
    }

    /// Test-only: enable ciphertext tampering simulation.
    #[cfg(feature = "test-utils")]
    pub fn test_enable_tampering(&mut self) {
        self.simulate_tampering = true;
    }

    /// Test-only: simulate homeserver write failures during handshake.
    ///
    /// When enabled, `handle_handshake` will skip the `put()` call and
    /// return `Err(PubkyNoiseError::HomeserverWriteError)` on Write actions.
    #[cfg(feature = "test-utils")]
    pub fn test_enable_write_failure(&mut self) {
        self.simulate_write_failure = true;
    }

    /// Test-only: disable simulated homeserver write failures.
    #[cfg(feature = "test-utils")]
    pub fn test_disable_write_failure(&mut self) {
        self.simulate_write_failure = false;
    }

    /// Test-only: get the last ciphertext produced by `prepare_send`.
    #[cfg(feature = "test-utils")]
    pub fn test_last_ciphertext(&self) -> Option<[u8; PUBKY_NOISE_TRANSPORT_PACKET_LEN]> {
        self.last_ciphertext
    }
}

impl std::fmt::Debug for PubkyNoiseConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubkyNoiseConfig")
            .field("write_path", &self.write_path)
            .field("read_path", &self.read_path)
            .field("pubky_noise_version", &self.pubky_noise_version)
            .field("default_pattern", &self.default_pattern)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snow_crypto::{HandshakePattern, NoisePhase, NoiseStep};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    async fn response_for_test(wire: Vec<u8>) -> pubky::Result<reqwest::Response> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            stream.write_all(&wire).unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .map_err(Error::from);
        server.join().unwrap();
        response
    }

    async fn http2_response_for_test() -> (
        reqwest::Response,
        h2::SendStream<bytes::Bytes>,
        tokio::task::JoinSet<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (_, mut respond) = connection.accept().await.unwrap().unwrap();
            let body = respond
                .send_response(http::Response::new(()), false)
                .unwrap();
            sender.send(body).unwrap();
            while connection.accept().await.is_some() {}
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .http2_prior_knowledge()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        (response, receiver.await.unwrap(), tasks)
    }

    #[tokio::test]
    async fn handshake_read_distinguishes_absence_from_errors() {
        for status in [404, 410, 401, 403, 405, 406, 408, 429, 500, 503] {
            let response = Err(Error::Request(RequestError::Server {
                status: StatusCode::from_u16(status).unwrap(),
                message: String::new(),
            }));
            let result = read_handshake_packet(response).await;
            if status == 404 || status == 410 {
                assert_eq!(result, Ok(None), "status {status}");
            } else {
                assert_eq!(
                    result,
                    Err(PubkyNoiseError::HomeserverHttpError { status }),
                    "status {status}"
                );
            }
        }
        let error = Error::Request(RequestError::Validation {
            message: "invalid resource".into(),
        });
        assert_eq!(
            read_handshake_packet(Err(error)).await,
            Err(PubkyNoiseError::HomeserverResponseError)
        );
        let response = response_for_test(Vec::new()).await;
        assert!(matches!(
            response,
            Err(Error::Request(RequestError::Transport(_)))
        ));
        assert_eq!(
            read_handshake_packet(response).await,
            Err(PubkyNoiseError::HomeserverResponseError)
        );
    }

    #[tokio::test]
    async fn bounded_packet_read_enforces_limit() {
        let cases = [
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
                Ok(b"abcd".to_vec()),
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n2\r\nab\r\n2\r\ncd\r\n0\r\n\r\n",
                Ok(b"abcd".to_vec()),
            ),
            (
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
                Err(PubkyNoiseError::BadLengthCiphertext),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\n",
                Err(PubkyNoiseError::BadLengthCiphertext),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nab",
                Err(PubkyNoiseError::HomeserverResponseError),
            ),
        ];
        for (wire, expected) in cases {
            let response = response_for_test(wire.as_bytes().to_vec()).await.unwrap();
            assert_eq!(read_bounded_packet(response, 4).await, expected);
        }
    }

    #[tokio::test]
    async fn bounded_packet_read_accepts_empty_http2_frames() {
        let (mut response, mut body, _tasks) = http2_response_for_test().await;
        for _ in 0..3 {
            body.send_data(bytes::Bytes::new(), false).unwrap();
        }
        body.send_data(bytes::Bytes::from_static(b"abcd"), true)
            .unwrap();
        // Verify that the transport exposes an empty frame, not end-of-stream.
        assert!(response.chunk().await.unwrap().unwrap().is_empty());
        assert_eq!(read_bounded_packet(response, 4).await.unwrap(), b"abcd");
    }

    #[tokio::test]
    async fn bounded_packet_read_obeys_total_deadline() {
        for send_empty_frames in [false, true] {
            let (mut response, mut body, mut tasks) = http2_response_for_test().await;
            tasks.spawn(async move {
                loop {
                    if send_empty_frames && body.send_data(bytes::Bytes::new(), false).is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            if send_empty_frames {
                assert!(response.chunk().await.unwrap().unwrap().is_empty());
            }
            let result =
                tokio::time::timeout(Duration::from_secs(5), read_bounded_packet(response, 4))
                    .await
                    .expect("the configured HTTP deadline must end the body read");
            assert_eq!(result, Err(PubkyNoiseError::HomeserverResponseError));
            tasks.shutdown().await;
        }
    }

    #[tokio::test]
    async fn handshake_read_validates_packet_framing() {
        let cases = [
            (
                encode_handshake_packet(b"abc", 3).to_vec(),
                Some(&b"abc"[..]),
            ),
            (vec![0, 1, 42], Some(&[42][..])),
            (vec![], None),
            (vec![0], None),
            (vec![0, 2, 42], None),
            (vec![0xff, 0xff], None),
            (vec![0; PUBKY_NOISE_CIPHERTEXT_LEN + 3], None),
        ];
        for (body, expected) in cases {
            let mut wire = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            wire.extend_from_slice(&body);
            let result = read_handshake_packet(response_for_test(wire).await).await;
            if let Some(expected) = expected {
                let (message, len) = result.unwrap().unwrap();
                assert_eq!(&message[..len], expected);
            } else {
                assert_eq!(result, Err(PubkyNoiseError::BadLengthCiphertext));
            }
        }
    }

    fn state_with_secrets() -> PubkyNoiseSessionState {
        PubkyNoiseSessionState {
            version: SESSION_STATE_VERSION,
            phase: NoisePhase::Transport,
            pattern: HandshakePattern::PatternNN,
            initiator: true,
            ephemeral_secret: [0xAA; 32],
            static_secret: Some([0xBB; 32]),
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

    #[test]
    fn loaded_snapshot_debug_redacts_secrets() {
        let loaded = LoadedSnapshot {
            generation: 7,
            state: state_with_secrets(),
        };

        let rendered = format!("{loaded:?}");

        assert!(
            !rendered.contains(format!("{:?}", [0xAA; 32]).as_str()),
            "ephemeral secret leaked in Debug: {rendered}"
        );
        assert!(
            !rendered.contains(format!("{:?}", [0xBB; 32]).as_str()),
            "static secret leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("generation"));
    }

    #[test]
    fn prepared_handshake_debug_redacts_state_and_packet() {
        let prepared = PreparedHandshakeStep {
            resulting_session_state: state_with_secrets(),
            outgoing: Some((
                "/pub/private-handshake/2".into(),
                [42; PUBKY_NOISE_CIPHERTEXT_LEN + 2],
            )),
            handshake_complete: true,
            remote_static_public_key: Some(vec![99; 32]),
        };
        let rendered = format!("{prepared:?}");
        for sensitive in [
            prepared.destination_path().unwrap().to_string(),
            format!("{:?}", prepared.packet().unwrap()),
            format!("{:?}", [0xAA; 32]),
            format!("{:?}", [0xBB; 32]),
            format!("{:?}", [99; 32]),
        ] {
            assert!(!rendered.contains(&sensitive));
        }
        assert!(rendered.contains("handshake_complete: true"));
    }

    #[test]
    fn transport_plaintext_round_trip_preserves_body_and_trailing_zeros() {
        let body = b"message with trailing zeros\0\0";
        let plaintext = encode_transport_plaintext(body).unwrap();
        let (decoded, decoded_len) = decode_transport_plaintext(&plaintext).unwrap();

        assert_eq!(&decoded[..decoded_len], body);
        assert_eq!(decoded_len, body.len());
        assert!(plaintext[body.len() + 2..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn transport_plaintext_accepts_maximum_body() {
        let body = [42; PUBKY_NOISE_MSG_LEN];
        let plaintext = encode_transport_plaintext(&body).unwrap();
        let (decoded, decoded_len) = decode_transport_plaintext(&plaintext).unwrap();

        assert_eq!(decoded, body);
        assert_eq!(decoded_len, PUBKY_NOISE_MSG_LEN);
    }

    #[test]
    fn transport_plaintext_rejects_invalid_lengths() {
        assert_eq!(
            encode_transport_plaintext(&[0; PUBKY_NOISE_MSG_LEN + 1]).unwrap_err(),
            PubkyNoiseError::EncryptionError
        );

        let mut plaintext = [0; PUBKY_NOISE_TRANSPORT_PLAINTEXT_LEN];
        plaintext[..2].copy_from_slice(&((PUBKY_NOISE_MSG_LEN + 1) as u16).to_be_bytes());
        assert_eq!(
            decode_transport_plaintext(&plaintext).unwrap_err(),
            PubkyNoiseError::BadLengthCiphertext
        );
    }

    #[test]
    fn transport_plaintext_rejects_non_zero_padding() {
        let mut plaintext = encode_transport_plaintext(b"message").unwrap();
        *plaintext.last_mut().unwrap() = 1;

        assert_eq!(
            decode_transport_plaintext(&plaintext).unwrap_err(),
            PubkyNoiseError::BadLengthCiphertext
        );
    }
}
