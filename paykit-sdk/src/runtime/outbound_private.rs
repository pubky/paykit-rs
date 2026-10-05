use super::*;
use crate::domain::endpoint_reservations::terminal_private_list_reservation_cancellations_in_transaction;

enum PrivateSendPreparation {
    Idle,
    Skipped(Option<u64>),
    Ready(
        Box<OutboundPrivateMessageRecord>,
        Option<Box<paykit_lib::PreparedPrivateApplicationMessageSend>>,
    ),
    Failed(Box<OutboundPrivateMessageRecord>, paykit_lib::PaykitError),
}

enum PrivateSendStep {
    Idle,
    Skipped(Option<u64>),
    Sent { has_more: bool },
    Failed(Box<OutboundPrivateMessageRecord>, paykit_lib::PaykitError),
}

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
        let lease_timeout = ChronoDuration::from_std(PEER_LINK_OPERATION_LEASE_TIMEOUT)
            .expect("fixed peer link lease timeout must fit chrono duration");
        let work = self
            .retry_storage_transaction(|| {
                let counterparty = counterparty.clone();
                move |tx| {
                    let has_queued = !tx
                        .queued_outbound_private_messages(&counterparty)
                        .is_empty();
                    if !has_queued
                        && !tx
                            .payment_endpoint_reservations(&counterparty)
                            .iter()
                            .any(|reservation| reservation.app_id == self.config.app_id)
                    {
                        return Ok(None);
                    }
                    let cancellations =
                        terminal_private_list_reservation_cancellations_in_transaction(
                            tx,
                            &counterparty,
                        )
                        .into_iter()
                        .filter(|record| record.app_id == self.config.app_id)
                        .collect::<Vec<_>>();
                    if !has_queued && cancellations.is_empty() {
                        return Ok(None);
                    }
                    let now = self.clock.now();
                    let lease =
                        tx.claim_peer_link_operation(&counterparty, now, now + lease_timeout)?;
                    let readiness =
                        super::encrypted_links::require_peer_not_recovery_required_or_blocked(
                            tx.linked_peer(&counterparty).map(|peer| peer.state),
                            &counterparty,
                        );
                    Ok(Some((lease, has_queued, cancellations, readiness)))
                }
            })
            .await?;
        let Some((lease, has_queued, cancellations, readiness)) = work else {
            return Ok(OutboundPrivateSendReport::default());
        };
        let lease = lease.ok_or_else(|| PaykitSdkError::ConcurrentUpdate {
            context: format!(
                "peer link operation already in progress for counterparty {counterparty}"
            ),
            source: None,
        })?;

        let result = async {
            let mut report = OutboundPrivateSendReport::default();
            report.reservation_cleanup_failures.extend(
                self.cancel_reservation_records(cancellations, Some(&lease), None)
                    .await,
            );
            if !has_queued {
                return Ok(report);
            }

            readiness?;
            self.process_outbound_private_messages_with_claim(counterparty, report, lease.clone())
                .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// List counterparties with queued private messages ready for retry.
    pub async fn pending_outbound_private_counterparties(&self) -> Result<Vec<PubkyPublicKey>> {
        let now = self.clock.now();
        let (stale_before, failed_retry_after) = self.outbound_retry_thresholds(now)?;
        let app_id = self.config.app_id.clone();
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

                let mut message_candidates = HashSet::new();
                for (counterparty, messages) in by_counterparty {
                    if outbound_private_queue_head_is_claimable(
                        &messages,
                        &snapshot.registered_paykit_apps,
                        &snapshot.retired_paykit_apps,
                        stale_before,
                        failed_retry_after,
                    ) {
                        message_candidates.insert(counterparty);
                    }
                }
                let mut cleanup_candidates = HashSet::new();
                for reservation in snapshot.payment_endpoint_reservations.values() {
                    if reservation.app_id == app_id
                        && terminal_private_list_reservation_needs_cleanup(
                            reservation,
                            &by_outbound_id,
                        )
                    {
                        cleanup_candidates.insert(reservation.counterparty.clone());
                    }
                }

                message_candidates.retain(|counterparty| {
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
                });
                message_candidates.extend(cleanup_candidates);
                let mut counterparties = message_candidates.into_iter().collect::<Vec<_>>();
                counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                Ok(counterparties)
            })
            .await
    }

    /// Process queued outbound private messages for every pending counterparty.
    /// Peers are processed sequentially so queued work does not consume their leases.
    pub async fn process_pending_private_messages(
        &self,
    ) -> Result<Vec<OutboundPrivateCounterpartySendReport>> {
        let counterparties = self.pending_outbound_private_counterparties().await?;
        let mut reports = Vec::with_capacity(counterparties.len());
        for counterparty in counterparties {
            let result = self
                .process_outbound_private_messages(counterparty.clone())
                .await;
            reports.push(match result {
                Ok(report) => OutboundPrivateCounterpartySendReport {
                    counterparty,
                    report: Some(report),
                    error: None,
                },
                Err(err) => OutboundPrivateCounterpartySendReport {
                    counterparty,
                    report: None,
                    error: Some(err.to_string()),
                },
            });
        }
        Ok(reports)
    }

    async fn process_outbound_private_messages_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        mut report: OutboundPrivateSendReport,
        lease: PeerLinkOperationLease,
    ) -> Result<OutboundPrivateSendReport> {
        let (session_access, mut link, mut link_state) = self
            .with_storage_operation(Box::pin(async {
                let (session_access, _) = self.private_link_session_access().await?;
                let (link, state) = self
                    .restore_link_for_outbound_send(&counterparty, &lease, &session_access)
                    .await?;
                Ok((session_access, link, state))
            }))
            .await?;
        // Freeze retry eligibility so each event is attempted at most once per run.
        let started_at = self.clock.now();
        let (stale_before, failed_retry_after) = self.outbound_retry_thresholds(started_at)?;

        loop {
            let (step, cancellations) = self
                .with_guarded_storage_operation(
                    Arc::clone(&session_access._guard),
                    Box::pin(async {
                        let now = self.clock.now().max(started_at);
                        let (preparation, mut cancellations) = self
                            .storage
                            .transaction(|tx| {
                                crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                                let sending = tx.claim_next_outbound_private_message(
                                    &counterparty,
                                    now,
                                    stale_before,
                                    failed_retry_after,
                                );
                                let preparation = match sending {
                                    Some(sending) => self.prepare_claimed_private_message(
                                        tx,
                                        &lease,
                                        &mut link,
                                        &mut link_state,
                                        &mut report,
                                        sending,
                                    )?,
                                    None => PrivateSendPreparation::Idle,
                                };
                                let cancellations =
                                    terminal_private_list_reservation_cancellations_in_transaction(
                                        tx,
                                        &counterparty,
                                    );
                                Ok((preparation, cancellations))
                            })
                            .await?;
                        let step = match preparation {
                            PrivateSendPreparation::Ready(sending, prepared) => {
                                // A failed or uncertain commit must never advance or publish Noise.
                                if let Some(prepared) = prepared {
                                    link.acknowledge_persisted_private_send(*prepared)?;
                                }
                                self.send_prepared_private_message(
                                    &lease,
                                    &link,
                                    &mut report,
                                    *sending,
                                    &session_access,
                                    (stale_before, failed_retry_after),
                                )
                                .await?
                            }
                            PrivateSendPreparation::Idle => PrivateSendStep::Idle,
                            PrivateSendPreparation::Skipped(message_id) => {
                                PrivateSendStep::Skipped(message_id)
                            }
                            PrivateSendPreparation::Failed(sending, error) => {
                                PrivateSendStep::Failed(sending, error)
                            }
                        };
                        if matches!(step, PrivateSendStep::Skipped(_)) {
                            cancellations = self.storage.transaction(|tx| {
                                Ok(terminal_private_list_reservation_cancellations_in_transaction(tx, &counterparty))
                            }).await?;
                        }
                        Ok((step, cancellations))
                    }),
                )
                .await?;
            // Wallet callbacks run without the identity-wide lock.
            report.reservation_cleanup_failures.extend(
                self.cancel_reservation_records(cancellations, Some(&lease), None)
                    .await,
            );
            match step {
                PrivateSendStep::Idle => break,
                PrivateSendStep::Skipped(Some(message_id)) => {
                    self.record_outbound_recovery_marker_result(
                        &mut report,
                        &counterparty,
                        &session_access,
                        &lease,
                        Some(message_id),
                    )
                    .await;
                    break;
                }
                PrivateSendStep::Skipped(None) => continue,
                PrivateSendStep::Failed(sending, error) => {
                    self.record_private_send_error(
                        &counterparty,
                        *sending,
                        error,
                        &lease,
                        &session_access,
                        &mut report,
                    )
                    .await?;
                    break;
                }
                PrivateSendStep::Sent { has_more: false } => break,
                PrivateSendStep::Sent { has_more: true } => {}
            }
        }

        Ok(report)
    }

    fn prepare_claimed_private_message(
        &self,
        tx: &mut dyn StorageTransaction,
        lease: &PeerLinkOperationLease,
        link: &mut paykit_lib::EncryptedLink,
        link_state: &mut EncryptedLinkStateRecord,
        report: &mut OutboundPrivateSendReport,
        sending: OutboundPrivateMessageRecord,
    ) -> Result<PrivateSendPreparation> {
        report.attempted.push(sending.outbound_message_id);

        let prepared_message_id = sending
            .prepared_send
            .as_ref()
            .map(|_| sending.outbound_message_id);
        let Some(sending) =
            self.claimed_message_ready_for_send(tx, sending, lease, report, self.clock.now())?
        else {
            return Ok(PrivateSendPreparation::Skipped(prepared_message_id));
        };

        if sending.prepared_send.is_some() {
            return Ok(PrivateSendPreparation::Ready(Box::new(sending), None));
        }
        let prepared = match link.prepare_private_application_message_json(&sending.raw_json) {
            Ok(prepared) => prepared,
            Err(err) => return Ok(PrivateSendPreparation::Failed(Box::new(sending), err)),
        };
        let mut sending = sending;
        sending.prepared_send = Some(crate::storage::PreparedOutboundPrivateSend {
            destination_path: prepared.destination_path().to_owned(),
            ciphertext: prepared.ciphertext().to_vec(),
        });
        link_state.link_snapshot = Some(prepared.resulting_snapshot().serialize());
        link_state.handshake_snapshot = None;
        link_state.handshake_role = None;
        link_state.generation = link_state.generation.saturating_add(1);
        link_state.checkpointed_at = self.clock.now();
        tx.save_outbound_private_message(sending.clone())?;
        tx.save_encrypted_link_state(link_state.clone());
        Ok(PrivateSendPreparation::Ready(
            Box::new(sending),
            Some(Box::new(prepared)),
        ))
    }

    async fn send_prepared_private_message(
        &self,
        lease: &PeerLinkOperationLease,
        link: &paykit_lib::EncryptedLink,
        report: &mut OutboundPrivateSendReport,
        sending: OutboundPrivateMessageRecord,
        session_access: &PubkySessionAccess,
        retry_thresholds: (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<PrivateSendStep> {
        let message_id = sending.outbound_message_id;
        let sending = self
            .storage
            .transaction(|tx| {
                self.require_current_peer_link_operation_in_transaction(tx, lease, session_access)?;
                // The reservation can expire while its prepared checkpoint is being saved.
                self.claimed_message_ready_for_send(tx, sending, lease, report, self.clock.now())
            })
            .await?;
        let Some(sending) = sending else {
            return Ok(PrivateSendStep::Skipped(Some(message_id)));
        };
        let prepared = sending
            .prepared_send
            .as_ref()
            .expect("prepared send must be durable before publication");
        match link
            .publish_prepared_private_application_message(
                &prepared.destination_path,
                &prepared.ciphertext,
            )
            .await
        {
            Ok(()) => {
                let has_more = self
                    .record_private_send_success(sending, lease, report, retry_thresholds)
                    .await?;
                Ok(PrivateSendStep::Sent { has_more })
            }
            Err(error) => Ok(PrivateSendStep::Failed(Box::new(sending), error)),
        }
    }

    async fn restore_link_for_outbound_send(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
    ) -> Result<(paykit_lib::EncryptedLink, EncryptedLinkStateRecord)> {
        let authorization = self
            .counterparty_noise_key_authorization(counterparty)
            .await?;
        let remote_noise_public_key = authorization.noise_public_key().clone();
        self.observe_remote_recovery_marker_with_lease(
            counterparty,
            session_access,
            lease,
            &remote_noise_public_key,
        )
        .await?;
        let (peer, stored_link_state) = self
            .storage
            .transaction(|tx| {
                crate::storage::require_peer_link_operation_lease(tx, lease)?;
                Ok((
                    tx.linked_peer(counterparty),
                    tx.encrypted_link_state(counterparty),
                ))
            })
            .await?;
        require_private_automation_ready(
            peer.as_ref().map(|peer| peer.state.clone()),
            stored_link_state
                .as_ref()
                .is_some_and(|state| state.link_snapshot.is_some()),
            counterparty,
        )?;
        let secret_key = session_access.paykit_noise_secret_key()?;
        let remote_public_key = counterparty.to_public_key()?;
        let stored_link_state =
            stored_link_state.ok_or_else(|| PaykitSdkError::RecoveryRequired {
                context: format!("no Encrypted Link state for counterparty {counterparty}"),
                source: None,
            })?;
        let Some(snapshot_bytes) = stored_link_state.link_snapshot.as_ref() else {
            self.mark_outbound_link_recovery_required(
                counterparty,
                lease,
                session_access,
                &remote_noise_public_key,
            )
            .await?;
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
                self.mark_outbound_link_recovery_required(
                    counterparty,
                    lease,
                    session_access,
                    &remote_noise_public_key,
                )
                .await?;
                return Err(err.into());
            }
        };
        crate::domain::linked_peers::require_recovery_context(
            &peer.expect("private automation requires a linked peer"),
            snapshot.recovery_context(),
        )?;
        if snapshot.remote_noise_public_key() != &remote_noise_public_key {
            self.mark_outbound_link_recovery_required(
                counterparty,
                lease,
                session_access,
                &remote_noise_public_key,
            )
            .await?;
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
            Err(err) => {
                self.mark_outbound_link_recovery_required(
                    counterparty,
                    lease,
                    session_access,
                    &remote_noise_public_key,
                )
                .await?;
                return Err(err);
            }
        };
        Ok((link, stored_link_state))
    }

    async fn mark_outbound_link_recovery_required(
        &self,
        counterparty: &PubkyPublicKey,
        lease: &PeerLinkOperationLease,
        session_access: &PubkySessionAccess,
        remote_noise_public_key: &paykit_lib::PublicKey,
    ) -> Result<()> {
        mark_recovery_required_with_lease(
            &self.storage,
            counterparty.clone(),
            lease.clone(),
            self.clock.now(),
        )
        .await?;
        let _ = self
            .publish_local_recovery_marker_with_key(
                counterparty,
                session_access,
                lease,
                remote_noise_public_key,
            )
            .await;
        Ok(())
    }

    fn outbound_retry_thresholds(
        &self,
        now: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let lease_timeout = ChronoDuration::from_std(OUTBOUND_PRIVATE_SEND_LEASE_TIMEOUT)
            .expect("fixed outbound send lease timeout must fit chrono duration");
        let retry_backoff = ChronoDuration::from_std(OUTBOUND_PRIVATE_RETRY_BACKOFF)
            .expect("fixed outbound retry backoff must fit chrono duration");
        Ok((now - lease_timeout, now - retry_backoff))
    }

    pub(super) fn claimed_message_ready_for_send(
        &self,
        tx: &mut dyn StorageTransaction,
        sending: OutboundPrivateMessageRecord,
        lease: &PeerLinkOperationLease,
        report: &mut OutboundPrivateSendReport,
        now: DateTime<Utc>,
    ) -> Result<Option<OutboundPrivateMessageRecord>> {
        crate::storage::require_peer_link_operation_lease(tx, lease)?;
        let now = sending
            .last_attempt_at
            .map_or(now, |claimed_at| now.max(claimed_at));
        if let Err(err) = validate_queued_outbound_private_message(&sending) {
            let error = err.to_string();
            let failed = self.invalidate_outbound_in_transaction(tx, sending, error.clone())?;
            report.failed.push(OutboundPrivateSendFailure {
                outbound_message_id: failed.outbound_message_id,
                error,
            });
            return Ok(None);
        }

        if sending.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            return Ok(Some(sending));
        }

        let expired_releases = expired_outbound_reservation_cancellations_in_transaction(
            tx,
            &sending.counterparty,
            sending.outbound_message_id,
            now,
        );
        if expired_releases.is_empty() {
            return Ok(Some(sending));
        }

        let error = "Payment Endpoint Reservation expired before private list send".to_owned();
        let failed = self.invalidate_outbound_in_transaction(tx, sending, error.clone())?;
        report.failed.push(OutboundPrivateSendFailure {
            outbound_message_id: failed.outbound_message_id,
            error,
        });
        Ok(None)
    }

    fn invalidate_outbound_in_transaction(
        &self,
        tx: &mut dyn StorageTransaction,
        sending: OutboundPrivateMessageRecord,
        error: String,
    ) -> Result<OutboundPrivateMessageRecord> {
        let now = self.clock.now();
        let requires_recovery = sending.prepared_send.is_some();
        let failed = mark_outbound_invalid(sending, error, now);
        if requires_recovery {
            // The allocated Noise slot cannot be skipped on this link.
            mark_recovery_required_in_transaction(tx, &failed.counterparty, now)?;
        }
        tx.save_outbound_private_message(failed.clone())?;
        Ok(failed)
    }

    async fn record_private_send_success(
        &self,
        sending: OutboundPrivateMessageRecord,
        lease: &PeerLinkOperationLease,
        report: &mut OutboundPrivateSendReport,
        (stale_before, failed_retry_after): (DateTime<Utc>, DateTime<Utc>),
    ) -> Result<bool> {
        let now = self.clock.now();
        let sent = mark_outbound_sent(sending, now);
        let (link_id, has_more) = self
            .retry_storage_transaction(|| {
                let sent = sent.clone();
                let lease = lease.clone();
                move |tx| {
                    crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                    tx.save_outbound_private_message(sent.clone())?;
                    // The prepared-send transaction already persisted the advanced
                    // snapshot. Read its link ID in the same transaction as Sent.
                    let link_id = tx
                        .encrypted_link_state(&sent.counterparty)
                        .and_then(|state| state.link_snapshot)
                        .and_then(|snapshot| {
                            paykit_lib::EncryptedLinkSnapshot::deserialize(&snapshot).ok()
                        })
                        .and_then(|snapshot| snapshot.link_id());
                    // Reuse the committed queue view instead of acquiring another lock
                    // just to discover that this drain is complete.
                    let queued = tx.queued_outbound_private_messages(&sent.counterparty);
                    let registered_apps = queued
                        .iter()
                        .filter(|message| tx.paykit_app_is_registered(&message.app_id))
                        .map(|message| message.app_id.clone())
                        .collect();
                    let retired_apps = queued
                        .iter()
                        .filter(|message| tx.paykit_app_is_retired(&message.app_id))
                        .map(|message| message.app_id.clone())
                        .collect();
                    let has_more = outbound_private_queue_head_is_claimable(
                        &queued,
                        &registered_apps,
                        &retired_apps,
                        stale_before,
                        failed_retry_after,
                    );
                    Ok((link_id, has_more))
                }
            })
            .await?;
        if sent.kind == PrivateMessageKind::PrivatePaymentList.as_str() {
            if let Some(link_id) = link_id {
                if let Ok(mut publications) = self.private_payment_list_publications.lock() {
                    publications.insert(
                        (sent.counterparty.clone(), sent.app_id.clone()),
                        PrivatePaymentListPublication {
                            link_id,
                            outbound_message_id: sent.outbound_message_id,
                        },
                    );
                }
            }
        }
        report.sent.push(sent.outbound_message_id);
        Ok(has_more)
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
        let requires_recovery = private_send_error_requires_recovery(&err);
        let now = self.clock.now();
        let error = err.to_string();
        if requires_recovery {
            let failed = mark_outbound_recovery_required(sending, error.clone(), now);
            let failed = self
                .retry_storage_transaction(|| {
                    let failed = failed.clone();
                    let lease = lease.clone();
                    let counterparty = counterparty.clone();
                    move |tx| {
                        crate::storage::require_peer_link_operation_lease(tx, &lease)?;
                        mark_recovery_required_in_transaction(tx, &counterparty, now)?;
                        tx.save_outbound_private_message(failed.clone())?;
                        Ok(failed)
                    }
                })
                .await
                .map_err(|_| PaykitSdkError::from(err))?;
            report.failed.push(OutboundPrivateSendFailure {
                outbound_message_id: failed.outbound_message_id,
                error,
            });
            self.record_outbound_recovery_marker_result(
                report,
                counterparty,
                session_access,
                lease,
                Some(failed.outbound_message_id),
            )
            .await;
            return Ok(());
        }

        let failed = mark_outbound_failed(sending, error.clone(), now);
        let failed = self
            .save_outbound_with_lease(failed, lease)
            .await
            .map_err(|_| PaykitSdkError::from(err))?;
        report.failed.push(OutboundPrivateSendFailure {
            outbound_message_id: failed.outbound_message_id,
            error,
        });
        report.reservation_cleanup_failures.extend(
            self.cancel_terminal_private_list_reservations(counterparty, Some(lease), None)
                .await,
        );
        Ok(())
    }

    async fn save_outbound_with_lease(
        &self,
        record: OutboundPrivateMessageRecord,
        lease: &PeerLinkOperationLease,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.retry_storage_transaction(|| {
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
        lease: &PeerLinkOperationLease,
        outbound_message_id: Option<u64>,
    ) {
        if let Err(err) = self
            .publish_local_recovery_marker_with_session(counterparty, session_access, lease)
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

pub(super) fn private_send_error_requires_recovery(err: &paykit_lib::PaykitError) -> bool {
    err.is_non_retryable_private_send_error()
        || matches!(
            err,
            paykit_lib::PaykitError::Validation(_) | paykit_lib::PaykitError::InvalidData { .. }
        )
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
