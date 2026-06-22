use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return the latest valid Private Payment List view for a counterparty.
    pub async fn current_private_payment_list(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Option<crate::PrivatePaymentListView>> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(None);
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        let required_capabilities = self.config.required_session_capabilities();
        if session_access
            .as_ref()
            .map(|session| session.private_link_capable_for_capabilities(&required_capabilities))
            .transpose()?
            .unwrap_or(false)
        {
            self.observe_remote_recovery_marker_for_cached_private_state(
                counterparty,
                session_access.as_ref(),
            )
            .await?;
        }
        load_current_private_payment_list(&self.storage, counterparty).await
    }

    /// Enqueue the current complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_outbound_ready(&counterparty).await?;
            self.enqueue_private_payment_list_from_receiving_details_with_claim(
                counterparty,
                &lease,
            )
            .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Enqueue an explicit complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list_with_receiving_details(
        &self,
        counterparty: PubkyPublicKey,
        receiving_details: Vec<ReceivingDetail>,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_outbound_ready(&counterparty).await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                receiving_details,
                self.clock.now(),
                &lease,
            )
            .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Enqueue a reservation-backed Private Payment List for one counterparty.
    ///
    /// An empty reservation list queues an empty Private Payment List. If a
    /// non-empty reservation list cannot be queued, the SDK asks the payment
    /// adapter to cancel any unpersisted reservations.
    pub async fn enqueue_private_payment_list_with_reservations(
        &self,
        counterparty: PubkyPublicKey,
        reservations: Vec<PaymentEndpointReservation>,
    ) -> Result<OutboundPrivateMessageRecord> {
        let cancellations = reservations
            .iter()
            .map(|reservation| reservation_cancellation(&counterparty, reservation))
            .collect::<Vec<_>>();
        let lease = match self.claim_peer_link_operation(&counterparty).await {
            Ok(lease) => lease,
            Err(err) => {
                return self
                    .cancel_reservations_and_return_queue_error(&cancellations, &counterparty, err)
                    .await;
            }
        };
        let result = async {
            self.ensure_private_outbound_ready(&counterparty).await?;
            queue_private_payment_list_with_reservations_with_link_lease(
                &self.storage,
                &counterparty,
                reservations,
                self.clock.now(),
                &lease,
            )
            .await
        }
        .await;
        let result = match result {
            Ok(record) => Ok(record),
            Err(err) => {
                self.cancel_reservations_and_return_queue_error(&cancellations, &counterparty, err)
                    .await
            }
        };
        self.finish_peer_link_operation(lease, result).await
    }

    /// Enqueue an empty Private Payment List for one counterparty.
    ///
    /// This removes locally shared private receiving details after the queued
    /// message is delivered. It does not cancel payment-method state that was
    /// already shared with the counterparty.
    pub async fn clear_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_outbound_ready(&counterparty).await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                Vec::new(),
                self.clock.now(),
                &lease,
            )
            .await
        }
        .await;
        self.finish_peer_link_operation(lease, result).await
    }

    /// Queue an empty Private Payment List and process that counterparty's queue.
    pub async fn clear_private_payment_list_and_process_outbound(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivatePaymentListDeliveryReport> {
        self.sync_private_payment_lists_with_reservations_and_process_outbound(
            vec![PrivatePaymentListReservationUpdate {
                counterparty,
                reservations: Vec::new(),
            }],
            false,
        )
        .await
    }

    /// Queue Private Payment List updates for saved contacts.
    ///
    /// Saved contacts receive the current private receiving details. When
    /// `clear_unlisted_linked_peers` is true, linked peers that are no longer
    /// saved contacts receive an empty Private Payment List.
    pub async fn sync_contact_private_payment_lists(
        &self,
        clear_unlisted_linked_peers: bool,
    ) -> Result<PrivatePaymentListSyncReport> {
        self.require_initialized_identity("sync contact Private Payment Lists")
            .await?;
        let (mut contacts, mut clear_counterparties) = self
            .storage
            .transaction(|tx| {
                let mut contacts = tx
                    .contact_records()
                    .into_iter()
                    .map(|record| record.public_key)
                    .collect::<Vec<_>>();
                contacts.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                contacts.dedup();

                let clear_counterparties = if clear_unlisted_linked_peers {
                    let contact_set = contacts.iter().cloned().collect::<HashSet<_>>();
                    let snapshot = tx.export_storage_state();
                    let active_link_counterparties = snapshot
                        .encrypted_link_states
                        .iter()
                        .filter(|(_, state)| state.link_snapshot.is_some())
                        .map(|(counterparty, _)| counterparty.clone())
                        .collect::<HashSet<_>>();
                    snapshot
                        .linked_peers
                        .into_values()
                        .filter(|peer| {
                            peer.state == LinkedPeerState::Linked
                                && !contact_set.contains(&peer.counterparty)
                                && active_link_counterparties.contains(&peer.counterparty)
                        })
                        .map(|peer| peer.counterparty)
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                Ok((contacts, clear_counterparties))
            })
            .await?;

        clear_counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        clear_counterparties.dedup();

        let mut report = PrivatePaymentListSyncReport::default();
        for counterparty in contacts.drain(..) {
            match self
                .enqueue_private_payment_list(counterparty.clone())
                .await
            {
                Ok(record) => report.queued.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: Some(record.outbound_message_id),
                    error: None,
                }),
                Err(err) => report.failed.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: None,
                    error: Some(err.to_string()),
                }),
            }
        }

        for counterparty in clear_counterparties {
            match self.clear_private_payment_list(counterparty.clone()).await {
                Ok(record) => report.cleared.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: Some(record.outbound_message_id),
                    error: None,
                }),
                Err(err) => report.failed.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: None,
                    error: Some(err.to_string()),
                }),
            }
        }

        Ok(report)
    }

    /// Queue contact Private Payment Lists and process pending private messages.
    pub async fn sync_contact_private_payment_lists_and_process_outbound(
        &self,
        clear_unlisted_linked_peers: bool,
    ) -> Result<PrivatePaymentListDeliveryReport> {
        let sync = self
            .sync_contact_private_payment_lists(clear_unlisted_linked_peers)
            .await?;
        let mut report = delivery_report_from_sync_report(sync);
        let outbound = self.process_pending_private_messages().await?;
        for counterparty_report in outbound {
            if let Some(send_report) = counterparty_report.report {
                report
                    .failed_to_deliver
                    .extend(delivery_failures_from_send_report(
                        counterparty_report.counterparty.clone(),
                        send_report,
                    ));
            }
            if let Some(error) = counterparty_report.error {
                report
                    .failed_to_deliver
                    .push(PrivatePaymentListDeliveryFailure {
                        counterparty: counterparty_report.counterparty,
                        outbound_message_id: None,
                        reservation_id: None,
                        error,
                    });
            }
        }
        Ok(report)
    }

    /// Queue reservation-backed Private Payment Lists and process their queues.
    ///
    /// Each update is the complete Private Payment List for one counterparty.
    /// An update with no reservations queues an empty Private Payment List for
    /// that counterparty. When `clear_unlisted_linked_peers` is true, linked
    /// peers that are not included in `updates` also receive empty lists.
    pub async fn sync_private_payment_lists_with_reservations_and_process_outbound(
        &self,
        mut updates: Vec<PrivatePaymentListReservationUpdate>,
        clear_unlisted_linked_peers: bool,
    ) -> Result<PrivatePaymentListDeliveryReport> {
        self.require_initialized_identity("sync reservation-backed Private Payment Lists")
            .await?;

        updates.sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));
        let mut update_counts = HashMap::new();
        for update in &updates {
            *update_counts
                .entry(update.counterparty.clone())
                .or_insert(0usize) += 1;
        }
        let update_counterparties = update_counts.keys().cloned().collect::<HashSet<_>>();

        let mut clear_counterparties = if clear_unlisted_linked_peers {
            self.linked_private_counterparties_not_in(&update_counterparties)
                .await?
        } else {
            Vec::new()
        };
        clear_counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        clear_counterparties.dedup();

        let mut report = PrivatePaymentListDeliveryReport::default();
        let mut queued_counterparties = Vec::new();

        for update in updates {
            let counterparty = update.counterparty;
            let is_clear = update.reservations.is_empty();
            if update_counts
                .get(&counterparty)
                .copied()
                .unwrap_or_default()
                > 1
            {
                let cancellations = update
                    .reservations
                    .iter()
                    .map(|reservation| reservation_cancellation(&counterparty, reservation))
                    .collect::<Vec<_>>();
                let error = format!(
                    "duplicate Private Payment List update for {}",
                    counterparty.redacted_app_key()
                );
                let error = match self
                    .cancel_reservations_after_queue_error(&cancellations, &counterparty)
                    .await
                {
                    Ok(()) => error,
                    Err(cancellation_err) => {
                        format!("{error}; reservation cleanup also failed: {cancellation_err}")
                    }
                };
                report.failed_to_queue.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: None,
                    error: Some(error),
                });
                continue;
            }
            match self
                .enqueue_private_payment_list_with_reservations(
                    counterparty.clone(),
                    update.reservations,
                )
                .await
            {
                Ok(record) => {
                    queued_counterparties.push(counterparty.clone());
                    let change = PrivatePaymentListSyncChange {
                        counterparty,
                        outbound_message_id: Some(record.outbound_message_id),
                        error: None,
                    };
                    if is_clear {
                        report.cleared.push(change);
                    } else {
                        report.queued.push(change);
                    }
                }
                Err(err) => report.failed_to_queue.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: None,
                    error: Some(err.to_string()),
                }),
            }
        }

        for counterparty in clear_counterparties {
            match self.clear_private_payment_list(counterparty.clone()).await {
                Ok(record) => {
                    queued_counterparties.push(counterparty.clone());
                    report.cleared.push(PrivatePaymentListSyncChange {
                        counterparty,
                        outbound_message_id: Some(record.outbound_message_id),
                        error: None,
                    });
                }
                Err(err) => report.failed_to_queue.push(PrivatePaymentListSyncChange {
                    counterparty,
                    outbound_message_id: None,
                    error: Some(err.to_string()),
                }),
            }
        }

        queued_counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        queued_counterparties.dedup();
        for counterparty in queued_counterparties {
            match self
                .process_outbound_private_messages(counterparty.clone())
                .await
            {
                Ok(send_report) => {
                    report
                        .failed_to_deliver
                        .extend(delivery_failures_from_send_report(
                            counterparty,
                            send_report,
                        ));
                }
                Err(err) => report
                    .failed_to_deliver
                    .push(PrivatePaymentListDeliveryFailure {
                        counterparty,
                        outbound_message_id: None,
                        reservation_id: None,
                        error: err.to_string(),
                    }),
            }
        }

        Ok(report)
    }

    async fn linked_private_counterparties_not_in(
        &self,
        keep: &HashSet<PubkyPublicKey>,
    ) -> Result<Vec<PubkyPublicKey>> {
        self.storage
            .transaction({
                let keep = keep.clone();
                move |tx| {
                    let snapshot = tx.export_storage_state();
                    let active_link_counterparties = snapshot
                        .encrypted_link_states
                        .iter()
                        .filter(|(_, state)| state.link_snapshot.is_some())
                        .map(|(counterparty, _)| counterparty.clone())
                        .collect::<HashSet<_>>();
                    Ok(snapshot
                        .linked_peers
                        .into_values()
                        .filter(|peer| {
                            peer.state == LinkedPeerState::Linked
                                && !keep.contains(&peer.counterparty)
                                && active_link_counterparties.contains(&peer.counterparty)
                        })
                        .map(|peer| peer.counterparty)
                        .collect())
                }
            })
            .await
    }

    #[cfg(test)]
    pub(super) async fn enqueue_private_payment_list_from_receiving_details(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = self
            .enqueue_private_payment_list_from_receiving_details_with_claim(counterparty, &lease)
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    pub(super) async fn enqueue_private_payment_list_from_receiving_details_with_claim(
        &self,
        counterparty: PubkyPublicKey,
        lease: &PeerLinkOperationLease,
    ) -> Result<OutboundPrivateMessageRecord> {
        if let Some(reservations) = self
            .payment
            .reserve_receiving_details(&counterparty)
            .await?
        {
            let cancellations = reservations
                .iter()
                .map(|reservation| reservation_cancellation(&counterparty, reservation))
                .collect::<Vec<_>>();
            let now = self.clock.now();
            let result = queue_private_payment_list_with_reservations_with_link_lease(
                &self.storage,
                &counterparty,
                reservations,
                now,
                lease,
            )
            .await;
            match result {
                Ok(record) => Ok(record),
                Err(err) => {
                    if let Err(cancellation_err) = self
                        .cancel_reservations_after_queue_error(&cancellations, &counterparty)
                        .await
                    {
                        return Err(PaykitSdkError::Policy(format!(
                            "failed to queue reserved receiving details: {err}; reservation cleanup also failed: {cancellation_err}"
                        )));
                    }
                    Err(err)
                }
            }
        } else {
            let receiving_details = self.private_receiving_details(&counterparty).await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                receiving_details,
                self.clock.now(),
                lease,
            )
            .await
        }
    }

    async fn cancel_reservations_after_queue_error(
        &self,
        cancellations: &[PaymentEndpointReservationCancellation],
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let mut cancellation_errors = Vec::new();
        for cancellation in cancellations {
            let can_cancel = self
                .storage
                .transaction({
                    let counterparty = counterparty.clone();
                    let cancellation = cancellation.clone();
                    move |tx| {
                        Ok(!tx
                            .payment_endpoint_reservation(
                                &counterparty,
                                &cancellation.reservation_id,
                            )
                            .is_some_and(|record| {
                                record.reservation_id == cancellation.reservation_id
                                    && record.counterparty == cancellation.counterparty
                                    && record.identifier == cancellation.identifier
                                    && record.payload_hash == cancellation.payload_hash
                            }))
                    }
                })
                .await?;
            if !can_cancel {
                continue;
            }
            if let Err(err) = self
                .payment
                .cancel_receiving_detail_reservation(cancellation)
                .await
            {
                cancellation_errors.push(format!("{}: {err}", cancellation.reservation_id));
            }
        }
        if cancellation_errors.is_empty() {
            Ok(())
        } else {
            Err(PaykitSdkError::Policy(format!(
                "failed to cancel reserved receiving details: {}",
                cancellation_errors.join("; ")
            )))
        }
    }

    async fn cancel_reservations_and_return_queue_error<T>(
        &self,
        cancellations: &[PaymentEndpointReservationCancellation],
        counterparty: &PubkyPublicKey,
        err: PaykitSdkError,
    ) -> Result<T> {
        if let Err(cancellation_err) = self
            .cancel_reservations_after_queue_error(cancellations, counterparty)
            .await
        {
            return Err(PaykitSdkError::Policy(format!(
                "failed to queue reserved receiving details: {err}; reservation cleanup also failed: {cancellation_err}"
            )));
        }
        Err(err)
    }

    pub(super) async fn cancel_unattempted_superseded_reservations(
        &self,
        counterparty: &PubkyPublicKey,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let cancellations =
            match unattempted_superseded_reservation_cancellations(&self.storage, counterparty)
                .await
            {
                Ok(cancellations) => cancellations,
                Err(err) => {
                    return vec![ReservationCleanupFailure {
                        reservation_id: None,
                        error: err.to_string(),
                    }];
                }
            };
        self.cancel_reservation_records(cancellations, lease).await
    }

    pub(super) async fn cancel_terminal_private_list_reservations(
        &self,
        counterparty: &PubkyPublicKey,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let mut failures = self
            .cancel_unattempted_superseded_reservations(counterparty, lease)
            .await;
        let cancellations =
            match invalid_private_list_reservation_cancellations(&self.storage, counterparty).await
            {
                Ok(cancellations) => cancellations,
                Err(err) => {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: None,
                        error: err.to_string(),
                    });
                    return failures;
                }
            };
        failures.extend(self.cancel_reservation_records(cancellations, lease).await);
        failures
    }

    pub(super) async fn cancel_reservation_records(
        &self,
        cancellations: Vec<PaymentEndpointReservationCancellationRecord>,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let mut failures = Vec::new();
        for cancellation_record in cancellations {
            let cancellation = cancellation_record.cancellation;
            match self
                .claim_reservation_cancellation(
                    &cancellation,
                    cancellation_record.outbound_message_id,
                    lease,
                    self.clock.now(),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: Some(cancellation.reservation_id),
                        error: err.to_string(),
                    });
                    continue;
                }
            }
            match self
                .payment
                .cancel_receiving_detail_reservation(&cancellation)
                .await
            {
                Ok(()) => {
                    if let Err(err) = self
                        .storage
                        .transaction({
                            let cancellation = cancellation.clone();
                            let outbound_message_id = cancellation_record.outbound_message_id;
                            move |tx| {
                                if tx
                                    .payment_endpoint_reservation(
                                        &cancellation.counterparty,
                                        &cancellation.reservation_id,
                                    )
                                    .is_some_and(|record| {
                                        record.outbound_message_id == outbound_message_id
                                            && record.identifier == cancellation.identifier
                                            && record.payload_hash == cancellation.payload_hash
                                            && record.cancellation_started_at.is_some()
                                    })
                                {
                                    tx.remove_payment_endpoint_reservation(
                                        &cancellation.counterparty,
                                        &cancellation.reservation_id,
                                    );
                                }
                                Ok(())
                            }
                        })
                        .await
                    {
                        failures.push(ReservationCleanupFailure {
                            reservation_id: Some(cancellation.reservation_id.clone()),
                            error: err.to_string(),
                        });
                    }
                }
                Err(err) => failures.push(ReservationCleanupFailure {
                    reservation_id: Some(cancellation.reservation_id),
                    error: err.to_string(),
                }),
            }
        }
        failures
    }

    async fn claim_reservation_cancellation(
        &self,
        cancellation: &PaymentEndpointReservationCancellation,
        outbound_message_id: u64,
        lease: Option<&PeerLinkOperationLease>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.storage
            .transaction({
                let cancellation = cancellation.clone();
                let lease = lease.cloned();
                move |tx| {
                    if let Some(lease) = lease.as_ref() {
                        crate::storage::require_peer_link_operation_lease(tx, lease)?;
                    }
                    let Some(mut record) = tx.payment_endpoint_reservation(
                        &cancellation.counterparty,
                        &cancellation.reservation_id,
                    ) else {
                        return Ok(false);
                    };
                    if record.outbound_message_id != outbound_message_id
                        || record.identifier != cancellation.identifier
                        || record.payload_hash != cancellation.payload_hash
                    {
                        return Ok(false);
                    }
                    record.cancellation_started_at = Some(now);
                    tx.save_payment_endpoint_reservation(record);
                    Ok(true)
                }
            })
            .await
    }

    async fn private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<ReceivingDetail>> {
        self.payment
            .current_receiving_details(ReceivingDetailScope::Private {
                counterparty: counterparty.clone(),
            })
            .await
    }
}

