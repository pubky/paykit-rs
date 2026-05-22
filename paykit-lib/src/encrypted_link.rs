//! Known Peer encrypted-link lifecycle over pubky-noise.
//!
//! This Module owns the Noise handshake lifecycle, snapshot restore, private
//! path derivation, private send retry policy, and private-message inbox
//! attachment for a Known Peer. Paykit Protocol helpers in `lib.rs` keep
//! parsing/serialization for Private Payment Envelopes and Receipt Access.

use tracing::{debug, instrument, warn};

use crate::{private_message_dispatch, pubky_routing, PaykitError, PublicKey, Result};

/// Handle to an established encrypted Noise link with a peer.
///
/// Created by [`advance_handshake`] (via [`HandshakeProgress::Complete`]) after
/// a successful Noise handshake. Used by the private payment helper functions to
/// encrypt and decrypt payment data. Must be closed via [`close_encrypted_link`]
/// when no longer needed.
///
/// The link wraps a [`pubky_noise::PubkyNoiseEncryptor`] in transport mode.
///
/// # Session resumption
///
/// An established link can be snapshotted via [`snapshot`](Self::snapshot) (or
/// serialized directly via [`serialize`](Self::serialize)) and later restored
/// with [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// without re-doing the Noise handshake.
///
/// # Private message dispatch
///
/// All Paykit application messages on this Noise link share one ordered stream.
/// The link therefore buffers decrypted messages after low-level receipt and
/// lets typed helpers consume only their own message kind. This prevents future
/// helpers (for example receipt access) from losing messages simply because a
/// different typed getter was called first.
///
/// The buffer is in-memory only. If callers need crash-safe processing of
/// event-like message kinds, they must persist handled/unhandled application
/// state before dropping or serializing the link.
///
/// # Automatic send retry
///
/// [`crate::set_private_payment_envelope`] automatically retries failed `send_message` calls
/// up to [`max_send_retries`](Self::set_max_send_retries) times (default:
/// [`DEFAULT_MAX_SEND_RETRIES`]). Since transport-phase send failures do not
/// corrupt the Noise state, retries are safe without snapshot-based recovery.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    pub(crate) encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's public key.
    pub(crate) recipient: PublicKey,
    /// Shared Noise configuration retained for snapshot-based session resumption.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Maximum number of automatic `send_message` retries in
    /// [`set_private_payment_envelope`].
    pub(crate) max_send_retries: u32,
    /// Decrypted application messages that have been read from the ordered
    /// Noise stream but not yet consumed by a typed Paykit helper.
    ///
    /// This prevents a typed receiver such as [`get_private_payment_envelope`] from
    /// discarding unrelated supported message kinds (for example receipt-access
    /// messages) after the underlying Noise read counter has advanced.
    pub(crate) private_messages: private_message_dispatch::PrivateMessageInbox,
}

impl EncryptedLink {
    /// Set the maximum number of automatic `send_message` retries before
    /// [`crate::set_private_payment_envelope`] gives up and returns [`PaykitError::Transport`].
    ///
    /// Transport-phase send failures do not corrupt the Noise state, so retries
    /// are safe without snapshot-based recovery.
    ///
    /// Default: [`DEFAULT_MAX_SEND_RETRIES`] (3).
    pub fn set_max_send_retries(&mut self, max: u32) -> &mut Self {
        self.max_send_retries = max;
        self
    }

    /// Capture the current link state as a serializable snapshot.
    ///
    /// The snapshot contains everything needed to restore the session later
    /// via [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
    /// without re-doing the Noise handshake.
    ///
    /// # When to snapshot
    ///
    /// Take a snapshot after the link is established and periodically after
    /// exchanging messages (the snapshot includes nonce counters that must stay
    /// in sync). Persist serialized bytes only in encrypted durable storage.
    /// Snapshot bytes include sensitive key material and must be treated as
    /// secrets (never log or expose them in telemetry/crash reports).
    pub fn snapshot(&self) -> EncryptedLinkSnapshot {
        EncryptedLinkSnapshot {
            state: self.encryptor.snapshot(),
            recipient: self.recipient.clone(),
        }
    }

