use tracing::{debug, instrument};

use crate::{PaykitAppId, PaykitError, PublicKey, Result};

use super::{
    paths::compute_private_payment_paths,
    private_application_message::{self, PrivateApplicationMessage},
    EncryptedLinkSnapshot,
};

/// Handle to an established Encrypted Link with a counterparty.
///
/// Created by [`advance_handshake`](crate::advance_handshake) (via
/// [`HandshakeProgress::Complete`](crate::HandshakeProgress::Complete)) after
/// a successful Noise handshake. It wraps a transport-mode
/// [`pubky_noise::PubkyNoiseEncryptor`] and should be closed with
/// [`close_encrypted_link`] when no longer needed.
///
/// # Session resumption
///
/// An established Encrypted Link can be snapshotted via [`snapshot`](Self::snapshot)
/// or serialized via [`serialize`](Self::serialize), then restored with
/// [`restore_encrypted_link`] or [`restore_encrypted_link_from_config`].
///
/// # Private Application Message stream
///
/// All private Paykit protocol messages on this link share one ordered stream.
/// Use [`receive_private_application_messages`](Self::receive_private_application_messages)
/// for ordered intake. Callers or SDK layers that derive durable Event Message
/// state should persist and route the returned messages before replacing a
/// stored link snapshot whose read counter has advanced.
///
/// # Automatic send retry
///
/// Send helpers retry transient homeserver write failures according to
/// [`set_max_send_retries`](Self::set_max_send_retries). Deterministic Noise
/// state, counter, nonce, or encryption errors fail immediately.
pub struct EncryptedLink {
    /// The Noise session manager in transport mode.
    encryptor: pubky_noise::PubkyNoiseEncryptor,
    /// The counterparty's Pubky identity key.
    recipient: PublicKey,
    /// The counterparty's identity-wide Noise public key.
    remote_noise_public_key: PublicKey,
    /// Shared Noise configuration retained for snapshot-based session resumption.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Maximum number of automatic Private Application Message `send_message`
    /// retries.
    max_send_retries: u32,
}

impl EncryptedLink {
    pub(super) fn from_parts(
        encryptor: pubky_noise::PubkyNoiseEncryptor,
        recipient: PublicKey,
        remote_noise_public_key: PublicKey,
        config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    ) -> Self {
        Self {
            encryptor,
            recipient,
            remote_noise_public_key,
            config,
            max_send_retries: DEFAULT_MAX_SEND_RETRIES,
        }
    }

    /// Set the maximum number of automatic Private Application Message
    /// `send_message` retries before Paykit gives up and returns
    /// [`PaykitError::Transport`].
    ///
    /// Retryable homeserver write failures are retried. Deterministic Noise
    /// state, counter, nonce, or encryption errors fail immediately.
    ///
    /// Default: [`DEFAULT_MAX_SEND_RETRIES`] (3).
    pub fn set_max_send_retries(&mut self, max: u32) -> &mut Self {
        self.max_send_retries = max;
        self
    }

    /// Capture the current link state as a serializable snapshot.
    ///
    /// The snapshot captures transport keys, counters, and counterparty identity.
    /// Take a new snapshot after receiving or sending messages when the caller
    /// wants the persisted read/write counters to catch up with local state.
    ///
    /// Snapshot bytes include sensitive key material and must be stored as
    /// secrets. Do not log them or include them in telemetry.
    pub fn snapshot(&self) -> EncryptedLinkSnapshot {
        EncryptedLinkSnapshot::from_state(
            self.encryptor.snapshot(),
            self.recipient.clone(),
            self.remote_noise_public_key.clone(),
        )
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

    async fn send_private_application_message_with_context(
        &mut self,
        plaintext: &[u8],
        context: &'static str,
    ) -> Result<()> {
        private_application_message::send_private_application_message(
            &mut self.encryptor,
            self.max_send_retries,
            plaintext,
            context,
        )
        .await
    }

    pub(crate) async fn send_private_payment_list_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Private Payment List")
            .await
    }

