//! Explicit, empty-only publication across a local peer block.
//!
//! The packet is prepared before blocking. Retained ciphertext never restores
//! a Noise session and cannot become general send or receive authority.
use super::*;
use crate::storage::{PrivatePaymentListWithdrawalRecord, ValidatedStorageState};
use crate::{PrivatePaymentListWithdrawal, PrivatePaymentListWithdrawalAcknowledgement};

fn refused() -> PaykitSdkError {
    PaykitSdkError::Policy {
        context: "original private list withdrawal is unavailable or no longer authorized".into(),
        source: None,
    }
}

#[derive(PartialEq, Eq)]
struct WithdrawalGrant {
    owner: PubkyPublicKey,
    homeserver: PubkyPublicKey,
    grant_id: String,
    client_id: String,
    noise_public_key: PubkyPublicKey,
}

impl WithdrawalGrant {
    async fn capture(access: &PubkySessionAccess) -> Result<Self> {
        access.validate_for_capabilities(PAYKIT_SESSION_CAPABILITIES)?;
        let grant = access.session.as_grant().ok_or_else(refused)?;
        let info = grant.session_info().await;
        if info.pubky != *access.session.info().public_key()
            || info.grant_id != grant.grant_id().await
        {
            return Err(refused());
        }
        let key = access.paykit_identity_secret_key().ok_or_else(refused)?;
        Ok(Self {
            owner: access.public_key()?,
            homeserver: PubkyPublicKey::from_public_key(&info.homeserver),
            grant_id: info.grant_id.to_string(),
            client_id: info.client_id.to_string(),
            noise_public_key: crate::storage::paykit_noise_public_key(&key),
        })
    }

    fn matches(&self, record: &PrivatePaymentListWithdrawalRecord) -> bool {
        self.matches_original_grant(record) && self.noise_public_key == record.noise_public_key
    }

