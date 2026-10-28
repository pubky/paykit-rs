mod collections;
mod links;
mod private_stream;
mod receipts;
mod records;

pub(in crate::backup) use collections::*;
pub(in crate::backup) use links::*;
pub(in crate::backup) use private_stream::*;
pub(in crate::backup) use receipts::*;
pub(in crate::backup) use records::*;

use super::*;

/// Validate a decoded live SDK storage snapshot without repairing it.
pub fn validate_storage_state(state: &StorageState) -> Result<()> {
    validate_live_identity(state)?;
    if let Some(accounting) = &state.allowance_accounting {
        crate::domain::allowance_accounting::validate_accounting(accounting)?;
        let identity = state
            .identity_state
            .as_ref()
            .and_then(|identity| identity.public_key.as_ref())
            .ok_or_else(|| invalid_live_state("Accounting requires a payer identity"))?;
        crate::domain::allowance_accounting::payer_scope(accounting, identity)?;
    }
    validate_live_record_keys(state)?;
    validate_live_app_state(state)?;
    validate_live_link_state(state)?;
    validate_private_intake_state(state)?;

    let outbound_private_messages =
        unique_outbound_messages(state.outbound_private_messages.clone())?;
    require_original_order(
        &outbound_private_messages,
        &state.outbound_private_messages,
        "outbound Private Application Message records are not ordered by id",
    )?;
    let private_stream_items = unique_private_stream_items(state.private_stream_items.clone())?;
    require_original_order(
        &private_stream_items,
        &state.private_stream_items,
        "private stream item records are not ordered by id",
    )?;
    validate_live_counters(state)?;

    validate_linked_peer_records(&state.linked_peers)?;
    validate_contact_records(&state.contact_records)?;
    validate_public_endpoint_records(&state.public_endpoint_records)?;
    validate_encrypted_link_snapshots(&state.encrypted_link_states)?;
    validate_outbound_private_messages(&outbound_private_messages)?;
    validate_payment_endpoint_reservations(
        &state.payment_endpoint_reservations,
        &outbound_private_messages,
    )?;
    validate_private_stream_items(&private_stream_items)?;
    validate_event_dedup_records(&state.event_dedup_records, &private_stream_items)?;
    validate_payment_request_execution_claims(state)?;
    validate_receipt_access_records(&state.receipt_access_records, &private_stream_items)?;
    validate_required_private_stream_indexes(
        &private_stream_items,
        &state.event_dedup_records,
        &state.receipt_access_records,
    )?;
    let expected_receipt_recipient = state
        .identity_state
        .as_ref()
        .and_then(|identity| identity.public_key.as_ref());
    validate_receipt_records(
        &state.receipt_records,
        &state.receipt_access_records,
        expected_receipt_recipient,
    )?;
    validate_receipt_issuance_records(&state.receipt_issuance_records, &outbound_private_messages)?;
    validate_retired_app_outbound_messages(&state.retired_paykit_apps, &outbound_private_messages)?;
    validate_retired_app_private_payment_lists(
        &state.retired_paykit_apps,
        &outbound_private_messages,
    )?;
    validate_retired_app_payment_requests(
        &state.retired_paykit_apps,
        &private_stream_items,
        &outbound_private_messages,
        &state.event_dedup_records,
    )?;
    validate_retired_app_receipt_issuance(
        &state.retired_paykit_apps,
        &state.receipt_issuance_records,
        &outbound_private_messages,
    )?;
    Ok(())
}