    pub(crate) async fn send_receipt_access_message(&mut self, plaintext: &[u8]) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Receipt Access")
            .await
    }

    pub(crate) async fn send_payment_request_message(&mut self, plaintext: &[u8]) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Payment Request")
            .await
    }

    pub(crate) async fn send_payment_request_acceptance_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Payment Request Acceptance")
            .await
    }

    pub(crate) async fn send_payment_request_rejection_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Payment Request Rejection")
            .await
    }

    pub(crate) async fn send_payment_request_cancellation_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(
            plaintext,
            "Payment Request Cancellation",
        )
        .await
    }

    pub(crate) async fn send_payment_proof_message(&mut self, plaintext: &[u8]) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Payment Proof")
            .await
    }

    /// Send one raw JSON Private Application Message.
    ///
    /// This is the low-level send counterpart to
    /// [`receive_private_application_messages`](Self::receive_private_application_messages).
    /// It validates the generic `version`, `kind`, and `app_id` envelope
    /// fields; it does not require a known Paykit kind or validate known Paykit
    /// message bodies. Use the typed serializers or SDK queue for
    /// protocol-managed Paykit messages.
    ///
    /// Higher-level callers should persist the exact JSON before sending when
    /// retrying the same message matters.
    pub async fn send_private_application_message_json(&mut self, raw_json: &str) -> Result<()> {
        validate_private_application_message_json(raw_json)?;
        self.send_private_application_message_with_context(
            raw_json.as_bytes(),
            "Private Application Message",
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn send_private_application_message_for_test(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(
            plaintext,
            "raw test Private Application Message",
        )
        .await
    }

    /// Receive a bounded batch of available Private Application Messages in
    /// stream order.
    ///
    /// The Noise read checkpoint advances past the returned messages. Callers
    /// that need crash-safe Event Message handling should persist returned
    /// messages before replacing a stored link snapshot. Call again to drain
    /// more than [`PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT`](crate::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT)
    /// available stream slots.
    #[instrument(skip(self))]
    pub async fn receive_private_application_messages(
        &mut self,
    ) -> Result<Vec<PrivateApplicationMessage>> {
        private_application_message::receive_private_application_messages(&mut self.encryptor).await
    }
}

fn validate_private_application_message_json(raw_json: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(raw_json).map_err(|err| PaykitError::Validation(err.to_string()))?;
    if value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .is_none_or(|version| u8::try_from(version).is_err())
    {
        return Err(PaykitError::Validation(
            "Private Application Message version must be a u8 integer".into(),
        ));
    }
    if !value.get("kind").is_some_and(serde_json::Value::is_string) {
        return Err(PaykitError::Validation(
            "Private Application Message kind must be a string".into(),
        ));
    }
    let app_id = value
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            PaykitError::Validation("Private Application Message app_id must be a string".into())
        })?;
    PaykitAppId::new(app_id)?;
    Ok(())
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
/// Use this after an app restart when the original Noise configuration is no
/// longer available. The caller supplies a fresh authenticated Pubky session,
/// the same local secret key used for the original link, the counterparty
/// public key, and the saved snapshot.
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// `remote_identity_public_key` must match `snapshot.recipient()`.
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_identity_public_key: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring Encrypted Link from snapshot (raw params)");

    let (write_path, read_path) =
        compute_private_payment_paths(&secret_key, snapshot.remote_noise_public_key());

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

    restore_encrypted_link_inner(config, remote_identity_public_key, snapshot).await
}

/// Restores an [`EncryptedLink`] from a previously saved snapshot using an
/// existing Noise configuration.
///
/// Use this in-process when the original `Arc<PubkyNoiseConfig>` is still
/// available. For cross-restart recovery, use [`restore_encrypted_link`].
///
/// Restored links reset `max_send_retries` to [`DEFAULT_MAX_SEND_RETRIES`].
/// `remote_identity_public_key` must match `snapshot.recipient()`.
#[instrument(skip(config, snapshot))]
pub async fn restore_encrypted_link_from_config(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_identity_public_key: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring Encrypted Link from snapshot (existing config)");
    restore_encrypted_link_inner(config, remote_identity_public_key, snapshot).await
}

/// Shared implementation for both restore variants.
async fn restore_encrypted_link_inner(
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    remote_identity_public_key: &PublicKey,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    if snapshot.recipient() != remote_identity_public_key {
        return Err(PaykitError::Validation(format!(
            "remote_identity_public_key does not match snapshot recipient (remote={}, snapshot={})",
            remote_identity_public_key,
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

    let remote_noise_public_key = snapshot.remote_noise_public_key().clone();
    let state = snapshot.into_state();
    let encryptor = pubky_noise::PubkyNoiseEncryptor::restore(
        config.clone(),
        state,
        remote_identity_public_key.clone(),
    )
    .await
    .map_err(|err| PaykitError::Transport {
        context: format!("failed to restore Encrypted Link: {err:?}"),
        source: anyhow::anyhow!("pubky-noise restore failed: {err:?}"),
    })?;

    debug!("Encrypted Link restored successfully");
    Ok(EncryptedLink::from_parts(
        encryptor,
        remote_identity_public_key.clone(),
        remote_noise_public_key,
        config,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_private_application_message_json_requires_header() {
        assert!(validate_private_application_message_json(
            r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit"}"#
        )
        .is_ok());
        assert!(validate_private_application_message_json(r#"{"version":1}"#).is_err());
        assert!(validate_private_application_message_json(r#"{"kind":"paykit.test"}"#).is_err());
        assert!(
            validate_private_application_message_json(r#"{"version":1,"kind":"paykit.test"}"#)
                .is_err()
        );
        assert!(validate_private_application_message_json(
            r#"{"version":1,"kind":"paykit.test","app_id":"bad/path"}"#
        )
        .is_err());
    }
}
