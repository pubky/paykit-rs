use super::*;

#[derive(PartialEq)]
pub(super) struct RecoveryObservationCheckpoint {
    local_public_key: PubkyPublicKey,
    local_noise_public_key: PubkyPublicKey,
    peer: LinkedPeerRecord,
    pub(super) link_state: EncryptedLinkStateRecord,
}

impl RecoveryObservationCheckpoint {
    pub(super) fn load(tx: &dyn StorageTransaction, counterparty: &PubkyPublicKey) -> Option<Self> {
        recovery_observation_checkpoint(
            tx,
            counterparty,
            &tx.load_identity_state()?.public_key?,
            &tx.paykit_noise_public_key()?,
        )
    }
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return tracked Encrypted Link recovery marker state for a counterparty.
    pub async fn encrypted_link_recovery_marker_status(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<EncryptedLinkRecoveryMarkerReport>> {
        recovery_marker_report(&self.storage, counterparty).await
    }

    /// Publish a minimal local recovery marker for a counterparty.
    pub async fn publish_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, _) = self.private_link_session_access().await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .publish_encrypted_link_recovery_marker_with_claim(
                counterparty,
                session_access,
                lease.clone(),
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Observe a counterparty's public recovery marker.
    pub async fn observe_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, _) = self.private_link_session_access().await?;
        self.observe_remote_recovery_marker_with_session(&counterparty, &session_access)
            .await
    }

    /// Remove a blocked counterparty's public recovery marker.
    ///
    /// Active links retain their markers because those IDs select their streams.
    pub async fn remove_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_noise_public_key = self.counterparty_noise_public_key(&counterparty).await?;
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            let expected_attempt_id = self
                .storage
                .transaction(|tx| {
                    crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                    if !tx
                        .linked_peer(&counterparty)
                        .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                    {
                        return Err(PaykitSdkError::Policy {
                            context: "recovery markers can only be removed for blocked peers"
                                .into(),
                            source: None,
                        });
                    }
                    Ok(tx
                        .linked_peer(&counterparty)
                        .and_then(|peer| peer.local_recovery_attempt_id))
                })
                .await?;

            let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
                &secret_key,
                session_access.session.info().public_key(),
                &counterparty.to_public_key()?,
                &remote_noise_public_key,
            );
            let removal: Result<()> =
                paykit_lib::with_write_lock(&session_access.session, &path, |lock| {
                    let session_access = &session_access;
                    let lease = &lease;
                    async move {
                        self.require_current_peer_link_operation(lease, session_access)
                            .await?;
                        match session_access.session.storage().delete_locked(&lock).await {
                            Ok(_) => Ok(()),
                            Err(err) if is_pubky_not_found(&err) => Ok(()),
                            Err(err) => Err(map_pubky_transport_error(
                                "remove Encrypted Link recovery marker",
                                err,
                            )),
                        }
                    }
                })
                .await;
            removal?;

            self.storage
                .transaction(|tx| {
                    crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                    if let Some(mut peer) = tx.linked_peer(&counterparty) {
                        if peer.local_recovery_attempt_id == expected_attempt_id {
                            peer.local_recovery_marker_last_error = None;
                            tx.save_linked_peer(peer);
                        }
                    }
                    Ok(())
                })
                .await?;
            self.recovery_marker_report_or_default(&counterparty, false)
                .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    #[cfg(test)]
    pub(super) async fn mark_private_recovery_pending(
        &self,
        counterparty: &PubkyPublicKey,
        expected_link_generation: Option<u64>,
    ) -> Result<RecoveryRequiredUpdate> {
        self.mark_private_recovery_pending_inner(counterparty, expected_link_generation, None)
            .await
    }

    #[cfg(test)]
    async fn mark_private_recovery_pending_inner(
        &self,
        counterparty: &PubkyPublicKey,
        expected_link_generation: Option<u64>,
        lease: Option<PeerLinkOperationLease>,
    ) -> Result<RecoveryRequiredUpdate> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                if let Some(lease) = lease.as_ref() {
                    crate::storage::require_peer_link_operation_lease(tx, lease)?;
                } else if tx.peer_link_operation_lease(counterparty).is_some() {
                    return Ok(RecoveryRequiredUpdate::Skipped);
                }
                let current_generation = tx
                    .encrypted_link_state(counterparty)
                    .map(|state| state.generation);
                if current_generation != expected_link_generation {
                    return Ok(RecoveryRequiredUpdate::Skipped);
                }
                mark_recovery_required_in_transaction(tx, counterparty, now)?;
                Ok(RecoveryRequiredUpdate::Marked)
            })
            .await
    }

    async fn publish_encrypted_link_recovery_marker_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        session_access: GuardedSessionAccess,
        lease: PeerLinkOperationLease,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        self.mark_recovery_required_for_marker_with_lease(&counterparty, &lease)
            .await?;
        self.publish_local_recovery_marker_with_session(&counterparty, &session_access, &lease)
            .await
    }

    async fn mark_recovery_required_for_marker_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
    ) -> Result<()> {
        let now = self.clock.now();
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let existing_peer = tx.linked_peer(&counterparty);
                let has_link_state = tx.encrypted_link_state(&counterparty).is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    return Err(PaykitSdkError::Policy {
                        context: format!(
                            "cannot publish Encrypted Link recovery marker without existing private link state for counterparty {counterparty}"
                        ),
                        source: None,
                    });
                }
                mark_recovery_required_for_marker_in_transaction(tx, &counterparty, now, None)?;
                Ok(())
            }
        })
            .await
    }

    pub(super) async fn publish_local_recovery_marker_if_possible(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: Option<&PubkySessionAccess>,
    ) {
        // Reuse guarded access: a queued rotation writer prevents recursive reads.
        let loaded_access;
        let session_access = match session_access {
            Some(access) => access,
            None => match self.private_link_session_access().await {
                Ok((access, ..)) => {
                    loaded_access = access;
                    &loaded_access
                }
                Err(err) => {
                    let _ = self
                        .save_local_recovery_marker_last_error(
                            counterparty,
                            lease,
                            Some(recovery_marker_error_text(&err)),
                        )
                        .await;
                    return;
                }
            },
        };
        if let Err(err) = self
            .publish_local_recovery_marker_with_session(counterparty, session_access, lease)
            .await
        {
            let _ = self
                .save_local_recovery_marker_last_error(counterparty, lease, Some(err.to_string()))
                .await;
        }
    }

    async fn save_local_recovery_marker_last_error(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        error: Option<String>,
    ) -> Result<()> {
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            let lease = lease.clone();
            let error = error.clone();
            move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                if let Some(mut peer) = tx.linked_peer(&counterparty) {
                    peer.local_recovery_marker_last_error = error;
                    tx.save_linked_peer(peer);
                }
                Ok(())
            }
        })
        .await
    }

    pub(super) async fn publish_local_recovery_marker_with_session(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        lease: &PeerLinkOperationLease,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let remote_noise_public_key = self.counterparty_noise_public_key(counterparty).await?;
        self.publish_local_recovery_marker_with_key(
            counterparty,
            session_access,
            lease,
            &remote_noise_public_key,
        )
        .await
    }

    pub(super) async fn publish_local_recovery_marker_with_key(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        lease: &PeerLinkOperationLease,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let secret_key = session_access.paykit_noise_secret_key()?;
        let now = self.clock.now();
        let marker = self
            .retry_storage_transaction(|| {
                let counterparty = counterparty.clone();
                let lease = lease.clone();
                move |tx| {
                    crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                    let mut peer =
                        recovery_peer_or_default(tx.linked_peer(&counterparty), &counterparty);
                    if peer.state == LinkedPeerState::Blocked {
                        return Err(PaykitSdkError::Policy {
                            context: format!("counterparty {counterparty} is blocked"),
                            source: None,
                        });
                    }
                    let reusable_marker = peer
                        .local_recovery_attempt_id
                        .as_ref()
                        .zip(peer.local_recovery_marker_created_at)
                        .map(|(attempt_id, created_at)| {
                            let created_at_text =
                                created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
                            EncryptedLinkRecoveryMarker::new(attempt_id.clone(), created_at_text)
                                .map(|marker| (marker, created_at))
                        })
                        .transpose()?;
                    let (marker, marker_created_at) =
                        reusable_marker.map(Ok).unwrap_or_else(|| {
                            EncryptedLinkRecoveryMarker::new_v4(
                                now.to_rfc3339_opts(SecondsFormat::Secs, true),
                            )
                            .map(|marker| (marker, now))
                        })?;
                    peer.local_recovery_attempt_id = Some(marker.attempt_id().to_owned());
                    peer.local_recovery_marker_created_at = Some(marker_created_at);
                    tx.save_linked_peer(peer);
                    Ok(marker)
                }
            })
            .await?;
        let (path, _) = paykit_lib::encrypted_link_recovery_marker_paths(
            &secret_key,
            session_access.session.info().public_key(),
            &counterparty.to_public_key()?,
            remote_noise_public_key,
        );
        let publication: Result<()> =
            paykit_lib::with_write_lock(&session_access.session, &path, |lock| async move {
                self.require_current_peer_link_operation(lease, session_access)
                    .await?;
                let payload = paykit_lib::serialize_encrypted_link_recovery_marker(&marker)?;
                session_access
                    .session
                    .storage()
                    .put_locked(&lock, payload)
                    .await
                    .map_err(|err| {
                        map_pubky_transport_error("publish Encrypted Link recovery marker", err)
                    })?;
                Ok(())
            })
            .await;
        if let Err(sdk_err) = publication {
            self.save_local_recovery_marker_last_error(
                counterparty,
                lease,
                Some(sdk_err.to_string()),
            )
            .await?;
            return Err(sdk_err);
        }

        self.save_local_recovery_marker_last_error(counterparty, lease, None)
            .await?;

        self.recovery_marker_report_or_default(counterparty, false)
            .await
    }

    pub(super) async fn observe_remote_recovery_marker_for_cached_private_state(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: Option<&GuardedSessionAccess>,
    ) -> Result<()> {
        let session_access = match session_access {
            Some(session_access) => session_access,
            None => {
                let (session_access, ..) = self.private_link_session_access().await?;
                return self
                    .observe_remote_recovery_marker_with_session(counterparty, &session_access)
                    .await
                    .map(|_| ());
            }
        };

        self.observe_remote_recovery_marker_with_session(counterparty, session_access)
            .await
            .map(|_| ())
    }

    pub(super) async fn observe_remote_recovery_marker_with_session(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &GuardedSessionAccess,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let local_public_key = session_access.public_key()?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let local_noise_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&secret_key).public_key());
        let checkpoint = self
            .storage
            .transaction(|tx| {
                Ok(recovery_observation_checkpoint(
                    tx,
                    counterparty,
                    &local_public_key,
                    &local_noise_public_key,
                ))
            })
            .await?;
        self.observe_remote_recovery_marker_from_checkpoint(
            counterparty,
            session_access,
            checkpoint,
        )
        .await
    }

    pub(super) async fn observe_remote_recovery_marker_from_checkpoint(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        checkpoint: Option<RecoveryObservationCheckpoint>,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        // Keep public reads outside bounded storage; session_access retains the identity guard.
        if let Some(checkpoint) =
            Box::pin(self.unchanged_link_checkpoint(counterparty, session_access, checkpoint))
                .await?
        {
            return Ok(EncryptedLinkRecoveryMarkerReport::from_peer(
                &checkpoint.peer,
                false,
            ));
        }
        self.with_guarded_storage_operation(
            Arc::clone(&session_access._guard),
            Box::pin(async {
                let lease = self.claim_peer_link_operation(counterparty).await?;
                let result = async {
                    self.ensure_peer_not_blocked(counterparty).await?;
                    let remote_key = match self.counterparty_noise_public_key(counterparty).await {
                        Ok(key) => key,
                        Err(PaykitSdkError::NotFound { .. }) => return Ok(false),
                        Err(err) => return Err(err),
                    };
                    self.observe_remote_recovery_marker_with_lease(
                        counterparty,
                        session_access,
                        &lease,
                        &remote_key,
                    )
                    .await
                }
                .await;
                let changed = self.finish_peer_link_operation(lease, result).await?;
                self.recovery_marker_report_or_default(counterparty, changed)
                    .await
            }),
        )
        .await
    }

    pub(super) async fn unchanged_link_checkpoint(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        checkpoint: Option<RecoveryObservationCheckpoint>,
    ) -> Result<Option<RecoveryObservationCheckpoint>> {
        let local_public_key = session_access.public_key()?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let local_noise_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&secret_key).public_key());
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };
        if checkpoint.local_public_key != local_public_key
            || checkpoint.local_noise_public_key != local_noise_public_key
        {
            return Ok(None);
        }
        let authorization = match self
            .paykit_noise_key_authorization(counterparty.clone())
            .await
        {
            Ok(authorization) => authorization,
            Err(PaykitSdkError::NotFound { .. }) => return Ok(None),
            Err(err) => return Err(err),
        };
        if checkpoint.peer.noise_key_authorization.as_ref() != Some(&authorization) {
            return Ok(None);
        }
        let Some(public_storage) = self.pubky.load_public_storage().await? else {
            return Ok(None);
        };
        if let Some(marker) = paykit_lib::fetch_encrypted_link_recovery_marker(
            &public_storage,
            &secret_key,
            session_access.session.info().public_key(),
            &counterparty.to_public_key()?,
            authorization.noise_public_key(),
        )
        .await?
        {
            if should_observe_remote_recovery_marker(
                Some(&checkpoint.peer),
                counterparty,
                marker.attempt_id(),
            )? {
                return Ok(None);
            }
        }

        // A read-only report still requires the exact checkpoint used for the public lookup.
        self.storage
            .transaction(|tx| {
                let current = recovery_observation_checkpoint(
                    tx,
                    counterparty,
                    &local_public_key,
                    &local_noise_public_key,
                );
                Ok(current.filter(|current| current == &checkpoint))
            })
            .await
    }

    pub(super) async fn observe_remote_recovery_marker_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        lease: &PeerLinkOperationLease,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<bool> {
        self.storage
            .transaction(|tx| {
                if tx
                    .linked_peer(counterparty)
                    .is_some_and(|peer| peer.state == LinkedPeerState::Blocked)
                {
                    return Err(PaykitSdkError::Policy {
                        context: format!("counterparty {counterparty} is blocked"),
                        source: None,
                    });
                }
                self.require_current_peer_link_operation_in_transaction(tx, lease, session_access)
            })
            .await?;
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for recovery marker lookup".into(),
                    source: None,
                })?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let remote_public_key = counterparty.to_public_key()?;
        let Some(marker) = paykit_lib::fetch_encrypted_link_recovery_marker(
            &public_storage,
            &secret_key,
            session_access.session.info().public_key(),
            &remote_public_key,
            remote_noise_public_key,
        )
        .await?
        else {
            return Ok(false);
        };
        self.mark_remote_recovery_marker_observed_with_lease(
            counterparty,
            marker.attempt_id(),
            lease.clone(),
        )
        .await
    }

    #[cfg(test)]
    async fn remote_recovery_marker_needs_observation(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
    ) -> Result<bool> {
        self.storage
            .transaction(|tx| {
                let existing_peer = tx.linked_peer(counterparty);
                should_observe_remote_recovery_marker(
                    existing_peer.as_ref(),
                    counterparty,
                    attempt_id,
                )
            })
            .await
    }

    #[cfg(test)]
    pub(super) async fn mark_remote_recovery_marker_observed_if_needed(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
    ) -> Result<bool> {
        if !self
            .remote_recovery_marker_needs_observation(counterparty, attempt_id)
            .await?
        {
            return Ok(false);
        }

        let lease = self.claim_peer_link_operation(counterparty).await?;
        let result = self
            .mark_remote_recovery_marker_observed_with_lease(
                counterparty,
                attempt_id,
                lease.clone(),
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    #[cfg(test)]
    pub(super) async fn should_observe_remote_recovery_marker_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
        lease: &PeerLinkOperationLease,
    ) -> Result<bool> {
        self.storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, lease)?;
                let existing_peer = tx.linked_peer(counterparty);
                should_observe_remote_recovery_marker(
                    existing_peer.as_ref(),
                    counterparty,
                    attempt_id,
                )
            })
            .await
    }

    async fn mark_remote_recovery_marker_observed_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
        lease: PeerLinkOperationLease,
    ) -> Result<bool> {
        let now = self.clock.now();
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            let attempt_id = attempt_id.to_owned();
            let lease = lease.clone();
            move |tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let existing_peer = tx.linked_peer(&counterparty);
                if !should_observe_remote_recovery_marker(
                    existing_peer.as_ref(),
                    &counterparty,
                    &attempt_id,
                )? {
                    return Ok(false);
                }
                mark_recovery_required_for_marker_in_transaction(
                    tx,
                    &counterparty,
                    now,
                    Some(&attempt_id),
                )?;
                Ok(true)
            }
        })
        .await
    }

    pub(super) async fn recovery_marker_report_or_default(
        &self,
        counterparty: &PubkyPublicKey,
        remote_marker_changed: bool,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let peer = self
            .storage
            .transaction(|tx| {
                Ok(recovery_peer_or_default(
                    tx.linked_peer(counterparty),
                    counterparty,
                ))
            })
            .await?;
        Ok(EncryptedLinkRecoveryMarkerReport::from_peer(
            &peer,
            remote_marker_changed,
        ))
    }
}

