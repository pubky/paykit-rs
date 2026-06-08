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
        self.ensure_private_workflows_enabled("private Payment List access")?;
        let (session_access, identity) = self.load_session_access_and_refresh_identity().await?;
        if identity.public_key.is_none() {
            return Ok(None);
        }
        self.ensure_peer_not_blocked(counterparty).await?;
        if session_access
            .as_ref()
            .is_some_and(PubkySessionAccess::private_link_capable)
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
            self.ensure_private_outbound_ready(
                &counterparty,
                "private Payment List sharing is disabled",
            )
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
        let request = PaymentEndpointReservationRequest {
            counterparty: counterparty.clone(),
        };
        if let Some(reservations) = self.payment.reserve_receiving_details(&request).await? {
            let releases = reservations
                .iter()
                .map(|reservation| reservation_release(&counterparty, reservation))
                .collect::<Vec<_>>();
            let now = self.clock.now();
            let result = queue_private_payment_list_with_reservations_with_link_lease(
                &self.storage,
                &request,
                reservations,
                now,
                lease,
            )
            .await;
            match result {
                Ok(record) => Ok(record),
                Err(err) => {
                    if let Err(release_err) = self
                        .release_reservations_after_queue_error(&releases, &counterparty)
                        .await
                    {
                        return Err(PaykitSdkError::Policy(format!(
                            "failed to queue reserved receiving details: {err}; reservation cleanup also failed: {release_err}"
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

    async fn release_reservations_after_queue_error(
        &self,
        releases: &[PaymentEndpointReservationRelease],
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let mut release_errors = Vec::new();
        for release in releases {
            let can_release = self
                .storage
                .transaction({
                    let counterparty = counterparty.clone();
                    let release = release.clone();
                    move |tx| {
                        Ok(!tx
                            .payment_endpoint_reservation(&counterparty, &release.reservation_id)
                            .is_some_and(|record| {
                                record.reservation_id == release.reservation_id
                                    && record.counterparty == release.counterparty
                                    && record.identifier == release.identifier
                                    && record.payload_hash == release.payload_hash
                            }))
                    }
                })
                .await?;
            if !can_release {
                continue;
            }
            if let Err(err) = self
                .payment
                .release_receiving_detail_reservation(release)
                .await
            {
                release_errors.push(format!("{}: {err}", release.reservation_id));
            }
        }
        if release_errors.is_empty() {
            Ok(())
        } else {
            Err(PaykitSdkError::Policy(format!(
                "failed to release reserved receiving details: {}",
                release_errors.join("; ")
            )))
        }
    }

    pub(super) async fn release_unattempted_superseded_reservations(
        &self,
        counterparty: &PubkyPublicKey,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let releases =
            match unattempted_superseded_reservation_releases(&self.storage, counterparty).await {
                Ok(releases) => releases,
                Err(err) => {
                    return vec![ReservationCleanupFailure {
                        reservation_id: None,
                        error: err.to_string(),
                    }];
                }
            };
        self.release_reservation_records(releases, lease).await
    }

    pub(super) async fn release_terminal_private_list_reservations(
        &self,
        counterparty: &PubkyPublicKey,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let mut failures = self
            .release_unattempted_superseded_reservations(counterparty, lease)
            .await;
        let releases =
            match invalid_private_list_reservation_releases(&self.storage, counterparty).await {
                Ok(releases) => releases,
                Err(err) => {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: None,
                        error: err.to_string(),
                    });
                    return failures;
                }
            };
        failures.extend(self.release_reservation_records(releases, lease).await);
        failures
    }

    pub(super) async fn release_reservation_records(
        &self,
        releases: Vec<PaymentEndpointReservationReleaseRecord>,
        lease: Option<&PeerLinkOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let mut failures = Vec::new();
        for release_record in releases {
            let release = release_record.release;
            match self
                .claim_reservation_release(
                    &release,
                    release_record.outbound_message_id,
                    lease,
                    self.clock.now(),
                )
                .await
            {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: Some(release.reservation_id),
                        error: err.to_string(),
                    });
                    continue;
                }
            }
            match self
                .payment
                .release_receiving_detail_reservation(&release)
                .await
            {
                Ok(()) => {
                    if let Err(err) = self
                        .storage
                        .transaction({
                            let release = release.clone();
                            let outbound_message_id = release_record.outbound_message_id;
                            move |tx| {
                                if tx
                                    .payment_endpoint_reservation(
                                        &release.counterparty,
                                        &release.reservation_id,
                                    )
                                    .is_some_and(|record| {
                                        record.outbound_message_id == outbound_message_id
                                            && record.identifier == release.identifier
                                            && record.payload_hash == release.payload_hash
                                            && record.release_started_at.is_some()
                                    })
                                {
                                    tx.remove_payment_endpoint_reservation(
                                        &release.counterparty,
                                        &release.reservation_id,
                                    );
                                }
                                Ok(())
                            }
                        })
                        .await
                    {
                        failures.push(ReservationCleanupFailure {
                            reservation_id: Some(release.reservation_id.clone()),
                            error: err.to_string(),
                        });
                    }
                }
                Err(err) => failures.push(ReservationCleanupFailure {
                    reservation_id: Some(release.reservation_id),
                    error: err.to_string(),
                }),
            }
        }
        failures
    }

    async fn claim_reservation_release(
        &self,
        release: &PaymentEndpointReservationRelease,
        outbound_message_id: u64,
        lease: Option<&PeerLinkOperationLease>,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        self.storage
            .transaction({
                let release = release.clone();
                let lease = lease.cloned();
                move |tx| {
                    if let Some(lease) = lease.as_ref() {
                        crate::storage::require_peer_link_operation_lease(tx, lease)?;
                    }
                    let Some(mut record) = tx.payment_endpoint_reservation(
                        &release.counterparty,
                        &release.reservation_id,
                    ) else {
                        return Ok(false);
                    };
                    if record.outbound_message_id != outbound_message_id
                        || record.identifier != release.identifier
                        || record.payload_hash != release.payload_hash
                    {
                        return Ok(false);
                    }
                    record.release_started_at = Some(now);
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

fn reservation_release(
    counterparty: &PubkyPublicKey,
    reservation: &PaymentEndpointReservation,
) -> PaymentEndpointReservationRelease {
    PaymentEndpointReservationRelease {
        reservation_id: reservation.reservation_id.clone(),
        counterparty: counterparty.clone(),
        identifier: reservation.receiving_detail.identifier.clone(),
        payload_hash: reservation_payload_hash(&reservation.receiving_detail.payload),
        attribution: reservation.attribution.clone(),
    }
}
