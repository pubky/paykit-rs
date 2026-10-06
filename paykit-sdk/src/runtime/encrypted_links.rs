use super::recovery::RecoveryObservationCheckpoint;
use super::*;
use crate::domain::linked_peers::{
    handshake_checkpoint_is_current, save_handshake_checkpoint_in_transaction,
    HandshakeCheckpointSave, HandshakeCheckpointUpdate,
};

struct HandshakeSource {
    peer: LinkedPeerRecord,
    state: EncryptedLinkStateRecord,
    identity: PubkyPublicKey,
    noise_key: PubkyPublicKey,
}

impl HandshakeSource {
    fn require_current(
        &self,
        tx: &dyn StorageTransaction,
        lease: &PeerLinkOperationLease,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let identity = tx.load_identity_state().and_then(|state| state.public_key);
        if identity.as_ref() != Some(&self.identity)
            || tx.paykit_noise_public_key().as_ref() != Some(&self.noise_key)
            || !handshake_checkpoint_is_current(tx, &self.peer, &self.state, lease, now)
        {
            return Err(PaykitSdkError::ConcurrentUpdate {
                context: "Encrypted Link Handshake checkpoint changed".into(),
                source: None,
            });
        }
        Ok(())
    }

    fn report(&self) -> LinkedPeerHandshakeReport {
        LinkedPeerHandshakeReport {
            counterparty: self.peer.counterparty.clone(),
            state: self.peer.state.clone(),
            generation: self.state.generation,
            handshake_role: self.state.handshake_role,
        }
    }

    fn snapshot(&self) -> Result<paykit_lib::EncryptedLinkHandshakeSnapshot> {
        let bytes = self
            .state
            .handshake_snapshot
            .as_deref()
            .filter(|_| self.state.handshake_role.is_some())
            .ok_or_else(|| PaykitSdkError::RecoveryRequired {
                context: "incomplete Encrypted Link Handshake checkpoint".into(),
                source: None,
            })?;
        let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(bytes)?;
        crate::domain::linked_peers::require_recovery_context(
            &self.peer,
            snapshot.recovery_context(),
        )?;
        if PubkyPublicKey::from_public_key(snapshot.recipient()) != self.peer.counterparty
            || self
                .peer
                .noise_key_authorization
                .as_ref()
                .map(|record| record.noise_public_key())
                != Some(snapshot.remote_noise_public_key())
        {
            return Err(PaykitSdkError::RecoveryRequired {
                context: "handshake checkpoint identity or authorization changed".into(),
                source: None,
            });
        }
        Ok(snapshot)
    }

    fn apply(
        &self,
        tx: &mut dyn StorageTransaction,
        lease: &PeerLinkOperationLease,
        now: DateTime<Utc>,
        observation: Result<Option<EncryptedLinkRecoveryMarker>>,
        update: Result<Option<HandshakeCheckpointUpdate>>,
    ) -> Result<Result<LinkedPeerHandshakeReport>> {
        self.require_current(tx, lease, now)?;
        // Recovery effects commit only for the exact source; failed effects roll back.
        let checked = match observation {
            Ok(Some(marker))
                if Some(marker.attempt_id()) != self.peer.remote_recovery_attempt_id.as_deref() =>
            {
                mark_recovery_required_for_marker_in_transaction(
                    tx,
                    &lease.counterparty,
                    now,
                    Some(marker.attempt_id()),
                )?;
                Err(PaykitSdkError::RecoveryRequired {
                    context: "counterparty changed recovery attempt during handshake advancement"
                        .into(),
                    source: None,
                })
            }
            Ok(_) => update,
            Err(error) => Err(error),
        };
        let update = match checked {
            Ok(Some(update)) => update,
            Ok(None) => return Ok(Ok(self.report())),
            Err(error) => {
                if link_handshake_error_requires_recovery(&error)
                    && tx
                        .linked_peer(&lease.counterparty)
                        .is_some_and(|peer| peer.state != LinkedPeerState::RecoveryRequired)
                {
                    mark_recovery_required_in_transaction(tx, &lease.counterparty, now)?;
                }
                return Ok(Err(error));
            }
        };
        let report = match save_handshake_checkpoint_in_transaction(
            tx,
            &self.peer,
            &self.state,
            &update,
            lease,
            now,
        )? {
            HandshakeCheckpointSave::Applied(report) => report,
            HandshakeCheckpointSave::Stale => {
                return Err(PaykitSdkError::ConcurrentUpdate {
                    context: "Encrypted Link Handshake checkpoint changed".into(),
                    source: None,
                })
            }
        };
        Ok(Ok(report))
    }
}

