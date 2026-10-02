use super::*;
use futures_util::{stream, StreamExt};

const PRIVATE_INBOX_PROBE_CONCURRENCY: usize = 4;

struct PrivateReceiveSnapshot {
    link: paykit_lib::EncryptedLinkSnapshot,
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
    /// Concurrent link changes are observed on a subsequent poll unless a
    /// message is available, in which case state is reloaded under the lease.
    pub async fn receive_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateStreamIntakeReport> {
        let (session_access, snapshots) = self.private_receive_context(Some(&counterparty)).await?;
        let snapshot = snapshots
            .into_iter()
            .next()
            .and_then(|(_, snapshot)| snapshot);
        let empty = self
            .private_inbox_is_empty(&counterparty, &session_access, snapshot.as_ref())
            .await?;
        self.receive_probed_private_messages(counterparty, &session_access, empty)
            .await
    }

    async fn receive_probed_private_messages(
        &self,
        counterparty: PubkyPublicKey,
        session_access: &PubkySessionAccess,
        empty: bool,
    ) -> Result<PrivateStreamIntakeReport> {
        if empty {
            return Ok(PrivateStreamIntakeReport {
                receive_batch_id: None,
                stream_item_ids: Vec::new(),
                event_conflicts: Vec::new(),
            });
        }
        // The probe grants no authority to receive. Reload under a fresh lease.
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
        let mut probes = stream::iter(counterparties)
            .map(|(counterparty, snapshot)| {
                let session_access = &session_access;
                async move {
                    let empty = self
                        .private_inbox_is_empty(&counterparty, session_access, snapshot.as_ref())
                        .await;
                    (counterparty, empty)
                }
            })
            .buffer_unordered(PRIVATE_INBOX_PROBE_CONCURRENCY);
        while let Some((counterparty, empty)) = probes.next().await {
            // Only advisory reads run concurrently. Receive commits stay serialized.
            let result = match empty {
                Ok(empty) => {
                    self.receive_probed_private_messages(
                        counterparty.clone(),
                        &session_access,
                        empty,
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

    async fn private_inbox_is_empty(
        &self,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        snapshot: Option<&PrivateReceiveSnapshot>,
    ) -> Result<bool> {
        let Some(snapshot) = snapshot else {
            return Ok(false);
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
            return Ok(false);
        }
        let snapshot = &snapshot.link;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let marker = paykit_lib::fetch_encrypted_link_recovery_marker(
            &public_storage,
            &secret_key,
            session_access.session.info().public_key(),
            &remote_public_key,
            snapshot.remote_noise_public_key(),
        )
        .await?;
        if marker.is_some_and(|marker| {
            Some(marker.attempt_id()) != snapshot.recovery_context().remote_attempt_id()
        }) {
            return Ok(false);
        }
        match snapshot
            .has_pending_private_application_message(
                &public_storage,
                session_access.session.info().public_key(),
                &secret_key,
            )
            .await
        {
            Ok(pending) => Ok(!pending),
            Err(paykit_lib::PaykitError::Validation(_)) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    async fn receive_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<PrivateStreamIntakeReport> {
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
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let remote_public_key = counterparty.to_public_key()?;
        let authorized_receipt_apps =
            match self.authorized_receipt_apps_for_peer(&counterparty).await {
                Ok(app_ids) => app_ids,
                Err(_) => {
                    self.storage
                        .transaction(|tx| {
                            Ok(tx.authorized_paykit_apps(&counterparty).map(|apps| {
                                apps.into_iter()
                                    .filter(|(_, capabilities)| capabilities.receipts)
                                    .map(|(app_id, _)| app_id)
                                    .collect()
                            }))
                        })
                        .await?
                }
            };

        let mut stored_link_state = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
            .ok_or_else(|| PaykitSdkError::RecoveryRequired {
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
        self.require_snapshot_recovery_context(&counterparty, snapshot.recovery_context(), &lease)
            .await?;
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

        let mut link = match paykit_lib::restore_encrypted_link(
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
        let mut aggregate: Option<PrivateStreamIntakeReport> = None;
        for _ in 0..paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
            let prepared = match link.prepare_next_private_application_message().await {
                Ok(Some(prepared)) => prepared,
                Ok(None) => break,
                Err(err)
                    if err.is_non_retryable_private_receive_error()
                        || matches!(err, paykit_lib::PaykitError::InvalidData { .. }) =>
                {
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
            let report = persist_private_stream_batch_write(
                &self.storage,
                PrivateStreamBatchWrite {
                    counterparty: counterparty.clone(),
                    confirmation_app_id: self.config.app_id.clone(),
                    messages: vec![prepared.message().clone()],
                    link_state: Some(next_link_state.clone()),
                    authorized_receipt_apps: authorized_receipt_apps.clone(),
                    link_lease: Some(lease.clone()),
                    receive_batch_id: aggregate
                        .as_ref()
                        .and_then(|report| report.receive_batch_id),
                    received_at: now,
                },
            )
            .await?;
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
            Some(report) => Ok(report),
            None => {
                persist_private_stream_batch_write(
                    &self.storage,
                    PrivateStreamBatchWrite {
                        counterparty,
                        confirmation_app_id: self.config.app_id.clone(),
                        messages: Vec::new(),
                        link_state: None,
                        authorized_receipt_apps,
                        link_lease: Some(lease),
                        receive_batch_id: None,
                        received_at: self.clock.now(),
                    },
                )
                .await
            }
        }
    }
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
        authorization,
    })
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
