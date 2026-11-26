use tracing::{debug, instrument};

use crate::{PaykitAppId, PaykitError, PublicKey, Result};

use super::{
    paths::{compute_private_payment_paths, validate_private_payment_paths},
    private_application_message::{self, PrivateApplicationMessage},
    EncryptedLinkRecoveryContext, EncryptedLinkSnapshot,
};

/// Exhausted write cursor: pubky-noise refuses to write at or past this slot.
const LAST_WRITE_SLOT: u32 = u32::MAX - 1;

/// Outbound private message prepared for atomic persistence before publication.
///
/// Persist [`destination_path`](Self::destination_path),
/// [`ciphertext`](Self::ciphertext), and
/// [`resulting_snapshot`](Self::resulting_snapshot) together before
/// acknowledging this value.
pub struct PreparedPrivateApplicationMessageSend {
    prepared: pubky_noise::PreparedSend,
    resulting_snapshot: EncryptedLinkSnapshot,
}

impl PreparedPrivateApplicationMessageSend {
    /// Pubky storage path for the exact ciphertext.
    pub fn destination_path(&self) -> &str {
        self.prepared.destination_path()
    }

    /// Exact ciphertext to publish or retry after a crash.
    pub fn ciphertext(&self) -> &[u8] {
        self.prepared.ciphertext()
    }

    /// Encrypted Link snapshot after this send.
    pub fn resulting_snapshot(&self) -> &EncryptedLinkSnapshot {
        &self.resulting_snapshot
    }
}

impl std::fmt::Debug for PreparedPrivateApplicationMessageSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPrivateApplicationMessageSend")
            .field("destination_path", &"<redacted>")
            .field(
                "ciphertext",
                &format_args!("<redacted:{} bytes>", self.ciphertext().len()),
            )
            .field("resulting_snapshot", &"<redacted>")
            .finish()
    }
}

/// What the destination slot of a prepared send already holds.
///
/// Each write slot of an Encrypted Link is encrypted under its own sending
/// nonce. Link state restored from an older copy can point at a slot that a
/// later send already used, so a slot holding a different packet must never
/// receive a second ciphertext.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedPrivateSendSlot {
    /// The slot is empty, so the prepared ciphertext can be published.
    Free,
    /// The slot already holds this exact ciphertext from an earlier publication.
    Published,
    /// The slot holds a different packet. Do not publish the prepared
    /// ciphertext: call
    /// [`skip_published_write_slots`](EncryptedLink::skip_published_write_slots)
    /// and prepare the message again.
    Taken,
}

/// Inbound private message prepared for atomic application-state persistence.
pub struct PreparedPrivateApplicationMessageReceive {
    prepared: pubky_noise::PreparedReceive,
    message: PrivateApplicationMessage,
    resulting_snapshot: EncryptedLinkSnapshot,
}

impl PreparedPrivateApplicationMessageReceive {
    /// Decrypted Paykit application message.
    pub fn message(&self) -> &PrivateApplicationMessage {
        &self.message
    }

    /// Encrypted Link snapshot after this receive.
    pub fn resulting_snapshot(&self) -> &EncryptedLinkSnapshot {
        &self.resulting_snapshot
    }
}

impl std::fmt::Debug for PreparedPrivateApplicationMessageReceive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedPrivateApplicationMessageReceive")
            .field("message", &"<redacted>")
            .field("resulting_snapshot", &"<redacted>")
            .finish()
    }
}

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
    recovery_context: EncryptedLinkRecoveryContext,
    /// Shared Noise configuration retained for snapshot-based session resumption.
    config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    /// Maximum number of automatic Private Application Message `send_message`
    /// retries.
    max_send_retries: u32,
}

impl EncryptedLink {
    /// X25519 static key authenticated by the Noise transcript.
    /// The caller must bind this key to the expected identity, including after restore.
    pub fn remote_static_public_key(&self) -> Option<&[u8]> {
        self.encryptor.remote_static_public_key()
    }

