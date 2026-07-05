use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    pub(super) async fn ensure_peer_allows_private_automation(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let (peer_state, has_active_link) = self
            .storage
            .transaction(|tx| {
                let peer_state = tx.linked_peer(counterparty).map(|peer| peer.state);
                let has_active_link = tx
                    .encrypted_link_state(counterparty)
                    .and_then(|state| state.link_snapshot)
                    .is_some();
                Ok((peer_state, has_active_link))
            })
            .await?;
        match peer_state {
            Some(LinkedPeerState::Linked) if has_active_link => Ok(()),
            Some(LinkedPeerState::Linking) => Err(PaykitSdkError::RecoveryRequired(format!(
                "Encrypted Link Handshake is still in progress for counterparty {counterparty}"
            ))),
            Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired(
                format!("Encrypted Link recovery is required for counterparty {counterparty}"),
            )),
            Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            ))),
            _ => Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            ))),
        }
    }

    pub(super) async fn private_queue_readiness(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateQueueReadiness> {
        let (peer_state, has_active_link, has_restorable_handshake) = self
            .storage
            .transaction(|tx| {
                let peer_state = tx.linked_peer(counterparty).map(|peer| peer.state);
                let state = tx.encrypted_link_state(counterparty);
                let has_active_link = state
                    .as_ref()
                    .and_then(|state| state.link_snapshot.as_ref())
                    .is_some();
                let has_restorable_handshake = state.as_ref().is_some_and(|state| {
                    state.handshake_snapshot.is_some() && state.handshake_role.is_some()
                });
                Ok((peer_state, has_active_link, has_restorable_handshake))
            })
            .await?;
        let Some(peer_state) = peer_state else {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active or in-progress Encrypted Link state for counterparty {counterparty}"
            )));
        };
        match peer_state {
            LinkedPeerState::Linked if has_active_link => Ok(PrivateQueueReadiness::Ready),
            LinkedPeerState::Linking if has_restorable_handshake => {
                Ok(PrivateQueueReadiness::PendingHandshake)
            }
            LinkedPeerState::Linking => Err(PaykitSdkError::RecoveryRequired(format!(
                "Encrypted Link Handshake state is incomplete for counterparty {counterparty}"
            ))),
            LinkedPeerState::RecoveryRequired => Err(PaykitSdkError::RecoveryRequired(format!(
                "Encrypted Link recovery is required for counterparty {counterparty}"
            ))),
            LinkedPeerState::Blocked => Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            ))),
            _ => Err(PaykitSdkError::RecoveryRequired(format!(
                "no active or in-progress Encrypted Link state for counterparty {counterparty}"
            ))),
        }
    }

    pub(super) async fn ensure_peer_not_blocked(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(counterparty).map(|peer| peer.state)))
            .await?;
        if matches!(peer_state, Some(LinkedPeerState::Blocked)) {
            Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            )))
        } else {
            Ok(())
        }
    }

    pub(super) async fn ensure_peer_not_recovery_required_or_blocked(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(counterparty).map(|peer| peer.state)))
            .await?;
        match peer_state {
            Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired(
                format!("Encrypted Link recovery is required for counterparty {counterparty}"),
            )),
            Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            ))),
            _ => Ok(()),
        }
    }

    /// Block a counterparty for local Paykit private workflows.
    ///
    /// Blocking is local policy. It clears stored Encrypted Link state so the
    /// peer cannot resume private workflows until explicitly unblocked and
    /// linked again.
    pub async fn block_peer(&self, counterparty: PubkyPublicKey) -> Result<LinkedPeerRecord> {
        let local_public_key = self.require_initialized_identity("block peer").await?;
        if counterparty == local_public_key {
            return Err(PaykitSdkError::Policy(
                "cannot block the local Paykit identity".into(),
            ));
        }
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .block_peer_with_claim(counterparty, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn block_peer_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerRecord> {
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let mut record = tx
                    .linked_peer(&counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                record.state = LinkedPeerState::Blocked;
                record.last_sync_at = Some(now);
                record.failure_count = 0;
                tx.save_linked_peer(record.clone());
                clear_encrypted_link_state(tx, &counterparty, now);
                Ok(record)
            })
            .await
    }

    /// Remove a local peer block and return the peer to `NotLinked`.
    ///
    /// Existing Encrypted Link snapshots are not restored. Callers should start
    /// a fresh Encrypted Link Handshake before private workflows resume.
    pub async fn unblock_peer(&self, counterparty: PubkyPublicKey) -> Result<LinkedPeerRecord> {
        let local_public_key = self.require_initialized_identity("unblock peer").await?;
        if counterparty == local_public_key {
            return Err(PaykitSdkError::Policy(
                "cannot unblock the local Paykit identity".into(),
            ));
        }
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .unblock_peer_with_claim(counterparty, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn unblock_peer_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerRecord> {
        let now = self.clock.now();
        self.storage
            .transaction(move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let mut record = tx
                    .linked_peer(&counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                if record.state != LinkedPeerState::Blocked {
                    return Ok(record);
                }
                record.state = LinkedPeerState::NotLinked;
                record.last_sync_at = Some(now);
                record.failure_count = 0;
                tx.save_linked_peer(record.clone());
                clear_encrypted_link_state(tx, &counterparty, now);
                Ok(record)
            })
            .await
    }

    /// Start an Encrypted Link Handshake as the initiator.
    pub async fn initiate_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Initiator)
            .await
    }

    /// Start an Encrypted Link Handshake as the responder.
    pub async fn accept_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Responder)
            .await
    }

    /// Advance the stored Encrypted Link Handshake for one counterparty.
    pub async fn advance_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_peer_not_recovery_required_or_blocked(&counterparty)
            .await?;
        let _ = self.private_link_session_access().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .advance_link_handshake_with_claim(counterparty, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Ensure an Encrypted Link is started or advanced for one counterparty.
    ///
    /// The SDK deterministically chooses the local handshake role from the two
    /// public keys. Existing active links are returned as linked. Existing
    /// pending handshakes are advanced. `max_advance_steps` bounds how many
    /// stored handshake advances this call attempts after starting or finding a
    /// pending handshake.
    pub async fn ensure_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
        max_advance_steps: u32,
    ) -> Result<LinkedPeerHandshakeReport> {
        let (session_access, _) = self.private_link_session_access().await?;
        let local_public_key = session_access.public_key()?;
        if local_public_key == counterparty {
            return Err(PaykitSdkError::Policy(
                "cannot establish an Encrypted Link with the local identity".into(),
            ));
        }
        let role = deterministic_handshake_role(&local_public_key, &counterparty);
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .ensure_link_with_peer_with_claim(counterparty, role, max_advance_steps, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    pub(super) async fn ensure_link_with_peer_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
        max_advance_steps: u32,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        let (peer_state, link_state) = self
            .storage
            .transaction(|tx| {
                Ok((
                    tx.linked_peer(&counterparty).map(|peer| peer.state),
                    tx.encrypted_link_state(&counterparty),
                ))
            })
            .await?;

        let mut report = match (peer_state, link_state) {
            (Some(LinkedPeerState::RecoveryRequired), _) => {
                self.start_link_handshake_with_claim(counterparty.clone(), role, lease.clone())
                    .await?
            }
            (_, Some(state)) if state.link_snapshot.is_some() => {
                save_linked_peer_state_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    LinkedPeerState::Linked,
                    lease.clone(),
                    self.clock.now(),
                )
                .await?;
                LinkedPeerHandshakeReport {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    generation: state.generation,
                    handshake_role: None,
                }
            }
            (_, Some(state)) if state.handshake_snapshot.is_some() => {
                if state.handshake_role.is_none() {
                    let mark = mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        self.clock.now(),
                    )
                    .await?;
                    self.publish_local_recovery_marker_if_possible(&counterparty, mark.new_episode)
                        .await;
                    return Err(PaykitSdkError::RecoveryRequired(format!(
                        "missing Encrypted Link Handshake role for counterparty {counterparty}"
                    )));
                }
                save_linked_peer_state_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    LinkedPeerState::Linking,
                    lease.clone(),
                    self.clock.now(),
                )
                .await?;
                LinkedPeerHandshakeReport {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linking,
                    generation: state.generation,
                    handshake_role: state.handshake_role,
                }
            }
            _ => {
                self.start_link_handshake_with_claim(counterparty.clone(), role, lease.clone())
                    .await?
            }
        };

        for _ in 0..max_advance_steps {
            if report.state == LinkedPeerState::Linked {
                return Ok(report);
            }
            report = match self
                .advance_link_handshake_with_claim(counterparty.clone(), lease.clone())
                .await
            {
                Ok(report) => report,
                Err(err) if Self::link_handshake_error_requires_recovery(&err) => {
                    let recovery_required = self
                        .storage
                        .transaction(|tx| {
                            Ok(tx.linked_peer(&counterparty).is_some_and(|peer| {
                                peer.state == LinkedPeerState::RecoveryRequired
                            }))
                        })
                        .await?;
                    if !recovery_required {
                        return Err(err);
                    }
                    self.start_link_handshake_with_claim(counterparty.clone(), role, lease.clone())
                        .await?
                }
                Err(err) => return Err(err),
            };
        }

        Ok(report)
    }

    async fn advance_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_peer_not_recovery_required_or_blocked(&counterparty)
            .await?;
        let Some(stored_link_state) = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
        else {
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no Encrypted Link state for counterparty {counterparty}"
            )));
        };
        if stored_link_state.link_snapshot.is_some() {
            save_linked_peer_state_with_lease(
                &self.storage,
                counterparty.clone(),
                LinkedPeerState::Linked,
                lease.clone(),
                self.clock.now(),
            )
            .await?;
            return Ok(LinkedPeerHandshakeReport {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::Linked,
                generation: stored_link_state.generation,
                handshake_role: None,
            });
        }

        let Some(handshake_role) = stored_link_state.handshake_role else {
            self.mark_link_recovery_required(&counterparty, lease)
                .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "missing Encrypted Link Handshake role for counterparty {counterparty}"
            )));
        };
        let Some(snapshot_bytes) = stored_link_state.handshake_snapshot.as_ref() else {
            self.mark_link_recovery_required(&counterparty, lease)
                .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no in-progress Encrypted Link Handshake snapshot for counterparty {counterparty}"
            )));
        };

        let handshake = match self
            .restore_link_handshake_from_snapshot(counterparty.clone(), snapshot_bytes)
            .await
        {
            Ok(handshake) => handshake,
            Err(err) => {
                if Self::link_handshake_error_requires_recovery(&err) {
                    self.mark_link_recovery_required(&counterparty, lease)
                        .await?;
                }
                return Err(err);
            }
        };

        self.advance_restored_link_handshake(
            counterparty,
            handshake,
            handshake_role,
            stored_link_state.generation,
            lease,
        )
        .await
    }

    async fn mark_link_recovery_required(
        &self,
        counterparty: &PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<()> {
        let mark = mark_recovery_required_with_lease(
            &self.storage,
            counterparty.clone(),
            lease,
            self.clock.now(),
        )
        .await?;
        self.publish_local_recovery_marker_if_possible(counterparty, mark.new_episode)
            .await;
        Ok(())
    }

    async fn restore_link_handshake_from_snapshot(
        &self,
        counterparty: PubkyPublicKey,
        snapshot_bytes: &[u8],
    ) -> Result<paykit_lib::EncryptedLinkHandshake> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)?;
        paykit_lib::restore_encrypted_link_handshake(
            session_access.session,
            secret_key,
            &remote_public_key,
            session_access.outbox_client,
            snapshot,
        )
        .await
        .map_err(Into::into)
    }

    fn link_handshake_error_requires_recovery(err: &PaykitSdkError) -> bool {
        matches!(
            err,
            PaykitSdkError::Transport { .. }
                | PaykitSdkError::NotFound(_)
                | PaykitSdkError::Protocol(_)
                | PaykitSdkError::RecoveryRequired(_)
        )
    }

    async fn advance_restored_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
        handshake: paykit_lib::EncryptedLinkHandshake,
        handshake_role: EncryptedLinkHandshakeRole,
        expected_generation: u64,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        let progress = match paykit_lib::advance_handshake(handshake).await {
            Ok(progress) => progress,
            Err(err) => {
                let err = PaykitSdkError::from(err);
                if Self::link_handshake_error_requires_recovery(&err) {
                    self.mark_link_recovery_required(&counterparty, lease)
                        .await?;
                }
                return Err(err);
            }
        };

        match progress {
            paykit_lib::HandshakeProgress::Pending(handshake) => {
                save_link_handshake_state_if_generation_with_lease(
                    &self.storage,
                    counterparty,
                    handshake_role,
                    handshake.serialize(),
                    expected_generation,
                    lease,
                    self.clock.now(),
                )
                .await
            }
            paykit_lib::HandshakeProgress::Complete(link) => {
                let report = save_linked_peer_link_state_if_generation_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    link.serialize(),
                    expected_generation,
                    lease,
                    self.clock.now(),
                )
                .await?;
                self.remove_local_recovery_marker_if_recorded(&counterparty)
                    .await?;
                Ok(report)
            }
        }
    }

    async fn start_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
    ) -> Result<LinkedPeerHandshakeReport> {
        let _ = self.private_link_session_access().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .start_link_handshake_with_claim(counterparty, role, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    pub(super) async fn start_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
        lease: PeerLinkOperationLease,
    ) -> Result<LinkedPeerHandshakeReport> {
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(&counterparty).map(|peer| peer.state)))
            .await?;
        if matches!(peer_state, Some(LinkedPeerState::Blocked)) {
            return Err(PaykitSdkError::Policy(format!(
                "counterparty {counterparty} is blocked"
            )));
        }

        if !matches!(peer_state, Some(LinkedPeerState::RecoveryRequired)) {
            if let Some(existing) = self
                .storage
                .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
                .await?
            {
                if existing.link_snapshot.is_some() {
                    save_linked_peer_state_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        LinkedPeerState::Linked,
                        lease.clone(),
                        self.clock.now(),
                    )
                    .await?;
                    return Ok(LinkedPeerHandshakeReport {
                        counterparty,
                        state: LinkedPeerState::Linked,
                        generation: existing.generation,
                        handshake_role: None,
                    });
                }
                if existing.handshake_snapshot.is_some() {
                    if existing.handshake_role.is_none() {
                        let mark = mark_recovery_required_with_lease(
                            &self.storage,
                            counterparty.clone(),
                            lease.clone(),
                            self.clock.now(),
                        )
                        .await?;
                        self.publish_local_recovery_marker_if_possible(
                            &counterparty,
                            mark.new_episode,
                        )
                        .await;
                        return Err(PaykitSdkError::RecoveryRequired(format!(
                            "missing Encrypted Link Handshake role for counterparty {counterparty}"
                        )));
                    }
                    save_linked_peer_state_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        LinkedPeerState::Linking,
                        lease.clone(),
                        self.clock.now(),
                    )
                    .await?;
                    return Ok(LinkedPeerHandshakeReport {
                        counterparty,
                        state: LinkedPeerState::Linking,
                        generation: existing.generation,
                        handshake_role: existing.handshake_role,
                    });
                }
            }
        }

        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        if matches!(peer_state, Some(LinkedPeerState::RecoveryRequired)) {
            paykit_lib::clear_encrypted_link_outbox(
                &session_access.session,
                &secret_key,
                &remote_public_key,
            )
            .await?;
        }
        let handshake = match role {
            EncryptedLinkHandshakeRole::Initiator => paykit_lib::initiate_encrypted_link(
                session_access.session,
                secret_key,
                &remote_public_key,
                session_access.outbox_client,
            )?,
            EncryptedLinkHandshakeRole::Responder => paykit_lib::accept_encrypted_link(
                session_access.session,
                secret_key,
                &remote_public_key,
                session_access.outbox_client,
            )?,
        };

        save_link_handshake_state_with_lease(
            &self.storage,
            counterparty,
            role,
            handshake.serialize(),
            lease,
            self.clock.now(),
        )
        .await
    }

    pub(super) async fn claim_peer_link_operation(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PeerLinkOperationLease> {
        let now = self.clock.now();
        let lease_timeout = ChronoDuration::from_std(self.config.peer_link_operation_lease_timeout)
            .map_err(|err| {
                PaykitSdkError::Policy(format!("invalid peer link lease timeout: {err}"))
            })?;
        let expires_at = now + lease_timeout;
        self.storage
            .transaction(|tx| Ok(tx.claim_peer_link_operation(counterparty, now, expires_at)))
            .await?
            .ok_or_else(|| {
                PaykitSdkError::Policy(format!(
                    "peer link operation already in progress for counterparty {counterparty}"
                ))
            })
    }

    pub(super) async fn release_peer_link_operation(
        &self,
        lease: &PeerLinkOperationLease,
    ) -> Result<()> {
        self.storage
            .transaction(|tx| {
                tx.release_peer_link_operation(&lease.counterparty, lease.lease_id);
                Ok(())
            })
            .await
    }

    pub(super) async fn finish_peer_link_operation<T>(
        &self,
        lease: PeerLinkOperationLease,
        result: Result<T>,
    ) -> Result<T> {
        let release_result = self.release_peer_link_operation(&lease).await;
        match (result, release_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    pub(super) async fn private_link_session_access(
        &self,
    ) -> Result<(PubkySessionAccess, [u8; 32])> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let secret_key = *session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "local Pubky secret key is unavailable for Encrypted Links".into(),
                source: None,
            })?
            .as_bytes();
        Ok((session_access, secret_key))
    }
}

fn clear_encrypted_link_state(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) {
    if let Some(link_state) = tx.encrypted_link_state(counterparty) {
        tx.save_encrypted_link_state(EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: link_state.generation.saturating_add(1),
            checkpointed_at: now,
        });
    }
}

fn deterministic_handshake_role(
    local_public_key: &PubkyPublicKey,
    counterparty: &PubkyPublicKey,
) -> EncryptedLinkHandshakeRole {
    if local_public_key.as_str() < counterparty.as_str() {
        EncryptedLinkHandshakeRole::Initiator
    } else {
        EncryptedLinkHandshakeRole::Responder
    }
}
