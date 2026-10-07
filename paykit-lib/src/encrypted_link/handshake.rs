use std::fmt;

use tracing::{debug, instrument, warn};

use crate::{PaykitError, PublicKey, Result};

use super::{
    link::EncryptedLink,
    paths::{compute_private_payment_paths, validate_private_payment_paths},
    snapshot::EncryptedLinkHandshakeSnapshot,
    EncryptedLinkRecoveryContext,
};

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
/// The caller owns polling, timeouts, and backoff. Homeserver write failures are
/// automatically recovered up to
/// [`DEFAULT_MAX_RECOVERY_ATTEMPTS`] unless overridden.
pub struct EncryptedLinkHandshake {
    /// The Noise session manager in handshake mode.
    encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's Pubky identity key, used for homeserver reads.
    remote_identity_public_key: PublicKey,
    /// The counterparty's identity-wide Noise key, used for pairwise paths.
    remote_noise_public_key: PublicKey,
    recovery_context: EncryptedLinkRecoveryContext,
    /// Shared Noise configuration needed for snapshot-based recovery.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Number of consecutive recovery attempts so far.
    recovery_attempts: u32,
    /// Maximum consecutive recovery attempts before giving up.
    max_recovery_attempts: u32,
}

/// A failed handshake advance, optionally retaining unchanged read-retry state.
///
/// Only a transport read failure preserves the handle. This does not mean the
/// failure is transient: retry policy, deadlines, and session authorization
/// remain the caller's responsibility. Other failures do not expose potentially
/// mutated handshake state. Debug output never includes the retained handle.
pub struct HandshakeAdvanceError {
    error: PaykitError,
    retry_handshake: Option<Box<EncryptedLinkHandshake>>,
}

impl HandshakeAdvanceError {
    /// Inspect the failure without consuming its optional retry handle.
    pub fn error(&self) -> &PaykitError {
        &self.error
    }

    /// Take the failure and, only for an unchanged transport read, its retry handle.
    /// Pass `*handshake` back to [`advance_handshake`] after deciding to retry.
    pub fn into_parts(self) -> (PaykitError, Option<Box<EncryptedLinkHandshake>>) {
        (self.error, self.retry_handshake)
    }
}

impl From<PaykitError> for HandshakeAdvanceError {
    fn from(error: PaykitError) -> Self {
        Self {
            error,
            retry_handshake: None,
        }
    }
}

impl fmt::Debug for HandshakeAdvanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandshakeAdvanceError")
            .field("error", &self.error)
            .field("has_retry_handshake", &self.retry_handshake.is_some())
            .finish()
    }
}

impl fmt::Display for HandshakeAdvanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for HandshakeAdvanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// One locally prepared handshake step, including its durable pending output.
/// Persist the snapshot before publication; session authorization, capability
/// scope, and key rotation remain the caller's responsibility.
#[must_use]
pub struct PreparedEncryptedLinkHandshakeStep {
    snapshot: EncryptedLinkHandshakeSnapshot,
    handshake_complete: bool,
    remote_static_public_key: Option<Vec<u8>>,
}

impl PreparedEncryptedLinkHandshakeStep {
    /// Whether cryptography completed, not whether publication is acknowledged.
    pub fn is_handshake_complete(&self) -> bool {
        self.handshake_complete
    }

    /// Authenticated remote static key, to compare with signed authorization.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        self.remote_static_public_key.as_deref()
    }

    /// Consume the preparation into the checkpoint that must be persisted.
    pub fn into_snapshot(self) -> EncryptedLinkHandshakeSnapshot {
        self.snapshot
    }
}