    /// Serialize the current link state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot().serialize()`.
    pub fn serialize(&self) -> Vec<u8> {
        self.snapshot().serialize()
    }

    /// Access the shared Noise configuration for this link.
    ///
    /// Useful for passing to [`restore_encrypted_link_from_config`] when
    /// performing in-process session recovery without an app restart.
    pub fn config(&self) -> &std::sync::Arc<pubky_noise::PubkyNoiseConfig> {
        &self.config
    }

    /// Access the counterparty public key for this encrypted link.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Serializable snapshot of an established [`EncryptedLink`].
///
/// Created by [`EncryptedLink::snapshot`]. Can be serialized to a compact
/// binary format via [`serialize`](Self::serialize) for durable storage, and
/// deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// to resume the session after an app restart without re-doing the Noise
/// handshake.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The remote peer's public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkSnapshot {
    /// The underlying pubky-noise session state.
    pub(crate) state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    pub(crate) recipient: PublicKey,
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
                    context: format!("failed to deserialize encrypted link snapshot: {err:?}"),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "encrypted link snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Serializable snapshot of an in-progress [`EncryptedLinkHandshake`].
///
/// Created by [`EncryptedLinkHandshake::snapshot`]. Can be serialized to a
/// compact binary format via [`serialize`](Self::serialize) for durable
/// storage, and deserialized back via [`deserialize`](Self::deserialize).
///
/// Snapshot bytes include sensitive key material and must be treated as
/// secrets (store encrypted at rest; never log or expose them).
///
/// Pass to [`restore_encrypted_link_handshake`] or
/// [`restore_encrypted_link_handshake_from_config`] to resume the handshake
/// after an app restart without starting over.
///
/// # Wire format
///
/// The serialized representation is the 197-byte
/// [`PubkyNoiseSessionState`](pubky_noise::serializer::PubkyNoiseSessionState)
/// binary format produced by `pubky-noise` 0.1.0-rc5. The remote peer's public
/// key is embedded in the snapshot (bytes 165-196) and reconstructed
/// automatically during deserialization.
pub struct EncryptedLinkHandshakeSnapshot {
    /// The underlying pubky-noise session state.
    pub(crate) state: pubky_noise::serializer::PubkyNoiseSessionState,
    /// The counterparty's public key (derived from `state.endpoint_pubkey`).
    pub(crate) recipient: PublicKey,
}

impl std::fmt::Debug for EncryptedLinkHandshakeSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedLinkHandshakeSnapshot")
            .field("recipient", &self.recipient)
            .finish_non_exhaustive()
    }
}

impl EncryptedLinkHandshakeSnapshot {
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
                        "failed to deserialize encrypted link handshake snapshot: {err:?}"
                    ),
                    source: None,
                }
            })?;

        let recipient = recipient_from_snapshot_state(&state, "encrypted link handshake snapshot")?;

        Ok(Self { state, recipient })
    }

    /// Access the counterparty's public key embedded in the snapshot.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }
}

/// Default maximum number of automatic `send_message` retries before
/// [`crate::set_private_payment_envelope`] gives up and returns an error.
///
/// Override per-link via [`EncryptedLink::set_max_send_retries`].
pub const DEFAULT_MAX_SEND_RETRIES: u32 = 3;

/// Default maximum number of consecutive automatic recovery attempts before
/// [`advance_handshake`] gives up and returns an error.
///
/// Override per-handshake via [`EncryptedLinkHandshake::set_max_recovery_attempts`].
pub const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 3;

