use super::*;
use crate::domain::private_stream::persist_private_stream_batch_in_transaction;
use futures_util::{stream, StreamExt};

const PRIVATE_INBOX_PROBE_CONCURRENCY: usize = 16;

struct PrivateReceiveSnapshot {
    link: paykit_lib::EncryptedLinkSnapshot,
    state: EncryptedLinkStateRecord,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
    has_peer_lease: bool,
}

enum PrivateInboxProbe {
    Empty,
    Ready(Box<PrivateReceiveSnapshot>),
    Reload,
}

struct PrivateReceiveLink {
    link: paykit_lib::EncryptedLink,
    state: EncryptedLinkStateRecord,
    authorized_apps: Option<HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>>,
    authorization: paykit_lib::PaykitNoiseKeyAuthorization,
}

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Receive and durably persist available private messages.
    ///
    /// This requires a stored Encrypted Link snapshot for the counterparty.
    /// Handshake establishment and recovery are separate workflows.
    /// Empty inboxes are probed without acquiring a peer lease or writing state.
    /// Available messages are prepared read-only and committed only while the
    /// original link checkpoint remains current and no peer operation holds a lease.
    pub async fn receive_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateStreamIntakeReport> {
        let (session_access, snapshots) = self.private_receive_context(Some(&counterparty)).await?;
        let snapshot = snapshots
            .into_iter()
            .next()
            .and_then(|(_, snapshot)| snapshot);
        let probe = self
            .probe_private_inbox(&counterparty, &session_access, snapshot)
            .await?;
        self.receive_probed_private_messages(counterparty, &session_access, probe)
            .await
    }

    async fn receive_probed_private_messages(
        &self,
        counterparty: PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        probe: PrivateInboxProbe,
    ) -> Result<PrivateStreamIntakeReport> {
        match probe {
            PrivateInboxProbe::Empty => {
                return Ok(PrivateStreamIntakeReport {
                    receive_batch_id: None,
                    stream_item_ids: Vec::new(),
                    event_conflicts: Vec::new(),
                })
            }
            PrivateInboxProbe::Ready(snapshot) => {
                if let Some(report) = Box::pin(self.receive_private_messages_from_snapshot(
                    counterparty.clone(),
                    session_access,
                    *snapshot,
                ))
                .await?
                {
                    return Ok(report);
                }
            }
            PrivateInboxProbe::Reload => {}
        }
        // Recovery decisions require a fresh read under the peer lease.
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .receive_private_messages_with_claim(counterparty, lease.clone(), session_access)
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Receive private messages from every locally linked counterparty.
    pub async fn receive_private_messages_from_linked_peers(
        &self,
    ) -> Result<Vec<PrivateStreamCounterpartyIntakeReport>> {
        let (session_access, counterparties) = match self.private_receive_context(None).await {
            Ok(context) => context,
            Err(error) => {
                let counterparties = self
                    .storage
                    .transaction(|tx| Ok(linked_counterparties(tx)))
                    .await?;
                return Ok(counterparties
                    .into_iter()
                    .map(|counterparty| PrivateStreamCounterpartyIntakeReport {
                        counterparty,
                        report: None,
                        error: Some(error.to_string()),
                    })
                    .collect());
            }
        };
        let mut reports = Vec::with_capacity(counterparties.len());
        let probes = stream::iter(counterparties)
            .map(|(counterparty, snapshot)| {
                let session_access = &session_access;
                async move {
                    let probe = self
                        .probe_private_inbox(&counterparty, session_access, snapshot)
                        .await;
                    (counterparty, probe)
                }
            })
            .buffer_unordered(PRIVATE_INBOX_PROBE_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        // Finish polling every probe before a serialized receive can block it.
        for (counterparty, probe) in probes {
            let result = match probe {
                Ok(probe) => {
                    self.receive_probed_private_messages(
                        counterparty.clone(),
                        &session_access,
                        probe,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(report) => reports.push(PrivateStreamCounterpartyIntakeReport {
                    counterparty,
                    report: Some(report),
                    error: None,
                }),
                Err(err) => reports.push(PrivateStreamCounterpartyIntakeReport {
                    counterparty,
                    report: None,
                    error: Some(err.to_string()),
                }),
            }
        }
        reports.sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));
        Ok(reports)
    }

    async fn private_receive_context(
        &self,
        counterparty: Option<&PubkyPublicKey>,
    ) -> Result<(
        GuardedSessionAccess,
        Vec<(PubkyPublicKey, Option<PrivateReceiveSnapshot>)>,
    )> {
        let (access, _, peers) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                let peers = counterparty
                    .map_or_else(|| linked_counterparties(tx), |peer| vec![peer.clone()]);
                Ok(peers
                    .into_iter()
                    .map(|peer| {
                        let snapshot = private_receive_snapshot(tx, &peer);
                        (peer, snapshot)
                    })
                    .collect())
            })
            .await?;
        let access = access.ok_or_else(|| PaykitSdkError::Identity {
            context: "no Pubky session available".into(),
            source: None,
        })?;
        access.paykit_noise_secret_key()?;
        self.validate_local_noise_key_authorization(&access).await?;
        Ok((access, peers.expect("active session reads peer state")))
    }

    async fn probe_private_inbox(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        snapshot: Option<PrivateReceiveSnapshot>,
    ) -> Result<PrivateInboxProbe> {
        let Some(snapshot) = snapshot else {
            return Ok(PrivateInboxProbe::Reload);
        };
        let public_storage =
            self.pubky
                .load_public_storage()
                .await?
                .ok_or_else(|| PaykitSdkError::Identity {
                    context: "no Pubky public storage available for private message lookup".into(),
                    source: None,
                })?;
        let remote_public_key = counterparty.to_public_key()?;
        let authorization =
            noise_key_authorization::require_authorization(&public_storage, &remote_public_key)
                .await?;
        if authorization != snapshot.authorization {
            return Ok(PrivateInboxProbe::Reload);
        }
        let link = &snapshot.link;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let session_info = session_access.session.info();
        let (marker, pending) = tokio::join!(
            paykit_lib::fetch_encrypted_link_recovery_marker(
                &public_storage,
                &secret_key,
                session_info.public_key(),
                &remote_public_key,
                link.remote_noise_public_key(),
            ),
            link.has_pending_private_application_message(
                &public_storage,
                session_info.public_key(),
                &secret_key,
            ),
        );
        let marker = marker?;
        if marker.is_some_and(|marker| {
            Some(marker.attempt_id()) != link.recovery_context().remote_attempt_id()
        }) {
            return Ok(PrivateInboxProbe::Reload);
        }
        match pending {
            // Read-only empty probes can coexist with a lease. Actual receives
            // must claim it, including reclaiming an expired record.
            Ok(true) if snapshot.has_peer_lease => Ok(PrivateInboxProbe::Reload),
            Ok(true) => Ok(PrivateInboxProbe::Ready(Box::new(snapshot))),
            Ok(false) => Ok(PrivateInboxProbe::Empty),
            Err(paykit_lib::PaykitError::Validation(_)) => Ok(PrivateInboxProbe::Reload),
            Err(err) => Err(err.into()),
        }
    }

    async fn receive_private_messages_from_snapshot(
        &self,
        counterparty: PubkyPublicKey,
        session_access: &GuardedSessionAccess,
        snapshot: PrivateReceiveSnapshot,
    ) -> Result<Option<PrivateStreamIntakeReport>> {
        let authorized_apps = self.private_receive_authorized_apps(&counterparty).await;
        let link = match paykit_lib::restore_encrypted_link(
            session_access.session.clone(),
            session_access.paykit_noise_secret_key()?,
            &counterparty.to_public_key()?,
            session_access.outbox_client.clone(),
            snapshot.link,
        )
        .await
        {
            Ok(link) => link,
            Err(err @ paykit_lib::PaykitError::Transport { .. }) => return Err(err.into()),
            Err(_) => return Ok(None),
        };
        if noise_key_authorization::validate_remote_static_key(
            &snapshot.authorization,
            link.remote_static_public_key(),
            true,
        )
        .is_err()
        {
            return Ok(None);
        }
        self.receive_private_messages_with_link(
            counterparty,
            None,
            session_access,
            PrivateReceiveLink {
                link,
                state: snapshot.state,
                authorized_apps,
                authorization: snapshot.authorization,
            },
        )
        .await
    }

    async fn private_receive_authorized_apps(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>> {
        // A failed lookup keeps cached authorization; a missing registry clears it.
        self.paykit_app_registry(counterparty.clone())
            .await
            .ok()
            .map(|registry| {
                registry
                    .map(|registry| {
                        registry
                            .apps()
                            .iter()
                            .map(|(id, app)| (id.clone(), app.capabilities()))
                            .collect()
                    })
                    .unwrap_or_default()
            })
    }

    async fn restore_private_receive_link(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<PrivateReceiveLink> {
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let authorization = self
            .counterparty_noise_key_authorization(&counterparty)
            .await?;
        let remote_noise_public_key = authorization.noise_public_key().clone();
        self.observe_remote_recovery_marker_with_lease(
            &counterparty,
            session_access,
            &lease,
            &remote_noise_public_key,
        )
        .await?;
        let (peer, stored_link_state) = self
            .storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                let peer = tx.linked_peer(&counterparty);
                let state = tx.encrypted_link_state(&counterparty);
                require_private_automation_ready(
                    peer.as_ref().map(|peer| peer.state.clone()),
                    state
                        .as_ref()
                        .is_some_and(|state| state.link_snapshot.is_some()),
                    &counterparty,
                )?;
                Ok((
                    peer.expect("private automation requires a linked peer"),
                    state,
                ))
            })
            .await?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let remote_public_key = counterparty.to_public_key()?;
        let authorized_apps = self.private_receive_authorized_apps(&counterparty).await;

        let stored_link_state =
            stored_link_state.ok_or_else(|| PaykitSdkError::RecoveryRequired {
                context: format!("no Encrypted Link state for counterparty {counterparty}"),
                source: None,
            })?;
        let Some(snapshot_bytes) = stored_link_state.link_snapshot.as_ref() else {
            let now = self.clock.now();
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_key(
                    &counterparty,
                    session_access,
                    &lease,
                    &remote_noise_public_key,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!(
                    "no active Encrypted Link snapshot for counterparty {counterparty}"
                ),
                source: None,
            });
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                let now = self.clock.now();
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_key(
                        &counterparty,
                        session_access,
                        &lease,
                        &remote_noise_public_key,
                    )
                    .await;
                return Err(err.into());
            }
        };
        crate::domain::linked_peers::require_recovery_context(&peer, snapshot.recovery_context())?;
        if snapshot.remote_noise_public_key() != &remote_noise_public_key {
            let now = self.clock.now();
            mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_key(
                    &counterparty,
                    session_access,
                    &lease,
                    &remote_noise_public_key,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired {
                context: format!("counterparty {counterparty} rotated its Paykit identity key"),
                source: None,
            });
        }

        let link = match paykit_lib::restore_encrypted_link(
            session_access.session.clone(),
            secret_key,
            &remote_public_key,
            session_access.outbox_client.clone(),
            snapshot,
        )
        .await
        .map_err(PaykitSdkError::from)
        .and_then(|link| {
            noise_key_authorization::validate_remote_static_key(
                &authorization,
                link.remote_static_public_key(),
                true,
            )?;
            Ok(link)
        }) {
            Ok(link) => link,
            Err(err @ PaykitSdkError::Transport { .. }) => return Err(err),
            Err(err) => {
                let now = self.clock.now();
                mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_key(
                        &counterparty,
                        session_access,
                        &lease,
                        &remote_noise_public_key,
                    )
                    .await;
                return Err(err);
            }
        };
        Ok(PrivateReceiveLink {
            link,
            state: stored_link_state,
            authorized_apps,
            authorization,
        })
    }

    async fn receive_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: &GuardedSessionAccess,
    ) -> Result<PrivateStreamIntakeReport> {
        let link = self
            .with_guarded_storage_operation(
                Arc::clone(&session_access._guard),
                Box::pin(self.restore_private_receive_link(
                    counterparty.clone(),
                    lease.clone(),
                    session_access,
                )),
            )
            .await?;
        Ok(self
            .receive_private_messages_with_link(counterparty, Some(lease), session_access, link)
            .await?
            .expect("leased receive handles recovery directly"))
    }

    async fn receive_private_messages_with_link(
        &self,
        counterparty: PubkyPublicKey,
        lease: Option<PeerLinkOperationLease>,
        session_access: &GuardedSessionAccess,
        received_link: PrivateReceiveLink,
    ) -> Result<Option<PrivateStreamIntakeReport>> {
        let PrivateReceiveLink {
            mut link,
            state: mut stored_link_state,
            mut authorized_apps,
            authorization,
        } = received_link;
        let local_noise_public_key = PubkyPublicKey::from_public_key(
            &pubky::Keypair::from_secret(&session_access.paykit_noise_secret_key()?).public_key(),
        );
        let mut aggregate: Option<PrivateStreamIntakeReport> = None;
        for _ in 0..paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
            let prepared = match link.prepare_next_private_application_message().await {
                Ok(Some(prepared)) => prepared,
                Ok(None) => break,
                Err(err)
                    if err.is_non_retryable_private_receive_error()
                        || matches!(err, paykit_lib::PaykitError::InvalidData { .. }) =>
                {
                    let Some(lease) = lease.as_ref() else {
                        // The speculative read cannot authorize recovery of possibly newer state.
                        return Ok(None);
                    };
                    let now = self.clock.now();
                    mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        now,
                    )
                    .await?;
                    let _ = self
                        .publish_local_recovery_marker_with_key(
                            &counterparty,
                            session_access,
                            lease,
                            authorization.noise_public_key(),
                        )
                        .await;
                    return Err(err.into());
                }
                Err(err) => return Err(err.into()),
            };
            let now = self.clock.now();
            let next_link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: Some(prepared.resulting_snapshot().serialize()),
                handshake_snapshot: None,
                handshake_role: None,
                generation: stored_link_state.generation.saturating_add(1),
                checkpointed_at: now,
            };
            let write = PrivateStreamBatchWrite {
                counterparty: counterparty.clone(),
                confirmation_app_id: self.config.app_id.clone(),
                messages: vec![prepared.message().clone()],
                link_state: Some(next_link_state.clone()),
                authorized_receipt_apps: None,
                link_lease: lease.clone(),
                receive_batch_id: aggregate
                    .as_ref()
                    .and_then(|report| report.receive_batch_id),
                received_at: now,
            };
            let report = self
                .retry_storage_transaction(|| {
                    let mut write = write.clone();
                    let authorized_apps = &authorized_apps;
                    let stored_link_state = &stored_link_state;
                    let authorization = &authorization;
                    let local_noise_public_key = &local_noise_public_key;
                    move |tx| {
                        if write.link_lease.is_none() {
                            if let Err(error) = require_private_receive_checkpoint(
                                tx,
                                stored_link_state,
                                authorization,
                                local_noise_public_key,
                            ) {
                                // A stale preparation needs a new receive, not a storage retry.
                                return Ok(Err(error));
                            }
                        }
                        write.authorized_receipt_apps = cache_private_receive_authorization(
                            tx,
                            &write.counterparty,
                            authorized_apps.as_ref(),
                        );
                        persist_private_stream_batch_in_transaction(tx, write).map(Ok)
                    }
                })
                .await??;
            authorized_apps = None;
            link.acknowledge_persisted_private_receive(prepared)?;
            stored_link_state = next_link_state;
            match aggregate.as_mut() {
                Some(aggregate) => {
                    aggregate.stream_item_ids.extend(report.stream_item_ids);
                    aggregate.event_conflicts.extend(report.event_conflicts);
                }
                None => aggregate = Some(report),
            }
        }

        match aggregate {
            Some(report) => Ok(Some(report)),
            None if lease.is_none() => Ok(Some(PrivateStreamIntakeReport {
                receive_batch_id: None,
                stream_item_ids: Vec::new(),
                event_conflicts: Vec::new(),
            })),
            None => self
                .retry_storage_transaction(|| {
                    let counterparty = counterparty.clone();
                    let lease = lease.clone();
                    let authorized_apps = &authorized_apps;
                    move |tx| {
                        let authorized_receipt_apps = cache_private_receive_authorization(
                            tx,
                            &counterparty,
                            authorized_apps.as_ref(),
                        );
                        persist_private_stream_batch_in_transaction(
                            tx,
                            PrivateStreamBatchWrite {
                                counterparty,
                                confirmation_app_id: self.config.app_id.clone(),
                                messages: Vec::new(),
                                link_state: None,
                                authorized_receipt_apps,
                                link_lease: lease,
                                receive_batch_id: None,
                                received_at: self.clock.now(),
                            },
                        )
                    }
                })
                .await
                .map(Some),
        }
    }
}

