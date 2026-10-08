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
        mut err: PaykitSdkError,
    ) -> Result<T> {
        if self
            .cancel_reservations_after_queue_error(cancellations, counterparty)
            .await
            .is_err()
        {
            let context = match &mut err {
                PaykitSdkError::ConcurrentUpdate { context, .. }
                | PaykitSdkError::SharedStateBusy { context, .. }
                | PaykitSdkError::Storage { context, .. }
                | PaykitSdkError::Identity { context, .. }
                | PaykitSdkError::Transport { context, .. }
                | PaykitSdkError::NotFound { context, .. }
                | PaykitSdkError::Protocol { context, .. }
                | PaykitSdkError::Policy { context, .. }
                | PaykitSdkError::PaymentAdapter { context, .. }
                | PaykitSdkError::RecoveryRequired { context, .. }
                | PaykitSdkError::LinkObservation { context } => context,
            };
            context.push_str("; reservation cleanup also failed");
        }
        Err(err)
    }

    pub(super) async fn cancel_terminal_private_list_reservations(
        &self,
        counterparty: &PubkyPublicKey,
        lease: Option<&PeerLinkOperationLease>,
        app_lease: Option<&PaykitAppOperationLease>,
    ) -> Vec<ReservationCleanupFailure> {
        let cancellations = match terminal_private_list_reservation_cancellations(
            &self.storage,
            counterparty,
        )
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
        self.cancel_reservation_records(cancellations, lease, app_lease)
            .await
    }

    pub(super) async fn cancel_reservation_records(
        &self,
        cancellations: Vec<PaymentEndpointReservationCancellationRecord>,
        lease: Option<&PeerLinkOperationLease>,
        app_lease: Option<&PaykitAppOperationLease>,
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
                    app_lease,
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
            if let Some(app_lease) = app_lease {
                if let Err(err) = self.require_paykit_app_operation_lease(app_lease).await {
                    failures.push(ReservationCleanupFailure {
                        reservation_id: Some(cancellation.reservation_id),
                        error: err.to_string(),
                    });
                    continue;
                }
            }
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
                            let app_lease = app_lease.cloned();
                            move |tx| {
                                if let Some(app_lease) = app_lease.as_ref() {
                                    crate::storage::require_paykit_app_operation_lease(
                                        tx, app_lease,
                                    )?;
                                }
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
        app_lease: Option<&PaykitAppOperationLease>,
    ) -> Result<Option<DateTime<Utc>>> {
        let claim_timeout = ChronoDuration::from_std(RESERVATION_CANCELLATION_CLAIM_TIMEOUT)
            .expect("fixed reservation cancellation timeout must fit chrono duration");
        self.storage
            .transaction({
                let cancellation = cancellation.clone();
                let app_id = app_id.clone();
                let lease = lease.cloned();
                let app_lease = app_lease.cloned();
                move |tx| {
                    let now = self.clock.now();
                    let stale_before = now - claim_timeout;
                    if let Some(app_lease) = app_lease.as_ref() {
                        crate::storage::require_paykit_app_operation_lease(tx, app_lease)?;
                    }
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