fn recovery_marker_error_text(err: &PaykitSdkError) -> String {
    match err {
        PaykitSdkError::Identity { context, .. } => context.clone(),
        PaykitSdkError::Storage { context, .. } => context.clone(),
        PaykitSdkError::Transport { context, .. } => context.clone(),
        PaykitSdkError::PaymentAdapter { context, .. } => context.clone(),
        PaykitSdkError::NotFound { .. }
        | PaykitSdkError::Protocol { .. }
        | PaykitSdkError::Policy { .. }
        | PaykitSdkError::ConcurrentUpdate { .. }
        | PaykitSdkError::SharedStateBusy { .. }
        | PaykitSdkError::RecoveryRequired { .. } => err.to_string(),
    }
}
fn recovery_peer_or_default(
    peer: Option<LinkedPeerRecord>,
    counterparty: &PubkyPublicKey,
) -> LinkedPeerRecord {
    peer.unwrap_or_else(|| LinkedPeerRecord {
        counterparty: counterparty.clone(),
        state: LinkedPeerState::NotLinked,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
        noise_key_authorization: None,
    })
}

fn can_publish_recovery_marker(peer: Option<&LinkedPeerRecord>, has_link_state: bool) -> bool {
    has_link_state
        || peer.is_some_and(|peer| {
            matches!(
                peer.state,
                LinkedPeerState::Linking
                    | LinkedPeerState::Linked
                    | LinkedPeerState::RecoveryRequired
            )
        })
}

