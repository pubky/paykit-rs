use super::payment_resolution::filter_private_views_by_authorized_apps;
use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    /// Return the latest valid Private Payment List from each counterparty app.
    pub async fn current_private_payment_lists(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<crate::PrivatePaymentListView>> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(Vec::new());
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        if session_access
            .as_ref()
            .map(|session| {
                session.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)
            })
            .transpose()?
            .unwrap_or(false)
        {
            self.observe_remote_recovery_marker_for_cached_private_state(
                counterparty,
                session_access.as_deref(),
            )
            .await?;
        }
        let (_, authorized_app_ids) = self.private_app_authorization_context(counterparty).await?;
        let mut views = load_current_private_payment_lists(&self.storage, counterparty).await?;
        filter_private_views_by_authorized_apps(&mut views, authorized_app_ids.as_deref());
        Ok(views)
    }

    /// Enqueue the current complete Private Payment List for one counterparty.
    pub async fn enqueue_private_payment_list(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_list_queue_allowed(&counterparty)
                .await?;
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
        receiving_details: Vec<PrivateReceivingDetail>,
    ) -> Result<OutboundPrivateMessageRecord> {
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_list_queue_allowed(&counterparty)
                .await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                self.config.app_id.clone(),
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
    /// non-empty reservation list fails after this operation acquires the peer
    /// lease, the SDK asks the payment adapter to cancel any unpersisted
    /// reservations.
    pub async fn enqueue_private_payment_list_with_reservations(
        &self,
        counterparty: PubkyPublicKey,
        reservations: Vec<PrivatePaymentEndpointReservation>,
    ) -> Result<OutboundPrivateMessageRecord> {
        let cancellations = reservations
            .iter()
            .map(|reservation| reservation_cancellation(&counterparty, reservation))
            .collect::<Vec<_>>();
        let lease = self.claim_peer_link_operation(&counterparty).await?;
        let result = async {
            self.ensure_private_list_queue_allowed(&counterparty)
                .await?;
            queue_private_payment_list_with_reservations_with_link_lease(
                &self.storage,
                &counterparty,
                self.config.app_id.clone(),
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
            self.ensure_private_list_queue_allowed(&counterparty)
                .await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                self.config.app_id.clone(),
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
                    linked_private_counterparties_not_in_storage_state(
                        tx.export_storage_state(),
                        &contact_set,
                    )
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
        let tracked_message_ids = private_list_delivery_message_ids(&report);
        let tracked_counterparties = private_list_delivery_counterparties(&report);
        let outbound = self.process_pending_private_messages().await?;
        for counterparty_report in outbound {
            if !tracked_counterparties.contains(&counterparty_report.counterparty) {
                continue;
            }
            if let Some(send_report) = counterparty_report.report {
                report
                    .failed_to_deliver
                    .extend(delivery_failures_from_send_report(
                        counterparty_report.counterparty.clone(),
                        send_report,
                        &tracked_message_ids,
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
    /// Counterparties with an in-progress Encrypted Link can still have their
    /// lists queued; they remain eligible for a later outbound worker run after
    /// the link reaches `Linked`.
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
                    Err(_) => format!("{error}; reservation cleanup also failed"),
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
        let tracked_message_ids = private_list_delivery_message_ids(&report);
        for counterparty in queued_counterparties {
            match self.private_list_delivery_ready(&counterparty).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    report
                        .failed_to_deliver
                        .push(PrivatePaymentListDeliveryFailure {
                            counterparty,
                            outbound_message_id: None,
                            reservation_id: None,
                            error: err.to_string(),
                        });
                    continue;
                }
            }
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
                            &tracked_message_ids,
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

    async fn ensure_private_list_queue_allowed(&self, counterparty: &PubkyPublicKey) -> Result<()> {
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "local Pubky identity is not initialized".into(),
                source: None,
            });
        }
        if session_access.is_none() {
            return Err(PaykitSdkError::Identity {
                context: "no Pubky session available".into(),
                source: None,
            });
        }
        self.private_queue_readiness(counterparty).await.map(|_| ())
    }

    async fn private_list_delivery_ready(&self, counterparty: &PubkyPublicKey) -> Result<bool> {
        self.private_queue_readiness(counterparty)
            .await
            .map(|readiness| readiness == PrivateQueueReadiness::Ready)
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
                    Ok(linked_private_counterparties_not_in_storage_state(
                        snapshot, &keep,
                    ))
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
            .reserve_private_receiving_details(&counterparty)
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
                self.config.app_id.clone(),
                reservations,
                now,
                lease,
            )
            .await;
            match result {
                Ok(record) => Ok(record),
                Err(err) => {
                    self.cancel_reservations_after_queue_error(&cancellations, &counterparty)
                        .await?;
                    Err(err)
                }
            }
        } else {
            let receiving_details = self.private_receiving_details(&counterparty).await?;
            enqueue_private_payment_list_message_with_link_lease(
                &self.storage,
                counterparty,
                self.config.app_id.clone(),
                receiving_details,
                self.clock.now(),
                lease,
            )
            .await
        }
    }

    async fn private_receiving_details(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PrivateReceivingDetail>> {
        self.payment
            .current_private_receiving_details(counterparty)
            .await
    }
}

fn linked_private_counterparties_not_in_storage_state(
    snapshot: crate::storage::StorageState,
    keep: &HashSet<PubkyPublicKey>,
) -> Vec<PubkyPublicKey> {
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
                && !keep.contains(&peer.counterparty)
                && active_link_counterparties.contains(&peer.counterparty)
        })
        .map(|peer| peer.counterparty)
        .collect()
}