impl EncryptedLinkHandshake {
    /// Fetch at most one bounded packet and prepare one existing Noise step.
    /// Returns `None` only for an absent required packet (404/410). No publication
    /// occurs. The caller must validate the source checkpoint before applying
    /// either the result or an error, and persist pending output before sending.
    /// Session authorization, capability scope, and rotation are caller-owned.
    pub async fn prepare_next_step(self) -> Result<Option<PreparedEncryptedLinkHandshakeStep>> {
        let snapshot = self.snapshot()?;
        let input = match snapshot.next_handshake_read_path(
            self.config.local_session.info().public_key(),
            &self.config.pubky_root_keypair.secret_key(),
        )? {
            Some(path) => {
                let response = match self.config.outbox_client.public_storage().get(path).await {
                    Ok(response) => response,
                    Err(pubky::Error::Request(pubky::errors::RequestError::Server {
                        status,
                        ..
                    })) if status == pubky::StatusCode::NOT_FOUND
                        || status == pubky::StatusCode::GONE =>
                    {
                        return Ok(None)
                    }
                    Err(error) => {
                        return Err(PaykitError::Transport {
                            context: "fetch Encrypted Link Handshake packet".into(),
                            source: error.into(),
                        })
                    }
                };
                Some(
                    crate::pubky_routing::read_bounded_body(
                        response,
                        pubky_noise::snow_crypto::PUBKY_NOISE_CIPHERTEXT_LEN + 2,
                        "read Encrypted Link Handshake packet",
                    )
                    .await?,
                )
            }
            None => None,
        };
        let prepared = self
            .encryptor
            .prepare_handshake_step(input.as_deref())
            .map_err(|error| handshake_error("prepare Encrypted Link Handshake", error))?;
        let mut snapshot = EncryptedLinkHandshakeSnapshot::from_state(
            prepared.resulting_session_state().clone(),
            self.remote_identity_public_key,
            self.remote_noise_public_key,
            self.recovery_context,
        );
        if let (Some(path), Some(packet)) = (prepared.destination_path(), prepared.packet()) {
            snapshot = snapshot.with_pending_publication(
                self.config.local_session.info().public_key(),
                path.to_owned(),
                packet.to_vec(),
            )?;
        }
        Ok(Some(PreparedEncryptedLinkHandshakeStep {
            snapshot,
            handshake_complete: prepared.is_handshake_complete(),
            remote_static_public_key: prepared.remote_static_public_key().map(<[u8]>::to_vec),
        }))
    }

    /// Complete a restored handshake without any reads or writes.
    /// An incomplete handshake is returned unchanged. Restore only an
    /// acknowledged checkpoint; authorization and key rotation are caller-owned.
    pub fn finish_if_complete(self) -> Result<HandshakeProgress> {
        if self.encryptor.is_handshake_complete() {
            finish_handshake(self)
        } else {
            Ok(HandshakeProgress::Pending(self))
        }
    }

    /// X25519 static key authenticated by the completed Noise handshake.
    /// Returns `None` while the handshake is incomplete.
    /// The caller must authenticate this key before trusting the completed link.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        self.encryptor.remote_static_public_key()
    }

    /// Set the maximum number of consecutive automatic recovery attempts
    /// before [`advance_handshake`] gives up and returns
    /// an error whose underlying cause is [`PaykitError::Transport`].
    ///
    /// Default: [`DEFAULT_MAX_RECOVERY_ATTEMPTS`] (3).
    pub fn set_max_recovery_attempts(&mut self, max: u32) -> &mut Self {
        self.max_recovery_attempts = max;
        self
    }

    /// Capture the current handshake state as a serializable snapshot.
    ///
    /// Snapshot bytes include sensitive key material and must be stored as
    /// secrets.
    pub fn snapshot(&self) -> Result<EncryptedLinkHandshakeSnapshot> {
        Ok(EncryptedLinkHandshakeSnapshot::from_state(
            self.encryptor
                .snapshot()
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("capture Encrypted Link Handshake snapshot: {err:?}"),
                    source: None,
                })?,
            self.remote_identity_public_key.clone(),
            self.remote_noise_public_key.clone(),
            self.recovery_context.clone(),
        ))
    }

    /// Serialize the current handshake state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot()?.serialize()`.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        Ok(self.snapshot()?.serialize())
    }

    /// Access the shared Noise configuration for this handshake.
    ///
    /// Useful for passing to [`restore_encrypted_link_handshake_from_config`]
    /// when performing in-process recovery without an app restart.
    pub fn config(&self) -> &std::sync::Arc<pubky_noise::PubkyNoiseConfig> {
        &self.config
    }

    #[cfg(test)]
    pub(crate) fn recovery_attempts_for_test(&self) -> u32 {
        self.recovery_attempts
    }

    #[cfg(test)]
    pub(crate) fn max_recovery_attempts_for_test(&self) -> u32 {
        self.max_recovery_attempts
    }
}

