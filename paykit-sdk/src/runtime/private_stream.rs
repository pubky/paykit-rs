use super::*;

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
    pub async fn receive_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivateStreamIntakeReport> {
        let (session_access, _) = self.private_link_session_access().await?;
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
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
        let counterparties = self
            .linked_peers()
            .await?
            .into_iter()
            .filter(|record| record.state == LinkedPeerState::Linked)
            .map(|record| record.counterparty)
            .collect::<Vec<_>>();
        let mut reports = Vec::with_capacity(counterparties.len());
        for counterparty in counterparties {
            match self.receive_private_messages(counterparty.clone()).await {
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
        Ok(reports)
    }

    async fn receive_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: PeerLinkOperationLease,
        session_access: GuardedSessionAccess,
    ) -> Result<PrivateStreamIntakeReport> {
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
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_session(
                    &counterparty,
                    &session_access,
                    &lease,
                    mark.new_episode,
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
                let mark = mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_session(
                        &counterparty,
                        &session_access,
                        &lease,
                        mark.new_episode,
                    )
                    .await;
                return Err(err.into());
            }
        };
        if !self
            .snapshot_uses_current_counterparty_noise_key(
                &counterparty,
                snapshot.remote_noise_public_key(),
            )
            .await?
        {
            let now = self.clock.now();
            let mark = mark_recovery_required_with_lease(
                &self.storage,
                counterparty.clone(),
                lease.clone(),
                now,
            )
            .await?;
            let _ = self
                .publish_local_recovery_marker_with_session(
                    &counterparty,
                    &session_access,
                    &lease,
                    mark.new_episode,
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
        {
            Ok(link) => link,
            Err(err) => {
                let now = self.clock.now();
                let mark = mark_recovery_required_with_lease(
                    &self.storage,
                    counterparty.clone(),
                    lease.clone(),
                    now,
                )
                .await?;
                let _ = self
                    .publish_local_recovery_marker_with_session(
                        &counterparty,
                        &session_access,
                        &lease,
                        mark.new_episode,
                    )
                    .await;
                return Err(err.into());
            }
        };
        let mut aggregate: Option<PrivateStreamIntakeReport> = None;
        for _ in 0..paykit_lib::PRIVATE_APPLICATION_MESSAGE_RECEIVE_LIMIT {
            let prepared = match link.prepare_next_private_application_message().await {
                Ok(Some(prepared)) => prepared,
                Ok(None) => break,
                Err(err) if err.is_non_retryable_private_receive_error() => {
                    let now = self.clock.now();
                    let mark = mark_recovery_required_with_lease(
                        &self.storage,
                        counterparty.clone(),
                        lease.clone(),
                        now,
                    )
                    .await?;
                    let _ = self
                        .publish_local_recovery_marker_with_session(
                            &counterparty,
                            &session_access,
                            &lease,
                            mark.new_episode,
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
                    messages: vec![prepared.message().clone()],
                    link_state: Some(next_link_state.clone()),
                    authorized_receipt_apps: authorized_receipt_apps.clone(),
                    link_lease: Some(lease.clone()),
                    receive_batch_id: aggregate.as_ref().map(|report| report.receive_batch_id),
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
                persist_private_stream_batch_with_link_lease(
                    &self.storage,
                    counterparty,
                    Vec::new(),
                    None,
                    authorized_receipt_apps,
                    Some(lease),
                    self.clock.now(),
                )
                .await
            }
        }
    }
}