    pub(super) fn from_parts(
        encryptor: pubky_noise::PubkyNoiseEncryptor,
        recipient: PublicKey,
        remote_noise_public_key: PublicKey,
        recovery_context: EncryptedLinkRecoveryContext,
        config: std::sync::Arc<pubky_noise::PubkyNoiseConfig>,
    ) -> Self {
        Self {
            encryptor,
            recipient,
            remote_noise_public_key,
            recovery_context,
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
    pub fn snapshot(&self) -> Result<EncryptedLinkSnapshot> {
        Ok(EncryptedLinkSnapshot::from_state(
            self.encryptor
                .snapshot()
                .map_err(|err| PaykitError::InvalidData {
                    context: format!("capture Encrypted Link snapshot: {err:?}"),
                    source: None,
                })?,
            self.recipient.clone(),
            self.remote_noise_public_key.clone(),
            self.recovery_context.clone(),
        ))
    }

    /// Serialize the current link state to bytes for persistence.
    ///
    /// Convenience method equivalent to `self.snapshot()?.serialize()`.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        Ok(self.snapshot()?.serialize())
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
        private_application_message::validate_private_application_message_size(plaintext, context)?;
        // Callers restore this link from their own storage, which can be older
        // than its write stream. Never write a second packet into a used slot.
        // A retained failed send must be republished first; the send reports it.
        if self.encryptor.snapshot().is_ok() {
            // Boxed so every send helper's future does not embed the link restore.
            Box::pin(self.skip_published_write_slots()).await?;
        }
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

    pub(crate) async fn send_payment_conversion_quote_message(
        &mut self,
        plaintext: &[u8],
    ) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Payment Conversion Quote")
            .await
    }

    pub(crate) async fn send_allowance_message(&mut self, plaintext: &[u8]) -> Result<()> {
        self.send_private_application_message_with_context(plaintext, "Allowance Event Message")
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
    ///
    /// Before encrypting, the send moves this link past write slots that
    /// already hold packets, as
    /// [`skip_published_write_slots`](Self::skip_published_write_slots) does.
    /// This costs one `HEAD` request when the next slot is free.
    ///
    /// Transient outbox write failures are retried up to the configured budget.
    /// If every attempt fails, the prepared packet is retained and blocks
    /// further sends, receives, and snapshots on this link until it is
    /// republished with [`retry_pending_send`](Self::retry_pending_send).
    ///
    /// Do not restore an older snapshot to recover from a failed send: the
    /// failed write may still have reached the homeserver, and rolling back
    /// the sending nonce and slot would reuse the transport key and nonce for
    /// the next plaintext. Only the identical prepared packet may be
    /// republished; otherwise establish a fresh link.
    pub async fn send_private_application_message_json(&mut self, raw_json: &str) -> Result<()> {
        validate_private_application_message_json(raw_json)?;
        self.send_private_application_message_with_context(
            raw_json.as_bytes(),
            "Private Application Message",
        )
        .await
    }

    /// Republish the prepared packet retained after a failed convenience send.
    ///
    /// Until the identical packet is republished, sends, receives, and snapshots
    /// remain blocked. Never restore an older snapshot to retry with different
    /// plaintext: an ambiguous write may already have used that key and nonce.
    /// SDK callers using staged sends should instead publish their durably
    /// persisted ciphertext with [`publish_prepared_private_application_message`](Self::publish_prepared_private_application_message).
    #[allow(deprecated)]
    pub async fn retry_pending_send(&mut self) -> Result<()> {
        self.encryptor
            .retry_pending_send()
            .await
            .map_err(|err| PaykitError::Transport {
                context: format!("retry pending private send: {err:?}"),
                source: anyhow::anyhow!("pubky-noise retry_pending_send failed: {err:?}"),
            })
    }

    /// Prepare one raw JSON Private Application Message for durable handoff.
    ///
    /// Persist the returned ciphertext and resulting snapshot atomically before
    /// calling [`acknowledge_persisted_private_send`](Self::acknowledge_persisted_private_send).
    pub fn prepare_private_application_message_json(
        &mut self,
        raw_json: &str,
    ) -> Result<PreparedPrivateApplicationMessageSend> {
        private_application_message::validate_private_application_message_size(
            raw_json.as_bytes(),
            "Private Application Message",
        )?;
        validate_private_application_message_json(raw_json)?;
        let prepared = self
            .encryptor
            .prepare_send(raw_json.as_bytes())
            .map_err(|err| PaykitError::Transport {
                context: format!("prepare Private Application Message send: {err:?}"),
                source: anyhow::Error::new(crate::error::NonRetryablePrivateSendError(err)),
            })?;
        let resulting_snapshot = EncryptedLinkSnapshot::from_state(
            prepared.resulting_session_state().clone(),
            self.recipient.clone(),
            self.remote_noise_public_key.clone(),
            self.recovery_context.clone(),
        );
        Ok(PreparedPrivateApplicationMessageSend {
            prepared,
            resulting_snapshot,
        })
    }

    /// Acknowledge that a prepared send and its resulting snapshot are durable.
    pub fn acknowledge_persisted_private_send(
        &mut self,
        prepared: PreparedPrivateApplicationMessageSend,
    ) -> Result<()> {
        self.encryptor
            .acknowledge_persisted_send(prepared.prepared)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("acknowledge persisted private send: {err:?}"),
                source: None,
            })
    }

