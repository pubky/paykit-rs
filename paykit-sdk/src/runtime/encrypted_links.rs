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

    /// Start an Encrypted Link Handshake as the initiator.
    pub async fn initiate_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_private_workflows_enabled("Encrypted Link initiation")?;
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Initiator)
            .await
    }

    /// Start an Encrypted Link Handshake as the responder.
    pub async fn accept_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_private_workflows_enabled("Encrypted Link acceptance")?;
        self.start_link_handshake(counterparty, EncryptedLinkHandshakeRole::Responder)
            .await
    }

    /// Advance the stored Encrypted Link Handshake for one counterparty.
    pub async fn advance_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.ensure_private_workflows_enabled("Encrypted Link Handshake advancement")?;
        self.ensure_peer_not_recovery_required_or_blocked(&counterparty)
            .await?;
        let _ = self.private_link_session_access().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .advance_link_handshake_with_claim(counterparty, lease.clone())
            .await;
        self.finish_peer_link_operation(lease, result).await
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
                if Self::handshake_restore_error_requires_recovery(&err) {
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

    fn handshake_restore_error_requires_recovery(err: &PaykitSdkError) -> bool {
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
        match paykit_lib::advance_handshake(handshake).await? {
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