/// Handle to an in-progress Noise handshake.
///
/// Created by [`initiate_encrypted_link`] (initiator) or
/// [`accept_encrypted_link`] (responder). Drive the handshake forward by
/// repeatedly calling [`advance_handshake`] until it returns
/// [`HandshakeProgress::Complete`].
///
/// The caller controls the polling strategy — timing between retries, timeouts,
/// back-off, etc. are all the caller's responsibility.
///
/// # Automatic recovery
///
/// If a homeserver write fails during the handshake (corrupting the internal
/// Noise state), [`advance_handshake`] automatically restores from a
/// pre-mutation snapshot and returns [`HandshakeProgress::Pending`] so the
/// caller's polling loop retries transparently. The maximum number of
/// consecutive recovery attempts is configurable via
/// [`set_max_recovery_attempts`](Self::set_max_recovery_attempts) (default:
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]).
///
/// # Session resumption
///
/// An in-progress handshake can be snapshotted via [`snapshot`](Self::snapshot)
/// (or serialized directly via [`serialize`](Self::serialize)) and later
/// restored with [`restore_encrypted_link_handshake`] or
/// [`restore_encrypted_link_handshake_from_config`].
///
/// Restored handshakes always reset recovery tuning to defaults:
/// `recovery_attempts` starts at `0` and `max_recovery_attempts` is set to
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`].
pub struct EncryptedLinkHandshake {
    /// The Noise session manager in handshake mode.
    pub(crate) encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's public key (used for homeserver path construction).
    pub(crate) remote_pubkey: PublicKey,
    /// Shared Noise configuration needed for snapshot-based recovery.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Number of consecutive recovery attempts so far.
    pub(crate) recovery_attempts: u32,
    /// Maximum consecutive recovery attempts before giving up.
    pub(crate) max_recovery_attempts: u32,
}

impl EncryptedLinkHandshake {
    /// Set the maximum number of consecutive automatic recovery attempts
    /// before [`advance_handshake`] gives up and returns
    /// [`PaykitError::Transport`].
    ///
    /// The recovery-attempt counter resets to zero after every successful
    /// handshake step.
    /// Default: [`DEFAULT_MAX_RECOVERY_ATTEMPTS`] (3).
    pub fn set_max_recovery_attempts(&mut self, max: u32) -> &mut Self {
        self.max_recovery_attempts = max;
        self
    }

    /// Capture the current handshake state as a serializable snapshot.
    ///
    /// The snapshot contains everything needed to restore and continue the
    /// handshake later via [`restore_encrypted_link_handshake`] or
    /// [`restore_encrypted_link_handshake_from_config`].
    pub fn snapshot(&self) -> EncryptedLinkHandshakeSnapshot {
        EncryptedLinkHandshakeSnapshot {
            state: self.encryptor.snapshot(),
            recipient: self.remote_pubkey.clone(),
        }
    }

    /// Serialize the current handshake state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot().serialize()`.
    pub fn serialize(&self) -> Vec<u8> {
        self.snapshot().serialize()
    }

    /// Access the shared Noise configuration for this handshake.
    ///
    /// Useful for passing to [`restore_encrypted_link_handshake_from_config`]
    /// when performing in-process recovery without an app restart.
    pub fn config(&self) -> &std::sync::Arc<pubky_noise::PubkyNoiseConfig> {
        &self.config
    }
}

/// Result of a single [`advance_handshake`] step.
pub enum HandshakeProgress {
    /// Handshake is still in progress. The peer may not have written their next
    /// message yet. Pass the returned handle back to [`advance_handshake`] after
    /// a caller-chosen delay.
    Pending(EncryptedLinkHandshake),

    /// Handshake completed successfully. The [`EncryptedLink`] is ready for use
    /// with [`crate::set_private_payment_envelope`] and [`crate::get_private_payment_envelope`].
    Complete(EncryptedLink),
}

/// Domain separation string for Paykit private payment path derivation.
///
/// Ensures that different applications using the same key pairs derive
/// different storage paths, preventing cross-protocol path collisions.
const PAYKIT_PATH_DOMAIN: &[u8] = b"paykit-path-v0";

