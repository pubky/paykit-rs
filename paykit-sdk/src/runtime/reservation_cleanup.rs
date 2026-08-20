use super::*;

impl<S, K, P, C> PaykitSdk<S, K, P, C>
where
    S: StorageAdapter,
    K: PubkySessionProvider,
    P: PaymentAdapter,
    C: Clock,
{
    pub(super) async fn cancel_reservations_after_queue_error(
        &self,
        cancellations: &[PrivatePaymentEndpointReservationCancellation],
        counterparty: &PubkyPublicKey,
    ) -> Result<()> {
        let mut first_cancellation_error = None;
        for cancellation in cancellations {
            let app_id = self.config.app_id.clone();
            let can_cancel = self
                .storage
                .transaction({
                    let counterparty = counterparty.clone();
                    let cancellation = cancellation.clone();
                    move |tx| {
                        Ok(!tx
                            .payment_endpoint_reservation(
                                &counterparty,
                                &app_id,
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
                .cancel_private_receiving_detail_reservation(cancellation)
                .await
            {
                if first_cancellation_error.is_none() {
                    first_cancellation_error = Some(err);
                }
            }
        }
        match first_cancellation_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    pub(super) async fn cancel_reservations_and_return_queue_error<T>(
        &self,
        cancellations: &[PrivatePaymentEndpointReservationCancellation],
        counterparty: &PubkyPublicKey,
        err: PaykitSdkError,
    ) -> Result<T> {
        self.cancel_reservations_after_queue_error(cancellations, counterparty)
            .await?;
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
            if cancellation_record.app_id != self.config.app_id {
                continue;
            }
            let app_id = cancellation_record.app_id;
            let cancellation = cancellation_record.cancellation;
            let cancellation_claimed_at = match self
                .claim_reservation_cancellation(
                    &cancellation,
                    &app_id,
                    cancellation_record.outbound_message_id,
                    lease,
                    self.clock.now(),
                )
                .await
            {
                Ok(Some(claimed_at)) => claimed_at,
                Ok(None) => continue,
                Err(err) => {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: Some(cancellation.reservation_id),
                        error: err.to_string(),
                    });
                    continue;
                }
            };
            match self
                .payment
                .cancel_private_receiving_detail_reservation(&cancellation)
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
                                        &app_id,
                                        &cancellation.reservation_id,
                                    )
                                    .is_some_and(|record| {
                                        record.outbound_message_id == outbound_message_id
                                            && record.identifier == cancellation.identifier
                                            && record.payload_hash == cancellation.payload_hash
                                            && record.cancellation_started_at
                                                == Some(cancellation_claimed_at)
                                    })
                                {
                                    tx.remove_payment_endpoint_reservation(
                                        &cancellation.counterparty,
                                        &app_id,
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
        cancellation: &PrivatePaymentEndpointReservationCancellation,
        app_id: &paykit_lib::PaykitAppId,
        outbound_message_id: u64,
        lease: Option<&PeerLinkOperationLease>,
        now: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>> {
        let claim_timeout = ChronoDuration::from_std(RESERVATION_CANCELLATION_CLAIM_TIMEOUT)
            .expect("fixed reservation cancellation timeout must fit chrono duration");
        let stale_before = now - claim_timeout;
        self.storage
            .transaction({
                let cancellation = cancellation.clone();
                let app_id = app_id.clone();
                let lease = lease.cloned();
                move |tx| {
                    if let Some(lease) = lease.as_ref() {
                        crate::storage::require_peer_link_operation_lease(tx, lease)?;
                    }
                    let Some(mut record) = tx.payment_endpoint_reservation(
                        &cancellation.counterparty,
                        &app_id,
                        &cancellation.reservation_id,
                    ) else {
                        return Ok(None);
                    };
                    if record.outbound_message_id != outbound_message_id
                        || record.identifier != cancellation.identifier
                        || record.payload_hash != cancellation.payload_hash
                    {
                        return Ok(None);
                    }
                    if record
                        .cancellation_started_at
                        .is_some_and(|started_at| started_at > stale_before)
                    {
                        return Ok(None);
                    }
                    record.cancellation_started_at = Some(now);
                    tx.save_payment_endpoint_reservation(record);
                    Ok(Some(now))
                }
            })
            .await
    }
}