/// Result of a single [`advance_handshake`] step.
pub enum HandshakeProgress {
    /// Handshake is still in progress. The counterparty may not have written
    /// their next message yet. Pass the returned handle back to
    /// [`advance_handshake`] after a caller-chosen delay.
    Pending(EncryptedLinkHandshake),

    /// Handshake completed successfully. The [`EncryptedLink`] is ready to send
    /// and receive Private Application Messages.
    Complete(EncryptedLink),
}

/// Initiates a Noise XX Encrypted Link Handshake with a counterparty
/// (initiator role).
///
/// `sender_secret_key` is the local Noise secret, not the Pubky identity secret.
/// `receiver_identity_public_key` selects the counterparty's homeserver, while
/// `receiver_noise_public_key` is its authorized Ed25519 routing key.
/// Authenticate [`EncryptedLink::remote_static_public_key`] against the signed
/// X25519 key before using the completed link; routing keys alone do not authenticate it.
/// `recovery_context` selects the current pair of recovery attempts; both peers
/// must use the same IDs in opposite local/remote order.
/// Session creation, capability scope, and key rotation remain the caller's responsibility.
///
/// Call [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
#[instrument(skip(session, sender_secret_key, outbox_client))]
pub fn initiate_encrypted_link(
    session: pubky::PubkySession,
    sender_secret_key: [u8; 32],
    receiver_identity_public_key: &PublicKey,
    receiver_noise_public_key: &PublicKey,
    recovery_context: EncryptedLinkRecoveryContext,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (initiator)");

    let (write_path, read_path) = compute_private_payment_paths(
        &sender_secret_key,
        session.info().public_key(),
        receiver_identity_public_key,
        receiver_noise_public_key,
        &recovery_context,
    );

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
        receiver_identity_public_key.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (initiator)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_identity_public_key: receiver_identity_public_key.clone(),
        remote_noise_public_key: receiver_noise_public_key.clone(),
        recovery_context,
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Accepts a Noise XX Encrypted Link Handshake from a counterparty
/// (responder role).
///
/// `receiver_secret_key` is the local Noise secret, not the Pubky identity secret.
/// `sender_identity_public_key` selects the counterparty's homeserver, while
/// `sender_noise_public_key` is its authorized Ed25519 routing key.
/// Authenticate [`EncryptedLink::remote_static_public_key`] against the signed
/// X25519 key before using the completed link; routing keys alone do not authenticate it.
/// `recovery_context` must mirror the initiator's current recovery attempt IDs.
/// Session creation, capability scope, and key rotation remain the caller's responsibility.
///
/// Call [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
#[instrument(skip(session, receiver_secret_key, outbox_client))]
pub fn accept_encrypted_link(
    session: pubky::PubkySession,
    receiver_secret_key: [u8; 32],
    sender_identity_public_key: &PublicKey,
    sender_noise_public_key: &PublicKey,
    recovery_context: EncryptedLinkRecoveryContext,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (responder)");

    let (write_path, read_path) = compute_private_payment_paths(
        &receiver_secret_key,
        session.info().public_key(),
        sender_identity_public_key,
        sender_noise_public_key,
        &recovery_context,
    );

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
        sender_identity_public_key.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (responder)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_identity_public_key: sender_identity_public_key.clone(),
        remote_noise_public_key: sender_noise_public_key.clone(),
        recovery_context,
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Advances the handshake by one step.
///
/// This is polling-safe: calling it when the counterparty has not
/// written their next message yet returns [`HandshakeProgress::Pending`] without
/// corrupting internal state. Homeserver write failures are automatically
/// recovered from the pre-mutation snapshot until the recovery limit is reached.
/// Transport read failures return an error retaining the unchanged handle via
/// [`HandshakeAdvanceError::into_parts`]; they are not successful Pending steps.
/// Session creation, capability scope, key rotation, deadlines, and retry timing
/// remain the caller's responsibility.
#[instrument(skip(handshake))]
pub async fn advance_handshake(
    mut handshake: EncryptedLinkHandshake,
) -> std::result::Result<HandshakeProgress, HandshakeAdvanceError> {
    // Check whether the handshake has already finished.
    if handshake.encryptor.is_handshake_complete() {
        return finish_handshake(handshake).map_err(Into::into);
    }

    // Process the next handshake step.
    match handshake.encryptor.handle_handshake().await {
        Ok(pubky_noise::HandshakeResult::Pending)
            if handshake.encryptor.is_handshake_complete() =>
        {
            finish_handshake(handshake).map_err(Into::into)
        }
        Ok(pubky_noise::HandshakeResult::Pending) => {
            debug!("handshake step pending (waiting for counterparty)");
            handshake.recovery_attempts = 0;
            Ok(HandshakeProgress::Pending(handshake))
        }
        Ok(pubky_noise::HandshakeResult::Terminal) => {
            debug!("handshake terminal, transitioning to transport");
            finish_handshake(handshake).map_err(Into::into)
        }
        Err(pubky_noise::PubkyNoiseError::HomeserverResponseError) => Err(HandshakeAdvanceError {
            error: handshake_error(
                "handshake step failed",
                pubky_noise::PubkyNoiseError::HomeserverResponseError,
            ),
            retry_handshake: Some(Box::new(handshake)),
        }),
        Err(pubky_noise::PubkyNoiseError::HomeserverWriteError) => {
            handshake.recovery_attempts += 1;

            if handshake.recovery_attempts > handshake.max_recovery_attempts {
                return Err(PaykitError::Transport {
                    context: format!(
                        "handshake recovery exhausted after {} consecutive attempts",
                        handshake.max_recovery_attempts,
                    ),
                    source: anyhow::anyhow!(
                        "HomeserverWriteError persisted beyond recovery limit ({})",
                        handshake.max_recovery_attempts,
                    ),
                }
                .into());
            }

            warn!(
                attempts = handshake.recovery_attempts,
                max = handshake.max_recovery_attempts,
                "handshake write failed, attempting automatic recovery from snapshot"
            );

            let snapshot = handshake
                .encryptor
                .last_good_snapshot()
                .cloned()
                .ok_or_else(|| PaykitError::Transport {
                    context: "handshake recovery failed: missing last-good snapshot".into(),
                    source: anyhow::anyhow!(
                        "pubky-noise returned HomeserverWriteError but no recovery snapshot"
                    ),
                })?;

            let restored = pubky_noise::PubkyNoiseEncryptor::restore(
                handshake.config.clone(),
                snapshot,
                handshake.remote_identity_public_key.clone(),
            )
            .await
            .map_err(|err| handshake_error("handshake recovery via restore() failed", err))?;

            debug!("handshake recovered successfully, returning Pending");
            Ok(HandshakeProgress::Pending(EncryptedLinkHandshake {
                encryptor: restored,
                config: handshake.config,
                remote_identity_public_key: handshake.remote_identity_public_key,
                remote_noise_public_key: handshake.remote_noise_public_key,
                recovery_context: handshake.recovery_context,
                recovery_attempts: handshake.recovery_attempts,
                max_recovery_attempts: handshake.max_recovery_attempts,
            }))
        }
        Err(err) => Err(handshake_error("handshake step failed", err).into()),
    }
}

fn handshake_error(context: &str, err: pubky_noise::PubkyNoiseError) -> PaykitError {
    let context = format!("{context}: {err:?}");
    let source = anyhow::anyhow!("pubky-noise handshake failed: {err:?}");
    match err {
        pubky_noise::PubkyNoiseError::HomeserverResponseError
        | pubky_noise::PubkyNoiseError::HomeserverWriteError => {
            PaykitError::Transport { context, source }
        }
        _ => PaykitError::InvalidData {
            context,
            source: Some(source),
        },
    }
}

/// Transitions a completed handshake into an [`EncryptedLink`].
fn finish_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    let _link_id = handshake
        .encryptor
        .transition_transport()
        .map_err(|err| handshake_error("failed to transition to transport mode", err))?;

    debug!("Encrypted Link established");
    Ok(HandshakeProgress::Complete(EncryptedLink::from_parts(
        handshake.encryptor,
        handshake.remote_identity_public_key,
        handshake.remote_noise_public_key,
        handshake.recovery_context,
        handshake.config,
    )))
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot.
///
/// Restored handshakes reset recovery tuning to defaults. `remote_identity_public_key` must
/// match `snapshot.recipient()`.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link_handshake(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_identity_public_key: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (raw params)");

    let (write_path, read_path) = compute_private_payment_paths(
        &secret_key,
        session.info().public_key(),
        remote_identity_public_key,
        snapshot.remote_noise_public_key(),
        snapshot.recovery_context(),
    );

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

    restore_encrypted_link_handshake_inner(config, remote_identity_public_key, snapshot).await
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot
/// using an existing Noise configuration.
///
/// Restored handshakes reset recovery tuning to defaults. `remote_identity_public_key` must
/// match `snapshot.recipient()`.
/// The config's Noise key must match the snapshot's static key.
/// The config paths must match the session's local Pubky identity, the remote
/// identity, and the Noise keys; mismatches return [`PaykitError::Validation`].
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_handshake_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_identity_public_key: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (existing config)");
    restore_encrypted_link_handshake_inner(config, remote_identity_public_key, snapshot).await
}

/// Shared implementation for both handshake restore variants.
async fn restore_encrypted_link_handshake_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_identity_public_key: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    if snapshot.recipient() != remote_identity_public_key {
        return Err(PaykitError::Validation(format!(
            "remote_identity_public_key does not match snapshot recipient (remote={}, snapshot={})",
            remote_identity_public_key,
            snapshot.recipient(),
        )));
    }

    let phase = snapshot.phase();
    if !matches!(phase, pubky_noise::snow_crypto::NoisePhase::HandShake) {
        return Err(PaykitError::Validation(format!(
            "handshake restore requires handshake-phase snapshot, got {:?}",
            phase,
        )));
    }

    let remote_noise_public_key = snapshot.remote_noise_public_key().clone();
    let recovery_context = snapshot.recovery_context().clone();
    validate_private_payment_paths(
        &config,
        remote_identity_public_key,
        &remote_noise_public_key,
        &recovery_context,
    )?;
    let state = snapshot.into_state()?;
    if state.static_secret != Some(config.pubky_root_keypair.secret_key()) {
        return Err(PaykitError::Validation(
            "Noise config key does not match snapshot static key".into(),
        ));
    }
    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        state,
        remote_identity_public_key.clone(),
    )
    .await
    .map_err(|err| handshake_error("failed to restore Encrypted Link handshake", err))?;

    debug!("Encrypted Link handshake restored successfully (recovery tuning reset to defaults)");

    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_identity_public_key: remote_identity_public_key.clone(),
        remote_noise_public_key,
        recovery_context,
        config,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handshake_io_failures_remain_retryable() {
        for error in [
            pubky_noise::PubkyNoiseError::HomeserverResponseError,
            pubky_noise::PubkyNoiseError::HomeserverWriteError,
        ] {
            assert!(matches!(
                handshake_error("handshake failed", error),
                PaykitError::Transport { .. }
            ));
        }
        assert!(matches!(
            handshake_error(
                "handshake replay failed",
                pubky_noise::PubkyNoiseError::RestoreBackupReplayError,
            ),
            PaykitError::InvalidData { .. }
        ));
    }
}
