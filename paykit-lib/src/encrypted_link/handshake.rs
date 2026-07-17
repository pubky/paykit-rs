use tracing::{debug, instrument, warn};

use crate::{PaykitError, PaykitReceiverPath, PublicKey, Result};

use super::{
    link::EncryptedLink,
    paths::{compute_private_payment_paths, validate_private_payment_paths},
    snapshot::EncryptedLinkHandshakeSnapshot,
};

/// Default maximum number of consecutive automatic recovery attempts before
/// [`advance_handshake`] gives up and returns an error.
///
/// Override per-handshake via [`EncryptedLinkHandshake::set_max_recovery_attempts`].
pub const DEFAULT_MAX_RECOVERY_ATTEMPTS: u32 = 3;

struct HandshakeReceiverScope<'a> {
    remote_identity_public_key: &'a PublicKey,
    remote_noise_public_key: &'a PublicKey,
    local_receiver_path: &'a PaykitReceiverPath,
    remote_receiver_path: &'a PaykitReceiverPath,
}

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
    /// The counterparty's public key (used for homeserver path construction).
    remote_pubkey: PublicKey,
    /// Counterparty receiver Noise public key used for path derivation.
    remote_noise_public_key: PublicKey,
    /// Shared Noise configuration needed for snapshot-based recovery.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Local receiver path used by this handshake.
    local_receiver_path: PaykitReceiverPath,
    /// Counterparty receiver path used by this handshake.
    remote_receiver_path: PaykitReceiverPath,
    /// Number of consecutive recovery attempts so far.
    recovery_attempts: u32,
    /// Maximum consecutive recovery attempts before giving up.
    max_recovery_attempts: u32,
}

impl EncryptedLinkHandshake {
    /// Set the maximum number of consecutive automatic recovery attempts
    /// before [`advance_handshake`] gives up and returns
    /// [`PaykitError::Transport`].
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
    pub fn snapshot(&self) -> EncryptedLinkHandshakeSnapshot {
        EncryptedLinkHandshakeSnapshot::from_state(
            self.encryptor.snapshot(),
            self.remote_pubkey.clone(),
            self.remote_noise_public_key.clone(),
            self.local_receiver_path.clone(),
            self.remote_receiver_path.clone(),
        )
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
/// `receiver_pubkey` identifies the counterparty homeserver. The separate
/// `receiver_noise_public_key` is discovered from that receiver's public
/// [`PaykitReceiverMarker`](crate::PaykitReceiverMarker) and is used with
/// `sender_noise_secret_key` for private path derivation and the Noise static
/// key.
///
/// Call [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
#[instrument(skip(session, sender_noise_secret_key, outbox_client))]
pub fn initiate_encrypted_link(
    session: pubky::PubkySession,
    sender_noise_secret_key: [u8; 32],
    receiver_pubkey: &PublicKey,
    receiver_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    let sender_pubkey = session.info().public_key().clone();
    let paths = compute_private_payment_paths(
        &sender_noise_secret_key,
        &sender_pubkey,
        receiver_pubkey,
        receiver_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
    initiate_encrypted_link_with_paths(
        session,
        sender_noise_secret_key,
        HandshakeReceiverScope {
            remote_identity_public_key: receiver_pubkey,
            remote_noise_public_key: receiver_noise_public_key,
            local_receiver_path,
            remote_receiver_path,
        },
        outbox_client,
        paths,
    )
}

fn initiate_encrypted_link_with_paths(
    session: pubky::PubkySession,
    sender_noise_secret_key: [u8; 32],
    receiver_scope: HandshakeReceiverScope<'_>,
    outbox_client: pubky::Pubky,
    paths: (String, String),
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (initiator)");

    let (write_path, read_path) = paths;

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        sender_noise_secret_key,
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
        sender_noise_secret_key,
        true,
        receiver_scope.remote_identity_public_key.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (initiator)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: receiver_scope.remote_identity_public_key.clone(),
        remote_noise_public_key: receiver_scope.remote_noise_public_key.clone(),
        config,
        local_receiver_path: receiver_scope.local_receiver_path.clone(),
        remote_receiver_path: receiver_scope.remote_receiver_path.clone(),
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}

/// Accepts a Noise XX Encrypted Link Handshake from a counterparty
/// (responder role).
///
/// `sender_pubkey` identifies the counterparty homeserver. The separate
/// `sender_noise_public_key` is discovered from that receiver's public
/// [`PaykitReceiverMarker`](crate::PaykitReceiverMarker) and is used with
/// `receiver_noise_secret_key` for private path derivation and the Noise static
/// key.
///
/// Call [`advance_handshake`] until it returns [`HandshakeProgress::Complete`].
#[instrument(skip(session, receiver_noise_secret_key, outbox_client))]
pub fn accept_encrypted_link(
    session: pubky::PubkySession,
    receiver_noise_secret_key: [u8; 32],
    sender_pubkey: &PublicKey,
    sender_noise_public_key: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
    outbox_client: pubky::Pubky,
) -> Result<EncryptedLinkHandshake> {
    let receiver_pubkey = session.info().public_key().clone();
    let paths = compute_private_payment_paths(
        &receiver_noise_secret_key,
        &receiver_pubkey,
        sender_pubkey,
        sender_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
    accept_encrypted_link_with_paths(
        session,
        receiver_noise_secret_key,
        HandshakeReceiverScope {
            remote_identity_public_key: sender_pubkey,
            remote_noise_public_key: sender_noise_public_key,
            local_receiver_path,
            remote_receiver_path,
        },
        outbox_client,
        paths,
    )
}

fn accept_encrypted_link_with_paths(
    session: pubky::PubkySession,
    receiver_noise_secret_key: [u8; 32],
    sender_scope: HandshakeReceiverScope<'_>,
    outbox_client: pubky::Pubky,
    paths: (String, String),
) -> Result<EncryptedLinkHandshake> {
    debug!("initializing Encrypted Link handshake (responder)");

    let (write_path, read_path) = paths;

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        receiver_noise_secret_key,
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
        receiver_noise_secret_key,
        false,
        sender_scope.remote_identity_public_key.clone(),
    )
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to initialize encryptor: {err:?}"),
        source: anyhow::anyhow!("pubky-noise PubkyNoiseEncryptor::new failed: {err:?}"),
    })?;

    debug!("handshake context initialized (responder)");
    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: sender_scope.remote_identity_public_key.clone(),
        remote_noise_public_key: sender_scope.remote_noise_public_key.clone(),
        config,
        local_receiver_path: sender_scope.local_receiver_path.clone(),
        remote_receiver_path: sender_scope.remote_receiver_path.clone(),
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
#[instrument(skip(handshake))]
pub async fn advance_handshake(mut handshake: EncryptedLinkHandshake) -> Result<HandshakeProgress> {
    // Check whether the handshake has already finished.
    if handshake.encryptor.is_handshake_complete() {
        return finish_handshake(handshake);
    }

    // Process the next handshake step.
    match handshake.encryptor.handle_handshake().await {
        Ok(pubky_noise::HandshakeResult::Pending) => {
            debug!("handshake step pending (waiting for counterparty)");
            handshake.recovery_attempts = 0;
            Ok(HandshakeProgress::Pending(handshake))
        }
        Ok(pubky_noise::HandshakeResult::Terminal) => {
            debug!("handshake terminal, transitioning to transport");
            finish_handshake(handshake)
        }
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
                });
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
                remote_noise_public_key: handshake.remote_noise_public_key,
                local_receiver_path: handshake.local_receiver_path,
                remote_receiver_path: handshake.remote_receiver_path,
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

    debug!("Encrypted Link established");
    Ok(HandshakeProgress::Complete(EncryptedLink::from_parts(
        handshake.encryptor,
        handshake.remote_pubkey,
        handshake.remote_noise_public_key,
        handshake.config,
        handshake.local_receiver_path,
        handshake.remote_receiver_path,
    )))
}

/// Restores an [`EncryptedLinkHandshake`] from a previously saved snapshot.
///
/// `noise_secret_key` must be the local receiver Noise key used to create the
/// original handshake. The snapshot carries the counterparty receiver Noise
/// public key needed to reconstruct the private paths.
///
/// Restored handshakes reset recovery tuning to defaults. `remote_pubkey` must
/// match `snapshot.recipient()`.
#[instrument(skip(session, noise_secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link_handshake(
    session: pubky::PubkySession,
    noise_secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    local_receiver_path: &PaykitReceiverPath,
    remote_receiver_path: &PaykitReceiverPath,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    snapshot.validate_receiver_scope(local_receiver_path, remote_receiver_path)?;
    let remote_noise_public_key = snapshot.remote_noise_public_key().clone();
    let local_pubkey = session.info().public_key().clone();
    let paths = compute_private_payment_paths(
        &noise_secret_key,
        &local_pubkey,
        remote_pubkey,
        &remote_noise_public_key,
        local_receiver_path,
        remote_receiver_path,
    );
    restore_encrypted_link_handshake_with_paths(
        session,
        noise_secret_key,
        remote_pubkey,
        outbox_client,
        snapshot,
        paths,
    )
    .await
}