fn recovery_observation_checkpoint(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    local_public_key: &PubkyPublicKey,
    local_noise_public_key: &PubkyPublicKey,
) -> Option<RecoveryObservationCheckpoint> {
    if tx.load_identity_state()?.public_key.as_ref() != Some(local_public_key)
        || tx.paykit_noise_public_key().as_ref() != Some(local_noise_public_key)
        || tx.peer_link_operation_lease(counterparty).is_some()
    {
        return None;
    }
    let peer = tx.linked_peer(counterparty)?;
    if peer.state != LinkedPeerState::Linked || peer.failure_count != 0 {
        return None;
    }
    let state = tx.encrypted_link_state(counterparty)?;
    let snapshot =
        paykit_lib::EncryptedLinkSnapshot::deserialize(state.link_snapshot.as_ref()?).ok()?;
    if PubkyPublicKey::from_public_key(snapshot.recipient()) != *counterparty {
        return None;
    }
    crate::domain::linked_peers::require_recovery_context(&peer, snapshot.recovery_context())
        .ok()?;
    if snapshot.remote_noise_public_key()
        != peer.noise_key_authorization.as_ref()?.noise_public_key()
    {
        return None;
    }
    Some(RecoveryObservationCheckpoint {
        local_public_key: local_public_key.clone(),
        local_noise_public_key: local_noise_public_key.clone(),
        peer,
        link_state: state,
    })
}

fn should_observe_remote_recovery_marker(
    existing_peer: Option<&LinkedPeerRecord>,
    counterparty: &PubkyPublicKey,
    attempt_id: &str,
) -> Result<bool> {
    if existing_peer.is_some_and(|peer| peer.state == LinkedPeerState::Blocked) {
        return Err(PaykitSdkError::Policy {
            context: format!("counterparty {counterparty} is blocked"),
            source: None,
        });
    }
    Ok(
        existing_peer.and_then(|peer| peer.remote_recovery_attempt_id.as_deref())
            != Some(attempt_id),
    )
}

#[cfg(test)]
pub(super) enum RecoveryRequiredUpdate {
    Skipped,
    Marked,
}