    fn matches_original_grant(&self, record: &PrivatePaymentListWithdrawalRecord) -> bool {
        self.owner == record.owner
            && self.homeserver == record.homeserver
            && self.grant_id == record.grant_id
            && self.client_id == record.client_id
    }
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Prepare exactly one empty Private Payment List and atomically block its peer.
    ///
    /// Call only after explicit confirmation to withdraw this app's private list.
    /// No withdrawal packet is published here. Retain the returned handle with the original
    /// owner/app operation and pass it to [`Self::retry_private_payment_list_withdrawal`].
    /// An uncertain storage result must be reconciled by calling this method
    /// again with the same original session. Already-lost or blocked state with
    /// no retained intent refuses; no handshake, relink, or unblock is attempted.
    /// An allocated nonempty/Event slot, or a newer competing list, also refuses.
    ///
    /// Normal block APIs do not create this intent. Backup export is refused
    /// while retained withdrawals exist; live state blobs preserve them. Session
    /// creation, capabilities, and key rotation remain the caller's concern.
    pub async fn prepare_private_payment_list_withdrawal_and_block(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivatePaymentListWithdrawal> {
        let access = self
            .load_session_access_for_initialized_identity("prepare private list withdrawal")
            .await?;
        let binding = WithdrawalGrant::capture(&access).await?;
        let _identity = self.claim_identity_operation("prepare private list withdrawal")?;
        self.with_guarded_storage_operation(
            Arc::clone(&access._guard),
            Box::pin(async {
                let app_lease = self.claim_paykit_app_operation().await?;
                let result = async {
                    let lease = self.claim_peer_link_operation(&counterparty).await?;
                    let result = self
                        .prepare_withdrawal_with_leases(
                            &counterparty,
                            &binding,
                            &access,
                            &app_lease,
                            &lease,
                        )
                        .await;
                    self.finish_peer_link_operation(lease, result).await
                }
                .await;
                self.finish_paykit_app_operation(app_lease, result).await
            }),
        )
        .await
    }

    async fn prepare_withdrawal_with_leases(
        &self,
        peer: &PubkyPublicKey,
        binding: &WithdrawalGrant,
        access: &PubkySessionAccess,
        app_lease: &PaykitAppOperationLease,
        lease: &PeerLinkOperationLease,
    ) -> Result<PrivatePaymentListWithdrawal> {
        let retained = self
            .storage
            .transaction(|tx| {
                self.require_withdrawal_leases(tx, binding, access, app_lease, lease)?;
                let state = tx.export_storage_state();
                if let Some(record) = state.private_payment_list_withdrawals.get(peer) {
                    self.require_retained_withdrawal(
                        &state,
                        record,
                        binding,
                        record.outbound_message_id,
                    )?;
                    return Ok(Some(withdrawal_handle(record)?));
                }
                Ok(None)
            })
            .await?;
        if let Some(id) = retained {
            return Ok(id);
        }
        let (mut link, original_state) = self
            .restore_link_for_outbound_send(peer, lease, access)
            .await?;
        if WithdrawalGrant::capture(access).await? != *binding {
            return Err(refused());
        }
        self.storage
            .transaction(|tx| {
                self.require_withdrawal_leases(tx, binding, access, app_lease, lease)?;
                if tx.encrypted_link_state(peer).as_ref() != Some(&original_state)
                    || tx
                        .linked_peer(peer)
                        .is_none_or(|p| p.state != LinkedPeerState::Linked)
                    || tx
                        .export_storage_state()
                        .private_payment_list_withdrawals
                        .contains_key(peer)
                {
                    return Err(refused());
                }
                let now = self.clock.now();
                let messages = tx.outbound_private_messages(peer);
                let reserved = messages
                    .iter()
                    .filter(|m| m.prepared_send.is_some())
                    .collect::<Vec<_>>();
                if reserved.len() > 1 {
                    return Err(refused());
                }
                let (message, prepared) = if let Some(message) = reserved.first() {
                    require_empty_message(message, &self.config.app_id)?;
                    if messages.iter().any(|other| {
                        other.app_id == self.config.app_id
                            && other.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                            && other.outbound_message_id > message.outbound_message_id
                    }) {
                        return Err(refused());
                    }
                    let prepared = message.prepared_send.clone().ok_or_else(refused)?;
                    link.validate_prepared_private_application_message(
                        &prepared.destination_path,
                        &prepared.ciphertext,
                    )?;
                    ((*message).clone(), prepared)
                } else {
                    // No allocated slot may be skipped. This fresh packet is never
                    // published or acknowledged to the live Noise object before the
                    // packet and the irreversible local block commit together.
                    let raw = paykit_lib::serialize_private_payment_list_json(
                        &paykit_lib::PrivatePaymentList::new(
                            self.config.app_id.clone(),
                            HashMap::new(),
                        ),
                    )?;
                    let packet = link.prepare_private_application_message_json(&raw)?;
                    let message = tx.insert_outbound_private_message(
                        crate::storage::NewOutboundPrivateMessage::new(
                            peer.clone(),
                            self.config.app_id.clone(),
                            PrivateMessageKind::PrivatePaymentList.as_str().into(),
                            raw,
                            now,
                        ),
                    )?;
                    (
                        message,
                        crate::storage::PreparedOutboundPrivateSend {
                            destination_path: packet.destination_path().to_owned(),
                            ciphertext: packet.ciphertext().to_vec(),
                        },
                    )
                };
                // Drop the live transport, including its advanced snapshot. Only
                // this exact packet remains; no old key/nonce can encrypt again.
                super::encrypted_links::block_peer_with_lease_in_transaction(tx, peer, now, lease)?;
                let mut state = tx.export_storage_state();
                let generation = state
                    .encrypted_link_states
                    .get(peer)
                    .ok_or_else(refused)?
                    .generation;
                state.private_payment_list_withdrawals.insert(
                    peer.clone(),
                    PrivatePaymentListWithdrawalRecord {
                        owner: binding.owner.clone(),
                        app_id: self.config.app_id.clone(),
                        counterparty: peer.clone(),
                        outbound_message_id: message.outbound_message_id,
                        homeserver: binding.homeserver.clone(),
                        grant_id: binding.grant_id.clone(),
                        client_id: binding.client_id.clone(),
                        noise_public_key: binding.noise_public_key.clone(),
                        blocked_generation: generation,
                        prepared,
                        confirmed_at: None,
                        invalidated: false,
                    },
                );
                crate::validate_storage_state(&state)?;
                tx.replace_storage_state(ValidatedStorageState::new(state));
                withdrawal_handle(&tx.export_storage_state().private_payment_list_withdrawals[peer])
            })
            .await
    }

    /// Explicitly retry only the retained original empty-list packet.
    ///
    /// This performs one logical Pubky PUT, with the configured client's transport
    /// policy, and then commits a bound receipt. It never encrypts, drains queued
    /// work, restores a link, or unblocks the peer. Reconfirm uncertain outcomes
    /// before calling again. A receipt means publication, not counterparty receipt
    /// or completed wallet cleanup. Same-original-grant bearer refresh is allowed;
    /// replacement grants, owners, apps, keys, or block generations are refused.
    /// Dropped/failed attempts remain uncertain and retain the identical packet.
    pub async fn retry_private_payment_list_withdrawal(
        &self,
        withdrawal: PrivatePaymentListWithdrawal,
    ) -> Result<PrivatePaymentListWithdrawalAcknowledgement> {
        let counterparty = withdrawal.counterparty.clone();
        let original_message_id = withdrawal.outbound_message_id;
        let access = self
            .load_session_access_for_initialized_identity("retry private list withdrawal")
            .await?;
        let binding = WithdrawalGrant::capture(&access).await?;
        let _identity = self.claim_identity_operation("retry private list withdrawal")?;
        self.with_guarded_storage_operation(
            Arc::clone(&access._guard),
            Box::pin(async {
                let app_lease = self.claim_paykit_app_operation().await?;
                let result = async {
                    let lease = self.claim_peer_link_operation(&counterparty).await?;
                    let result = async {
                        let record = self
                            .storage
                            .transaction(|tx| {
                                self.require_withdrawal_leases(
                                    tx, &binding, &access, &app_lease, &lease,
                                )?;
                                let mut state = tx.export_storage_state();
                                let record = state
                                    .private_payment_list_withdrawals
                                    .get(&counterparty)
                                    .ok_or_else(refused)?
                                    .clone();
                                self.require_retained_withdrawal(
                                    &state,
                                    &record,
                                    &binding,
                                    original_message_id,
                                )?;
                                require_original_handle(&record, &withdrawal)?;
                                if record.confirmed_at.is_none() {
                                    let message = state
                                        .outbound_private_messages
                                        .iter_mut()
                                        .find(|m| m.outbound_message_id == original_message_id)
                                        .ok_or_else(refused)?;
                                    message.attempt_count = message.attempt_count.saturating_add(1);
                                    message.last_attempt_at = Some(self.clock.now());
                                    message.updated_at = self.clock.now();
                                    crate::validate_storage_state(&state)?;
                                    tx.replace_storage_state(ValidatedStorageState::new(state));
                                }
                                Ok(record)
                            })
                            .await?;
                        if record.confirmed_at.is_none() {
                            if WithdrawalGrant::capture(&access).await? != binding {
                                return Err(refused());
                            }
                            // Session inspection above awaits. Recheck the durable
                            // admission after it; neither an earlier lease nor an
                            // unchanged grant authorizes a stale queued request.
                            self.storage
                                .transaction(|tx| {
                                    self.require_withdrawal_leases(
                                        tx, &binding, &access, &app_lease, &lease,
                                    )?;
                                    let state = tx.export_storage_state();
                                    let current = state
                                        .private_payment_list_withdrawals
                                        .get(&counterparty)
                                        .ok_or_else(refused)?;
                                    if current != &record {
                                        return Err(refused());
                                    }
                                    self.require_retained_withdrawal(
                                        &state,
                                        current,
                                        &binding,
                                        original_message_id,
                                    )
                                })
                                .await?;
                            // Only SDK-created, validated immutable packet bytes reach
                            // this write. No caller payload or destination is accepted.
                            access
                                .session
                                .storage()
                                .put(
                                    &record.prepared.destination_path,
                                    record.prepared.ciphertext.clone(),
                                )
                                .await
                                .map_err(|_| PaykitSdkError::Policy {
                                    context:
                                        "private list withdrawal publication outcome is unknown"
                                            .into(),
                                    source: None,
                                })?;
                        }
                        if WithdrawalGrant::capture(&access).await? != binding {
                            return Err(refused());
                        }
                        self.storage
                            .transaction(|tx| {
                                self.require_withdrawal_leases(
                                    tx, &binding, &access, &app_lease, &lease,
                                )?;
                                let mut state = tx.export_storage_state();
                                let current = state
                                    .private_payment_list_withdrawals
                                    .get(&counterparty)
                                    .ok_or_else(refused)?;
                                if current != &record {
                                    return Err(refused());
                                }
                                self.require_retained_withdrawal(
                                    &state,
                                    current,
                                    &binding,
                                    original_message_id,
                                )?;
                                let now = self.clock.now();
                                for message in &mut state.outbound_private_messages {
                                    if message.counterparty != counterparty
                                        || message.app_id != self.config.app_id
                                        || message.kind
                                            != PrivateMessageKind::PrivatePaymentList.as_str()
                                    {
                                        continue;
                                    }
                                    if message.outbound_message_id == original_message_id {
                                        *message = mark_outbound_sent(message.clone(), now);
                                    } else if message.outbound_message_id < original_message_id
                                        && matches!(
                                            message.status,
                                            OutboundPrivateMessageStatus::Pending
                                                | OutboundPrivateMessageStatus::Sending
                                                | OutboundPrivateMessageStatus::Failed
                                                | OutboundPrivateMessageStatus::RecoveryRequired
                                        )
                                    {
                                        message.last_error = None;
                                        message.status = OutboundPrivateMessageStatus::Superseded;
                                        message.prepared_send = None;
                                        message.updated_at = now;
                                    }
                                }
                                state
                                    .private_payment_list_withdrawals
                                    .get_mut(&counterparty)
                                    .ok_or_else(refused)?
                                    .confirmed_at
                                    .get_or_insert(now);
                                crate::validate_storage_state(&state)?;
                                tx.replace_storage_state(ValidatedStorageState::new(state));
                                Ok(PrivatePaymentListWithdrawalAcknowledgement {
                                    withdrawal: withdrawal.clone(),
                                })
                            })
                            .await
                    }
                    .await;
                    self.finish_peer_link_operation(lease, result).await
                }
                .await;
                self.finish_paykit_app_operation(app_lease, result).await
            }),
        )
        .await
    }

    /// Consume a completed withdrawal receipt after the caller durably accepts it.
    ///
    /// Removes only its ciphertext journal, allowing backup export again. Sent
    /// and attempt history remain. Missing, pending, and invalidated unknown
    /// work cannot be finalized. The original grant may consume its already
    /// confirmed receipt after key rotation; this never permits another send.
    /// This is not permission to erase credentials.
    pub async fn finish_private_payment_list_withdrawal(
        &self,
        withdrawal: PrivatePaymentListWithdrawal,
    ) -> Result<()> {
        let counterparty = withdrawal.counterparty.clone();
        let original_message_id = withdrawal.outbound_message_id;
        let access = self
            .load_session_access_for_initialized_identity("finish private list withdrawal")
            .await?;
        let binding = WithdrawalGrant::capture(&access).await?;
        self.with_guarded_storage_operation(
            Arc::clone(&access._guard),
            Box::pin(async {
                self.storage
                    .transaction(|tx| {
                        let mut state = tx.export_storage_state();
                        let record = state
                            .private_payment_list_withdrawals
                            .get(&counterparty)
                            .ok_or_else(refused)?;
                        require_original_handle(record, &withdrawal)?;
                        if !binding.matches_original_grant(record)
                            || record.app_id != self.config.app_id
                            || record.outbound_message_id != original_message_id
                            || record.confirmed_at.is_none()
                            || state
                                .peer_link_operation_leases
                                .get(&counterparty)
                                .is_some_and(|l| l.expires_at > self.clock.now())
                        {
                            return Err(refused());
                        }
                        crate::validate_storage_state(&state)?;
                        state.private_payment_list_withdrawals.remove(&counterparty);
                        tx.replace_storage_state(ValidatedStorageState::new(state));
                        Ok(())
                    })
                    .await
            }),
        )
        .await
    }

    fn require_withdrawal_leases(
        &self,
        tx: &mut dyn StorageTransaction,
        binding: &WithdrawalGrant,
        access: &PubkySessionAccess,
        app_lease: &PaykitAppOperationLease,
        lease: &PeerLinkOperationLease,
    ) -> Result<()> {
        self.require_current_peer_link_operation_in_transaction(tx, lease, access)?;
        crate::storage::require_paykit_app_operation_lease(tx, app_lease)?;
        if app_lease.expires_at <= self.clock.now()
            || !tx.paykit_app_is_registered(&self.config.app_id)
            || tx.paykit_app_is_retired(&self.config.app_id)
            || initialized_identity_in_transaction(tx, "private list withdrawal")? != binding.owner
            || tx.paykit_noise_public_key().as_ref() != Some(&binding.noise_public_key)
        {
            return Err(refused());
        }
        Ok(())
    }

    fn require_retained_withdrawal(
        &self,
        state: &crate::storage::StorageState,
        record: &PrivatePaymentListWithdrawalRecord,
        binding: &WithdrawalGrant,
        message_id: u64,
    ) -> Result<()> {
        if !binding.matches(record)
            || record.app_id != self.config.app_id
            || record.outbound_message_id != message_id
            || record.invalidated
            || state
                .linked_peers
                .get(&record.counterparty)
                .is_none_or(|p| p.state != LinkedPeerState::Blocked)
            || state
                .encrypted_link_states
                .get(&record.counterparty)
                .is_none_or(|s| {
                    s.generation != record.blocked_generation
                        || s.link_snapshot.is_some()
                        || s.handshake_snapshot.is_some()
                })
        {
            return Err(refused());
        }
        crate::validate_storage_state(state)
    }
}

fn require_empty_message(
    message: &OutboundPrivateMessageRecord,
    app: &paykit_lib::PaykitAppId,
) -> Result<()> {
    if message.app_id != *app || message.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
        return Err(refused());
    }
    let list = paykit_lib::parse_private_payment_list_json(&message.raw_json)?;
    if list.app_id() != app || !list.is_empty() {
        return Err(refused());
    }
    Ok(())
}

fn withdrawal_handle(
    record: &PrivatePaymentListWithdrawalRecord,
) -> Result<PrivatePaymentListWithdrawal> {
    let encoded = postcard::to_allocvec(&(
        "paykit.private_payment_list.withdrawal.v1",
        &record.owner,
        &record.app_id,
        &record.counterparty,
        record.outbound_message_id,
        &record.homeserver,
        &record.grant_id,
        &record.client_id,
        &record.noise_public_key,
        record.blocked_generation,
        &record.prepared.destination_path,
        &record.prepared.ciphertext,
    ))
    .map_err(|_| refused())?;
    Ok(PrivatePaymentListWithdrawal {
        owner: record.owner.clone(),
        app_id: record.app_id.clone(),
        counterparty: record.counterparty.clone(),
        outbound_message_id: record.outbound_message_id,
        original_binding: blake3::hash(&encoded).to_hex().to_string(),
    })
}

fn require_original_handle(
    record: &PrivatePaymentListWithdrawalRecord,
    original: &PrivatePaymentListWithdrawal,
) -> Result<()> {
    if withdrawal_handle(record)? != *original {
        return Err(refused());
    }
    Ok(())
}