fn reservation_cancellation(
    counterparty: &PubkyPublicKey,
    reservation: &PaymentEndpointReservation,
) -> PaymentEndpointReservationCancellation {
    PaymentEndpointReservationCancellation {
        reservation_id: reservation.reservation_id.clone(),
        counterparty: counterparty.clone(),
        identifier: reservation.receiving_detail.identifier.clone(),
        payload_hash: reservation_payload_hash(&reservation.receiving_detail.payload),
        attribution: reservation.attribution.clone(),
    }
}

fn delivery_report_from_sync_report(
    sync: PrivatePaymentListSyncReport,
) -> PrivatePaymentListDeliveryReport {
    PrivatePaymentListDeliveryReport {
        queued: sync.queued,
        cleared: sync.cleared,
        failed_to_queue: sync.failed,
        failed_to_deliver: Vec::new(),
    }
}

fn delivery_failures_from_send_report(
    counterparty: PubkyPublicKey,
    report: OutboundPrivateSendReport,
) -> Vec<PrivatePaymentListDeliveryFailure> {
    let mut failures = Vec::new();

    for failure in report.failed {
        failures.push(PrivatePaymentListDeliveryFailure {
            counterparty: counterparty.clone(),
            outbound_message_id: Some(failure.outbound_message_id),
            reservation_id: None,
            error: failure.error,
        });
    }

    for failure in report.reservation_cleanup_failures {
        failures.push(PrivatePaymentListDeliveryFailure {
            counterparty: counterparty.clone(),
            outbound_message_id: None,
            reservation_id: failure.reservation_id,
            error: failure.error,
        });
    }

    for failure in report.recovery_marker_failures {
        failures.push(PrivatePaymentListDeliveryFailure {
            counterparty: counterparty.clone(),
            outbound_message_id: failure.outbound_message_id,
            reservation_id: None,
            error: failure.error,
        });
    }

    failures
}