fn validate_payment_request_execution_claims(state: &StorageState) -> Result<()> {
    for claim in state.payment_request_execution_claims.values() {
        let records = derive_payment_request_records_from_parts(
            claim.counterparty.clone(),
            state
                .private_stream_items
                .iter()
                .filter(|item| item.counterparty == claim.counterparty)
                .cloned()
                .collect(),
            state
                .outbound_private_messages
                .iter()
                .filter(|message| message.counterparty == claim.counterparty)
                .cloned()
                .collect(),
            state
                .event_dedup_records
                .iter()
                .filter(|((counterparty, _), _)| counterparty == &claim.counterparty)
                .map(|((_, event_id), record)| (event_id.clone(), record.clone()))
                .collect(),
            claim.claimed_at,
        )?;
        let record = records
            .iter()
            .find(|record| record.payment_request_id == claim.payment_request_id)
            .ok_or_else(|| {
                invalid_live_state("Payment Request execution claim has no matching request")
            })?;
        let unresolved_payment = state
            .allowance_accounting
            .as_ref()
            .is_some_and(|accounting| {
                accounting.history.occurrences.iter().any(|occurrence| {
                    occurrence.key.request.counterparty == claim.counterparty
                        && occurrence.key.request.payment_request_id == claim.payment_request_id
                        && occurrence.attempts.iter().any(|attempt| {
                            matches!(
                                attempt.status,
                                crate::PaymentExecutionStatus::Prepared
                                    | crate::PaymentExecutionStatus::Submitted
                                    | crate::PaymentExecutionStatus::Unknown
                            )
                        })
                })
            });
        // Conflicting proposals can erase the derived role. Retain only claims backed by
        // unresolved accounting for the validated payer identity; this grants no execution.
        let unresolved_conflict = record.local_role.is_none()
            && record.state == PaymentRequestLifecycleState::InvalidConflict
            && unresolved_payment;
        if record.local_role != Some(PaymentRequestLocalRole::Payer) && !unresolved_conflict {
            return Err(invalid_live_state(
                "Payment Request execution claim belongs to the non-payer side",
            ));
        }
        if !unresolved_payment
            && !crate::domain::payment_requests::request_has_unreported_successful_payment(
                state.allowance_accounting.as_ref(),
                record,
            )
            && !matches!(
                record.state,
                PaymentRequestLifecycleState::Proposed
                    | PaymentRequestLifecycleState::Accepted
                    | PaymentRequestLifecycleState::ActiveRecurring
                    | PaymentRequestLifecycleState::RecoveryRequired
            )
        {
            return Err(invalid_live_state(
                "Payment Request execution claim has no unresolved payment work",
            ));
        }
    }
    Ok(())
}

fn validate_live_identity(state: &StorageState) -> Result<()> {
    let has_public_identity = state
        .identity_state
        .as_ref()
        .and_then(|identity| identity.public_key.as_ref())
        .is_some();
    if !has_public_identity && has_identity_scoped_live_state(state) {
        return Err(invalid_live_state(
            "SDK storage has identity-scoped state without a public identity",
        ));
    }
    Ok(())
}

fn has_identity_scoped_live_state(state: &StorageState) -> bool {
    state.allowance_accounting.is_some()
        || state.paykit_noise_public_key.is_some()
        || !state.linked_peers.is_empty()
        || !state.contact_records.is_empty()
        || !state.authorized_paykit_apps.is_empty()
        || !state.registered_paykit_apps.is_empty()
        || !state.registered_paykit_app_capabilities.is_empty()
        || !state.retired_paykit_apps.is_empty()
        || !state.public_endpoint_records.is_empty()
        || !state.payment_endpoint_reservations.is_empty()
        || !state.encrypted_link_states.is_empty()
        || !state.peer_link_operation_leases.is_empty()
        || !state.paykit_app_operation_leases.is_empty()
        || state.private_intake_supervisor_lease.is_some()
        || !state.payment_request_execution_claims.is_empty()
        || !state.outbound_private_messages.is_empty()
        || !state.private_stream_items.is_empty()
        || !state.event_dedup_records.is_empty()
        || !state.receipt_access_records.is_empty()
        || !state.receipt_records.is_empty()
        || !state.receipt_issuance_records.is_empty()
        || !state.private_intake_cursors.is_empty()
        || !state.private_intake_peers.is_empty()
}