async fn restore_encrypted_link_handshake_with_paths(
    session: pubky::PubkySession,
    noise_secret_key: [u8; 32],
    remote_pubkey: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkHandshakeSnapshot,
    paths: (String, String),
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (raw params)");

    let (write_path, read_path) = paths;

    let config = pubky_noise::PubkyNoiseConfig::new_with_paths(
        noise_secret_key,
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
/// Restored handshakes reset recovery tuning to defaults. `remote_pubkey` must
/// match `snapshot.recipient()`.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_handshake_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_pubkey: &PublicKey,
    snapshot: EncryptedLinkHandshakeSnapshot,
) -> Result<EncryptedLinkHandshake> {
    debug!("restoring Encrypted Link handshake from snapshot (existing config)");
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

    let phase = snapshot.phase();
    if !matches!(phase, pubky_noise::snow_crypto::NoisePhase::HandShake) {
        return Err(PaykitError::Validation(format!(
            "handshake restore requires handshake-phase snapshot, got {:?}",
            phase,
        )));
    }

    let local_receiver_path = snapshot.local_receiver_path().clone();
    let remote_receiver_path = snapshot.remote_receiver_path().clone();
    let remote_noise_public_key = snapshot.remote_noise_public_key().clone();
    validate_private_payment_paths(
        &config,
        remote_pubkey,
        &remote_noise_public_key,
        &local_receiver_path,
        &remote_receiver_path,
    )?;
    let state = snapshot.into_state();
    let encryptor =
        pubky_noise::PubkyNoiseEncryptor::restore(config.clone(), state, remote_pubkey.clone())
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to restore Encrypted Link handshake: {err:?}"),
                source: anyhow::anyhow!("pubky-noise handshake restore failed: {err:?}"),
            })?;

    debug!("Encrypted Link handshake restored successfully (recovery tuning reset to defaults)");

    Ok(EncryptedLinkHandshake {
        encryptor,
        remote_pubkey: remote_pubkey.clone(),
        remote_noise_public_key,
        config,
        local_receiver_path,
        remote_receiver_path,
        recovery_attempts: 0,
        max_recovery_attempts: DEFAULT_MAX_RECOVERY_ATTEMPTS,
    })
}
