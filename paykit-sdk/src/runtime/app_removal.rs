use super::*;
use crate::domain::payment_requests::payment_request_records_from_transaction;

pub(super) async fn begin_paykit_app_removal<S>(
    storage: &S,
    app_id: &paykit_lib::PaykitAppId,
    now: DateTime<Utc>,
) -> Result<PaykitAppRemovalBlockers>
where
    S: StorageAdapter,
{
    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                let blockers = app_removal_blockers_in_transaction(tx, &app_id, now)?;
                if blockers.is_empty() {
                    tx.retire_paykit_app(app_id);
                }
                Ok(blockers)
            }
        })
        .await
}

pub(super) fn retire_app_outbound_private_messages(
    tx: &mut dyn StorageTransaction,
    app_id: &paykit_lib::PaykitAppId,
    now: DateTime<Utc>,
    expires_at: DateTime<Utc>,
) -> Result<Vec<PeerLinkOperationLease>> {
    let snapshot = tx.export_storage_state();
    let mut affected_counterparties = snapshot
        .outbound_private_messages
        .iter()
        .filter(|message| {
            message.app_id == *app_id
                && matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Pending
                        | OutboundPrivateMessageStatus::Sending
                        | OutboundPrivateMessageStatus::Failed
                        | OutboundPrivateMessageStatus::RecoveryRequired
                )
        })
        .map(|message| message.counterparty.clone())
        .collect::<HashSet<_>>();
    affected_counterparties.extend(
        snapshot
            .payment_endpoint_reservations
            .values()
            .filter(|reservation| reservation.app_id == *app_id)
            .map(|reservation| reservation.counterparty.clone()),
    );
    if affected_counterparties.iter().any(|counterparty| {
        snapshot
            .peer_link_operation_leases
            .get(counterparty)
            .is_some_and(|lease| lease.expires_at > now)
    }) {
        return Err(PaykitSdkError::Policy {
            context: "cannot remove Paykit app while private delivery is in progress".into(),
            source: None,
        });
    }
    let mut affected_counterparties = affected_counterparties.into_iter().collect::<Vec<_>>();
    affected_counterparties.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let leases = affected_counterparties
        .iter()
        .map(|counterparty| {
            tx.claim_peer_link_operation(counterparty, now, expires_at)
                .and_then(|lease| {
                    lease.ok_or_else(|| PaykitSdkError::Policy {
                        context: "cannot remove Paykit app while private delivery is in progress"
                            .into(),
                        source: None,
                    })
                })
        })
        .collect::<Result<Vec<_>>>()?;
    tx.retire_paykit_app(app_id.clone());
    let mut counterparties = snapshot
        .payment_endpoint_reservations
        .values()
        .filter(|reservation| reservation.app_id == *app_id)
        .map(|reservation| reservation.counterparty.clone())
        .collect::<HashSet<_>>();
    for mut message in snapshot
        .outbound_private_messages
        .into_iter()
        .filter(|message| {
            message.app_id == *app_id
                && matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Pending
                        | OutboundPrivateMessageStatus::Sending
                        | OutboundPrivateMessageStatus::Failed
                        | OutboundPrivateMessageStatus::RecoveryRequired
                )
        })
    {
        counterparties.insert(message.counterparty.clone());
        if message.kind == PrivateMessageKind::PrivatePaymentList.as_str() {
            message.status = OutboundPrivateMessageStatus::Superseded;
            message.last_error = None;
            message.sent_at = None;
        } else {
            message.status = OutboundPrivateMessageStatus::Invalid;
            message.last_error = Some("Paykit app was removed before delivery".into());
        }
        message.updated_at = now;
        tx.save_outbound_private_message(message)?;
    }
    debug_assert_eq!(counterparties.len(), leases.len());
    Ok(leases)
}