fn validate_live_record_keys(state: &StorageState) -> Result<()> {
    validate_record_keys(
        &state.linked_peers,
        |key, record| key == &record.counterparty,
        "Linked Peer record key does not match its counterparty",
    )?;
    validate_record_keys(
        &state.contact_records,
        |key, record| key == &record.public_key,
        "Contact Record key does not match its public key",
    )?;
    validate_record_keys(
        &state.public_endpoint_records,
        |(app_id, identifier), record| app_id == &record.app_id && identifier == &record.identifier,
        "public Payment Endpoint key does not match its record",
    )?;
    validate_record_keys(
        &state.payment_endpoint_reservations,
        |(counterparty, app_id, reservation_id), record| {
            counterparty == &record.counterparty
                && app_id == &record.app_id
                && reservation_id == &record.reservation_id
        },
        "Payment Endpoint Reservation key does not match its record",
    )?;
    validate_record_keys(
        &state.encrypted_link_states,
        |key, record| key == &record.counterparty,
        "Encrypted Link state key does not match its counterparty",
    )?;
    validate_record_keys(
        &state.peer_link_operation_leases,
        |key, record| key == &record.counterparty,
        "peer link operation lease key does not match its counterparty",
    )?;
    validate_record_keys(
        &state.paykit_app_operation_leases,
        |key, record| key == &record.app_id,
        "Paykit App operation lease key does not match its app",
    )?;
    validate_record_keys(
        &state.payment_request_execution_claims,
        |(counterparty, payment_request_id), record| {
            counterparty == &record.counterparty && payment_request_id == &record.payment_request_id
        },
        "Payment Request execution claim key does not match its record",
    )?;
    validate_record_keys(
        &state.event_dedup_records,
        |(counterparty, event_id), record| {
            counterparty == &record.counterparty && event_id == &record.event_id
        },
        "Event dedupe key does not match its record",
    )?;
    validate_record_keys(
        &state.receipt_access_records,
        |(counterparty, event_id), record| {
            counterparty == &record.counterparty && event_id == &record.event_id
        },
        "Receipt Access key does not match its record",
    )?;
    validate_record_keys(
        &state.receipt_records,
        |(issuer, receipt_id), record| issuer == &record.issuer && receipt_id == &record.receipt_id,
        "Receipt key does not match its record",
    )?;
    validate_record_keys(
        &state.receipt_issuance_records,
        |(counterparty, receipt_id), record| {
            counterparty == &record.counterparty && receipt_id == &record.receipt_id
        },
        "Receipt issuance key does not match its record",
    )?;
    validate_record_keys(
        &state.private_intake_cursors,
        |key, record| key == &record.subscribed_user,
        "private intake cursor key does not match its subscribed user",
    )?;
    validate_record_keys(
        &state.private_intake_peers,
        |key, record| key == &record.counterparty,
        "private intake peer key does not match its counterparty",
    )?;
    if state
        .private_intake_supervisor_lease
        .as_ref()
        .is_some_and(|lease| lease.lease_id >= state.next_private_intake_supervisor_lease_id)
    {
        return Err(invalid_live_state(
            "private intake supervisor lease id is outside its allocated range",
        ));
    }
    Ok(())
}

fn validate_record_keys<K, V>(
    records: &HashMap<K, V>,
    key_matches_record: impl Fn(&K, &V) -> bool,
    context: &'static str,
) -> Result<()> {
    if records
        .iter()
        .any(|(key, record)| !key_matches_record(key, record))
    {
        return Err(invalid_live_state(context));
    }
    Ok(())
}

fn validate_live_app_state(state: &StorageState) -> Result<()> {
    if !state
        .registered_paykit_apps
        .is_disjoint(&state.retired_paykit_apps)
    {
        return Err(invalid_live_state(
            "a Paykit App cannot be both registered and retired",
        ));
    }
    if state.registered_paykit_apps.iter().any(|app_id| {
        !state
            .registered_paykit_app_capabilities
            .contains_key(app_id)
    }) {
        return Err(invalid_live_state(
            "a registered Paykit App has no persisted capabilities",
        ));
    }
    if state
        .registered_paykit_app_capabilities
        .keys()
        .any(|app_id| {
            !state.registered_paykit_apps.contains(app_id)
                && !state.retired_paykit_apps.contains(app_id)
        })
    {
        return Err(invalid_live_state(
            "Paykit App capabilities belong to neither a registered nor retired app",
        ));
    }
    if state
        .payment_request_execution_claims
        .values()
        .any(|claim| state.retired_paykit_apps.contains(&claim.app_id))
    {
        return Err(invalid_live_state(
            "Payment Request execution claim belongs to a retired app",
        ));
    }
    Ok(())
}

