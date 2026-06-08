use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Send queued outbound private messages for one counterparty in order.
    pub async fn process_outbound_private_messages(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateSendReport> {
        let mut report = OutboundPrivateSendReport::default();
        if self.config.private_sharing == PrivateSharingPolicy::Disabled {
            return Err(PaykitSdkError::Policy(
                "private Paykit message sending is disabled".into(),
            ));
        }
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let (session_access, _) = self.private_link_session_access().await?;
        let queued = queued_outbound_private_messages(&self.storage, &counterparty).await?;
        if queued.is_empty() {
            let lease = self.claim_peer_link_operation(&counterparty).await?;
            report.reservation_cleanup_failures.extend(
                self.release_terminal_private_list_reservations(&counterparty, Some(&lease))
                    .await,
            );
            return self.finish_peer_link_operation(lease, Ok(report)).await;
        }

        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .process_outbound_private_messages_with_claim(
                counterparty,
                report,
                lease.clone(),
                session_access,
            )
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// List counterparties with queued private messages ready for retry.
    pub async fn pending_outbound_private_counterparties(&self) -> Result<Vec<PubkyPublicKey>> {
        let now = self.clock.now();
        let lease_timeout = ChronoDuration::from_std(
            self.config.outbound_private_send_lease_timeout,
        )
        .map_err(|err| {
            PaykitSdkError::Policy(format!(
                "invalid outbound private send lease timeout: {err}"
            ))
        })?;
        let retry_backoff = ChronoDuration::from_std(self.config.outbound_private_retry_backoff)
            .map_err(|err| {
                PaykitSdkError::Policy(format!("invalid outbound private retry backoff: {err}"))
            })?;
        let stale_before = now - lease_timeout;
        let failed_retry_after = now - retry_backoff;
        self.storage
            .transaction(move |tx| {
                let mut by_counterparty = HashMap::new();
                for message in tx.export_storage_state().outbound_private_messages {
                    by_counterparty
                        .entry(message.counterparty.clone())
                        .or_insert_with(Vec::new)
                        .push(message);
                }
                let mut counterparties = by_counterparty
                    .into_iter()
                    .filter_map(|(counterparty, messages)| {
                        outbound_private_queue_head_is_claimable(
                            &messages,
                            stale_before,
                            failed_retry_after,
                        )
                        .then_some(counterparty)
                    })
                    .collect::<Vec<_>>();
                counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                Ok(counterparties)
            })
            .await
    }

    /// Process queued outbound private messages for every pending counterparty.
    pub async fn process_pending_private_messages(
        &self,
    ) -> Result<Vec<OutboundPrivateCounterpartySendReport>> {
        let counterparties = self.pending_outbound_private_counterparties().await?;
        let mut reports = Vec::with_capacity(counterparties.len());
        for counterparty in counterparties {
            match self
                .process_outbound_private_messages(counterparty.clone())
                .await
            {
                Ok(report) => reports.push(OutboundPrivateCounterpartySendReport {
                    counterparty,
                    report: Some(report),
                    error: None,
                }),
                Err(err) => reports.push(OutboundPrivateCounterpartySendReport {
                    counterparty,
                    report: None,
                    error: Some(err.to_string()),
                }),
            }
        }
        Ok(reports)
    }

    async fn process_outbound_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        mut report: OutboundPrivateSendReport,
        lease: PeerLinkOperationLease,
        session_access: PubkySessionAccess,
    ) -> Result<OutboundPrivateSendReport> {
        let secret_key = *session_access
            .local_secret_key
            .as_ref()
            .ok_or_else(|| PaykitSdkError::Identity {
                context: "local Pubky secret key is unavailable for Encrypted Links".into(),
                source: None,
            })?
            .as_bytes();
        let remote_public_key = counterparty.to_public_key()?;
        let stored_link_state = self
            .storage
            .transaction(|tx| Ok(tx.encrypted_link_state(&counterparty)))
            .await?
            .ok_or_else(|| {
                PaykitSdkError::RecoveryRequired(format!(
                    "no Encrypted Link state for counterparty {counterparty}"
                ))
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
                    mark.new_episode,
                )
                .await;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
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
                        mark.new_episode,
                    )
                    .await;
                return Err(err.into());
            }
        };
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
                        mark.new_episode,
                    )
                    .await;
                return Err(err.into());
            }
        };
        let mut link_state = stored_link_state;

        loop {
            let now = self.clock.now();
            let lease_timeout = ChronoDuration::from_std(
                self.config.outbound_private_send_lease_timeout,
            )
            .map_err(|err| {
                PaykitSdkError::Policy(format!(
                    "invalid outbound private send lease timeout: {err}"
                ))
            })?;
            let retry_backoff = ChronoDuration::from_std(
                self.config.outbound_private_retry_backoff,
            )
            .map_err(|err| {
                PaykitSdkError::Policy(format!("invalid outbound private retry backoff: {err}"))
            })?;
            let stale_before = now - lease_timeout;
            let failed_retry_after = now - retry_backoff;
            let sending = claim_next_outbound_private_message_with_peer_lease(
                &self.storage,
                &counterparty,
                now,
                stale_before,
                failed_retry_after,
                lease.clone(),
            )
            .await?;
            let Some(sending) = sending else {
                report.reservation_cleanup_failures.extend(
                    self.release_terminal_private_list_reservations(&counterparty, Some(&lease))
                        .await,
                );
                break;
            };
            report.attempted.push(sending.outbound_message_id);

            if let Err(err) = validate_queued_outbound_private_message(&sending) {
                let now = self.clock.now();
                let error = err.to_string();
                let failed = mark_outbound_invalid(sending, error.clone(), now);
                self.storage
                    .transaction({
                        let failed = failed.clone();
                        let lease = lease.clone();
                        move |tx| {
                            crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                            tx.save_outbound_private_message(failed);
                            Ok(())
                        }
                    })
                    .await?;
                report.failed.push(OutboundPrivateSendFailure {
                    outbound_message_id: failed.outbound_message_id,
                    error,
                });
                report.reservation_cleanup_failures.extend(
                    self.release_terminal_private_list_reservations(&counterparty, Some(&lease))
                        .await,
                );
                continue;
            }

            if sending.kind == PrivateMessageKind::PrivatePaymentList.as_str() {
                let expired_releases = expired_outbound_reservation_releases(
                    &self.storage,
                    &counterparty,
                    sending.outbound_message_id,
                    now,
                )
                .await?;
                if !expired_releases.is_empty() {
                    let now = self.clock.now();
                    let error =
                        "Payment Endpoint Reservation expired before private list send".to_owned();
                    let failed = mark_outbound_invalid(sending, error.clone(), now);
                    self.storage
                        .transaction({
                            let failed = failed.clone();
                            let lease = lease.clone();
                            move |tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                                tx.save_outbound_private_message(failed);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(OutboundPrivateSendFailure {
                        outbound_message_id: failed.outbound_message_id,
                        error,
                    });
                    report.reservation_cleanup_failures.extend(
                        self.release_reservation_records(expired_releases, Some(&lease))
                            .await,
                    );
                    report.reservation_cleanup_failures.extend(
                        self.release_terminal_private_list_reservations(
                            &counterparty,
                            Some(&lease),
                        )
                        .await,
                    );
                    continue;
                }
            }

            match link
                .send_private_application_message_json(&sending.raw_json)
                .await
            {
                Ok(()) => {
                    let now = self.clock.now();
                    let sent = mark_outbound_sent(sending, now);
                    link_state.link_snapshot = Some(link.serialize());
                    link_state.handshake_snapshot = None;
                    link_state.handshake_role = None;
                    link_state.generation = link_state.generation.saturating_add(1);
                    link_state.checkpointed_at = now;
                    self.storage
                        .transaction({
                            let sent = sent.clone();
                            let link_state = link_state.clone();
                            let lease = lease.clone();
                            move |tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                                tx.save_outbound_private_message(sent);
                                tx.save_encrypted_link_state(link_state);
                                Ok(())
                            }
                        })
                        .await?;
                    report.sent.push(sent.outbound_message_id);
                }
                Err(err) => {
                    let requires_recovery = err.is_non_retryable_private_send_error();
                    let now = self.clock.now();
                    let error = err.to_string();
                    let failed = mark_outbound_failed(sending, error.clone(), now);
                    self.storage
                        .transaction({
                            let failed = failed.clone();
                            let lease = lease.clone();
                            move |tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                                tx.save_outbound_private_message(failed);
                                Ok(())
                            }
                        })
                        .await?;
                    report.failed.push(OutboundPrivateSendFailure {
                        outbound_message_id: failed.outbound_message_id,
                        error,
                    });
                    if requires_recovery {
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
                                mark.new_episode,
                            )
                            .await;
                    }
                    report.reservation_cleanup_failures.extend(
                        self.release_terminal_private_list_reservations(
                            &counterparty,
                            Some(&lease),
                        )
                        .await,
                    );
                    break;
                }
            }
            report.reservation_cleanup_failures.extend(
                self.release_terminal_private_list_reservations(&counterparty, Some(&lease))
                    .await,
            );
        }

        Ok(report)
    }
}
