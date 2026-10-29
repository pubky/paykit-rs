use super::outbound_private::PrivateSendReadiness;
use super::payment_resolution::filter_private_views_by_authorized_apps;
use super::*;
use crate::domain::endpoint_reservations::{
    private_payment_list_sent_after_last_link_change,
    queue_private_payment_list_with_reservations_in_transaction,
};
use crate::PrivatePaymentListAcknowledgement;
use futures_util::FutureExt;

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
        let private_live = session_access
            .as_ref()
            .map(|session| {
                session.private_link_capable_for_capabilities(PAYKIT_SESSION_CAPABILITIES)
            })
            .transpose()?
            .unwrap_or(false);
        if private_live {
            self.observe_remote_recovery_marker_for_cached_private_state(
                counterparty,
                session_access.as_ref(),
            )
            .await?;
        }
        let authorized_app_ids = if private_live {
            self.private_app_authorization_context(counterparty)
                .await?
                .1
        } else {
            self.cached_counterparty_app_authorization_context(counterparty)
                .await?
                .private_apps
        };
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
    /// non-empty reservation list fails, the SDK acquires the peer lease before
    /// asking the payment adapter to cancel any unpersisted reservations.
    pub async fn enqueue_private_payment_list_with_reservations(
        &self,
        counterparty: PubkyPublicKey,
        reservations: Vec<PrivatePaymentEndpointReservation>,
    ) -> Result<OutboundPrivateMessageRecord> {
        self.queue_private_payment_list_with_reservations(counterparty, reservations, false)
            .await
    }

    async fn queue_private_payment_list_with_reservations(
        &self,
        counterparty: PubkyPublicKey,
        reservations: Vec<PrivatePaymentEndpointReservation>,
        reuse_unchanged: bool,
    ) -> Result<OutboundPrivateMessageRecord> {
        let cancellations = reservations
            .iter()
            .map(|reservation| reservation_cancellation(&counterparty, reservation))
            .collect::<Vec<_>>();
        let result = self
            .with_storage_operation(Box::pin(async {
                self.ensure_private_list_queue_allowed(&counterparty)
                    .await?;
                self.storage
                    .transaction(|tx| {
                        let now = self.clock.now();
                        // The transaction serializes queueing. Remove expired leases so
                        // their ID-based handles cannot authorize later writes.
                        if let Some(lease) = tx.peer_link_operation_lease(&counterparty) {
                            if lease.expires_at > now {
                                return Err(PaykitSdkError::ConcurrentUpdate {
                                    context: format!(
                                        "peer link operation already in progress for counterparty {counterparty}"
                                    ),
                                    source: None,
                                });
                            }
                            tx.release_peer_link_operation(&counterparty, lease.lease_id);
                        }
                        Self::private_queue_readiness_in_transaction(tx, &counterparty)?;
                        let policy = if reuse_unchanged {
                            PrivatePaymentListQueuePolicy::Sync {
                                sent_message_id: self
                                    .reusable_private_payment_list_id(tx, &counterparty),
                            }
                        } else {
                            PrivatePaymentListQueuePolicy::Always
                        };
                        queue_private_payment_list_with_reservations_in_transaction(
                            tx,
                            &counterparty,
                            self.config.app_id.clone(),
                            reservations,
                            now,
                            policy,
                        )
                    })
                    .await
            }))
            .await;
        let Err(err) = result else {
            return result;
        };
        if cancellations.is_empty() {
            return Err(err);
        }
        // A failed commit may still have persisted the list. Take the peer lease
        // before checking ownership and calling the wallet outside shared storage.
        let Ok(lease) = self.claim_peer_link_operation(&counterparty).await else {
            return Err(err);
        };
        let result = self
            .cancel_reservations_and_return_queue_error(&cancellations, &counterparty, err)
            .await;
        self.finish_peer_link_operation(lease, result).await
    }

    fn published_private_payment_list_id(
        &self,
        tx: &dyn StorageTransaction,
        counterparty: &PubkyPublicKey,
    ) -> Option<u64> {
        // Only a delivery witnessed on the current Noise link can make a sent row
        // reusable.
        let link_id = current_link_id(tx, counterparty)?;
        self.private_payment_list_publications
            .lock()
            .ok()
            .and_then(|publications| {
                publications
                    .get(&(counterparty.clone(), self.config.app_id.clone()))
                    .filter(|publication| publication.link_id == link_id)
                    .map(|publication| publication.outbound_message_id)
            })
    }

    /// Return the sent Private Payment List an unchanged sync may reuse.
    ///
    /// Without a runtime witness, for example after a restart, this falls back
    /// to durable timestamps so unchanged lists are not republished on every
    /// launch. The fallback is not delivery evidence, so acknowledgements use
    /// `published_private_payment_list_id` instead.
    fn reusable_private_payment_list_id(
        &self,
        tx: &dyn StorageTransaction,
        counterparty: &PubkyPublicKey,
    ) -> Option<u64> {
        self.published_private_payment_list_id(tx, counterparty)
            .or_else(|| {
                current_link_id(tx, counterparty)?;
                private_payment_list_sent_after_last_link_change(
                    tx,
                    counterparty,
                    &self.config.app_id,
                )
            })
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
    ///
    /// Each call queues a fresh empty list, which supersedes older pending
    /// lists from this Paykit App. While the counterparty is `Linked`, an empty
    /// `failed_to_queue` and `failed_to_deliver` mean the empty list was sent.
    pub async fn clear_private_payment_list_and_process_outbound(
        &self,
        counterparty: PubkyPublicKey,
    ) -> Result<PrivatePaymentListDeliveryReport> {
        // Bound the composed future's stack size and type depth for FFI callers.
        self.sync_private_payment_lists_with_reservations(
            vec![PrivatePaymentListReservationUpdate {
                counterparty,
                reservations: Vec::new(),
            }],
            false,
            false,
        )
        .boxed()
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
        let tracked_counterparties = private_list_delivery_counterparties(&report);
        let mut runs = HashMap::new();
        let mut failed_counterparties = HashSet::new();
        let outbound = self.process_pending_private_messages().await?;
        for OutboundPrivateCounterpartySendReport {
            counterparty,
            report: send_report,
            error,
        } in outbound
        {
            if !tracked_counterparties.contains(&counterparty) {
                continue;
            }
            if let Some(send_report) = send_report {
                let run = OutboundListRun::from_send_report(&send_report);
                let tracked_message_ids = private_list_delivery_message_ids(&report, &counterparty);
                report
                    .failed_to_deliver
                    .extend(delivery_failures_from_send_report(
                        counterparty.clone(),
                        send_report,
                        &tracked_message_ids,
                    ));
                runs.insert(counterparty.clone(), run);
            }
            if let Some(error) = error {
                // This counterparty-wide failure already covers its unsent lists.
                failed_counterparties.insert(counterparty.clone());
                report
                    .failed_to_deliver
                    .push(PrivatePaymentListDeliveryFailure {
                        counterparty,
                        outbound_message_id: None,
                        reservation_id: None,
                        error,
                    });
            }
        }
        // A counterparty whose queue head waits out a retry backoff is not
        // processed at all, so check every tracked counterparty, not only runs.
        let mut unchecked = tracked_counterparties
            .into_iter()
            .filter(|counterparty| !failed_counterparties.contains(counterparty))
            .collect::<Vec<_>>();
        unchecked.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        for counterparty in unchecked {
            let run = runs.remove(&counterparty).unwrap_or_default();
            self.report_unsent_private_payment_lists(&mut report, &counterparty, &run)
                .await?;
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
    /// Unchanged lists with the same reservations reuse their existing outbound
    /// record, including lists already sent on the current Encrypted Link.
    /// Outbound retries and reservation cleanup still run for reused records.
    /// After a restart, a sent list is reused only when it was sent more than
    /// five minutes after the peer's last link change or private receive;
    /// otherwise it is republished once.
    pub async fn sync_private_payment_lists_with_reservations_and_process_outbound(
        &self,
        updates: Vec<PrivatePaymentListReservationUpdate>,
        clear_unlisted_linked_peers: bool,
    ) -> Result<PrivatePaymentListDeliveryReport> {
        // Bound the composed future's stack size and type depth for FFI callers.
        self.sync_private_payment_lists_with_reservations(
            updates,
            clear_unlisted_linked_peers,
            true,
        )
        .boxed()
        .await
    }

    async fn sync_private_payment_lists_with_reservations(
        &self,
        mut updates: Vec<PrivatePaymentListReservationUpdate>,
        clear_unlisted_linked_peers: bool,
        reuse_unchanged: bool,
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
                .queue_private_payment_list_with_reservations(
                    counterparty.clone(),
                    update.reservations,
                    reuse_unchanged,
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
            match self
                .queue_private_payment_list_with_reservations(
                    counterparty.clone(),
                    Vec::new(),
                    reuse_unchanged,
                )
                .await
            {
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
        let mut deliveries = Vec::with_capacity(queued_counterparties.len());
        for counterparty in queued_counterparties {
            let result = self
                .process_outbound_private_messages_with_readiness(
                    counterparty.clone(),
                    PrivateSendReadiness::PrivatePaymentList,
                )
                .await;
            deliveries.push((counterparty, result));
        }
        for (counterparty, result) in deliveries {
            match result {
                Ok(send_report) => {
                    let run = OutboundListRun::from_send_report(&send_report);
                    let tracked_message_ids =
                        private_list_delivery_message_ids(&report, &counterparty);
                    report
                        .failed_to_deliver
                        .extend(delivery_failures_from_send_report(
                            counterparty.clone(),
                            send_report,
                            &tracked_message_ids,
                        ));
                    self.report_unsent_private_payment_lists(&mut report, &counterparty, &run)
                        .await?;
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

    /// Report tracked Private Payment Lists that an outbound run left unsent.
    ///
    /// A list is reported when it is still queued, when a newer list for the
    /// same Paykit App superseded it, for example one queued by another runtime
    /// or device, or when its outbound record is gone, for example after
    /// compaction removed a superseded list that was never attempted. Lists
    /// already `Sent`, including unchanged lists reused from an earlier send,
    /// are not reported. Neither are lists still queued for a counterparty
    /// whose Encrypted Link Handshake is pending: they stay eligible for a
    /// later run after the link reaches `Linked`. Superseded, invalid, and
    /// missing lists are reported even then, because the handshake cannot
    /// make them sendable.
    pub(super) async fn report_unsent_private_payment_lists(
        &self,
        report: &mut PrivatePaymentListDeliveryReport,
        counterparty: &PubkyPublicKey,
        run: &OutboundListRun,
    ) -> Result<()> {
        let tracked = private_list_delivery_message_ids(report, counterparty);
        if tracked.is_empty() {
            return Ok(());
        }
        let (handshake_pending, statuses, acknowledged) = self
            .storage
            .transaction(|tx| {
                let readiness = Self::private_queue_readiness_in_transaction(tx, counterparty);
                let handshake_pending =
                    matches!(readiness, Ok(PrivateQueueReadiness::PendingHandshake));
                let messages = tx.outbound_private_messages(counterparty);
                let latest_list_id = messages
                    .iter()
                    .filter(|message| {
                        message.app_id == self.config.app_id
                            && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                    })
                    .map(|message| message.outbound_message_id)
                    .max();
                let statuses = messages
                    .into_iter()
                    .map(|message| (message.outbound_message_id, message.status))
                    .collect::<HashMap<_, _>>();
                // A Sent row alone can come from restored/shared state. Require
                // this runtime's successful publication on the current link,
                // and refuse a witness superseded by a newer local publication.
                let acknowledged = matches!(readiness, Ok(PrivateQueueReadiness::Ready))
                    .then(|| self.published_private_payment_list_id(tx, counterparty))
                    .flatten()
                    .filter(|id| tracked.contains(id))
                    // Another commit may complete after Sent released its lease
                    // but before this runtime updates its publication witness.
                    // A newer list also makes an older clear unsafe to retain.
                    .filter(|id| latest_list_id == Some(*id))
                    .filter(|id| {
                        matches!(statuses.get(id), Some(OutboundPrivateMessageStatus::Sent))
                    });
                Ok((handshake_pending, statuses, acknowledged))
            })
            .await?;
        // Acknowledgements never suppress queue, reservation, or delivery errors.
        if let Some(outbound_message_id) = acknowledged {
            let entry = PrivatePaymentListAcknowledgement {
                counterparty: counterparty.clone(),
                outbound_message_id,
            };
            if !report.acknowledged.contains(&entry) {
                report.acknowledged.push(entry);
            }
        }
        let mut unsent = tracked
            .into_iter()
            .filter(|message_id| !run.sent.contains(message_id))
            .filter(|message_id| {
                !report.failed_to_deliver.iter().any(|failure| {
                    &failure.counterparty == counterparty
                        && failure.outbound_message_id == Some(*message_id)
                })
            })
            .collect::<Vec<_>>();
        unsent.sort_unstable();
        for message_id in unsent {
            let status = statuses.get(&message_id);
            // A list still queued behind a pending handshake is sent once the
            // link reaches `Linked`, so it is not a delivery failure yet.
            let awaits_handshake = handshake_pending
                && matches!(
                    status,
                    Some(
                        OutboundPrivateMessageStatus::Pending
                            | OutboundPrivateMessageStatus::Sending
                            | OutboundPrivateMessageStatus::Failed
                            | OutboundPrivateMessageStatus::RecoveryRequired
                    )
                );
            if awaits_handshake {
                continue;
            }
            // Fail closed: a tracked list without a record was not sent.
            if let Some(error) = unsent_private_list_error(message_id, status, run) {
                report
                    .failed_to_deliver
                    .push(PrivatePaymentListDeliveryFailure {
                        counterparty: counterparty.clone(),
                        outbound_message_id: Some(message_id),
                        reservation_id: None,
                        error,
                    });
            }
        }
        Ok(())
    }

    async fn ensure_private_list_queue_allowed(&self, counterparty: &PubkyPublicKey) -> Result<()> {
        let (session_access, identity, readiness) = self
            .load_session_access_and_refresh_identity_with(|tx| {
                Ok(Self::private_queue_readiness_in_transaction(
                    tx,
                    counterparty,
                ))
            })
            .await?;
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
        readiness
            .expect("active session loads queue readiness")
            .map(|_| ())
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
                PrivatePaymentListQueuePolicy::Always,
            )
            .await;
            match result {
                Ok(record) => Ok(record),
                Err(err) => {
                    self.cancel_reservations_and_return_queue_error(
                        &cancellations,
                        &counterparty,
                        err,
                    )
                    .await
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

/// Return the Noise link ID of the counterparty's current link snapshot.
///
/// Corrupt snapshots return `None` so the outbound worker can mark the peer as
/// requiring recovery instead of a sent row being reused.
fn current_link_id(tx: &dyn StorageTransaction, counterparty: &PubkyPublicKey) -> Option<[u8; 32]> {
    let snapshot = tx
        .encrypted_link_state(counterparty)
        .and_then(|state| state.link_snapshot)?;
    paykit_lib::EncryptedLinkSnapshot::deserialize(&snapshot)
        .ok()
        .and_then(|snapshot| snapshot.link_id())
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
        acknowledged: Vec::new(),
        failed_to_queue: sync.failed,
        failed_to_deliver: Vec::new(),
    }
}

fn private_list_delivery_message_ids(
    report: &PrivatePaymentListDeliveryReport,
    counterparty: &PubkyPublicKey,
) -> HashSet<u64> {
    report
        .queued
        .iter()
        .chain(&report.cleared)
        .filter(|change| &change.counterparty == counterparty)
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

/// What one outbound run sent, for finding tracked lists it left unsent.
#[derive(Default)]
pub(super) struct OutboundListRun {
    sent: Vec<u64>,
    first_failure: Option<OutboundPrivateSendFailure>,
}

impl OutboundListRun {
    pub(super) fn from_send_report(report: &OutboundPrivateSendReport) -> Self {
        Self {
            sent: report.sent.clone(),
            first_failure: report.failed.first().cloned(),
        }
    }
}

/// Explain why a tracked Private Payment List was not sent, if it was not.
///
/// `None` means the outbound record is gone. Compaction can remove a superseded
/// list that was never attempted, so a missing record is reported as unsent.
fn unsent_private_list_error(
    message_id: u64,
    status: Option<&OutboundPrivateMessageStatus>,
    run: &OutboundListRun,
) -> Option<String> {
    let Some(status) = status else {
        return Some(format!(
            "Private Payment List {message_id} was not sent and is no longer queued"
        ));
    };
    match status {
        OutboundPrivateMessageStatus::Sent => None,
        // A newer list for the same Paykit App, or that App's removal, replaced it.
        OutboundPrivateMessageStatus::Superseded => Some(format!(
            "Private Payment List {message_id} was superseded before it was sent"
        )),
        OutboundPrivateMessageStatus::Invalid => Some(format!(
            "Private Payment List {message_id} is invalid and will not be sent"
        )),
        OutboundPrivateMessageStatus::Pending
        | OutboundPrivateMessageStatus::Sending
        | OutboundPrivateMessageStatus::Failed
        | OutboundPrivateMessageStatus::RecoveryRequired => Some(match &run.first_failure {
            Some(failure) => format!(
                "Private Payment List {message_id} remains queued after message {} failed: {}",
                failure.outbound_message_id, failure.error,
            ),
            None => format!("Private Payment List {message_id} remains queued"),
        }),
    }
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
    fn test_private_list_delivery_reports_unsent_list_statuses() {
        let failed_run = OutboundListRun::from_send_report(&OutboundPrivateSendReport {
            attempted: vec![7],
            failed: vec![OutboundPrivateSendFailure {
                outbound_message_id: 7,
                error: "transport failed".into(),
            }],
            ..OutboundPrivateSendReport::default()
        });
        let idle_run = OutboundListRun::default();
        for status in [
            OutboundPrivateMessageStatus::Pending,
            OutboundPrivateMessageStatus::Sending,
            OutboundPrivateMessageStatus::Failed,
            OutboundPrivateMessageStatus::RecoveryRequired,
        ] {
            assert_eq!(
                unsent_private_list_error(8, Some(&status), &idle_run).as_deref(),
                Some("Private Payment List 8 remains queued")
            );
            assert_eq!(
                unsent_private_list_error(8, Some(&status), &failed_run).as_deref(),
                Some("Private Payment List 8 remains queued after message 7 failed: transport failed")
            );
        }
        assert!(unsent_private_list_error(
            8,
            Some(&OutboundPrivateMessageStatus::Superseded),
            &idle_run
        )
        .unwrap()
        .contains("superseded before it was sent"));
        assert!(unsent_private_list_error(
            8,
            Some(&OutboundPrivateMessageStatus::Invalid),
            &idle_run
        )
        .unwrap()
        .contains("will not be sent"));
        for run in [&idle_run, &failed_run] {
            assert_eq!(
                unsent_private_list_error(8, None, run).as_deref(),
                Some("Private Payment List 8 was not sent and is no longer queued")
            );
        }
    }

    #[test]
    fn test_private_list_delivery_does_not_report_sent_list() {
        assert_eq!(
            unsent_private_list_error(
                8,
                Some(&OutboundPrivateMessageStatus::Sent),
                &OutboundListRun::default()
            ),
            None
        );
    }

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