/// Computes the write and read path components for private payment storage.
///
/// Uses [`pubky_noise::path_derivation::derive_asymmetric_paths`] to derive
/// per-peer-pair paths from a DH shared secret. The derivation formula is:
///
/// ```text
/// dh_secret  = X25519(to_scalar_bytes(local_ed25519_seed), to_montgomery(remote_ed25519_pk))
/// write_path = "{base}/{hex(SHA-256(domain || dh_secret || local_pk))}"
/// read_path  = "{base}/{hex(SHA-256(domain || dh_secret || remote_pk))}"
/// ```
///
/// # Returns
///
/// A tuple `(write_path, read_path)` where:
/// - `write_path` — the full path the local party writes to on their own homeserver.
/// - `read_path` — the full path the local party reads from on the remote homeserver.
///
/// # Correctness
///
/// For parties Alice and Bob:
/// - `compute_private_paths(alice_sk, bob_pk).0 == compute_private_paths(bob_sk, alice_pk).1`
/// - `compute_private_paths(alice_sk, bob_pk).1 == compute_private_paths(bob_sk, alice_pk).0`
fn compute_private_payment_paths(
    local_secret_key: &[u8; 32],
    remote_pubkey: &PublicKey,
) -> (String, String) {
    pubky_noise::path_derivation::derive_asymmetric_paths(
        local_secret_key,
        remote_pubkey,
        PAYKIT_PATH_DOMAIN,
        pubky_routing::paths::PAYKIT_PRIVATE_PATH_PREFIX,
    )
}

pub(crate) fn send_attempts_from_retries(max_send_retries: u32) -> u32 {
    max_send_retries.saturating_add(1)
}

pub(crate) fn is_retryable_private_send_error(err: &pubky_noise::PubkyNoiseError) -> bool {
    matches!(err, pubky_noise::PubkyNoiseError::HomeserverWriteError)
}

fn record_handshake_write_failure(
    recovery_attempts: &mut u32,
    max_recovery_attempts: u32,
) -> Result<()> {
    *recovery_attempts += 1;
    if *recovery_attempts > max_recovery_attempts {
        return Err(PaykitError::Transport {
            context: format!(
                "handshake recovery exhausted after {max_recovery_attempts} consecutive attempts",
            ),
            source: anyhow::anyhow!(
                "HomeserverWriteError persisted beyond recovery limit ({max_recovery_attempts})",
            ),
        });
    }
    Ok(())
}

fn recovery_snapshot_or_error<T>(snapshot: Option<T>) -> Result<T> {
    snapshot.ok_or_else(|| PaykitError::Transport {
        context: "handshake recovery failed: missing last-good snapshot".into(),
        source: anyhow::anyhow!(
            "pubky-noise returned HomeserverWriteError but no recovery snapshot"
        ),
    })
}

async fn send_private_message_with<S, F>(
    max_send_retries: u32,
    plaintext: &[u8],
    context: &'static str,
    sender: &mut S,
    mut send: F,
) -> Result<()>
where
    F: for<'a> FnMut(
        &'a mut S,
        &'a [u8],
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = std::result::Result<(), pubky_noise::PubkyNoiseError>>
                + Send
                + 'a,
        >,
    >,
{
    if plaintext.len() > pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN {
        return Err(PaykitError::Validation(format!(
            "{context} payload ({} bytes) exceeds max message size ({} bytes)",
            plaintext.len(),
            pubky_noise::snow_crypto::PUBKY_NOISE_MSG_LEN,
        )));
    }

    let max_attempts = send_attempts_from_retries(max_send_retries);
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        match send(sender, plaintext).await {
            Ok(()) => {
                debug!(context, "private message sent successfully");
                return Ok(());
            }
            Err(err) if is_retryable_private_send_error(&err) => {
                last_error = Some(format!("{err:?}"));
                if attempt < max_attempts {
                    warn!(
                        attempt,
                        max_retries = max_send_retries,
                        error = ?err,
                        context,
                        "send_message failed, retrying"
                    );
                }
            }
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: format!("failed to send {context}: {err:?}"),
                    source: anyhow::anyhow!(
                        "pubky-noise send_message failed with non-retryable error: {err:?}"
                    ),
                });
            }
        }
    }

    Err(PaykitError::Transport {
        context: format!("failed to send {context} after {max_attempts} attempts"),
        source: anyhow::anyhow!(
            "pubky-noise send_message failed on all {} attempts; last error: {}",
            max_attempts,
            last_error.unwrap_or_else(|| "unknown error".to_string())
        ),
    })
}

pub(crate) async fn send_private_message(
    link: &mut EncryptedLink,
    plaintext: &[u8],
    context: &'static str,
) -> Result<()> {
    send_private_message_with(
        link.max_send_retries,
        plaintext,
        context,
        &mut link.encryptor,
        |encryptor, plaintext| Box::pin(encryptor.send_message(plaintext)),
    )
    .await
}