fn link_handshake_error_requires_recovery(err: &PaykitSdkError) -> bool {
    matches!(
        err,
        PaykitSdkError::Protocol { .. } | PaykitSdkError::RecoveryRequired { .. }
    )
}

struct PendingHandshake {
    snapshot: paykit_lib::EncryptedLinkHandshakeSnapshot,
    report: LinkedPeerHandshakeReport,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
}

struct HandshakeCheckpoint {
    pending: Option<PendingHandshake>,
    linked: bool,
    recovery: Option<RecoveryObservationCheckpoint>,
}

impl HandshakeCheckpoint {
    fn load(tx: &dyn StorageTransaction, counterparty: &PubkyPublicKey) -> Self {
        Self {
            pending: pending_handshake(tx, counterparty),
            linked: tx
                .linked_peer(counterparty)
                .is_some_and(|peer| peer.state == LinkedPeerState::Linked),
            recovery: RecoveryObservationCheckpoint::load(tx, counterparty),
        }
    }
}

enum HandshakeProbe {
    Idle(LinkedPeerHandshakeReport),
    Reload(Option<Box<paykit_lib::PaykitNoiseKeyAuthorization>>),
}

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

    #[cfg(test)]
    pub(super) async fn private_queue_readiness(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateQueueReadiness> {
        self.storage
            .transaction(|tx| Self::private_queue_readiness_in_transaction(tx, counterparty))
            .await
    }

    pub(super) fn private_queue_readiness_in_transaction(
        tx: &dyn StorageTransaction,
        counterparty: &PubkyPublicKey,
    ) -> Result<PrivateQueueReadiness> {
        let peer_state = tx.linked_peer(counterparty).map(|peer| peer.state);
        let state = tx.encrypted_link_state(counterparty);
        let has_active_link = state
            .as_ref()
            .and_then(|state| state.link_snapshot.as_ref())
            .is_some();
        let has_restorable_handshake = state.as_ref().is_some_and(|state| {
            state.handshake_snapshot.is_some() && state.handshake_role.is_some()
        });
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
        require_peer_not_recovery_required_or_blocked(peer_state, counterparty)
    }

    /// Block a counterparty for local Paykit private workflows.
    ///
    /// Blocking is local policy. It clears stored Encrypted Link state so the
    /// peer cannot resume private workflows until explicitly unblocked and
    /// linked again.
    pub async fn block_peer(&self, counterparty: PubkyPublicKey) -> Result<LinkedPeerRecord> {
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            move |tx| {
                let local_public_key = initialized_identity_in_transaction(tx, "block peer")?;
                if counterparty == local_public_key {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot block the local Paykit identity".into(),
                        source: None,
                    });
                }
                let now = self.clock.now();
                let Some(lease) =
                    tx.claim_peer_link_operation(&counterparty, now, now + lease_timeout)?
                else {
                    return Ok(None);
                };
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
                tx.release_peer_link_operation(&counterparty, lease.lease_id);
                Ok(Some(record))
            }
        })
        .await?
        // A live peer lease is contention, not a storage transaction to retry.
        .ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
            context: format!(
                "peer link operation already in progress for counterparty {counterparty}"
            ),
            source: None,
        })
    }

    /// Remove a local peer block and return the peer to `NotLinked`.
    ///
    /// Existing Encrypted Link snapshots are not restored. Callers should start
    /// a fresh Encrypted Link Handshake before private workflows resume.
    pub async fn unblock_peer(&self, counterparty: PubkyPublicKey) -> Result<LinkedPeerRecord> {
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        self.retry_storage_transaction(|| {
            let counterparty = counterparty.clone();
            move |tx| {
                let local_public_key = initialized_identity_in_transaction(tx, "unblock peer")?;
                if counterparty == local_public_key {
                    return Err(PaykitSdkError::Policy {
                        context: "cannot unblock the local Paykit identity".into(),
                        source: None,
                    });
                }
                let now = self.clock.now();
                let Some(lease) =
                    tx.claim_peer_link_operation(&counterparty, now, now + lease_timeout)?
                else {
                    return Ok(None);
                };
                let mut record = tx
                    .linked_peer(&counterparty)
                    .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
                if record.state == LinkedPeerState::Blocked {
                    record.state = LinkedPeerState::NotLinked;
                    record.local_recovery_attempt_id = None;
                    record.local_recovery_marker_created_at = None;
                    record.local_recovery_marker_last_error = None;
                    record.last_sync_at = Some(now);
                    record.failure_count = 0;
                    tx.save_linked_peer(record.clone());
                    clear_encrypted_link_state(tx, &counterparty, now);
                }
                tx.release_peer_link_operation(&counterparty, lease.lease_id);
                Ok(Some(record))
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
    ///
    /// An idle handshake is probed without taking a peer lease or rewriting state.
    /// Available work is reloaded and validated under the ordinary lease.
    pub async fn advance_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<LinkedPeerHandshakeReport> {
        let authorization = match self.probe_link_handshake(&counterparty).await? {
            HandshakeProbe::Idle(report) => return Ok(report),
            HandshakeProbe::Reload(authorization) => authorization.map(|value| *value),
        };
        async {
            let (session_access, _) = self.private_link_session_access().await?;
            let lease = self.claim_peer_link_operation(&counterparty).await?;
            let result = async {
                if let Some(authorization) = &authorization {
                    self.pin_counterparty_noise_key_authorization(&counterparty, authorization)
                        .await?;
                }
                self.advance_link_handshake_with_claim(
                    &session_access,
                    counterparty,
                    lease.clone(),
                    authorization.as_ref(),
                )
                .await
            }
            .await;
            self.finish_peer_link_operation(lease, result).await
        }
        .await
    }

    /// Ensure an Encrypted Link is started or advanced for one counterparty.
    ///
    /// The SDK deterministically chooses the local handshake role from the two
    /// public keys. Recovery markers are checked before reusing a link or
    /// advancing a handshake. Existing active links are returned as linked. Existing
    /// pending handshakes are advanced. `max_advance_steps` bounds how many
    /// stored handshake advances this call attempts after starting or finding a
    /// pending handshake. Idle handshakes use the same read-only probe as
    /// [`advance_link_handshake`](Self::advance_link_handshake).
    pub async fn ensure_link_with_peer(
        &self,
        counterparty: PubkyPublicKey,
        max_advance_steps: u32,
    ) -> Result<LinkedPeerHandshakeReport> {
        let probe = self.probe_link_handshake(&counterparty).await?;
        self.ensure_link_with_peer_from_probe(counterparty, max_advance_steps, probe)
            .await
    }

    pub(super) async fn ensure_link_with_peer_if_available(
        &self,
        counterparty: PubkyPublicKey,
        max_advance_steps: u32,
    ) -> Result<Option<LinkedPeerHandshakeReport>> {
        let (access, _, checkpoint) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                Ok(HandshakeCheckpoint::load(tx, &counterparty))
            })
            .await?;
        let Some(access) = access else {
            return Ok(None);
        };
        if !access.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)? {
            return Ok(None);
        }
        let probe = self
            .probe_link_handshake_from_checkpoint(
                &counterparty,
                &access,
                checkpoint.expect("active session loads handshake checkpoint"),
            )
            .await?;
        drop(access);
        self.ensure_link_with_peer_from_probe(counterparty, max_advance_steps, probe)
            .await
            .map(Some)
    }

    async fn ensure_link_with_peer_from_probe(
        &self,
        counterparty: PubkyPublicKey,
        max_advance_steps: u32,
        probe: HandshakeProbe,
    ) -> Result<LinkedPeerHandshakeReport> {
        let authorization = match probe {
            HandshakeProbe::Idle(report) => return Ok(report),
            HandshakeProbe::Reload(authorization) => authorization.map(|value| *value),
        };
        let operation = async {
            let (session_access, _) = self.private_link_session_access().await?;
            let (role, lease, initial) = self
                .with_guarded_storage_operation(
                    Arc::clone(&session_access._guard),
                    Box::pin(async {
                        let local_public_key = session_access.public_key()?;
                        require_distinct_link_identity(&local_public_key, &counterparty)?;
                        let role = deterministic_handshake_role(&local_public_key, &counterparty);
                        let lease = self.claim_peer_link_operation(&counterparty).await?;
                        let initial = self
                            .prepare_link_handshake_with_claim(
                                counterparty.clone(),
                                role,
                                lease.clone(),
                                authorization,
                            )
                            .await;
                        Ok((role, lease, initial))
                    }),
                )
                .await?;
            let result = async {
                let (mut report, authorization) = initial?;
                for _ in 0..max_advance_steps {
                    if report.state == LinkedPeerState::Linked {
                        return Ok(report);
                    }
                    report = match self
                        .advance_link_handshake_with_claim(
                            &session_access,
                            counterparty.clone(),
                            lease.clone(),
                            Some(&authorization),
                        )
                        .await
                    {
                        Ok(report) => Ok(report),
                        Err(err) if link_handshake_error_requires_recovery(&err) => {
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
                            self.with_guarded_storage_operation(
                                Arc::clone(&session_access._guard),
                                Box::pin(self.start_link_handshake_with_claim(
                                    counterparty.clone(),
                                    role,
                                    lease.clone(),
                                    authorization.noise_public_key(),
                                )),
                            )
                            .await
                        }
                        Err(err) => Err(err),
                    }?;
                }
                Ok(report)
            }
            .await;
            self.finish_peer_link_operation(lease, result).await
        };
        operation.await
    }

    async fn probe_link_handshake(&self, counterparty: &PubkyPublicKey) -> Result<HandshakeProbe> {
        let (access, _, checkpoint) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                Ok(HandshakeCheckpoint::load(tx, counterparty))
            })
            .await?;
        let Some(checkpoint) = checkpoint else {
            return Ok(HandshakeProbe::Reload(None));
        };
        let access = access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        self.probe_link_handshake_from_checkpoint(counterparty, &access, checkpoint)
            .await
    }

    async fn probe_link_handshake_from_checkpoint(
        &self,
        counterparty: &PubkyPublicKey,
        access: &GuardedSessionAccess,
        checkpoint: HandshakeCheckpoint,
    ) -> Result<HandshakeProbe> {
        if checkpoint.linked {
            require_distinct_link_identity(&access.public_key()?, counterparty)?;
            self.validate_local_noise_key_authorization(access).await?;
            if let Some(current) = self
                .unchanged_link_checkpoint(counterparty, access, checkpoint.recovery)
                .await?
            {
                return Ok(HandshakeProbe::Idle(LinkedPeerHandshakeReport {
                    counterparty: counterparty.clone(),
                    state: LinkedPeerState::Linked,
                    generation: current.link_state.generation,
                    handshake_role: None,
                }));
            }
        }
        let Some(pending) = checkpoint.pending else {
            return Ok(HandshakeProbe::Reload(None));
        };
        let secret_key = access.paykit_noise_secret_key()?;
        let session_info = access.session.info();
        let Ok(Some(read_path)) = pending
            .snapshot
            .next_handshake_read_path(session_info.public_key(), &secret_key)
        else {
            return Ok(HandshakeProbe::Reload(None));
        };
        self.validate_local_noise_key_authorization(access).await?;
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for handshake lookup".into(),
                    source: None,
                })?;
        let remote_public_key = counterparty.to_public_key()?;
        let authorization =
            noise_key_authorization::require_authorization(&public_storage, &remote_public_key)
                .await?;
        if authorization != pending.authorization {
            return Ok(HandshakeProbe::Reload(Some(Box::new(authorization))));
        }
        let (marker, available) = tokio::join!(
            paykit_lib::fetch_encrypted_link_recovery_marker(
                &public_storage,
                &secret_key,
                session_info.public_key(),
                &remote_public_key,
                pending.snapshot.remote_noise_public_key(),
            ),
            public_storage.exists(read_path),
        );
        if marker?.is_some_and(|marker| {
            Some(marker.attempt_id()) != pending.snapshot.recovery_context().remote_attempt_id()
        }) {
            return Ok(HandshakeProbe::Reload(Some(Box::new(authorization))));
        }
        let available = available.map_err(|error| {
            map_pubky_transport_error("check handshake message availability", error)
        })?;
        Ok(if available {
            HandshakeProbe::Reload(Some(Box::new(authorization)))
        } else {
            HandshakeProbe::Idle(pending.report)
        })
    }

    pub(super) async fn prepare_link_handshake_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
        lease: PeerLinkOperationLease,
        authorization: Option<paykit_lib::PaykitNoiseKeyAuthorization>,
    ) -> Result<(
        LinkedPeerHandshakeReport,
        paykit_lib::PaykitNoiseKeyAuthorization,
    )> {
        let authorization = match authorization {
            Some(authorization) => {
                self.pin_counterparty_noise_key_authorization(&counterparty, &authorization)
                    .await?;
                authorization
            }
            None => {
                self.counterparty_noise_key_authorization(&counterparty)
                    .await?
            }
        };
        let remote_noise_public_key = authorization.noise_public_key().clone();
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

        let report = match (peer_state, link_state) {
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
        Ok((report, authorization))
    }

    async fn advance_link_handshake_with_claim(
        &self,
        session_access: &GuardedSessionAccess,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        authorization: Option<&paykit_lib::PaykitNoiseKeyAuthorization>,
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
        let fetched_authorization;
        let authorization = match authorization {
            Some(authorization) => authorization,
            None => {
                fetched_authorization = self
                    .counterparty_noise_key_authorization(&counterparty)
                    .await?;
                &fetched_authorization
            }
        };
        if stored_link_state.link_snapshot.is_some() {
            let remote_noise_public_key = authorization.noise_public_key();
            let (changed, role) = {
                let role =
                    stored_link_state
                        .handshake_role
                        .unwrap_or(deterministic_handshake_role(
                            &session_access.public_key()?,
                            &counterparty,
                        ));
                let changed = self
                    .observe_remote_recovery_marker_with_lease(
                        &counterparty,
                        session_access,
                        &lease,
                        remote_noise_public_key,
                    )
                    .await?;
                (changed, role)
            };
            if changed {
                return self
                    .with_guarded_storage_operation(
                        Arc::clone(&session_access._guard),
                        Box::pin(self.start_link_handshake_with_claim(
                            counterparty,
                            role,
                            lease,
                            remote_noise_public_key,
                        )),
                    )
                    .await;
            }
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

        let source = self
            .storage
            .transaction(|tx| {
                let source = HandshakeSource {
                    peer: tx.linked_peer(&counterparty).ok_or_else(|| {
                        PaykitSdkError::RecoveryRequired {
                            context: "missing Linked Peer for handshake".into(),
                            source: None,
                        }
                    })?,
                    state: stored_link_state,
                    identity: session_access.public_key()?,
                    noise_key: PubkyPublicKey::from_public_key(
                        &pubky::Keypair::from_secret(&session_access.paykit_noise_secret_key()?)
                            .public_key(),
                    ),
                };
                source.require_current(tx, &lease, self.clock.now())?;
                Ok(source)
            })
            .await?;
        // Keep the handshake future out of each enclosing payment workflow's frame.
        Box::pin(self.advance_handshake_checkpoint(source, session_access, &lease, authorization))
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
        session_access: &GuardedSessionAccess,
        snapshot: paykit_lib::EncryptedLinkHandshakeSnapshot,
        authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    ) -> Result<paykit_lib::EncryptedLinkHandshake> {
        let remote_public_key = snapshot.recipient().clone();
        let handshake = paykit_lib::restore_encrypted_link_handshake(
            session_access.session.clone(),
            session_access.paykit_noise_secret_key()?,
            &remote_public_key,
            session_access.outbox_client.clone(),
            snapshot,
        )
        .await?;
        noise_key_authorization::validate_remote_static_key(
            authorization,
            handshake.remote_static_public_key(),
            false,
        )?;
        Ok(handshake)
    }

    async fn observe_handshake_source(
        &self,
        source: &HandshakeSource,
        access: &GuardedSessionAccess,
    ) -> Result<Option<EncryptedLinkRecoveryMarker>> {
        self.validate_local_noise_key_authorization(access).await?;
        let authorization = self
            .paykit_noise_key_authorization(source.peer.counterparty.clone())
            .await?;
        if source.peer.noise_key_authorization.as_ref() != Some(&authorization) {
            return Err(PaykitSdkError::RecoveryRequired {
                context: "handshake Noise key authorization changed".into(),
                source: None,
            });
        }
        Ok(paykit_lib::fetch_encrypted_link_recovery_marker(
            &access.outbox_client.public_storage(),
            &access.paykit_noise_secret_key()?,
            access.session.info().public_key(),
            &source.peer.counterparty.to_public_key()?,
            authorization.noise_public_key(),
        )
        .await?)
    }

    async fn apply_handshake_observation(
        &self,
        source: &mut HandshakeSource,
        access: &GuardedSessionAccess,
        lease: &PeerLinkOperationLease,
        observation: Result<Option<EncryptedLinkRecoveryMarker>>,
        update: Result<Option<HandshakeCheckpointUpdate>>,
    ) -> Result<LinkedPeerHandshakeReport> {
        let (result, peer, state) = self
            .with_guarded_storage_operation(
                Arc::clone(&access._guard),
                Box::pin(self.storage.transaction(|tx| {
                    let result = source.apply(tx, lease, self.clock.now(), observation, update)?;
                    Ok((
                        result,
                        tx.linked_peer(&lease.counterparty)
                            .expect("current handshake peer"),
                        tx.encrypted_link_state(&lease.counterparty)
                            .expect("current handshake checkpoint"),
                    ))
                })),
            )
            .await?;
        // A rejected or unconfirmed commit must leave the in-memory checkpoint unchanged.
        source.peer = peer;
        source.state = state;
        if result
            .as_ref()
            .is_err_and(link_handshake_error_requires_recovery)
        {
            self.publish_local_recovery_marker_if_possible(
                &lease.counterparty,
                lease,
                Some(access),
            )
            .await;
        }
        result
    }

    async fn acknowledged_handshake_update(
        &self,
        snapshot: paykit_lib::EncryptedLinkHandshakeSnapshot,
        access: &GuardedSessionAccess,
        authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    ) -> Result<HandshakeCheckpointUpdate> {
        let handshake = self
            .restore_link_handshake_from_snapshot(access, snapshot, authorization)
            .await?;
        match handshake.finish_if_complete()? {
            paykit_lib::HandshakeProgress::Pending(handshake) => {
                Ok(HandshakeCheckpointUpdate::Pending(handshake.snapshot()?))
            }
            paykit_lib::HandshakeProgress::Complete(link) => {
                noise_key_authorization::validate_remote_static_key(
                    authorization,
                    link.remote_static_public_key(),
                    true,
                )?;
                Ok(HandshakeCheckpointUpdate::Complete(link.snapshot()?))
            }
        }
    }

    async fn advance_handshake_checkpoint(
        &self,
        mut source: HandshakeSource,
        access: &GuardedSessionAccess,
        lease: &PeerLinkOperationLease,
        authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    ) -> Result<LinkedPeerHandshakeReport> {
        let secret = access.paykit_noise_secret_key()?;
        let local = access.session.info().public_key().clone();
        let mut observation = self.observe_handshake_source(&source, access).await;
        let unchanged = observation.as_ref().is_ok_and(|marker| {
            marker.as_ref().is_none_or(|marker| {
                Some(marker.attempt_id()) == source.peer.remote_recovery_attempt_id.as_deref()
            })
        });
        if let Ok(Some(marker)) = &observation {
            if !unchanged {
                let role = source
                    .state
                    .handshake_role
                    .unwrap_or(deterministic_handshake_role(
                        &source.identity,
                        &lease.counterparty,
                    ));
                return self
                    .with_guarded_storage_operation(
                        Arc::clone(&access._guard),
                        Box::pin(async {
                            self.storage
                                .transaction(|tx| {
                                    source.require_current(tx, lease, self.clock.now())?;
                                    mark_recovery_required_for_marker_in_transaction(
                                        tx,
                                        &lease.counterparty,
                                        self.clock.now(),
                                        Some(marker.attempt_id()),
                                    )
                                })
                                .await?;
                            self.start_link_handshake_with_claim(
                                lease.counterparty.clone(),
                                role,
                                lease.clone(),
                                authorization.noise_public_key(),
                            )
                            .await
                        }),
                    )
                    .await;
            }
        }
        let mut update = if unchanged {
            async {
                let snapshot = source.snapshot()?;
                if snapshot.pending_publication(&local, &secret)?.is_some() {
                    return Ok(None);
                }
                let handshake = self
                    .restore_link_handshake_from_snapshot(access, snapshot, authorization)
                    .await?;
                let handshake = match handshake.finish_if_complete()? {
                    paykit_lib::HandshakeProgress::Complete(link) => {
                        noise_key_authorization::validate_remote_static_key(
                            authorization,
                            link.remote_static_public_key(),
                            true,
                        )?;
                        return Ok(Some(HandshakeCheckpointUpdate::Complete(link.snapshot()?)));
                    }
                    paykit_lib::HandshakeProgress::Pending(handshake) => handshake,
                };
                let Some(prepared) = handshake.prepare_next_step().await? else {
                    return Ok(None);
                };
                noise_key_authorization::validate_remote_static_key(
                    authorization,
                    prepared.remote_static_public_key(),
                    prepared.is_handshake_complete(),
                )?;
                let snapshot = prepared.into_snapshot();
                Ok(Some(
                    if snapshot.pending_publication(&local, &secret)?.is_some() {
                        HandshakeCheckpointUpdate::Pending(snapshot)
                    } else {
                        self.acknowledged_handshake_update(snapshot, access, authorization)
                            .await?
                    },
                ))
            }
            .await
        } else {
            Ok(None)
        };
        let pending_packet = |snapshot: &paykit_lib::EncryptedLinkHandshakeSnapshot| {
            Ok::<_, PaykitSdkError>(
                snapshot
                    .pending_publication(&local, &secret)?
                    .map(|(path, packet)| (path.to_owned(), packet.to_vec())),
            )
        };
        let packet = match &update {
            Ok(Some(HandshakeCheckpointUpdate::Pending(snapshot))) => pending_packet(snapshot),
            Ok(None) if unchanged => source
                .snapshot()
                .and_then(|snapshot| pending_packet(&snapshot)),
            _ => Ok(None),
        };
        let packet = match packet {
            Ok(packet) => packet,
            Err(error) => {
                update = Err(error);
                None
            }
        };
        let Some((path, packet)) = packet else {
            if unchanged {
                observation = self.observe_handshake_source(&source, access).await;
            }
            return self
                .apply_handshake_observation(&mut source, access, lease, observation, update)
                .await;
        };
        // Only this outbox slot remains locked across the short pending save and PUT.
        let publication = {
            let source = &mut source;
            let packet = &packet;
            paykit_lib::with_write_lock(&access.session, &path, |lock| async move {
                let observation = self.observe_handshake_source(source, access).await;
                self.apply_handshake_observation(source, access, lease, observation, update)
                    .await?;
                // Only packet PUT failures enter post-publication validation.
                Ok::<_, PaykitSdkError>(
                    access
                        .session
                        .storage()
                        .put_locked(&lock, packet.to_vec())
                        .await
                        .map(|_| ())
                        .map_err(|error| {
                            map_pubky_transport_error("publish Encrypted Link Handshake", error)
                        }),
                )
            })
            .await?
        };
        let observation = self.observe_handshake_source(&source, access).await;
        // Final XX acknowledgement and Linked promotion share one durable boundary.
        let update = async {
            publication?;
            self.acknowledged_handshake_update(
                source.snapshot()?.acknowledge_publication(&path, &packet)?,
                access,
                authorization,
            )
            .await
            .map(Some)
        }
        .await;
        self.apply_handshake_observation(&mut source, access, lease, observation, update)
            .await
    }

    async fn start_link_handshake(
        &self,
        counterparty: PubkyPublicKey,
        role: EncryptedLinkHandshakeRole,
    ) -> Result<LinkedPeerHandshakeReport> {
        self.with_storage_operation(Box::pin(async {
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
        }))
        .await
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
        self.storage
            .transaction(|tx| {
                self.require_current_peer_link_operation_in_transaction(tx, lease, session_access)
            })
            .await
    }

    pub(super) fn require_current_peer_link_operation_in_transaction(
        &self,
        tx: &mut dyn StorageTransaction,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<()> {
        let secret = session_access.paykit_noise_secret_key()?;
        let noise_public_key =
            PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&secret).public_key());
        crate::storage::require_peer_link_operation_lease(tx, lease)?;
        if lease.expires_at <= self.clock.now() {
            return Err(PaykitSdkError::Policy {
                context: "peer link operation lease expired".into(),
                source: None,
            });
        }
        crate::storage::bind_paykit_noise_key(tx, noise_public_key)
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
        Ok(self
            .counterparty_noise_key_authorization(counterparty)
            .await?
            .noise_public_key()
            .clone())
    }

    pub(super) async fn counterparty_noise_key_authorization(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<paykit_lib::PaykitNoiseKeyAuthorization> {
        let authorization = self
            .paykit_noise_key_authorization(counterparty.clone())
            .await?;
        self.pin_counterparty_noise_key_authorization(counterparty, &authorization)
            .await?;
        Ok(authorization)
    }
}

fn pending_handshake(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
) -> Option<PendingHandshake> {
    let peer = tx.linked_peer(counterparty)?;
    if peer.state != LinkedPeerState::Linking || peer.failure_count != 0 {
        return None;
    }
    let state = tx.encrypted_link_state(counterparty)?;
    if state.link_snapshot.is_some() {
        return None;
    }
    let role = state.handshake_role?;
    let snapshot =
        paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(state.handshake_snapshot.as_ref()?)
            .ok()?;
    if PubkyPublicKey::from_public_key(snapshot.recipient()) != *counterparty {
        return None;
    }
    crate::domain::linked_peers::require_recovery_context(&peer, snapshot.recovery_context())
        .ok()?;
    let authorization = peer.noise_key_authorization?;
    if snapshot.remote_noise_public_key() != authorization.noise_public_key() {
        return None;
    }
    Some(PendingHandshake {
        snapshot,
        report: LinkedPeerHandshakeReport {
            counterparty: counterparty.clone(),
            state: LinkedPeerState::Linking,
            generation: state.generation,
            handshake_role: Some(role),
        },
        authorization,
    })
}

pub(super) fn require_peer_not_recovery_required_or_blocked(
    peer_state: Option<LinkedPeerState>,
    counterparty: &PubkyPublicKey,
) -> Result<()> {
    match peer_state {
        Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired {
            context: format!("Encrypted Link recovery is required for counterparty {counterparty}"),
            source: None,
        }),
        Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy {
            context: format!("counterparty {counterparty} is blocked"),
            source: None,
        }),
        _ => Ok(()),
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

#[cfg(test)]
mod handshake_checkpoint_tests {
    use super::super::tests::seed_private_capable_identity_and_handshake;
    use super::*;
    use crate::InMemoryStorage;

    #[tokio::test]
    async fn test_handshake_observations_preserve_current_state_and_errors() {
        let key = || PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let counterparty = key();
        let storage = InMemoryStorage::new();
        seed_private_capable_identity_and_handshake(&storage, counterparty.clone()).await;
        let now = Utc::now();
        let expires_at = now + ChronoDuration::minutes(1);
        let lease = storage
            .transaction(|tx| {
                tx.save_paykit_noise_public_key(key());
                Ok(tx
                    .claim_peer_link_operation(&counterparty, now, expires_at)?
                    .unwrap())
            })
            .await
            .unwrap();
        let baseline = storage.snapshot().unwrap();
        let identity = baseline.identity_state.as_ref().unwrap();
        for drift in [
            "idle",
            "protocol",
            "transport",
            "identity",
            "key",
            "save-rejected",
        ] {
            let source = HandshakeSource {
                peer: baseline.linked_peers[&counterparty].clone(),
                state: baseline.encrypted_link_states[&counterparty].clone(),
                identity: identity.public_key.clone().unwrap(),
                noise_key: baseline.paykit_noise_public_key.clone().unwrap(),
            };
            let mut current = baseline.clone();
            match drift {
                "identity" => current.identity_state.as_mut().unwrap().public_key = Some(key()),
                "key" => current.paykit_noise_public_key = Some(key()),
                _ => {}
            }
            let storage = InMemoryStorage::from_state(current.clone());
            let result = storage
                .transaction(|tx| {
                    let update = match drift {
                        "idle" => Ok(None),
                        "save-rejected" => {
                            use pubky_noise::{
                                serializer::PubkyNoiseSessionState,
                                snow_crypto::{HandshakePattern, NoisePhase, NoiseStep},
                            };
                            let state = PubkyNoiseSessionState {
                                version: pubky_noise::serializer::SESSION_STATE_VERSION,
                                phase: NoisePhase::HandShake,
                                pattern: HandshakePattern::PatternXX,
                                initiator: true,
                                ephemeral_secret: [1; 32],
                                static_secret: Some([2; 32]),
                                counter: 3,
                                noise_step: NoiseStep::Final,
                                sub_step_index: 0,
                                handshake_hash: Some([3; 32]),
                                link_id: None,
                                sending_nonce: 0,
                                receiving_nonce: 0,
                                write_counter: 0,
                                read_counter: 0,
                                endpoint_pubkey: counterparty.to_public_key().unwrap().to_bytes(),
                                handshake_messages: vec![vec![5; 96]],
                            };
                            let mut inner = state.serialize();
                            inner.extend_from_slice(
                                &pubky::Keypair::random().public_key().to_bytes(),
                            );
                            inner.extend_from_slice(&[0; 72]);
                            let mut bytes = vec![1];
                            bytes.extend_from_slice(&(inner.len() as u16).to_be_bytes());
                            bytes.extend_from_slice(&inner);
                            bytes.extend_from_slice(&[0; 2]);
                            Ok(Some(HandshakeCheckpointUpdate::Pending(
                                paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(&bytes)
                                    .unwrap(),
                            )))
                        }
                        "transport" => Err(PaykitSdkError::Transport {
                            context: "handshake PUT failed".into(),
                            source: None,
                        }),
                        _ => Err(PaykitSdkError::Protocol {
                            context: "invalid handshake packet".into(),
                            source: None,
                        }),
                    };
                    let result = source.apply(tx, &lease, now, Ok(None), update)?;
                    if drift == "save-rejected" {
                        assert_eq!(
                            result.as_ref().unwrap().generation,
                            baseline.encrypted_link_states[&counterparty].generation + 1
                        );
                        return Err(PaykitSdkError::Transport {
                            context: "handshake checkpoint quota exceeded".into(),
                            source: None,
                        });
                    }
                    Ok(result)
                })
                .await;
            match drift {
                "idle" => assert_eq!(result.unwrap().unwrap(), source.report()),
                "protocol" => assert!(matches!(result, Ok(Err(PaykitSdkError::Protocol { .. })))),
                "transport" => assert!(
                    matches!(result, Ok(Err(PaykitSdkError::Transport { context, .. })) if context == "handshake PUT failed")
                ),
                "save-rejected" => {
                    assert!(matches!(result, Err(PaykitSdkError::Transport { .. })));
                    assert_eq!(source.peer, baseline.linked_peers[&counterparty]);
                    assert_eq!(source.state, baseline.encrypted_link_states[&counterparty]);
                }
                _ => assert!(matches!(
                    result,
                    Err(PaykitSdkError::ConcurrentUpdate { .. })
                )),
            }
            let after = storage.snapshot().unwrap();
            if drift == "protocol" {
                assert_eq!(
                    after.linked_peers[&counterparty].state,
                    LinkedPeerState::RecoveryRequired
                );
            } else {
                assert_eq!(after, current, "{drift}");
            }
        }
    }
}