pub(super) async fn require_app_capability_downgrade_safe<S>(
    storage: &S,
    app_id: &paykit_lib::PaykitAppId,
    previous: paykit_lib::PaykitAppCapabilities,
    next: paykit_lib::PaykitAppCapabilities,
    now: DateTime<Utc>,
) -> Result<()>
where
    S: StorageAdapter,
{
    let disables_private = previous.private_payments && !next.private_payments;
    let disables_requests = previous.payment_requests && !next.payment_requests;
    let disables_receipts = previous.receipts && !next.receipts;
    if !disables_private && !disables_requests && !disables_receipts {
        return Ok(());
    }

    let snapshot = storage
        .transaction(|tx| Ok(tx.export_storage_state()))
        .await?;
    let active_status = |status: &OutboundPrivateMessageStatus| {
        matches!(
            status,
            OutboundPrivateMessageStatus::Pending
                | OutboundPrivateMessageStatus::Sending
                | OutboundPrivateMessageStatus::Failed
                | OutboundPrivateMessageStatus::RecoveryRequired
        )
    };

    let private_blocked = disables_private
        && (snapshot.outbound_private_messages.iter().any(|message| {
            message.app_id == *app_id
                && message.kind == PrivateMessageKind::PrivatePaymentList.as_str()
                && active_status(&message.status)
        }) || !counterparties_with_shared_private_payment_lists(
            &snapshot.outbound_private_messages,
            app_id,
        )?
        .is_empty());

    let request_kinds = [
        PrivateMessageKind::PaymentRequest,
        PrivateMessageKind::PaymentRequestAcceptance,
        PrivateMessageKind::PaymentRequestRejection,
        PrivateMessageKind::PaymentRequestCancellation,
        PrivateMessageKind::PaymentProof,
    ];
    let mut request_blocked = disables_requests
        && snapshot.outbound_private_messages.iter().any(|message| {
            message.app_id == *app_id
                && request_kinds
                    .iter()
                    .any(|kind| message.kind == kind.as_str())
                && active_status(&message.status)
        });
    if disables_requests && !request_blocked {
        let counterparties = snapshot
            .outbound_private_messages
            .iter()
            .map(|message| message.counterparty.clone())
            .chain(
                snapshot
                    .private_stream_items
                    .iter()
                    .map(|item| item.counterparty.clone()),
            )
            .collect::<HashSet<_>>();
        for counterparty in counterparties {
            if derive_payment_request_records(storage, &counterparty, now)
                .await?
                .iter()
                .any(|record| payment_request_record_blocks_app_removal(record, app_id))
            {
                request_blocked = true;
                break;
            }
        }
    }

    let outbound_statuses = snapshot
        .outbound_private_messages
        .iter()
        .map(|message| (message.outbound_message_id, &message.status))
        .collect::<HashMap<_, _>>();
    let receipt_blocked = disables_receipts
        && (snapshot.outbound_private_messages.iter().any(|message| {
            message.app_id == *app_id
                && message.kind == PrivateMessageKind::ReceiptAccess.as_str()
                && active_status(&message.status)
        }) || snapshot.receipt_issuance_records.values().any(|record| {
            record.app_id == *app_id
                && (record.status != ReceiptIssuanceStatus::AccessQueued
                    || record.outbound_message_id.is_none_or(|message_id| {
                        outbound_statuses
                            .get(&message_id)
                            .is_none_or(|status| **status != OutboundPrivateMessageStatus::Sent)
                    }))
        }));

    let mut blocked = Vec::new();
    if private_blocked {
        blocked.push("private_payments");
    }
    if request_blocked {
        blocked.push("payment_requests");
    }
    if receipt_blocked {
        blocked.push("receipts");
    }
    if blocked.is_empty() {
        return Ok(());
    }
    Err(PaykitSdkError::Policy {
        context: format!(
            "cannot disable Paykit app capability {} while app-owned work is still active",
            blocked.join(", ")
        ),
        source: None,
    })
}

pub(super) async fn stage_app_capability_update<S>(
    storage: &S,
    app_id: &paykit_lib::PaykitAppId,
    remote_previous: Option<paykit_lib::PaykitAppCapabilities>,
    next: paykit_lib::PaykitAppCapabilities,
    now: DateTime<Utc>,
) -> Result<
    Option<(
        paykit_lib::PaykitAppCapabilities,
        paykit_lib::PaykitAppCapabilities,
    )>,