/// Initiates a Noise XX handshake with a remote peer (initiator role).
///
/// Initializes the encryption stack and creates a handshake context. The actual
/// handshake messages are exchanged by repeatedly calling [`advance_handshake`]
/// until it returns [`HandshakeProgress::Complete`].
///
/// Ephemeral keys are managed internally by the Noise stack — callers only need
/// to provide their static identity key and the remote peer's public key.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `sender_secret_key` — 32-byte Ed25519 secret key of the local peer.
/// - `receiver_pubkey` — public key of the remote peer.
/// - `outbox_client` — HTTP client for reading from the remote homeserver
///   (consumed; caller should `.clone()` if needed elsewhere).
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the encryption stack cannot be
/// initialized or if the context creation fails.
#[instrument(skip(session, sender_secret_key, outbox_client))]
pub fn initiate_encrypted_link(
    session: pubky::PubkySession,
    sender_secret_key: [u8; 32],
    receiver_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing encrypted link handshake (initiator)");

    let (write_path, read_path) =
        compute_private_payment_paths(&sender_secret_key, receiver_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        sender_secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
        config.clone(),
        sender_secret_key,
        true,
        receiver_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (initiator)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: receiver_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Accepts a Noise XX handshake from a remote peer (responder role).
///
/// Initializes the encryption stack and creates a handshake context for the
/// responder side. The actual handshake messages are exchanged by repeatedly
/// calling [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (consumed; caller should `.clone()` if needed elsewhere).
/// - `receiver_secret_key` — 32-byte Ed25519 secret key of the local peer.
/// - `sender_pubkey` — public key of the remote peer (the initiator).
/// - `outbox_client` — HTTP client for reading from the remote homeserver
///   (consumed; caller should `.clone()` if needed elsewhere).
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the encryption stack cannot be
/// initialized or if the context creation fails.
#[instrument(skip(session, receiver_secret_key, outbox_client))]
pub fn accept_encrypted_link(
    session: pubky::PubkySession,
    receiver_secret_key: [u8; 32],
    sender_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing encrypted link handshake (responder)");

    let (write_path, read_path) =
        compute_private_payment_paths(&receiver_secret_key, sender_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        receiver_secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    let encryptor = pubky_noise::PubkyNoiseEncryptor::new(
        config.clone(),
        receiver_secret_key,
        false,
        sender_pubkey.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (responder)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: sender_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Advances the handshake by one step.
///
/// This function is **polling-safe**: calling it when the remote peer has not
/// written their next message yet returns [`HandshakeProgress::Pending`] without
/// corrupting internal state. The caller can safely retry after a delay.
///
/// # Automatic recovery
///
/// If the homeserver write fails during a handshake step
/// (`HomeserverWriteError`), the internal Noise state is irreversibly
/// corrupted. This function automatically recovers by restoring from the
/// pre-mutation snapshot captured at the start of the failed step and returns
/// [`HandshakeProgress::Pending`] so the caller's polling loop retries
/// transparently.
///
/// The maximum number of **consecutive** recovery attempts is configurable via
/// [`EncryptedLinkHandshake::set_max_recovery_attempts`] (default:
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`]). The recovery-attempt counter resets to
/// zero after every successful step. If the limit is exceeded, the function returns
/// [`PaykitError::Transport`].
///
/// # Polling strategy
///
/// The caller controls the polling strategy. Common patterns:
///
/// **Fixed interval:**
/// ```ignore
/// loop {
///     match advance_handshake(handshake).await? {
///         HandshakeProgress::Pending(h) => {
///             handshake = h;
///             tokio::time::sleep(Duration::from_millis(100)).await;
///         }
///         HandshakeProgress::Complete(link) => break link,
///     }
/// }
/// ```
///
/// **With timeout:**
/// ```ignore
/// let deadline = Instant::now() + Duration::from_secs(60);
/// loop {
///     if Instant::now() > deadline {
///         return Err(/* timeout */);
///     }
///     match advance_handshake(handshake).await? {
///         HandshakeProgress::Pending(h) => {
///             handshake = h;
///             tokio::time::sleep(Duration::from_millis(100)).await;
///         }
///         HandshakeProgress::Complete(link) => break link,
///     }
/// }
/// ```
///
/// # Parameters
/// - `handshake` — the in-progress handshake handle (consumed; returned inside
///   [`HandshakeProgress::Pending`] if the handshake is not yet finished).
///
/// # Errors
/// - Returns [`PaykitError::Transport`] if the handshake processing fails, if
///   the context is in an invalid state, or if automatic recovery is exhausted.
#[instrument(skip(handshake))]
pub async fn advance_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    // Check whether the handshake has already finished.
    if handshake.encryptor.is_handshake_complete() {
        return finish_handshake(handshake);
    }

    // Process the next handshake step.
    match handshake.encryptor.handle_handshake().await {
        Ok(pubky_noise::HandshakeResult::Pending) => {
            debug!("handshake step pending (waiting for peer)");
            handshake.recovery_attempts = 0;
            Ok(HandshakeProgress::Pending(handshake))
        }
        Ok(pubky_noise::HandshakeResult::Terminal) => {
            debug!("handshake terminal, transitioning to transport");
            finish_handshake(handshake)
        }
        Err(pubky_noise::PubkyNoiseError::HomeserverWriteError) => {
            record_handshake_write_failure(
                &mut handshake.recovery_attempts,
                handshake.max_recovery_attempts,
            )?;

            warn!(
                attempts = handshake.recovery_attempts,
                max = handshake.max_recovery_attempts,
                "handshake write failed, attempting automatic recovery from snapshot"
            );

            let snapshot =
                recovery_snapshot_or_error(handshake.encryptor.last_good_snapshot().cloned())?;

            let restored = pubky_noise::PubkyNoiseEncryptor::restore(
                handshake.config.clone(),
                snapshot,
                handshake.remote_pubkey.clone(),
            )
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("handshake recovery via restore() failed: {err:?}"),
                source: anyhow::anyhow!("restore after HomeserverWriteError failed: {err:?}"),
            })?;

            debug!("handshake recovered successfully, returning Pending");
            Ok(HandshakeProgress::Pending(EncryptedLinkHandshake {
                encryptor: restored,
                config: handshake.config,
                remote_pubkey: handshake.remote_pubkey,
                recovery_attempts: handshake.recovery_attempts,
                max_recovery_attempts: handshake.max_recovery_attempts,
            }))
        }
        Err(err) => Err(PaykitError::Transport {
            context: format!("handshake step failed: {err:?}"),
            source: anyhow::anyhow!("pubky-noise handle_handshake failed: {err:?}"),
        }),
    }
}

/// Transitions a completed handshake into an [`EncryptedLink`].
fn finish_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    let _link_id =
        handshake
            .encryptor
            .transition_transport()
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to transition to transport mode: {err:?}"),
                source: anyhow::anyhow!("pubky-noise transition_transport failed: {err:?}"),
            })?;

    debug!("encrypted link established");
    Ok(HandshakeProgress::Complete(EncryptedLink {
        encryptor: handshake.encryptor,
        recipient: handshake.remote_pubkey,
        config: handshake.config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
        private_messages: private_message_dispatch::PrivateMessageInbox::new(),
    }))
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot.
///
/// Use this to resume an in-progress handshake after an app restart. A fresh
/// [`pubky_noise::PubkyNoiseConfig`] is built from the supplied session and key
/// material, then replay restore reconstructs the handshake state from the
/// persisted snapshot and homeserver data.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing handshake messages
///   (a fresh session after app restart).
/// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
///   used in the original [`initiate_encrypted_link`] or
///   [`accept_encrypted_link`] call).
/// - `remote_pubkey` — public key of the remote peer.
/// - `outbox_client` — HTTP client for reading from the remote homeserver.
/// - `snapshot` — saved in-progress handshake snapshot (from
///   [`EncryptedLinkHandshake::snapshot`] or
///   [`EncryptedLinkHandshakeSnapshot::deserialize`]).
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored handshakes always reset recovery tuning to defaults:
/// - `recovery_attempts = 0`
/// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
/// created or if the underlying `restore()` fails. Returns
/// [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`, or when the snapshot is not in handshake
/// phase.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link_handshake(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring encrypted link handshake from snapshot (raw params)");

    let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config for handshake restore: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot
