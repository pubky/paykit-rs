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
        self.ensure_peer_not_recovery_required_or_blocked(&counterparty)
            .await?;
        let (session_access, _) = self.private_link_session_access().await?;
        self.ensure_peer_allows_private_automation(&counterparty)
            .await?;
        let queued = queued_outbound_private_messages(&self.storage, &counterparty).await?;
        if queued.is_empty() {
            let lease = self.claim_peer_link_operation(&counterparty).await?;
            report.reservation_cleanup_failures.extend(
                self.cancel_terminal_private_list_reservations(&counterparty, Some(&lease))
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
        let (stale_before, failed_retry_after) = self.outbound_retry_thresholds(now)?;
        self.storage
            .transaction(move |tx| {
                let snapshot = tx.export_storage_state();
                let mut by_counterparty = HashMap::new();
                let mut by_outbound_id = HashMap::new();
                for message in snapshot.outbound_private_messages {
                    by_outbound_id.insert(message.outbound_message_id, message.clone());
                    by_counterparty
                        .entry(message.counterparty.clone())
                        .or_insert_with(Vec::new)
                        .push(message);
                }

                let mut candidates = HashSet::new();
                for (counterparty, messages) in by_counterparty {
                    if outbound_private_queue_head_is_claimable(
                        &messages,
                        stale_before,
                        failed_retry_after,
                    ) {
                        candidates.insert(counterparty);
                    }
                }
                for reservation in snapshot.payment_endpoint_reservations.values() {
                    if terminal_private_list_reservation_needs_cleanup(reservation, &by_outbound_id)
                    {
                        candidates.insert(reservation.counterparty.clone());
                    }
                }

                let mut counterparties = candidates
                    .into_iter()
                    .filter(|counterparty| {
                        if snapshot.linked_peers.get(counterparty).is_some_and(|peer| {
                            matches!(
                                peer.state,
                                LinkedPeerState::Linking
                                    | LinkedPeerState::RecoveryRequired
                                    | LinkedPeerState::Blocked
                            )
                        }) {
                            return false;
                        }
                        true
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
        let (mut link, mut link_state) = self
            .restore_link_for_outbound_send(&counterparty, &lease, &session_access)
            .await?;

        loop {
            let now = self.clock.now();
            let (stale_before, failed_retry_after) = self.outbound_retry_thresholds(now)?;
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
                    self.cancel_terminal_private_list_reservations(&counterparty, Some(&lease))
                        .await,
                );
                break;
            };
            report.attempted.push(sending.outbound_message_id);

            let Some(sending) = self
                .claimed_message_ready_for_send(&counterparty, sending, &lease, &mut report, now)
                .await?
            else {
                continue;
            };

            match link
                .send_private_application_message_json(&sending.raw_json)
                .await
            {
                Ok(()) => {
                    self.record_private_send_success(
                        sending,
                        &link,
                        &mut link_state,
                        &lease,
                        &mut report,
                    )
                    .await?;
                }
                Err(err) => {
                    self.record_private_send_error(
                        &counterparty,
                        sending,
                        err,
                        &lease,
                        &session_access,
                        &mut report,
                    )
                    .await?;
                    break;
                }
            }
            report.reservation_cleanup_failures.extend(
                self.cancel_terminal_private_list_reservations(&counterparty, Some(&lease))
                    .await,
            );
        }

        Ok(report)
    }

    async fn restore_link_for_outbound_send(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<(paykit_lib::EncryptedLink, EncryptedLinkStateRecord)> {
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
            .transaction(|tx| Ok(tx.encrypted_link_state(counterparty)))
            .await?
            .ok_or_else(|| {
                PaykitSdkError::RecoveryRequired(format!(
                    "no Encrypted Link state for counterparty {counterparty}"
                ))
            })?;
        let Some(snapshot_bytes) = stored_link_state.link_snapshot.as_ref() else {
            self.mark_outbound_link_recovery_required(counterparty, lease, session_access)
                .await?;
            return Err(PaykitSdkError::RecoveryRequired(format!(
                "no active Encrypted Link snapshot for counterparty {counterparty}"
            )));
        };
        let snapshot = match paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                self.mark_outbound_link_recovery_required(counterparty, lease, session_access)
                    .await?;
                return Err(err.into());
            }
        };
        let link = match paykit_lib::restore_encrypted_link(
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
                self.mark_outbound_link_recovery_required(counterparty, lease, session_access)
                    .await?;
                return Err(err.into());
            }
        };
        Ok((link, stored_link_state))
    }

    async fn mark_outbound_link_recovery_required(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<()> {
        let mark = mark_recovery_required_with_lease(
            &self.storage,
            counterparty.clone(),
            lease.clone(),
            self.clock.now(),
        )
        .await?;
        let _ = self
            .publish_local_recovery_marker_with_session(
                counterparty,
                session_access,
                mark.new_episode,
            )
            .await;
        Ok(())
    }

    fn outbound_retry_thresholds(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
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
        Ok((now - lease_timeout, now - retry_backoff))
    }

    async fn claimed_message_ready_for_send(
        &self,
        counterparty: &PubkyPublicKey,
        sending: OutboundPrivateMessageRecord,
        lease: &PeerLinkOperationLease,
        report: &mut OutboundPrivateSendReport,
        now: DateTime<Utc>,
    ) -> Result<Option<OutboundPrivateMessageRecord>> {
        if let Err(err) = validate_queued_outbound_private_message(&sending) {
            let error = err.to_string();
            let failed = mark_outbound_invalid(sending, error.clone(), self.clock.now());
            let failed = self.save_outbound_with_lease(failed, lease).await?;
            report.failed.push(OutboundPrivateSendFailure {
                outbound_message_id: failed.outbound_message_id,
                error,
            });
            report.reservation_cleanup_failures.extend(
                self.cancel_terminal_private_list_reservations(counterparty, Some(lease))
                    .await,
            );
            return Ok(None);
        }

        if sending.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            return Ok(Some(sending));
        }

        let expired_releases = expired_outbound_reservation_cancellations(
            &self.storage,
            counterparty,
            sending.outbound_message_id,
            now,
        )
        .await?;
        if expired_releases.is_empty() {
            return Ok(Some(sending));
        }

        let error = "Payment Endpoint Reservation expired before private list send".to_owned();
        let failed = mark_outbound_invalid(sending, error.clone(), self.clock.now());
        let failed = self.save_outbound_with_lease(failed, lease).await?;
        report.failed.push(OutboundPrivateSendFailure {
            outbound_message_id: failed.outbound_message_id,
            error,
        });
        report.reservation_cleanup_failures.extend(
            self.cancel_reservation_records(expired_releases, Some(lease))
                .await,
        );
        report.reservation_cleanup_failures.extend(
            self.cancel_terminal_private_list_reservations(counterparty, Some(lease))
                .await,
        );
        Ok(None)
    }

    async fn record_private_send_success(
        &self,
        sending: OutboundPrivateMessageRecord,
        link: &paykit_lib::EncryptedLink,
        link_state: &mut EncryptedLinkStateRecord,
        lease: &PeerLinkOperationLease,
        report: &mut OutboundPrivateSendReport,
    ) -> Result<()> {
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
                    tx.save_outbound_private_message(sent.clone())?;
                    tx.save_encrypted_link_state(link_state);
                    Ok(())
                }
            })
            .await?;
        report.sent.push(sent.outbound_message_id);
        Ok(())
    }

    async fn record_private_send_error(
        &self,
        counterparty: &PubkyPublicKey,
        sending: OutboundPrivateMessageRecord,
        err: paykit_lib::PaykitError,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
        report: &mut OutboundPrivateSendReport,
    ) -> Result<()> {
        let requires_recovery = err.is_non_retryable_private_send_error();
        let now = self.clock.now();
        let error = err.to_string();
        if requires_recovery {
            let failed = mark_outbound_recovery_required(sending, error.clone(), now);
            let (failed, mark) = self
                .storage
                .transaction({
                    let failed = failed.clone();
                    let lease = lease.clone();
                    let counterparty = counterparty.clone();
                    move |tx| {
                        crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                        let mark = mark_recovery_required_in_transaction(tx, &counterparty, now)?;
                        tx.save_outbound_private_message(failed.clone())?;
                        Ok((failed, mark))
                    }
                })
                .await?;
            report.failed.push(OutboundPrivateSendFailure {
                outbound_message_id: failed.outbound_message_id,
                error,
            });
            self.record_outbound_recovery_marker_result(
                report,
                counterparty,
                session_access,
                mark.new_episode,
                Some(failed.outbound_message_id),
            )
            .await;
            return Ok(());
        }

        let failed = mark_outbound_failed(sending, error.clone(), now);
        let failed = self.save_outbound_with_lease(failed, lease).await?;
        report.failed.push(OutboundPrivateSendFailure {
            outbound_message_id: failed.outbound_message_id,
            error,
        });
        report.reservation_cleanup_failures.extend(
            self.cancel_terminal_private_list_reservations(counterparty, Some(lease))
                .await,
        );
        Ok(())
    }

    async fn save_outbound_with_lease(
        &self,
        record: OutboundPrivateMessageRecord,
        lease: &PeerLinkOperationLease,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.storage
            .transaction({
                let record = record.clone();
                let lease = lease.clone();
                move |tx| {
                    crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                    tx.save_outbound_private_message(record.clone())?;
                    Ok(record)
                }
            })
            .await
    }

    async fn record_outbound_recovery_marker_result(
        &self,
        report: &mut OutboundPrivateSendReport,
        counterparty: &PubkyPublicKey,
        session_access: &PubkySessionAccess,
        new_episode: bool,
        outbound_message_id: Option<u64>,
    ) {
        if let Err(err) = self
            .publish_local_recovery_marker_with_session(counterparty, session_access, new_episode)
            .await
        {
            report
                .recovery_marker_failures
                .push(RecoveryMarkerPublishFailure {
                    outbound_message_id,
                    error: err.to_string(),
                });
        }
    }
}

fn terminal_private_list_reservation_needs_cleanup(
    reservation: &crate::storage::PaymentEndpointReservationRecord,
    outbound_by_id: &HashMap<u64, OutboundPrivateMessageRecord>,
) -> bool {
    let Some(message) = outbound_by_id.get(&reservation.outbound_message_id) else {
        return false;
    };
    if message.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
        return false;
    }
    matches!(message.status, OutboundPrivateMessageStatus::Invalid)
        || (message.status == OutboundPrivateMessageStatus::Superseded
            && message.last_attempt_at.is_none())
}