fn cache_private_receive_authorization(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    fetched_apps: Option<&HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>>,
) -> Option<Vec<paykit_lib::PaykitAppId>> {
    if let Some(apps) = fetched_apps {
        tx.save_authorized_paykit_apps(counterparty.clone(), apps.clone());
    }
    tx.authorized_paykit_apps(counterparty).map(|apps| {
        apps.into_iter()
            .filter(|(_, capabilities)| capabilities.receipts)
            .map(|(id, _)| id)
            .collect()
    })
}

fn private_receive_snapshot(
    tx: &dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
) -> Option<PrivateReceiveSnapshot> {
    let peer = tx.linked_peer(counterparty)?;
    if peer.state != LinkedPeerState::Linked {
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
    let authorization = peer.noise_key_authorization?;
    if snapshot.remote_noise_public_key() != authorization.noise_public_key() {
        return None;
    }
    Some(PrivateReceiveSnapshot {
        link: snapshot,
        state,
        authorization,
        has_peer_lease: tx.peer_link_operation_lease(counterparty).is_some(),
    })
}

fn require_private_receive_checkpoint(
    tx: &dyn StorageTransaction,
    state: &EncryptedLinkStateRecord,
    authorization: &paykit_lib::PaykitNoiseKeyAuthorization,
    local_noise_public_key: &PubkyPublicKey,
) -> Result<()> {
    if tx.paykit_noise_public_key().as_ref() != Some(local_noise_public_key) {
        return Err(PaykitSdkError::Identity {
            context: "Paykit identity key changed during private receive".into(),
            source: None,
        });
    }
    let current = private_receive_snapshot(tx, &state.counterparty);
    if current.is_none_or(|current| {
        current.has_peer_lease || current.state != *state || current.authorization != *authorization
    }) {
        return Err(PaykitSdkError::ConcurrentUpdate {
            context: "Encrypted Link changed during private receive; retry from current state"
                .into(),
            source: None,
        });
    }
    Ok(())
}

fn linked_counterparties(tx: &dyn StorageTransaction) -> Vec<PubkyPublicKey> {
    let mut peers = tx
        .export_storage_state()
        .linked_peers
        .into_values()
        .filter(|peer| peer.state == LinkedPeerState::Linked)
        .map(|peer| peer.counterparty)
        .collect::<Vec<_>>();
    peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    peers
}