/// using an existing Noise configuration.
///
/// This is the in-process variant of [`restore_encrypted_link_handshake`] — use
/// it when the original `Arc<PubkyNoiseConfig>` is still available.
///
/// # Parameters
/// - `config` — shared Noise configuration matching the original handshake
///   session.
/// - `remote_pubkey` — public key of the remote peer.
/// - `snapshot` — saved in-progress handshake snapshot.
///
/// # Restore behavior
///
/// Restored handshakes always reset recovery tuning to defaults:
/// - `recovery_attempts = 0`
/// - `max_recovery_attempts = DEFAULT_MAX_RECOVERY_ATTEMPTS`
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
/// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`, or when the snapshot is not in handshake
/// phase.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_handshake_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring encrypted link handshake from snapshot (existing config)");
    restore_encrypted_link_handshake_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both handshake restore variants.
async fn restore_encrypted_link_handshake_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    if snapshot.recipient() != remote_pubkey {
        return Err(PaykitError::Validation(format!(
            "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
            remote_pubkey,
            snapshot.recipient(),
        )));
    }

    if !matches!(
        snapshot.state.phase,
        pubky_noise::snow_crypto::NoisePhase::HandShake
    ) {
        return Err(PaykitError::Validation(format!(
            "handshake restore requires handshake-phase snapshot, got {:?}",
            snapshot.state.phase,
        )));
    }

    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        snapshot.state,
        remote_pubkey.clone(),
    )
    .await
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to restore encrypted link handshake: {err:?}"),
        source: anyhow::anyhow!("pubky-noise handshake restore failed: {err:?}"),
    })?;

    debug!("encrypted link handshake restored successfully (recovery tuning reset to defaults)");

    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: remote_pubkey.clone(),
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Closes an encrypted link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing encrypted link");
    link.encryptor.close();
    debug!("encrypted link closed successfully");
    Ok(())
}