    /// Publish an exact prepared ciphertext, retrying without re-encryption.
    ///
    /// This accepts values restored from durable SDK state after a crash. The
    /// destination must belong to this link's local write stream. Check the
    /// destination with
    /// [`prepared_private_send_slot`](Self::prepared_private_send_slot) first:
    /// this method replaces whatever the slot holds.
    pub async fn publish_prepared_private_application_message(
        &self,
        destination_path: &str,
        ciphertext: &[u8],
    ) -> Result<()> {
        self.validate_prepared_send(destination_path, ciphertext)?;

        let max_attempts = self.max_send_retries.saturating_add(1);
        let mut last_error = None;
        for attempt in 1..=max_attempts {
            match self
                .config
                .local_session
                .storage()
                .put(destination_path, ciphertext.to_vec())
                .await
            {
                Ok(_) => return Ok(()),
                Err(err) => {
                    last_error = Some(err);
                    if attempt < max_attempts {
                        tracing::warn!(
                            attempt,
                            max_retries = self.max_send_retries,
                            "prepared private message publication failed, retrying"
                        );
                    }
                }
            }
        }

        Err(PaykitError::Transport {
            context: format!(
                "failed to publish prepared Private Application Message after {max_attempts} attempts"
            ),
            source: last_error
                .expect("at least one prepared private send attempt must run")
                .into(),
        })
    }

