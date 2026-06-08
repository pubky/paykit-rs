use super::*;

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
        self.ensure_private_workflows_enabled("Encrypted Link recovery marker publishing")?;
        self.ensure_recovery_marker_publishing_enabled()?;
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
        self.ensure_private_workflows_enabled("Encrypted Link recovery marker observation")?;
        let (session_access, _) = self.private_link_session_access().await?;
        self.observe_remote_recovery_marker_with_session(&counterparty, &session_access)
            .await
    }

    /// Remove the local public recovery marker for a counterparty.
    pub async fn remove_encrypted_link_recovery_marker(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let (session_access, secret_key) = self.private_link_session_access().await?;
        let remote_public_key = counterparty.to_public_key()?;
        paykit_lib::remove_encrypted_link_recovery_marker(
            &session_access.session,
            &secret_key,
            &remote_public_key,
        )
        .await?;

        self.storage
            .transaction(|tx| {
                if let Some(mut peer) = tx.linked_peer(&counterparty) {
                    peer.local_recovery_attempt_id = None;
                    peer.local_recovery_marker_created_at = None;
                    peer.local_recovery_marker_last_error = None;
                    tx.save_linked_peer(peer);
                }
                Ok(())
            })
            .await?;
        self.recovery_marker_report_or_default(&counterparty, false)
            .await
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

    pub(super) async fn mark_private_recovery_pending_and_publish_marker(
        &self,
        counterparty: &PubkyPublicKey,
        expected_link_generation: Option<u64>,
    ) -> Result<RecoveryRequiredUpdate> {
        let lease = self.claim_peer_link_operation(counterparty).await?;
        let result = async {
            let update = self
                .mark_private_recovery_pending_inner(
                    counterparty,
                    expected_link_generation,
                    Some(lease.clone()),
                )
                .await?;
            if let RecoveryRequiredUpdate::Marked { new_episode } = update {
                self.publish_local_recovery_marker_if_possible(counterparty, new_episode)
                    .await;
            }
            Ok(update)
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

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
                let mut peer = recovery_peer_or_default(tx.linked_peer(counterparty), counterparty);
                let new_episode = peer.state != LinkedPeerState::RecoveryRequired;
                peer.state = LinkedPeerState::RecoveryRequired;
                if new_episode || peer.last_sync_at.is_none() {
                    peer.last_sync_at = Some(now);
                }
                peer.failure_count = peer.failure_count.saturating_add(1);
                tx.save_linked_peer(peer);
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
                Ok(RecoveryRequiredUpdate::Marked { new_episode })
            })
            .await
    }

    async fn publish_encrypted_link_recovery_marker_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        session_access: PubkySessionAccess,
        lease: PeerLinkOperationLease,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        let new_episode = self
            .mark_recovery_required_for_marker_with_lease(&counterparty, lease)
            .await?;
        self.publish_local_recovery_marker_with_session(&counterparty, &session_access, new_episode)
            .await
    }

    async fn mark_recovery_required_for_marker_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        lease: PeerLinkOperationLease,
    ) -> Result<bool> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let existing_peer = tx.linked_peer(counterparty);
                let has_link_state = tx.encrypted_link_state(counterparty).is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    return Err(PaykitSdkError::Policy(format!(
                        "cannot publish Encrypted Link recovery marker without existing private link state for counterparty {counterparty}"
                    )));
                }
                let mut peer = recovery_peer_or_default(existing_peer, counterparty);
                if peer.state == LinkedPeerState::Blocked {
                    return Err(PaykitSdkError::Policy(format!(
                        "counterparty {counterparty} is blocked"
                    )));
                }
                let new_episode = peer.state != LinkedPeerState::RecoveryRequired;
                if peer.state != LinkedPeerState::RecoveryRequired {
                    peer.failure_count = peer.failure_count.saturating_add(1);
                }
                peer.state = LinkedPeerState::RecoveryRequired;
                if new_episode || peer.last_sync_at.is_none() {
                    peer.last_sync_at = Some(now);
                }
                tx.save_linked_peer(peer);
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
                Ok(new_episode)
            })
            .await
    }

    pub(super) async fn publish_local_recovery_marker_if_possible(
        &self,
        counterparty: &PubkyPublicKey,
        force_new_attempt: bool,
    ) {
        let (session_access, _) = match self.private_link_session_access().await {
            Ok(value) => value,
            Err(err) => {
                let _ = self
                    .save_local_recovery_marker_last_error(
                        counterparty,
                        Some(recovery_marker_error_text(&err)),
                    )
                    .await;
                return;
            }
        };
        if let Err(err) = self
            .publish_local_recovery_marker_with_session(
                counterparty,
                &session_access,
                force_new_attempt,
            )
            .await
        {
            let _ = self
                .save_local_recovery_marker_last_error(counterparty, Some(err.to_string()))
                .await;
        }
    }

    async fn save_local_recovery_marker_last_error(
        &self,
        counterparty: &PubkyPublicKey,
        error: Option<String>,
    ) -> Result<()> {
        self.storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
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
        force_new_attempt: bool,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        }

        let secret_key = session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context:
                    "local Pubky secret key is unavailable for Encrypted Link recovery markers"
                        .into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        let now = self.clock.now();
        let marker = self
            .storage
            .transaction(|tx| {
                let mut peer = recovery_peer_or_default(tx.linked_peer(counterparty), counterparty);
                if peer.state != LinkedPeerState::RecoveryRequired {
                    return Err(PaykitSdkError::Policy(format!(
                        "cannot publish Encrypted Link recovery marker unless counterparty {counterparty} is recovery-required"
                    )));
                }
                let has_link_state = tx.encrypted_link_state(counterparty).is_some();
                if !can_publish_recovery_marker(Some(&peer), has_link_state) {
                    return Err(PaykitSdkError::Policy(format!(
                        "cannot publish Encrypted Link recovery marker without existing private link state for counterparty {counterparty}"
                    )));
                }
                let reusable_marker = if !force_new_attempt
                    && peer.state == LinkedPeerState::RecoveryRequired
                    && local_recovery_marker_belongs_to_current_episode(&peer)
                {
                    peer.local_recovery_attempt_id
                        .as_ref()
                        .zip(peer.local_recovery_marker_created_at)
                        .map(|(attempt_id, created_at)| {
                            let created_at_text =
                                created_at.to_rfc3339_opts(SecondsFormat::Secs, true);
                            EncryptedLinkRecoveryMarker::new(attempt_id.clone(), created_at_text)
                                .map(|marker| (marker, created_at))
                        })
                        .transpose()?
                } else {
                    None
                };
                let (marker, marker_created_at) = reusable_marker
                    .map(Ok)
                    .unwrap_or_else(|| {
                        EncryptedLinkRecoveryMarker::new_v4(
                            now.to_rfc3339_opts(SecondsFormat::Secs, true),
                        )
                        .map(|marker| (marker, now))
                    })?;
                peer.local_recovery_attempt_id = Some(marker.attempt_id().to_owned());
                peer.local_recovery_marker_created_at = Some(marker_created_at);
                tx.save_linked_peer(peer);
                Ok(marker)
            })
            .await?;
        if let Err(err) = paykit_lib::publish_encrypted_link_recovery_marker(
            &session_access.session,
            secret_key,
            &remote_public_key,
            &marker,
        )
        .await
        {
            let sdk_err = PaykitSdkError::from(err);
            self.save_local_recovery_marker_last_error(counterparty, Some(sdk_err.to_string()))
                .await?;
            return Err(sdk_err);
        }

        self.save_local_recovery_marker_last_error(counterparty, None)
            .await?;

        self.recovery_marker_report_or_default(counterparty, false)
            .await
    }

    pub(super) async fn observe_remote_recovery_marker_for_cached_private_state(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: Option<&PubkySessionAccess>,
    ) -> Result<()> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return Ok(());
        }

        let session_access = match session_access {
            Some(session_access) => session_access,
            None => {
                let (session_access, _) = self.private_link_session_access().await?;
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
        session_access: &PubkySessionAccess,
    ) -> Result<EncryptedLinkRecoveryMarkerReport> {
        if self.config.encrypted_link_recovery_markers
            == EncryptedLinkRecoveryMarkerPolicy::Disabled
        {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        }

        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for recovery marker lookup".into(),
                    source: None,
                })?;
        let secret_key = session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context:
                    "local Pubky secret key is unavailable for Encrypted Link recovery markers"
                        .into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        let Some(marker) = paykit_lib::fetch_encrypted_link_recovery_marker(
            &public_storage,
            secret_key,
            &remote_public_key,
        )
        .await?
        else {
            return self
                .recovery_marker_report_or_default(counterparty, false)
                .await;
        };

        let attempt_id = marker.attempt_id().to_owned();
        let marker_created_at = parse_recovery_marker_created_at(&marker)?;
        let changed = self
            .mark_remote_recovery_marker_observed_if_needed(
                counterparty,
                &attempt_id,
                marker_created_at,
            )
            .await?;
        self.recovery_marker_report_or_default(counterparty, changed)
            .await
    }

    pub(super) async fn mark_remote_recovery_marker_observed_if_needed(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
        marker_created_at: DateTime<Utc>,
    ) -> Result<bool> {
        let should_mutate = self
            .storage
            .transaction(|tx| {
                let existing_peer = tx.linked_peer(counterparty);
                let link_state = tx.encrypted_link_state(counterparty);
                if remote_recovery_marker_is_stale(link_state.as_ref(), marker_created_at) {
                    return Ok(false);
                }
                let has_link_state = link_state.is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    return Ok(false);
                }
                let peer = recovery_peer_or_default(existing_peer, counterparty);
                if peer.remote_recovery_attempt_id.as_deref() == Some(attempt_id) {
                    return Ok(false);
                }
                if peer.state == LinkedPeerState::Blocked {
                    return Err(PaykitSdkError::Policy(format!(
                        "counterparty {counterparty} is blocked"
                    )));
                }
                Ok(true)
            })
            .await?;
        if !should_mutate {
            return Ok(false);
        }

        let lease = self.claim_peer_link_operation(counterparty).await?;
        let result = self
            .mark_remote_recovery_marker_observed_with_lease(
                counterparty,
                attempt_id,
                marker_created_at,
                lease.clone(),
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    async fn mark_remote_recovery_marker_observed_with_lease(
        &self,
        counterparty: &PubkyPublicKey,
        attempt_id: &str,
        marker_created_at: DateTime<Utc>,
        lease: PeerLinkOperationLease,
    ) -> Result<bool> {
        let now = self.clock.now();
        self.storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let existing_peer = tx.linked_peer(counterparty);
                let link_state = tx.encrypted_link_state(counterparty);
                if remote_recovery_marker_is_stale(link_state.as_ref(), marker_created_at) {
                    tx.release_peer_link_operation(counterparty, lease.lease_id);
                    return Ok(false);
                }
                let has_link_state = link_state.is_some();
                if !can_publish_recovery_marker(existing_peer.as_ref(), has_link_state) {
                    tx.release_peer_link_operation(counterparty, lease.lease_id);
                    return Ok(false);
                }
                let mut peer = recovery_peer_or_default(existing_peer, counterparty);
                if peer.remote_recovery_attempt_id.as_deref() == Some(attempt_id) {
                    tx.release_peer_link_operation(counterparty, lease.lease_id);
                    return Ok(false);
                }
                if peer.state == LinkedPeerState::Blocked {
                    return Err(PaykitSdkError::Policy(format!(
                        "counterparty {counterparty} is blocked"
                    )));
                }
                peer.remote_recovery_attempt_id = Some(attempt_id.to_owned());
                peer.remote_recovery_marker_observed_at = Some(now);
                peer.state = LinkedPeerState::RecoveryRequired;
                peer.last_sync_at = Some(now);
                peer.failure_count = peer.failure_count.saturating_add(1);
                tx.save_linked_peer(peer);
                if let Some(link_state) = link_state {
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                        link_snapshot: None,
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: link_state.generation.saturating_add(1),
                        checkpointed_at: now,
                    });
                }
                tx.release_peer_link_operation(counterparty, lease.lease_id);
                Ok(true)
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

    pub(super) fn private_recovery_window_open(&self, started_at: DateTime<Utc>) -> Result<bool> {
        let timeout =
            ChronoDuration::from_std(self.config.private_recovery_timeout).map_err(|err| {
                PaykitSdkError::Policy(format!("invalid private recovery timeout: {err}"))
            })?;
        Ok(self.clock.now() < started_at + timeout)
    }

    pub(super) async fn remove_local_recovery_marker_if_recorded(
        &self,
        counterparty: &PubkyPublicKey,
    ) {
        let (session_access, secret_key) = match self.private_link_session_access().await {
            Ok(value) => value,
            Err(err) => {
                if self
                    .has_local_recovery_marker(counterparty)
                    .await
                    .unwrap_or(false)
                {
                    let _ = self
                        .save_local_recovery_marker_last_error(
                            counterparty,
                            Some(recovery_marker_error_text(&err)),
                        )
                        .await;
                }
                return;
            }
        };
        let has_local_marker = self
            .has_local_recovery_marker(counterparty)
            .await
            .unwrap_or(false);
        if !has_local_marker {
            return;
        }

        let Ok(remote_public_key) = counterparty.to_public_key() else {
            let _ = self
                .save_local_recovery_marker_last_error(
                    counterparty,
                    Some("invalid counterparty Pubky public key".into()),
                )
                .await;
            return;
        };
        if let Err(err) = paykit_lib::remove_encrypted_link_recovery_marker(
            &session_access.session,
            &secret_key,
            &remote_public_key,
        )
        .await
        {
            let _ = self
                .save_local_recovery_marker_last_error(counterparty, Some(err.to_string()))
                .await;
            return;
        }
        let _ = self
            .storage
            .transaction(|tx| {
                if let Some(mut peer) = tx.linked_peer(counterparty) {
                    peer.local_recovery_attempt_id = None;
                    peer.local_recovery_marker_created_at = None;
                    peer.local_recovery_marker_last_error = None;
                    tx.save_linked_peer(peer);
                }
                Ok(())
            })
            .await;
    }

    pub(super) async fn has_local_recovery_marker(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<bool> {
        self.storage
            .transaction(|tx| {
                Ok(tx
                    .linked_peer(counterparty)
                    .and_then(|peer| peer.local_recovery_attempt_id)
                    .is_some())
            })
            .await
    }
}