fn validate_live_link_state(state: &StorageState) -> Result<()> {
    let mut lease_ids = HashSet::new();
    for lease in state.peer_link_operation_leases.values() {
        if !lease_ids.insert(lease.lease_id) {
            return Err(invalid_live_state(
                "peer link operation lease id is duplicated",
            ));
        }
        if lease.lease_id >= state.next_peer_link_operation_lease_id {
            return Err(invalid_live_state(
                "peer link operation lease id is outside its allocated range",
            ));
        }
        if lease.expires_at <= lease.claimed_at {
            return Err(invalid_live_state(
                "peer link operation lease has an invalid validity interval",
            ));
        }
    }

    let mut app_lease_ids = HashSet::new();
    for lease in state.paykit_app_operation_leases.values() {
        if !app_lease_ids.insert(lease.lease_id) {
            return Err(invalid_live_state(
                "Paykit App operation lease id is duplicated",
            ));
        }
        if lease.lease_id >= state.next_paykit_app_operation_lease_id {
            return Err(invalid_live_state(
                "Paykit App operation lease id is outside its allocated range",
            ));
        }
        if lease.expires_at <= lease.claimed_at {
            return Err(invalid_live_state(
                "Paykit App operation lease has an invalid validity interval",
            ));
        }
    }

    for record in state.encrypted_link_states.values() {
        if record.link_snapshot.is_some()
            && (record.handshake_snapshot.is_some() || record.handshake_role.is_some())
        {
            return Err(invalid_live_state(
                "Encrypted Link state mixes active link and handshake snapshots",
            ));
        }
        if record.handshake_snapshot.is_some() != record.handshake_role.is_some() {
            return Err(invalid_live_state(
                "Encrypted Link handshake snapshot and role must be stored together",
            ));
        }
    }
    Ok(())
}