/// Restores an [`EncryptedLink`] from a previously saved snapshot.
///
/// Use this to resume an encrypted session after an app restart without
/// re-doing the Noise handshake. The restore mechanism replays all handshake
/// messages from the homeservers through a fresh Noise state built with the
/// same ephemeral key material, then transitions to transport mode and sets
/// the nonces and transport slot counters from the saved state.
///
/// # Parameters
/// - `session` — authenticated Pubky session for writing messages
///   (a fresh session after app restart).
/// - `secret_key` — 32-byte Ed25519 secret key of the local peer (same key
///   used in the original [`initiate_encrypted_link`] or
///   [`accept_encrypted_link`] call).
/// - `remote_pubkey` — public key of the remote peer.
/// - `outbox_client` — HTTP client for reading from the remote homeserver.
/// - `snapshot` — the saved snapshot (from [`EncryptedLink::snapshot`] or
///   [`EncryptedLinkSnapshot::deserialize`]).
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// Call [`EncryptedLink::set_max_send_retries`] after restore if you need a
/// non-default value.
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the Noise configuration cannot be
/// created or if the underlying `restore()` fails (e.g. handshake messages
/// are no longer available on the homeservers, or the replayed handshake
/// hash does not match the saved one). Returns [`PaykitError::Validation`]
/// when `remote_pubkey` does not match the recipient embedded in `snapshot`.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring encrypted link from snapshot (raw params)");

    let (write_path, read_path) = compute_private_payment_paths(&secret_key, remote_pubkey);

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        secret_key,
        0,
        "XX",
        session,
        write_path,
        read_path,
        outbox_client,
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to create encryptor config for restore: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseConfig::new failed: {err:?}"),
    })?;

    restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
}