fn reservation_cancellation(
    counterparty: &PubkyPublicKey,
    reservation: &PrivatePaymentEndpointReservation,
) -> PrivatePaymentEndpointReservationCancellation {
    PrivatePaymentEndpointReservationCancellation {
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

fn private_list_delivery_message_ids(report: &PrivatePaymentListDeliveryReport) -> HashSet<u64> {
    report
        .queued
        .iter()
        .chain(&report.cleared)
        .filter_map(|change| change.outbound_message_id)
        .collect()
}

fn private_list_delivery_counterparties(
    report: &PrivatePaymentListDeliveryReport,
) -> HashSet<PubkyPublicKey> {
    report
        .queued
        .iter()
        .chain(&report.cleared)
        .map(|change| change.counterparty.clone())
        .collect()
}

fn delivery_failures_from_send_report(
    counterparty: PubkyPublicKey,
    report: OutboundPrivateSendReport,
    tracked_message_ids: &HashSet<u64>,
) -> Vec<PrivatePaymentListDeliveryFailure> {
    let mut failures = Vec::new();

    for failure in report
        .failed
        .into_iter()
        .filter(|failure| tracked_message_ids.contains(&failure.outbound_message_id))
    {
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

    for failure in report
        .recovery_marker_failures
        .into_iter()
        .filter(|failure| {
            failure
                .outbound_message_id
                .is_some_and(|message_id| tracked_message_ids.contains(&message_id))
        })
    {
        failures.push(PrivatePaymentListDeliveryFailure {
            counterparty: counterparty.clone(),
            outbound_message_id: failure.outbound_message_id,
            reservation_id: None,
            error: failure.error,
        });
    }

    failures
}

#[cfg(test)]
mod delivery_report_tests {
    use super::*;

    #[test]
    fn test_private_list_delivery_ignores_unrelated_outbound_failures() {
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let report = OutboundPrivateSendReport {
            failed: vec![
                OutboundPrivateSendFailure {
                    outbound_message_id: 7,
                    error: "tracked".into(),
                },
                OutboundPrivateSendFailure {
                    outbound_message_id: 8,
                    error: "unrelated".into(),
                },
            ],
            recovery_marker_failures: vec![RecoveryMarkerPublishFailure {
                outbound_message_id: Some(8),
                error: "unrelated marker".into(),
            }],
            ..OutboundPrivateSendReport::default()
        };

        let failures =
            delivery_failures_from_send_report(counterparty, report, &HashSet::from([7]));

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].outbound_message_id, Some(7));
    }
}