>
where
    S: StorageAdapter,
{
    let staged_update = storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                let local_previous = tx.paykit_app_capabilities(&app_id);
                let registered = tx.paykit_app_is_registered(&app_id);
                let retired = tx.paykit_app_is_retired(&app_id);
                match (registered, retired, local_previous) {
                    (true, _, Some(previous)) | (false, true, Some(previous)) => {
                        let staged = capability_intersection(
                            capability_intersection(previous, next),
                            remote_previous.unwrap_or_else(no_paykit_app_capabilities),
                        );
                        tx.save_paykit_app_capabilities(&app_id, staged);
                        Ok(Some((previous, staged)))
                    }
                    (false, _, None) => Ok(None),
                    _ => Err(PaykitSdkError::Storage {
                        context: "Paykit app capability state is inconsistent".into(),
                        source: None,
                    }),
                }
            }
        })
        .await?;

    let local_previous = staged_update.map(|(previous, _)| previous);

    let previous = match (local_previous, remote_previous) {
        (Some(local), Some(remote)) => paykit_lib::PaykitAppCapabilities {
            private_payments: local.private_payments || remote.private_payments,
            payment_requests: local.payment_requests || remote.payment_requests,
            receipts: local.receipts || remote.receipts,
            outgoing_payments: local.outgoing_payments || remote.outgoing_payments,
        },
        (Some(previous), None) | (None, Some(previous)) => previous,
        (None, None) => paykit_lib::PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    };
    if let Err(err) =
        require_app_capability_downgrade_safe(storage, app_id, previous, next, now).await
    {
        if let Some((local_previous, staged)) = staged_update {
            restore_app_capabilities(storage, app_id, staged, local_previous).await?;
        }
        return Err(err);
    }
    Ok(staged_update)
}

fn capability_intersection(
    left: paykit_lib::PaykitAppCapabilities,
    right: paykit_lib::PaykitAppCapabilities,
) -> paykit_lib::PaykitAppCapabilities {
    paykit_lib::PaykitAppCapabilities {
        private_payments: left.private_payments && right.private_payments,
        payment_requests: left.payment_requests && right.payment_requests,
        receipts: left.receipts && right.receipts,
        outgoing_payments: left.outgoing_payments && right.outgoing_payments,
    }
}

fn no_paykit_app_capabilities() -> paykit_lib::PaykitAppCapabilities {
    paykit_lib::PaykitAppCapabilities {
        private_payments: false,
        payment_requests: false,
        receipts: false,
        outgoing_payments: false,
    }
}

pub(super) async fn restore_app_capabilities<S>(
    storage: &S,
    app_id: &paykit_lib::PaykitAppId,
    staged: paykit_lib::PaykitAppCapabilities,
    previous: paykit_lib::PaykitAppCapabilities,
) -> Result<()>
where
    S: StorageAdapter,
{
    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| {
                if tx.paykit_app_capabilities(&app_id) == Some(staged) {
                    tx.save_paykit_app_capabilities(&app_id, previous);
                }
                Ok(())
            }
        })
        .await
}

/// Work that prevents safe removal of one Paykit application.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaykitAppRemovalBlockers {
    /// Active app-owned Payment Requests.
    pub active_payment_requests: usize,
    /// App-owned private event messages that have not been delivered.
    pub undelivered_private_events: usize,
    /// Receipt issuance records whose Receipt Access was not delivered.
    pub incomplete_receipt_issuances: usize,
    /// Counterparties that still have a non-empty app-owned Private Payment List.
    pub shared_private_payment_lists: usize,
}

impl PaykitAppRemovalBlockers {
    pub(super) fn is_empty(&self) -> bool {
        self.active_payment_requests == 0
            && self.undelivered_private_events == 0
            && self.incomplete_receipt_issuances == 0
            && self.shared_private_payment_lists == 0
    }
}

