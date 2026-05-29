use std::collections::VecDeque;

use tracing::{debug, instrument};

use crate::{PaykitError, PublicKey, Result};

use super::{
    paths::compute_private_payment_paths,
    private_message::{self, BufferedPrivateMessage, PrivateMessageKind},
    EncryptedLinkSnapshot,
};

/// Handle to an established Encrypted Link with a counterparty.
///
/// Created by [`advance_handshake`](crate::advance_handshake) (via
/// [`HandshakeProgress::Complete`](crate::HandshakeProgress::Complete)) after
/// a successful Noise handshake. Used by Private Application Message helpers to
/// encrypt and decrypt Paykit data. Must be closed via [`close_encrypted_link`]
/// when no longer needed.
///
/// The link wraps a [`pubky_noise::PubkyNoiseEncryptor`] in transport mode.
///
/// # Session resumption
///
/// An established Encrypted Link can be snapshotted via [`snapshot`](Self::snapshot) (or
/// serialized directly via [`serialize`](Self::serialize)) and later restored
/// with [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`]
/// without re-doing the Noise handshake.
///
/// # Private message dispatch
///
/// All Paykit application messages on this Noise link share one ordered stream.
/// The link therefore buffers decrypted messages after low-level receipt and
/// lets typed helpers consume only their own message kind. This prevents future
/// helpers (for example Receipt Access) from losing messages simply because a
/// different typed getter was called first.
///
/// The buffer is in-memory only. If callers need crash-safe processing of
/// Event Message kinds, they must persist handled/unhandled application state
/// before dropping or serializing the link.
///
/// # Automatic send retry
///
/// Paykit helpers that send Private Application Messages automatically retry
/// failed `send_message` calls up to
/// [`max_send_retries`](Self::set_max_send_retries) times (default:
/// [`DEFAULT_MAX_SEND_RETRIES`]). Since transport-phase send failures do not
/// corrupt the Noise state, retries are safe without snapshot-based recovery.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's public key.
    recipient: PublicKey,
    /// Shared Noise configuration retained for snapshot-based session resumption.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Maximum number of automatic Private Application Message `send_message`
    /// retries.
    max_send_retries: u32,
    /// Decrypted application messages that have been read from the ordered
    /// Noise stream but not yet consumed by a typed Paykit helper.
    ///
    /// This prevents a typed getter such as [`crate::get_private_payment_envelope`] from
    /// discarding unrelated supported message kinds (for example Receipt Access
    /// messages) after the underlying Noise read counter has advanced.
    pending_private_messages: VecDeque<BufferedPrivateMessage>,
}

impl EncryptedLink {
    pub(super) fn from_parts(
        encryptor: pubky_noise::PubkyNoiseEncryptor,
        recipient: PublicKey,
        config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    ) -> Self {
        Self {
            encryptor,
            recipient,
            config,
            max_send_retries: DEFAULT_MAX_SEND_RETRIES,
            pending_private_messages: VecDeque::new(),
        }
    }

    /// Set the maximum number of automatic Private Application Message
    /// `send_message` retries before Paykit gives up and returns
    /// [`PaykitError::Transport`].
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
        EncryptedLinkSnapshot::from_state(self.encryptor.snapshot(), self.recipient.clone())
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

    /// Access the counterparty public key for this Encrypted Link.
    pub fn recipient(&self) -> &PublicKey {
        &self.recipient
    }

    async fn send_private_message(
        &mut self,
        plaintext: &[u8],
        context: &'static str,
    ) -> Result<()> {
        private_message::send_private_message(
            &mut self.encryptor,
            self.max_send_retries,
            plaintext,
            context,
        )
        .await
    }

    pub(crate) async fn send_private_payment_envelope_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_message(plaintext, "Private Payment Envelope")
            .await
    }

    pub(crate) async fn send_receipt_access_message(&mut self, plaintext: &[u8]) -> Result<()> {
        self.send_private_message(plaintext, "Receipt Access").await
    }

    #[cfg(test)]
    pub(crate) async fn send_private_application_message_for_test(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_message(plaintext, "raw test Private Application Message")
            .await
    }

    async fn receive_private_messages(&mut self) -> Result<usize> {
        private_message::receive_private_messages(
            &mut self.encryptor,
            &mut self.pending_private_messages,
        )
        .await
    }

    pub(crate) async fn receive_latest_private_payment_envelope_message(
        &mut self,
    ) -> Result<(usize, Option<String>, usize)> {
        let received = self.receive_private_messages().await?;
        let message = private_message::take_latest_pending_message(
            &mut self.pending_private_messages,
            PrivateMessageKind::PrivatePaymentEnvelope,
        );
        Ok((received, message, self.pending_private_messages.len()))
    }

    pub(crate) async fn receive_receipt_access_messages(
        &mut self,
    ) -> Result<(usize, Vec<String>, usize)> {
        let received = self.receive_private_messages().await?;
        let messages = private_message::take_all_pending_messages(
            &mut self.pending_private_messages,
            PrivateMessageKind::ReceiptAccess,
        );
        Ok((received, messages, self.pending_private_messages.len()))
    }

    #[cfg(test)]
    pub(crate) fn pending_private_message_count_for_test(&self) -> usize {
        self.pending_private_messages.len()
    }

    #[cfg(test)]
    pub(crate) fn pending_private_message_kinds_for_test(&self) -> Vec<String> {
        self.pending_private_messages
            .iter()
            .map(|message| message.kind().to_owned())
            .collect()
    }
}

/// Default maximum number of automatic Private Application Message
/// `send_message` retries before Paykit gives up and returns an error.
///
/// Override per-link via [`EncryptedLink::set_max_send_retries`].
pub const DEFAULT_MAX_SEND_RETRIES: u32 = 3;

/// Closes an Encrypted Link and cleans up the Noise session state.
///
/// After calling this function, the [`EncryptedLink`] is consumed and can no
/// longer be used for encryption or decryption.
#[instrument(skip(link))]
pub async fn close_encrypted_link(mut link: EncryptedLink) -> Result<()> {
    debug!("closing Encrypted Link");
    link.encryptor.close();
    debug!("Encrypted Link closed successfully");
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
/// - `secret_key` — 32-byte Ed25519 secret key of the local party (same key
///   used in the original [`initiate_encrypted_link`](crate::initiate_encrypted_link) or
///   [`accept_encrypted_link`](crate::accept_encrypted_link) call).
/// - `remote_pubkey` — public key of the counterparty.
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
    debug!("restoring Encrypted Link from snapshot (raw params)");

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
/// - `remote_pubkey` — public key of the counterparty.
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
    debug!("restoring Encrypted Link from snapshot (existing config)");
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

    let phase = snapshot.phase();
    if !matches!(phase, pubky_noise::snow_crypto::NoisePhase::Transport) {
        return Err(PaykitError::Validation(format!(
            "Encrypted Link restore requires transport-phase snapshot, got {:?}",
            phase,
        )));
    }

    let state = snapshot.into_state();
    let encryptor =
        pubky_noise::PubkyNoiseEncryptor::restore(config.clone(), state, remote_pubkey.clone())
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("failed to restore Encrypted Link: {err:?}"),
                source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
            })?;

    debug!("Encrypted Link restored successfully");
    Ok(EncryptedLink::from_parts(
        encryptor,
        remote_pubkey.clone(),
        config,
    ))
}
