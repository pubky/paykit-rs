use super::super::*;
use chrono::{DateTime, Utc};

pub(in crate::backup) fn reconcile_restored_linked_peers(
    linked_peers: &mut HashMap<PubkyPublicKey, LinkedPeerRecord>,
    encrypted_link_states: &HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Vec<PubkyPublicKey> {
    for (counterparty, link_state) in encrypted_link_states {
        let restored_state = restored_peer_state_from_link_state(link_state);
        let checkpointed_at = link_state.checkpointed_at;
        linked_peers
            .entry(counterparty.clone())
            .and_modify(|peer| {
                if matches!(
                    peer.state,
                    LinkedPeerState::Blocked | LinkedPeerState::RecoveryRequired
                ) {
                    return;
                }
                match restored_state {
                    Some(LinkedPeerState::Linked) => peer.state = LinkedPeerState::Linked,
                    Some(LinkedPeerState::Linking) if peer.state != LinkedPeerState::Linked => {
                        peer.state = LinkedPeerState::Linking;
                    }
                    None if matches!(
                        peer.state,
                        LinkedPeerState::Linked | LinkedPeerState::Linking
                    ) =>
                    {
                        peer.state = LinkedPeerState::RecoveryRequired;
                    }
                    _ => {}
                }
                if peer.last_sync_at.is_none() {
                    peer.last_sync_at = Some(checkpointed_at);
                }
            })
            .or_insert_with(|| {
                restored_peer_record(
                    counterparty.clone(),
                    restored_state.unwrap_or(LinkedPeerState::RecoveryRequired),
                    checkpointed_at,
                )
            });
    }

    for (counterparty, peer) in linked_peers.iter_mut() {
        if !encrypted_link_states.contains_key(counterparty)
            && matches!(
                peer.state,
                LinkedPeerState::Linked | LinkedPeerState::Linking
            )
        {
            peer.state = LinkedPeerState::RecoveryRequired;
        }
    }

    let active_link_counterparties = encrypted_link_states
        .iter()
        .filter_map(|(counterparty, state)| state.link_snapshot.is_some().then_some(counterparty))
        .cloned()
        .collect::<HashSet<_>>();
    for message in outbound_private_messages {
        if outbound_status_requires_link(&message.status)
            && !active_link_counterparties.contains(&message.counterparty)
        {
            linked_peers
                .entry(message.counterparty.clone())
                .and_modify(|peer| {
                    if peer.state != LinkedPeerState::Blocked {
                        peer.state = LinkedPeerState::RecoveryRequired;
                    }
                })
                .or_insert_with(|| {
                    restored_peer_record(
                        message.counterparty.clone(),
                        LinkedPeerState::RecoveryRequired,
                        message.updated_at,
                    )
                });
        }
    }

    let mut peers = Vec::new();
    for record in linked_peers.values() {
        if record.state == LinkedPeerState::RecoveryRequired {
            peers.push(record.counterparty.clone());
        }
    }
    peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    peers
}

fn restored_peer_state_from_link_state(
    record: &EncryptedLinkStateRecord,
) -> Option<LinkedPeerState> {
    if record.link_snapshot.is_some() {
        Some(LinkedPeerState::Linked)
    } else if record.handshake_snapshot.is_some() && record.handshake_role.is_some() {
        Some(LinkedPeerState::Linking)
    } else {
        None
    }
}

fn restored_peer_record(
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    last_sync_at: DateTime<Utc>,
) -> LinkedPeerRecord {
    LinkedPeerRecord {
        counterparty,
        state,
        last_sync_at: Some(last_sync_at),
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    }
}

fn outbound_status_requires_link(status: &OutboundPrivateMessageStatus) -> bool {
    matches!(
        status,
        OutboundPrivateMessageStatus::Pending
            | OutboundPrivateMessageStatus::Sending
            | OutboundPrivateMessageStatus::Failed
    )
}

pub(in crate::backup) fn clear_recovery_required_link_snapshots(
    encrypted_link_states: &mut HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
    recovery_required_peers: &[PubkyPublicKey],
) {
    for counterparty in recovery_required_peers {
        let Some(record) = encrypted_link_states.get_mut(counterparty) else {
            continue;
        };
        let had_snapshot = record.link_snapshot.is_some()
            || record.handshake_snapshot.is_some()
            || record.handshake_role.is_some();
        record.link_snapshot = None;
        record.handshake_snapshot = None;
        record.handshake_role = None;
        if had_snapshot {
            record.generation = record.generation.saturating_add(1);
        }
    }
}

pub(in crate::backup) fn mark_restored_sending_outbound_recovery_required(
    records: &mut [OutboundPrivateMessageRecord],
    recovery_required_peers: &[PubkyPublicKey],
) {
    let recovery_required_peers = recovery_required_peers
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    for record in records.iter_mut() {
        if record.status == OutboundPrivateMessageStatus::Sending
            && recovery_required_peers.contains(&record.counterparty)
        {
            record.status = OutboundPrivateMessageStatus::RecoveryRequired;
            record.updated_at = record
                .updated_at
                .max(record.last_attempt_at.unwrap_or(record.created_at));
            record.last_error =
                Some("restored sending message requires Encrypted Link recovery".into());
            record.prepared_send = None;
        }
    }
}

pub(in crate::backup) fn validate_encrypted_link_snapshots(
    records: &HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
) -> Result<()> {
    for (counterparty, record) in records {
        let expected_recipient = counterparty.to_public_key()?;
        if let Some(snapshot_bytes) = record.link_snapshot.as_ref() {
            let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes)
                .map_err(PaykitSdkError::from)?;
            if snapshot.recipient() != &expected_recipient {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                    "Encrypted Link snapshot recipient does not match counterparty {counterparty}"
                ),
                    source: None,
                });
            }
        }
        if let Some(snapshot_bytes) = record.handshake_snapshot.as_ref() {
            let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)
                .map_err(PaykitSdkError::from)?;
            if snapshot.recipient() != &expected_recipient {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "Encrypted Link Handshake snapshot recipient does not match counterparty {counterparty}"
                ), source: None });
            }
        }
    }
    Ok(())
}

pub(in crate::backup) fn validate_linked_peer_records(
    records: &HashMap<PubkyPublicKey, LinkedPeerRecord>,
) -> Result<()> {
    for record in records.values() {
        validate_recovery_marker_fields(
            &record.counterparty,
            "local Encrypted Link recovery marker",
            record.local_recovery_attempt_id.as_deref(),
            record.local_recovery_marker_created_at,
        )?;
        validate_recovery_marker_fields(
            &record.counterparty,
            "remote Encrypted Link recovery marker",
            record.remote_recovery_attempt_id.as_deref(),
            record.remote_recovery_marker_observed_at,
        )?;
    }
    Ok(())
}

fn validate_recovery_marker_fields(
    counterparty: &PubkyPublicKey,
    label: &str,
    attempt_id: Option<&str>,
    timestamp: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<()> {
    match (attempt_id, timestamp) {
        (Some(attempt_id), Some(timestamp)) => {
            paykit_lib::EncryptedLinkRecoveryMarker::new(
                attempt_id,
                timestamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            )
            .map_err(|err| PaykitSdkError::Protocol {
                context: format!("{label} for counterparty {counterparty} is invalid: {err}"),
                source: None,
            })?;
        }
        (None, None) => {}
        _ => {
            return Err(PaykitSdkError::Protocol { context: format!(
                "{label} for counterparty {counterparty} must store attempt id and timestamp together"
            ), source: None });
        }
    }
    Ok(())
}