pub(super) async fn app_removal_blockers<S>(
    storage: &S,
    app_id: &paykit_lib::PaykitAppId,
    now: DateTime<Utc>,
) -> Result<PaykitAppRemovalBlockers>
where
    S: StorageAdapter,
{
    storage
        .transaction({
            let app_id = app_id.clone();
            move |tx| app_removal_blockers_in_transaction(tx, &app_id, now)
        })
        .await
}

fn app_removal_blockers_in_transaction(
    tx: &dyn StorageTransaction,
    app_id: &paykit_lib::PaykitAppId,
    now: DateTime<Utc>,
) -> Result<PaykitAppRemovalBlockers> {
    let snapshot = tx.export_storage_state();
    let counterparties = snapshot
        .outbound_private_messages
        .iter()
        .map(|message| message.counterparty.clone())
        .chain(
            snapshot
                .private_stream_items
                .iter()
                .map(|item| item.counterparty.clone()),
        )
        .collect::<HashSet<_>>();

    let mut active_payment_requests = 0;
    for counterparty in counterparties {
        active_payment_requests +=
            payment_request_records_from_transaction(tx, &counterparty, now)?
                .into_iter()
                .filter(|record| payment_request_record_blocks_app_removal(record, app_id))
                .count();
    }

    let undelivered_private_events = snapshot
        .outbound_private_messages
        .iter()
        .filter(|message| {
            message.app_id == *app_id
                && message.kind != PrivateMessageKind::PrivatePaymentList.as_str()
                && matches!(
                    message.status,
                    OutboundPrivateMessageStatus::Pending
                        | OutboundPrivateMessageStatus::Sending
                        | OutboundPrivateMessageStatus::Failed
                        | OutboundPrivateMessageStatus::RecoveryRequired
                )
        })
        .count();
    let outbound_statuses = snapshot
        .outbound_private_messages
        .iter()
        .map(|message| (message.outbound_message_id, &message.status))
        .collect::<HashMap<_, _>>();
    let incomplete_receipt_issuances = snapshot
        .receipt_issuance_records
        .values()
        .filter(|record| {
            record.app_id == *app_id
                && (record.status != ReceiptIssuanceStatus::AccessQueued
                    || record
                        .outbound_message_id
                        .is_none_or(|outbound_message_id| {
                            outbound_statuses
                                .get(&outbound_message_id)
                                .is_none_or(|status| **status != OutboundPrivateMessageStatus::Sent)
                        }))
        })
        .count();

    let shared_private_payment_lists = counterparties_with_shared_private_payment_lists(
        &snapshot.outbound_private_messages,
        app_id,
    )?
    .len();

    Ok(PaykitAppRemovalBlockers {
        active_payment_requests,
        undelivered_private_events,
        incomplete_receipt_issuances,
        shared_private_payment_lists,
    })
}

pub(super) fn detach_shared_app_reservations(
    tx: &mut dyn StorageTransaction,
    app_id: &paykit_lib::PaykitAppId,
) -> Result<usize> {
    let snapshot = tx.export_storage_state();
    let outbound_by_id = snapshot
        .outbound_private_messages
        .iter()
        .map(|message| (message.outbound_message_id, message))
        .collect::<HashMap<_, _>>();
    for reservation in snapshot
        .payment_endpoint_reservations
        .values()
        .filter(|reservation| reservation.app_id == *app_id)
    {
        let Some(message) = outbound_by_id.get(&reservation.outbound_message_id) else {
            continue;
        };
        let was_shared = message.status == OutboundPrivateMessageStatus::Sent
            || (message.status == OutboundPrivateMessageStatus::Superseded
                && message.last_attempt_at.is_some());
        if was_shared {
            // A shared receiving detail may still be used by the counterparty.
            // Stop SDK tracking without telling the adapter it is reusable.
            tx.remove_payment_endpoint_reservation(
                &reservation.counterparty,
                app_id,
                &reservation.reservation_id,
            );
        }
    }
    Ok(tx
        .export_storage_state()
        .payment_endpoint_reservations
        .values()
        .filter(|reservation| reservation.app_id == *app_id)
        .count())
}