fn validate_private_intake_state(state: &StorageState) -> Result<()> {
    match (
        state.private_intake_supervisor_lease.as_ref(),
        state.private_intake_stream_capacity,
    ) {
        (Some(_), Some(capacity))
            if capacity > 0
                && crate::storage::private_intake_stream_count(
                    &state.private_intake_admitted_homeservers,
                ) <= capacity => {}
        (None, None) if state.private_intake_admitted_homeservers.is_empty() => {}
        _ => {
            return Err(invalid_live_state(
                "private intake supervisor capacity ownership is inconsistent",
            ));
        }
    }
    if state
        .private_intake_admitted_homeservers
        .values()
        .any(String::is_empty)
    {
        return Err(invalid_live_state(
            "private intake admission has an empty homeserver",
        ));
    }
    for cursor in state.private_intake_cursors.values() {
        if cursor.homeserver.is_empty() {
            return Err(invalid_live_state(
                "private intake cursor has an empty homeserver",
            ));
        }
    }

    let mut global_count = 0usize;
    let mut global_bytes = 0usize;
    for peer in state.private_intake_peers.values() {
        let recovery_required = state
            .linked_peers
            .get(&peer.counterparty)
            .is_some_and(|linked| linked.state == LinkedPeerState::RecoveryRequired);
        let link = state.encrypted_link_states.get(&peer.counterparty);
        if let Some(link) = link {
            let fenced_recovery_generation = recovery_required
                && link.link_snapshot.is_none()
                && link.handshake_snapshot.is_none()
                && link.handshake_role.is_none()
                && link.generation > peer.link_generation;
            if link.generation != peer.link_generation && !fenced_recovery_generation {
                return Err(invalid_live_state(
                    "private intake peer generation does not match Encrypted Link",
                ));
            }
        } else if !recovery_required {
            return Err(invalid_live_state(
                "private intake peer without Encrypted Link is not recovery-required",
            ));
        }
        if peer.buffered_packets.len() > crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER {
            return Err(invalid_live_state(
                "private intake peer packet capacity is exceeded",
            ));
        }
        let mut peer_bytes = 0usize;
        let mut previous_counter: Option<u64> = None;
        for packet in &peer.buffered_packets {
            if packet.counterparty != peer.counterparty
                || packet.link_generation != peer.link_generation
            {
                return Err(invalid_live_state(
                    "private intake packet scope does not match its peer",
                ));
            }
            if packet.ciphertext.len() != crate::storage::PRIVATE_INTAKE_PACKET_BYTES {
                return Err(invalid_live_state(
                    "private intake packet has invalid ciphertext length",
                ));
            }
            if let Some(previous) = previous_counter {
                if previous.checked_add(1) != Some(packet.receive_counter) {
                    return Err(invalid_live_state(
                        "private intake receive counters are not contiguous",
                    ));
                }
            }
            previous_counter = Some(packet.receive_counter);
            peer_bytes = peer_bytes
                .checked_add(packet.ciphertext.len())
                .ok_or_else(|| invalid_live_state("private intake peer byte count overflow"))?;
            if let Some(source_cursor) = &packet.source_cursor {
                let durable_cursor = state
                    .private_intake_cursors
                    .get(&source_cursor.subscribed_user)
                    .ok_or_else(|| {
                        invalid_live_state("private intake packet cursor is not durable")
                    })?;
                if durable_cursor.homeserver != source_cursor.homeserver
                    || durable_cursor.event_id < source_cursor.event_id
                {
                    return Err(invalid_live_state(
                        "private intake packet cursor exceeds durable cursor",
                    ));
                }
            }
        }
        if peer_bytes > crate::storage::PRIVATE_INTAKE_MAX_BYTES_PER_PEER {
            return Err(invalid_live_state(
                "private intake peer byte capacity is exceeded",
            ));
        }
        if peer
            .cancelled_handshake_through_generation
            .is_some_and(|cancelled| {
                link.is_some_and(|link| {
                    link.handshake_snapshot.is_some() && link.generation <= cancelled
                })
            })
        {
            return Err(invalid_live_state(
                "cancelled private intake handshake generation remains active",
            ));
        }
        global_count = global_count
            .checked_add(peer.buffered_packets.len())
            .ok_or_else(|| invalid_live_state("private intake packet count overflow"))?;
        global_bytes = global_bytes
            .checked_add(peer_bytes)
            .ok_or_else(|| invalid_live_state("private intake global byte count overflow"))?;
    }
    if global_count > crate::storage::PRIVATE_INTAKE_MAX_PACKETS_GLOBAL
        || global_bytes > crate::storage::PRIVATE_INTAKE_MAX_BYTES_GLOBAL
    {
        return Err(invalid_live_state(
            "private intake global capacity is exceeded",
        ));
    }
    Ok(())
}

fn validate_live_counters(state: &StorageState) -> Result<()> {
    validate_next_id(
        state.next_outbound_private_message_id,
        state
            .outbound_private_messages
            .iter()
            .map(|record| record.outbound_message_id),
        "outbound Private Application Message id is outside its allocated range",
    )?;
    validate_next_id(
        state.next_receive_batch_id,
        state
            .private_stream_items
            .iter()
            .map(|record| record.receive_batch_id),
        "private receive batch id is outside its allocated range",
    )?;
    validate_next_id(
        state.next_private_stream_item_id,
        state
            .private_stream_items
            .iter()
            .map(|record| record.stream_item_id),
        "private stream item id is outside its allocated range",
    )
}

fn validate_next_id(
    next_id: u64,
    allocated_ids: impl Iterator<Item = u64>,
    context: &'static str,
) -> Result<()> {
    if allocated_ids.into_iter().any(|id| id >= next_id) {
        return Err(invalid_live_state(context));
    }
    Ok(())
}

fn require_original_order<T: PartialEq>(
    ordered: &[T],
    original: &[T],
    context: &'static str,
) -> Result<()> {
    if ordered != original {
        return Err(invalid_live_state(context));
    }
    Ok(())
}

fn invalid_live_state(context: &'static str) -> PaykitSdkError {
    PaykitSdkError::Storage {
        context: context.into(),
        source: None,
    }
}
