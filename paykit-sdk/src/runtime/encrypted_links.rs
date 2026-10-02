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
        require_private_automation_ready(peer_state, has_active_link, counterparty)
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
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no active or in-progress Encrypted Link state for counterparty {counterparty}"
                ),
                source: None,
            });
        };
        match peer_state {
            LinkedPeerState::Linked if has_active_link => Ok(PrivateQueueReadiness::Ready),
            LinkedPeerState::Linking if has_restorable_handshake => {
                Ok(PrivateQueueReadiness::PendingHandshake)
            }
            LinkedPeerState::Linking => Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "Encrypted Link Handshake state is incomplete for counterparty {counterparty}"
                ),
                source: None,
            }),
            LinkedPeerState::RecoveryRequired => Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "Encrypted Link recovery is required for counterparty {counterparty}"
                ),
                source: None,
            }),
            LinkedPeerState::Blocked => Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            }),
            _ => Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no active or in-progress Encrypted Link state for counterparty {counterparty}"
                ),
                source: None,
            }),
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
            Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            })
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
            Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "Encrypted Link recovery is required for counterparty {counterparty}"
                ),
                source: None,
            }),
            Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            }),
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
            return Err(PaykitSdkError::Policy {
                context: "cannot block the local Paykit identity".into(),
                source: None,
            });
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
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                for message in tx.outbound_private_messages(&counterparty) {
                    if matches!(
                        message.status,
                        OutboundPrivateMessageStatus::Pending
                            | OutboundPrivateMessageStatus::Sending
                            | OutboundPrivateMessageStatus::Failed
                            | OutboundPrivateMessageStatus::RecoveryRequired
                    ) {
                        tx.save_outbound_private_message(mark_outbound_recovery_required(
                            message,
                            "counterparty is blocked; a fresh Encrypted Link is required".into(),
                            now,
                        ))?;
                    }
                }
                let mut record = tx
                    .linked_peer(&counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                record.state = LinkedPeerState::Blocked;
                record.last_sync_at = Some(now);
                record.failure_count = 0;
                tx.save_linked_peer(record.clone());
                clear_encrypted_link_state(tx, &counterparty, now);
                Ok(record)
            }
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
            return Err(PaykitSdkError::Policy {
                context: "cannot unblock the local Paykit identity".into(),
                source: None,
            });
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
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let mut record = tx
                    .linked_peer(&counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                if record.state != LinkedPeerState::Blocked {
                    return Ok(record);
                }
                record.state = LinkedPeerState::NotLinked;
                record.local_recovery_attempt_id = None;
                record.local_recovery_marker_created_at = None;
                record.local_recovery_marker_last_error = None;
                record.last_sync_at = Some(now);
                record.failure_count = 0;
                tx.save_linked_peer(record.clone());
                clear_encrypted_link_state(tx, &counterparty, now);
                Ok(record)
            }
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
        let (session_access, _) = self.private_link_session_access().await?;
        drop(session_access);
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        // Keep the nested handshake future off the caller's stack.
        let result =
            Box::pin(self.advance_link_handshake_with_claim(counterparty, lease.clone(), None))
                .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Ensure an Encrypted Link is started or advanced for one counterparty.
    ///
    /// The SDK deterministically chooses the local handshake role from the two
    /// public keys. Recovery markers are checked before reusing a link or
    /// advancing a handshake. Existing active links are returned as linked. Existing
    /// pending handshakes are advanced. `max_advance_steps` bounds how many
    /// stored handshake advances this call attempts after starting or finding a
    /// pending handshake.
    pub async fn ensure_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
        max_advance_steps: u32,
    ) -> Result<LinkedPeerHandshakeReport> {
        let role = {
            let (session_access, _) = self.private_link_session_access().await?;
            let local_public_key = session_access.public_key()?;
            require_distinct_link_identity(&local_public_key, &counterparty)?;
            deterministic_handshake_role(&local_public_key, &counterparty)
        };
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = Box::pin(self.ensure_link_with_peer_with_claim(
            counterparty,
            role,
            max_advance_steps,
            lease.clone(),
        ))
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
        let remote_noise_public_key = self.counterparty_noise_public_key(&counterparty).await?;
        {
            let (session_access, _) = self.private_link_session_access().await?;
            self.observe_remote_recovery_marker_with_lease(
                &counterparty,
                &session_access,
                &lease,
                &remote_noise_public_key,
            )
            .await?;
        }
        let (mut peer_state, link_state) = self
            .storage
            .transaction(|tx| {
                Ok((
                    tx.linked_peer(&counterparty).map(|peer| peer.state),
                    tx.encrypted_link_state(&counterparty),
                ))
            })
            .await?;

        if !matches!(
            peer_state,
            Some(LinkedPeerState::RecoveryRequired | LinkedPeerState::Blocked)
        ) {
            if let Some(state) = link_state.as_ref() {
                let snapshot_remote_key = if let Some(snapshot) = state.link_snapshot.as_ref() {
                    Some(
                        paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot)
                            .map(|snapshot| snapshot.remote_noise_public_key().clone()),
                    )
                } else {
                    state.handshake_snapshot.as_ref().map(|snapshot| {
                        paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot)
                            .map(|snapshot| snapshot.remote_noise_public_key().clone())
                    })
                };
                if let Some(snapshot_remote_key) = snapshot_remote_key {
                    let requires_recovery = match snapshot_remote_key {
                        Ok(snapshot_remote_key) => snapshot_remote_key != remote_noise_public_key,
                        Err(_) => true,
                    };
                    if requires_recovery {
                        self.mark_link_recovery_required(&counterparty, lease.clone(), None)
                            .await?;
                        peer_state = Some(LinkedPeerState::RecoveryRequired);
                    }
                }
            }
        }

        let mut report = match (peer_state, link_state) {
            (Some(LinkedPeerState::RecoveryRequired), _) => {
                self.start_link_handshake_with_claim(
                    counterparty.clone(),
                    role,
                    lease.clone(),
                    &remote_noise_public_key,
                )
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
                    mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        self.clock.now(),
                    )
                    .await?;
                    self.publish_local_recovery_marker_if_possible(&counterparty, &lease, None)
                        .await;
                    return Err(PaykitSdkError::RecoveryRequired {
                        context: format!(
                            "missing Encrypted Link Handshake role for counterparty {counterparty}"
                        ),
                        source: None,
                    });
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
                self.start_link_handshake_with_claim(
                    counterparty.clone(),
                    role,
                    lease.clone(),
                    &remote_noise_public_key,
                )
                .await?
            }
        };

        for _ in 0..max_advance_steps {
            if report.state == LinkedPeerState::Linked {
                return Ok(report);
            }
            report = match self
                .advance_link_handshake_with_claim(
                    counterparty.clone(),
                    lease.clone(),
                    Some(&remote_noise_public_key),
                )
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
                    self.start_link_handshake_with_claim(
                        counterparty.clone(),
                        role,
                        lease.clone(),
                        &remote_noise_public_key,
                    )
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
        remote_noise_public_key: Option<&paykit_lib::PublicKey>,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_peer_not_recovery_required_or_blocked(&counterparty)
            .await?;
        let Some(stored_link_state) = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
        else {
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!("no Encrypted Link state for counterparty {counterparty}"),
                source: None,
            });
        };
        let fetched_key;
        let remote_noise_public_key = match remote_noise_public_key {
            Some(key) => key,
            None => {
                fetched_key = self.counterparty_noise_public_key(&counterparty).await?;
                &fetched_key
            }
        };
        let (changed, role) = {
            let (session_access, _) = self.private_link_session_access().await?;
            let role = stored_link_state
                .handshake_role
                .unwrap_or(deterministic_handshake_role(
                    &session_access.public_key()?,
                    &counterparty,
                ));
            let changed = self
                .observe_remote_recovery_marker_with_lease(
                    &counterparty,
                    &session_access,
                    &lease,
                    remote_noise_public_key,
                )
                .await?;
            (changed, role)
        };
        if changed {
            return self
                .start_link_handshake_with_claim(counterparty, role, lease, remote_noise_public_key)
                .await;
        }
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
            self.mark_link_recovery_required(&counterparty, lease, None)
                .await?;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "missing Encrypted Link Handshake role for counterparty {counterparty}"
                ),
                source: None,
            });
        };
        let Some(snapshot_bytes) = stored_link_state.handshake_snapshot.as_ref() else {
            self.mark_link_recovery_required(&counterparty, lease, None)
                .await?;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no in-progress Encrypted Link Handshake snapshot for counterparty {counterparty}"
                ),
                source: None,
            });
        };

        let (session_access, handshake) = match self
            .restore_link_handshake_from_snapshot(
                counterparty.clone(),
                snapshot_bytes,
                &lease,
                remote_noise_public_key,
            )
            .await
        {
            Ok(restored) => restored,
            Err(err) => {
                if Self::link_handshake_error_requires_recovery(&err) {
                    self.mark_link_recovery_required(&counterparty, lease, None)
                        .await?;
                }
                return Err(err);
            }
        };

        self.advance_restored_link_handshake(
            session_access,
            handshake,
            handshake_role,
            stored_link_state.generation,
            lease.clone(),
            remote_noise_public_key,
        )
        .await
    }

    async fn mark_link_recovery_required(
        &self,
        counterparty: &PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: Option<&PubkySessionAccess>,
    ) -> Result<()> {
        mark_recovery_required_with_lease(
            &self.storage,
            counterparty.clone(),
            lease.clone(),
            self.clock.now(),
        )
        .await?;
        self.publish_local_recovery_marker_if_possible(counterparty, &lease, session_access)
            .await;
        Ok(())
    }

    async fn restore_link_handshake_from_snapshot(
        &self,
        counterparty: PubkyPublicKey,
        snapshot_bytes: &[u8],
        lease: &PeerLinkOperationLease,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<(GuardedSessionAccess, paykit_lib::EncryptedLinkHandshake)> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)?;
        self.require_snapshot_recovery_context(&counterparty, snapshot.recovery_context(), lease)
            .await?;
        if snapshot.remote_noise_public_key() != remote_noise_public_key {
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!("counterparty {counterparty} rotated its Paykit identity key"),
                source: None,
            });
        }
        let handshake = paykit_lib::restore_encrypted_link_handshake(
            session_access.session.clone(),
            secret_key,
            &remote_public_key,
            session_access.outbox_client.clone(),
            snapshot,
        )
        .await?;
        // Keep rotation excluded until advancement and its durable checkpoint finish.
        Ok((session_access, handshake))
    }

    fn link_handshake_error_requires_recovery(err: &PaykitSdkError) -> bool {
        matches!(
            err,
            PaykitSdkError::Protocol { .. } | PaykitSdkError::RecoveryRequired { .. }
        )
    }

    async fn advance_restored_link_handshake(
        &self,
        session_access: GuardedSessionAccess,
        handshake: paykit_lib::EncryptedLinkHandshake,
        handshake_role: EncryptedLinkHandshakeRole,
        expected_generation: u64,
        lease: PeerLinkOperationLease,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        let counterparty = lease.counterparty.clone();
        self.require_current_peer_link_operation(&lease, &session_access)
            .await?;
        let progress = match paykit_lib::advance_handshake(handshake).await {
            Ok(progress) => progress,
            Err(err) => {
                let err = PaykitSdkError::from(err);
                if Self::link_handshake_error_requires_recovery(&err) {
                    self.mark_link_recovery_required(&counterparty, lease, Some(&session_access))
                        .await?;
                }
                return Err(err);
            }
        };

        if self
            .observe_remote_recovery_marker_with_lease(
                &counterparty,
                &session_access,
                &lease,
                remote_noise_public_key,
            )
            .await?
        {
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!("counterparty {counterparty} changed recovery attempt during handshake advancement"),
                source: None,
            });
        }

        match progress {
            paykit_lib::HandshakeProgress::Pending(handshake) => {
                save_link_handshake_state_if_generation_with_lease(
                    &self.storage,
                    counterparty,
                    handshake_role,
                    handshake.serialize()?,
                    expected_generation,
                    lease.clone(),
                    self.clock.now(),
                )
                .await
            }
            paykit_lib::HandshakeProgress::Complete(link) => {
                let report = save_linked_peer_link_state_if_generation_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    link.serialize()?,
                    expected_generation,
                    lease.clone(),
                    self.clock.now(),
                )
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
        {
            let (session_access, _) = self.private_link_session_access().await?;
            require_distinct_link_identity(&session_access.public_key()?, &counterparty)?;
        }
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            let remote_key = self.counterparty_noise_public_key(&counterparty).await?;
            self.start_link_handshake_with_claim(counterparty, role, lease.clone(), &remote_key)
                .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    pub(super) async fn start_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
        lease: PeerLinkOperationLease,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        self.observe_remote_recovery_marker_with_lease(
            &counterparty,
            &session_access,
            &lease,
            remote_noise_public_key,
        )
        .await?;
        let peer_state = self
            .storage
            .transaction(|tx| Ok(tx.linked_peer(&counterparty).map(|peer| peer.state)))
            .await?;
        if matches!(peer_state, Some(LinkedPeerState::Blocked)) {
            return Err(PaykitSdkError::Policy {
                context: format!("counterparty {counterparty} is blocked"),
                source: None,
            });
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
                        mark_recovery_required_with_lease(
                            &self.storage,
                            counterparty.clone(),
                            lease.clone(),
                            self.clock.now(),
                        )
                        .await?;
                        self.publish_local_recovery_marker_if_possible(
                            &counterparty,
                            &lease,
                            Some(&session_access),
                        )
                        .await;
                        return Err(PaykitSdkError::RecoveryRequired {
                            context: format!(
                                "missing Encrypted Link Handshake role for counterparty {counterparty}"
                            ),
                            source: None,
                        });
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

        let remote_public_key = counterparty.to_public_key()?;
        self.publish_local_recovery_marker_with_key(
            &counterparty,
            &session_access,
            &lease,
            remote_noise_public_key,
        )
        .await?;
        let recovery_context = self
            .storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let peer = tx
                    .linked_peer(&counterparty)
                    .expect("marker publication saves peer state");
                Ok(paykit_lib::EncryptedLinkRecoveryContext::new(
                    peer.local_recovery_attempt_id.as_deref(),
                    peer.remote_recovery_attempt_id.as_deref(),
                )?)
            })
            .await?;
        self.require_current_peer_link_operation(&lease, &session_access)
            .await?;
        let handshake = match role {
            EncryptedLinkHandshakeRole::Initiator => paykit_lib::initiate_encrypted_link(
                session_access.session.clone(),
                secret_key,
                &remote_public_key,
                remote_noise_public_key,
                recovery_context,
                session_access.outbox_client.clone(),
            )?,
            EncryptedLinkHandshakeRole::Responder => paykit_lib::accept_encrypted_link(
                session_access.session.clone(),
                secret_key,
                &remote_public_key,
                remote_noise_public_key,
                recovery_context,
                session_access.outbox_client.clone(),
            )?,
        };

        save_link_handshake_state_with_lease(
            &self.storage,
            counterparty,
            role,
            handshake.serialize()?,
            lease,
            self.clock.now(),
        )
        .await
    }

    pub(super) async fn claim_peer_link_operation(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PeerLinkOperationLease> {
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            move |tx| {
                let now = self.clock.now();
                tx.claim_peer_link_operation(&counterparty, now, now + lease_timeout)
            }
        })
        .await?
        .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
            context: format!(
                "peer link operation already in progress for counterparty {counterparty}"
            ),
            source: None,
        })
    }

    pub(super) async fn require_current_peer_link_operation(
        &self,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<()> {
        let secret = session_access.paykit_noise_secret_key()?;
        let noise_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&secret).public_key());
        self.storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, lease)?;
                if lease.expires_at <= self.clock.now() {
                    return Err(PaykitSdkError::Policy {
                        context: "peer link operation lease expired".into(),
                        source: None,
                    });
                }
                crate::storage::bind_paykit_noise_key(tx, noise_public_key)
            })
            .await
    }

    pub(super) async fn require_snapshot_recovery_context(
        &self,
        counterparty: &PubkyPublicKey,
        context: &paykit_lib::EncryptedLinkRecoveryContext,
        lease: &PeerLinkOperationLease,
    ) -> Result<()> {
        self.storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, lease)?;
                let peer = tx.linked_peer(counterparty).ok_or_else(|| {
                    PaykitSdkError::RecoveryRequired {
                        context: format!("no Linked Peer record for counterparty {counterparty}"),
                        source: None,
                    }
                })?;
                crate::domain::linked_peers::require_recovery_context(&peer, context)
            })
            .await
    }

    pub(super) async fn release_peer_link_operation(
        &self,
        lease: &PeerLinkOperationLease,
    ) -> Result<()> {
        self.retry_storage_transaction(|| {
            let lease = lease.clone();
            move |tx| {
                tx.release_peer_link_operation(&lease.counterparty, lease.lease_id);
                Ok(())
            }
        })
        .await
    }

    pub(super) async fn finish_peer_link_operation<T>(
        &self,
        lease: PeerLinkOperationLease,
        result: Result<T>,
    ) -> Result<T> {
        // Cleanup failure must not obscure a confirmed publication or its error.
        // An unreleased lease expires and can then be reclaimed.
        let _ = self.release_peer_link_operation(&lease).await;
        result
    }

    pub(super) async fn private_link_session_access(
        &self,
    ) -> Result<(GuardedSessionAccess, [u8; 32])> {
        let (session_access, _) = self.load_session_access_and_refresh_identity().await?;
        let session_access = session_access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        self.validate_local_noise_key_authorization(&session_access)
            .await?;
        Ok((session_access, secret_key))
    }

    /// Fetch and pin the current key; reuse it only within this peer operation.
    pub(super) async fn counterparty_noise_public_key(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<paykit_lib::PublicKey> {
        let authorization = self
            .paykit_noise_key_authorization(counterparty.clone())
            .await?;
        self.pin_counterparty_noise_key_authorization(counterparty, &authorization)
            .await?;
        Ok(authorization.noise_public_key().clone())
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

pub(super) fn require_distinct_link_identity(
    local_public_key: &PubkyPublicKey,
    counterparty: &PubkyPublicKey,
) -> Result<()> {
    if local_public_key == counterparty {
        return Err(PaykitSdkError::Policy {
            context: "cannot establish an Encrypted Link with the local identity".into(),
            source: None,
        });
    }
    Ok(())
}