fn recovery_marker_error_text(err: &PaykitSdkError) -> String {
    match err {
        PaykitSdkError::Identity { context, .. } => context.clone(),
        PaykitSdkError::Storage { context, .. } => context.clone(),
        PaykitSdkError::Transport { context, .. } => context.clone(),
        PaykitSdkError::PaymentAdapter { context, .. } => context.clone(),
        PaykitSdkError::NotFound(_)
        | PaykitSdkError::Protocol(_)
        | PaykitSdkError::Policy(_)
        | PaykitSdkError::RecoveryRequired(_) => err.to_string(),
    }
}
fn recovery_peer_or_default(
    peer: Option<LinkedPeerRecord>,
    counterparty: &PubkyPublicKey,
) -> LinkedPeerRecord {
    peer.unwrap_or_else(|| LinkedPeerRecord {
        counterparty: counterparty.clone(),
        state: LinkedPeerState::Unknown,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
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

fn parse_recovery_marker_created_at(marker: &EncryptedLinkRecoveryMarker) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(marker.created_at())
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|err| {
            PaykitSdkError::Protocol(format!("invalid recovery marker timestamp: {err}"))
        })
}

fn remote_recovery_marker_is_stale(
    link_state: Option<&EncryptedLinkStateRecord>,
    marker_created_at: DateTime<Utc>,
) -> bool {
    link_state
        .and_then(|state| {
            (state.link_snapshot.is_some() || state.handshake_snapshot.is_some())
                .then_some(state.checkpointed_at)
        })
        .is_some_and(|checkpointed_at| marker_created_at <= checkpointed_at)
}

pub(super) fn local_recovery_marker_belongs_to_current_episode(peer: &LinkedPeerRecord) -> bool {
    let Some(created_at) = peer.local_recovery_marker_created_at else {
        return false;
    };
    peer.last_sync_at
        .map(|recovery_started_at| created_at >= recovery_started_at)
        .unwrap_or(true)
}
pub(super) enum RecoveryRequiredUpdate {
    Skipped,
    Marked { new_episode: bool },
}