    /// Check what the destination slot of a prepared send already holds.
    ///
    /// Call before
    /// [`publish_prepared_private_application_message`](Self::publish_prepared_private_application_message),
    /// including for values restored from durable state. Link state restored
    /// from an older copy can point at a slot that a later send already used;
    /// publishing there would put two packets under one key and nonce. A send
    /// prepared right after
    /// [`skip_published_write_slots`](Self::skip_published_write_slots), at the
    /// [`next_write_slot_path`](Self::next_write_slot_path) the skip found
    /// free, needs no second check.
    ///
    /// Costs one `HEAD` request, plus a bounded `GET` when the slot exists. It
    /// reads this identity's own homeserver and relies on it to report stored
    /// packets.
    pub async fn prepared_private_send_slot(
        &self,
        destination_path: &str,
        ciphertext: &[u8],
    ) -> Result<PreparedPrivateSendSlot> {
        self.validate_prepared_send(destination_path, ciphertext)?;
        if !self.write_path_exists(destination_path).await? {
            return Ok(PreparedPrivateSendSlot::Free);
        }
        let response = match self
            .config
            .local_session
            .storage()
            .get(destination_path)
            .await
        {
            Ok(response) => response,
            Err(err) if private_application_message::is_not_found(&err) => {
                return Ok(PreparedPrivateSendSlot::Free)
            }
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "read prepared private send slot".into(),
                    source: err.into(),
                })
            }
        };
        match crate::pubky_routing::read_bounded_body(
            response,
            pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN,
            "read prepared private send slot",
        )
        .await
        {
            Ok(stored) if stored == ciphertext => Ok(PreparedPrivateSendSlot::Published),
            // An oversized body is not a packet this link wrote, but it still occupies the slot.
            Ok(_) | Err(PaykitError::InvalidData { .. }) => Ok(PreparedPrivateSendSlot::Taken),
            Err(err) => Err(err),
        }
    }

    /// Move the next write slot past packets already on this link's write stream.
    ///
    /// Use after restoring link state from storage, before the first
    /// preparation, or when
    /// [`prepared_private_send_slot`](Self::prepared_private_send_slot)
    /// reports [`PreparedPrivateSendSlot::Taken`]. The sending nonce moves with
    /// the slot, so no nonce whose slot holds a packet is used again. The peer
    /// reads the packets already in the skipped slots before this link's next
    /// packet. Returns the number of slots skipped.
    ///
    /// Each packet is published before the next one is prepared, so stored
    /// slots are contiguous from the link's first transport slot. This holds
    /// only while no slot of a write stream that link state can still write to
    /// has been deleted: a gap would end the search before packets stored after
    /// it. [`clear_encrypted_link_outbox`](crate::clear_encrypted_link_outbox)
    /// deletes page by page and can stop partway, so retire the recovery
    /// context before clearing it, as its docs say. With contiguous slots the
    /// first free slot is found with a logarithmic number of `HEAD` requests,
    /// and one `HEAD` request when the next slot is free.
    ///
    /// Fails while a prepared send awaits
    /// [`acknowledge_persisted_private_send`](Self::acknowledge_persisted_private_send)
    /// or a failed convenience send awaits [`retry_pending_send`](Self::retry_pending_send).
    /// Persist the resulting [`snapshot`](Self::snapshot) before relying on the
    /// skip after a restart.
    pub async fn skip_published_write_slots(&mut self) -> Result<u32> {
        let mut state = self.snapshot()?.into_state();
        let next_slot = state.write_counter;
        let free_slot = self.first_free_write_slot(next_slot).await?;
        let skipped = free_slot - next_slot;
        if skipped == 0 {
            return Ok(0);
        }
        state.write_counter = free_slot;
        state.sending_nonce = state
            .sending_nonce
            .checked_add(u64::from(skipped))
            .ok_or_else(|| PaykitError::InvalidData {
                context: "Encrypted Link sending nonces are exhausted".into(),
                source: None,
            })?;
        let snapshot = EncryptedLinkSnapshot::from_state(
            state,
            self.recipient.clone(),
            self.remote_noise_public_key.clone(),
            self.recovery_context.clone(),
        );
        let mut advanced =
            restore_encrypted_link_inner(self.config.clone(), &self.recipient, snapshot).await?;
        advanced.max_send_retries = self.max_send_retries;
        let mut previous = std::mem::replace(self, advanced);
        previous.encryptor.close();
        debug!(skipped, "skipped published Encrypted Link write slots");
        Ok(skipped)
    }

    /// Destination path of the next packet this link prepares.
    ///
    /// Right after [`skip_published_write_slots`](Self::skip_published_write_slots),
    /// this is the slot the skip found free. Fails while a prepared send awaits
    /// [`acknowledge_persisted_private_send`](Self::acknowledge_persisted_private_send)
    /// or a failed convenience send awaits [`retry_pending_send`](Self::retry_pending_send).
    pub fn next_write_slot_path(&self) -> Result<String> {
        let state = self.snapshot()?.into_state();
        Ok(format!(
            "{}/{}",
            self.config.write_path, state.write_counter
        ))
    }

    /// First slot at or after `start` that holds no packet.
    async fn first_free_write_slot(&self, start: u32) -> Result<u32> {
        first_free_slot(start, |slot| self.write_slot_exists(slot)).await
    }

    async fn write_slot_exists(&self, slot: u32) -> Result<bool> {
        self.write_path_exists(&format!("{}/{slot}", self.config.write_path))
            .await
    }

    async fn write_path_exists(&self, path: &str) -> Result<bool> {
        self.config
            .local_session
            .storage()
            .exists(path)
            .await
            .map_err(|err| PaykitError::Transport {
                context: "check Encrypted Link write slot".into(),
                source: err.into(),
            })
    }

    fn validate_prepared_send(&self, destination_path: &str, ciphertext: &[u8]) -> Result<()> {
        let expected_prefix = format!("{}/", self.config.write_path);
        if !destination_path.starts_with(&expected_prefix)
            || destination_path[expected_prefix.len()..]
                .parse::<u32>()
                .is_err()
        {
            return Err(PaykitError::Validation(
                "prepared private send destination does not belong to this Encrypted Link".into(),
            ));
        }
        if ciphertext.len() != pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN {
            return Err(PaykitError::InvalidData {
                context: "prepared private send ciphertext has an invalid length".into(),
                source: None,
            });
        }
        Ok(())
    }

    /// Fetch and prepare the next available inbound Private Application Message.
    ///
    /// Persist the application result and returned snapshot atomically before
    /// calling [`acknowledge_persisted_private_receive`](Self::acknowledge_persisted_private_receive).
    pub async fn prepare_next_private_application_message(
        &mut self,
    ) -> Result<Option<PreparedPrivateApplicationMessageReceive>> {
        let path = self
            .encryptor
            .next_receive_path()
            .map_err(|err| PaykitError::Transport {
                context: format!("prepare Private Application Message receive path: {err:?}"),
                source: anyhow::Error::new(crate::error::NonRetryablePrivateReceiveError(err)),
            })?;
        let response = match self.config.outbox_client.public_storage().get(path).await {
            Ok(response) => response,
            Err(pubky::Error::Request(pubky::errors::RequestError::Server { status, .. }))
                if status == pubky::StatusCode::NOT_FOUND || status == pubky::StatusCode::GONE =>
            {
                return Ok(None)
            }
            Err(err) => {
                return Err(PaykitError::Transport {
                    context: "fetch next Private Application Message ciphertext".into(),
                    source: err.into(),
                })
            }
        };
        // Bound hostile outbox bodies before buffering or authenticating them.
        let ciphertext = crate::pubky_routing::read_bounded_body(
            response,
            pubky_noise::snow_crypto::PUBKY_NOISE_TRANSPORT_PACKET_LEN,
            "read next Private Application Message ciphertext",
        )
        .await?;
        self.prepare_private_application_message_ciphertext(&ciphertext)
            .map(Some)
    }

    /// Prepare one supplied encrypted Private Application Message packet.
    ///
    /// This performs the same authenticated Noise receive and message decoding
    /// as [`prepare_next_private_application_message`](Self::prepare_next_private_application_message),
    /// without fetching the ciphertext. Persist the application result and
    /// returned snapshot atomically before acknowledging this value. A failure
    /// does not advance the link's receive checkpoint.
    pub fn prepare_private_application_message_ciphertext(
        &mut self,
        ciphertext: &[u8],
    ) -> Result<PreparedPrivateApplicationMessageReceive> {
        let prepared =
            self.encryptor
                .prepare_receive(ciphertext)
                .map_err(|err| PaykitError::Transport {
                    context: format!("prepare Private Application Message receive: {err:?}"),
                    source: anyhow::Error::new(crate::error::NonRetryablePrivateReceiveError(err)),
                })?;
        let message =
            private_application_message::decode_private_application_message(prepared.plaintext())?;
        let resulting_snapshot = EncryptedLinkSnapshot::from_state(
            prepared.resulting_session_state().clone(),
            self.recipient.clone(),
            self.remote_noise_public_key.clone(),
            self.recovery_context.clone(),
        );
        Ok(PreparedPrivateApplicationMessageReceive {
            prepared,
            message,
            resulting_snapshot,
        })
    }

    /// Acknowledge that a prepared receive and its application result are durable.
    pub fn acknowledge_persisted_private_receive(
        &mut self,
        prepared: PreparedPrivateApplicationMessageReceive,
    ) -> Result<()> {
        self.encryptor
            .acknowledge_persisted_receive(prepared.prepared)
            .map_err(|err| PaykitError::InvalidData {
                context: format!("acknowledge persisted private receive: {err:?}"),
                source: None,
            })
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

    #[cfg(test)]
    pub(crate) fn next_receive_path_for_test(&self) -> Result<String> {
        self.encryptor
            .next_receive_path()
            .map_err(|err| PaykitError::Transport {
                context: format!("next_receive_path: {err:?}"),
                source: anyhow::anyhow!("pubky-noise next_receive_path failed: {err:?}"),
            })
    }

    #[cfg(test)]
    pub(crate) fn enable_write_failure_for_test(&mut self) {
        self.encryptor.test_enable_write_failure();
    }

    #[cfg(test)]
    pub(crate) fn disable_write_failure_for_test(&mut self) {
        self.encryptor.test_disable_write_failure();
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

/// First slot at or after `start` for which `exists` is false.
///
/// Assumes stored slots are contiguous, so it gallops to a free slot and then
/// bisects back to the first one. That holds only while no slot of a stream
/// that link state can still write to has been deleted; see
/// [`crate::clear_encrypted_link_outbox`]. A free result can be
/// [`LAST_WRITE_SLOT`], which pubky-noise reports as exhausted on the next
/// write.
async fn first_free_slot<F, Fut>(start: u32, mut exists: F) -> Result<u32>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<bool>>,
{
    if !exists(start).await? {
        return Ok(start);
    }
    let mut taken = start;
    let mut step = 1u32;
    let mut free = loop {
        if taken >= LAST_WRITE_SLOT {
            return Err(PaykitError::InvalidData {
                context: "Encrypted Link write slots are exhausted".into(),
                source: None,
            });
        }
        let candidate = taken.saturating_add(step).min(LAST_WRITE_SLOT);
        if exists(candidate).await? {
            taken = candidate;
            step = step.saturating_mul(2);
        } else {
            break candidate;
        }
    };
    while free - taken > 1 {
        let middle = taken + (free - taken) / 2;
        if exists(middle).await? {
            taken = middle;
        } else {
            free = middle;
        }
    }
    Ok(free)
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
/// Download failures return [`PaykitError::Transport`]; retain the snapshot to retry.
/// Missing or invalid handshake transcripts return [`PaykitError::InvalidData`].
#[instrument(skip(session, secret_key, outbox_client, snapshot))]
pub async fn restore_encrypted_link(
    session: pubky::PubkySession,
    secret_key: [u8; 32],
    remote_identity_public_key: &PublicKey,
    outbox_client: pubky::Pubky,
    snapshot: EncryptedLinkSnapshot,
) -> Result<EncryptedLink> {
    debug!("restoring Encrypted Link from snapshot (raw params)");

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
    .map_err(|err| PaykitError::InvalidData {
        context: format!("failed to create encryptor config for restore: {err:?}"),
        source: Some(anyhow::anyhow!(
            "pubky-noise PubkyNoiseConfig::new failed: {err:?}"
        )),
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
/// The config's Noise key must match the snapshot's static key.
/// The config paths must match the session's local Pubky identity, the remote
/// identity, and the Noise keys; mismatches return [`PaykitError::Validation`].
/// Download failures return [`PaykitError::Transport`]; missing or invalid
/// handshake transcripts return [`PaykitError::InvalidData`].
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
    let recovery_context = snapshot.recovery_context().clone();
    validate_private_payment_paths(
        &config,
        remote_identity_public_key,
        &remote_noise_public_key,
        &recovery_context,
    )?;
    let state = snapshot.into_state();
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
    .map_err(|err| PaykitError::InvalidData {
        context: format!("failed to restore Encrypted Link: {err:?}"),
        source: Some(anyhow::anyhow!("pubky-noise restore failed: {err:?}")),
    })?;

    debug!("Encrypted Link restored successfully");
    Ok(EncryptedLink::from_parts(
        encryptor,
        remote_identity_public_key.clone(),
        remote_noise_public_key,
        recovery_context,
        config,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn first_free_slot_in(start: u32, stored: std::ops::Range<u32>) -> Result<u32> {
        first_free_slot(start, |slot| {
            let exists = stored.contains(&slot);
            async move { Ok(exists) }
        })
        .await
    }

    #[tokio::test]
    async fn test_first_free_slot_finds_end_of_stored_slots() {
        assert_eq!(first_free_slot_in(5, 0..5).await.unwrap(), 5);
        assert_eq!(first_free_slot_in(0, 0..0).await.unwrap(), 0);
        for end in 1..300 {
            assert_eq!(first_free_slot_in(0, 0..end).await.unwrap(), end);
            assert_eq!(first_free_slot_in(end / 2, 0..end).await.unwrap(), end);
        }
        assert_eq!(
            first_free_slot_in(7, 0..4_000_000_000).await.unwrap(),
            4_000_000_000
        );
    }

    #[tokio::test]
    async fn test_first_free_slot_uses_logarithmic_checks() {
        let mut checks = 0u32;
        let free = first_free_slot(0, |slot| {
            checks += 1;
            async move { Ok(slot < 1_000_000) }
        })
        .await
        .unwrap();
        assert_eq!(free, 1_000_000);
        // One start check, then at most 20 gallop and 20 bisect checks below 2^20.
        assert!(checks <= 41, "{checks} checks");
    }

    #[tokio::test]
    async fn test_first_free_slot_stops_at_exhausted_cursor() {
        assert_eq!(
            first_free_slot_in(0, 0..LAST_WRITE_SLOT).await.unwrap(),
            LAST_WRITE_SLOT
        );
        assert_eq!(
            first_free_slot_in(LAST_WRITE_SLOT - 1, 0..LAST_WRITE_SLOT)
                .await
                .unwrap(),
            LAST_WRITE_SLOT
        );
        assert!(matches!(
            first_free_slot_in(0, 0..u32::MAX).await,
            Err(PaykitError::InvalidData { .. })
        ));
        assert!(matches!(
            first_free_slot_in(LAST_WRITE_SLOT, 0..u32::MAX).await,
            Err(PaykitError::InvalidData { .. })
        ));
    }

    #[tokio::test]
    async fn test_first_free_slot_returns_check_errors() {
        let result = first_free_slot(0, |slot| async move {
            if slot < 3 {
                Ok(true)
            } else {
                Err(PaykitError::Validation("check failed".into()))
            }
        })
        .await;
        assert!(matches!(result, Err(PaykitError::Validation(_))));
    }

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