/// Restores an [`EncryptedLink`] from a previously saved snapshot using an
/// existing Noise configuration.
///
/// This is the in-process variant of [`restore_encrypted_link`] — use it when
/// the original `Arc<PubkyNoiseConfig>` is still available (e.g. the link
/// needs rebuilding without an app restart). For cross-restart recovery, use
/// [`restore_encrypted_link`] instead.
///
/// # Parameters
/// - `config` — the shared Noise configuration (must match the original
///   session's write/read paths and keypair).
/// - `remote_pubkey` — public key of the remote peer.
/// - `snapshot` — the saved snapshot.
///
/// The `remote_pubkey` must match `snapshot.recipient()`. A mismatch indicates
/// inconsistent caller input and is rejected.
///
/// # Restore behavior
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// Call [`EncryptedLink::set_max_send_retries`] after restore if you need a
/// non-default value.
///
/// # Errors
/// Returns [`PaykitError::Transport`] if the underlying `restore()` fails.
/// Returns [`PaykitError::Validation`] when `remote_pubkey` does not match the
/// recipient embedded in `snapshot`.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring encrypted link from snapshot (existing config)");
    restore_encrypted_link_inner(config, remote_pubkey, snapshot).await
}

/// Shared implementation for both restore variants.
async fn restore_encrypted_link_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    if snapshot.recipient() != remote_pubkey {
        return Err(PaykitError::Validation(format!(
            "remote_pubkey does not match snapshot recipient (remote={}, snapshot={})",
            remote_pubkey,
            snapshot.recipient(),
        )));
    }

    if !matches!(
        snapshot.state.phase,
        pubky_noise::snow_crypto::NoisePhase::Transport
    ) {
        return Err(PaykitError::Validation(format!(
            "encrypted link restore requires transport-phase snapshot, got {:?}",
            snapshot.state.phase,
        )));
    }

    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        snapshot.state,
        remote_pubkey.clone(),
    )
    .await
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to restore encrypted link: {err:?}"),
        source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
    })?;

    debug!("encrypted link restored successfully");
    Ok(EncryptedLink {
        encryptor,
        recipient: remote_pubkey.clone(),
        config,
        max_send_retries: DEFAULT_MAX_SEND_RETRIES,
        private_messages: private_message_dispatch::PrivateMessageInbox::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScriptedSender {
        attempts: u32,
        succeed_after: Option<u32>,
    }

    #[tokio::test]
    async fn test_send_private_message_with_retries_then_succeeds() {
        let mut sender = ScriptedSender {
            attempts: 0,
            succeed_after: Some(3),
        };

        send_private_message_with(
            2,
            b"hello",
            "test message",
            &mut sender,
            |sender, _plaintext| {
                sender.attempts += 1;
                let result = if Some(sender.attempts) == sender.succeed_after {
                    Ok(())
                } else {
                    Err(pubky_noise::PubkyNoiseError::HomeserverWriteError)
                };
                Box::pin(async move { result })
            },
        )
        .await
        .unwrap();

        assert_eq!(sender.attempts, 3);
    }

    #[tokio::test]
    async fn test_send_private_message_with_exhausts_retries() {
        let mut sender = ScriptedSender {
            attempts: 0,
            succeed_after: None,
        };

        let err = send_private_message_with(
            1,
            b"hello",
            "test message",
            &mut sender,
            |sender, _plaintext| {
                sender.attempts += 1;
                Box::pin(async { Err(pubky_noise::PubkyNoiseError::HomeserverWriteError) })
            },
        )
        .await
        .unwrap_err();

        assert_eq!(sender.attempts, 2);
        assert!(
            matches!(err, PaykitError::Transport { ref context, .. } if context.contains("after 2 attempts"))
        );
    }

    #[test]
    fn test_record_handshake_write_failure_exhausts_recovery_attempts() {
        let mut attempts = 1u32;

        let err = record_handshake_write_failure(&mut attempts, 1).unwrap_err();

        assert!(
            matches!(err, PaykitError::Transport { ref context, .. } if context.contains("recovery exhausted"))
        );
    }

    #[test]
    fn test_recovery_snapshot_or_error_rejects_missing_snapshot() {
        let err: PaykitError = recovery_snapshot_or_error::<()>(None).unwrap_err();

        assert!(
            matches!(err, PaykitError::Transport { ref context, .. } if context.contains("missing last-good snapshot"))
        );
    }
}
