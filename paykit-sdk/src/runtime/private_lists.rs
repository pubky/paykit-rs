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
