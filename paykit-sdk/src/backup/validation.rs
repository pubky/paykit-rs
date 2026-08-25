use super::*;
use chrono::{DateTime, Utc};

pub(super) fn preserve_current_sign_out_generation(
    backup_identity: &mut Option<IdentityState>,
    current_identity: Option<&IdentityState>,
) {
    match (backup_identity, current_identity) {
        (Some(backup_identity), Some(current_identity))
            if backup_identity.local_pubky_public_key
                == current_identity.local_pubky_public_key =>
        {
            backup_identity.sign_out_generation = backup_identity
                .sign_out_generation
                .max(current_identity.sign_out_generation);
        }
        (backup_identity @ None, Some(current_identity))
            if current_identity.local_pubky_public_key.is_none() =>
        {
            *backup_identity = Some(current_identity.clone());
        }
        _ => {}
    }
}

pub(super) fn keyed_by_peer<T>(records: Vec<T>, label: &str) -> Result<HashMap<PeerStorageKey, T>>
where
    T: HasPeerStorageKey,
{
    keyed_by_tuple(
        records,
        |record| {
            (
                record.counterparty().clone(),
                record.counterparty_receiver_path().clone(),
            )
        },
        label,
    )
}

pub(super) trait HasPeerStorageKey {
    fn counterparty(&self) -> &PubkyPublicKey;
    fn counterparty_receiver_path(&self) -> &PaykitReceiverPath;
}

impl HasPeerStorageKey for LinkedPeerRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }

    fn counterparty_receiver_path(&self) -> &PaykitReceiverPath {
        &self.counterparty_receiver_path
    }
}

impl HasPeerStorageKey for EncryptedLinkStateRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }

    fn counterparty_receiver_path(&self) -> &PaykitReceiverPath {
        &self.counterparty_receiver_path
    }
}

pub(super) fn keyed_by_string<T, F>(
    records: Vec<T>,
    key: F,
    label: &str,
) -> Result<HashMap<String, T>>
where
    F: Fn(&T) -> String,
{
    keyed_by_tuple(records, key, label)
}

pub(super) fn keyed_by_tuple<K, T, F>(records: Vec<T>, key: F, label: &str) -> Result<HashMap<K, T>>
where
    K: Eq + std::hash::Hash + fmt::Debug,
    F: Fn(&T) -> K,
{
    let mut keyed = HashMap::new();
    for record in records {
        let key = key(&record);
        if keyed.insert(key, record).is_some() {
            return Err(PaykitSdkError::Protocol {
                context: format!("duplicate {label} backup key"),
                source: None,
            });
        }
    }
    Ok(keyed)
}

pub(super) fn unique_outbound_messages(
    mut records: Vec<OutboundPrivateMessageRecord>,
) -> Result<Vec<OutboundPrivateMessageRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.outbound_message_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "duplicate outbound Private Application Message id {}",
                    record.outbound_message_id
                ),
                source: None,
            });
        }
    }
    records.sort_by_key(|record| record.outbound_message_id);
    Ok(records)
}

pub(super) fn unique_private_stream_items(
    mut records: Vec<PrivateStreamItemRecord>,
) -> Result<Vec<PrivateStreamItemRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.stream_item_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!("duplicate private stream item id {}", record.stream_item_id),
                source: None,
            });
        }
    }
    records.sort_by_key(|record| record.stream_item_id);
    Ok(records)
}

pub(super) fn next_outbound_id(records: &[OutboundPrivateMessageRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.outbound_message_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

pub(super) fn next_receive_batch_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.receive_batch_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

pub(super) fn next_private_stream_item_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.stream_item_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

pub(super) fn reconcile_restored_linked_peers(
    linked_peers: &mut HashMap<PeerStorageKey, LinkedPeerRecord>,
    encrypted_link_states: &HashMap<PeerStorageKey, EncryptedLinkStateRecord>,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Vec<PeerStorageKey> {
    for (peer_key, link_state) in encrypted_link_states {
        let (counterparty, counterparty_receiver_path) = peer_key;
        let restored_state = restored_peer_state_from_link_state(link_state);
        let checkpointed_at = link_state.checkpointed_at;
        linked_peers
            .entry(peer_key.clone())
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
                    counterparty_receiver_path.clone(),
                    restored_state.unwrap_or(LinkedPeerState::RecoveryRequired),
                    checkpointed_at,
                )
            });
    }

    for (peer_key, peer) in linked_peers.iter_mut() {
        if !encrypted_link_states.contains_key(peer_key)
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
        .filter_map(|(peer_key, state)| state.link_snapshot.is_some().then_some(peer_key))
        .cloned()
        .collect::<HashSet<_>>();
    for message in outbound_private_messages {
        let peer_key = (
            message.counterparty.clone(),
            message.counterparty_receiver_path.clone(),
        );
        if outbound_status_requires_link(&message.status)
            && !active_link_counterparties.contains(&peer_key)
        {
            linked_peers
                .entry(peer_key)
                .and_modify(|peer| {
                    if peer.state != LinkedPeerState::Blocked {
                        peer.state = LinkedPeerState::RecoveryRequired;
                    }
                })
                .or_insert_with(|| {
                    restored_peer_record(
                        message.counterparty.clone(),
                        message.counterparty_receiver_path.clone(),
                        LinkedPeerState::RecoveryRequired,
                        message.updated_at,
                    )
                });
        }
    }

    let mut peers = Vec::new();
    for record in linked_peers.values() {
        if record.state == LinkedPeerState::RecoveryRequired {
            peers.push((
                record.counterparty.clone(),
                record.counterparty_receiver_path.clone(),
            ));
        }
    }
    peers.sort_by(|left, right| {
        left.0
            .as_str()
            .cmp(right.0.as_str())
            .then(left.1.as_str().cmp(right.1.as_str()))
    });
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
    counterparty_receiver_path: PaykitReceiverPath,
    state: LinkedPeerState,
    last_sync_at: DateTime<Utc>,
) -> LinkedPeerRecord {
    LinkedPeerRecord {
        counterparty,
        counterparty_receiver_path,
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

pub(super) fn clear_recovery_required_link_snapshots(
    encrypted_link_states: &mut HashMap<PeerStorageKey, EncryptedLinkStateRecord>,
    recovery_required_peers: &[PeerStorageKey],
) {
    for peer_key in recovery_required_peers {
        let Some(record) = encrypted_link_states.get_mut(peer_key) else {
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

pub(super) fn mark_restored_sending_outbound_recovery_required(
    records: &mut [OutboundPrivateMessageRecord],
    recovery_required_peers: &[PeerStorageKey],
) {
    let recovery_required_peers = recovery_required_peers
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    for record in records.iter_mut() {
        let peer_key = (
            record.counterparty.clone(),
            record.counterparty_receiver_path.clone(),
        );
        if record.status == OutboundPrivateMessageStatus::Sending
            && recovery_required_peers.contains(&peer_key)
        {
            record.status = OutboundPrivateMessageStatus::RecoveryRequired;
            record.updated_at = record
                .updated_at
                .max(record.last_attempt_at.unwrap_or(record.created_at));
            record.last_error =
                Some("restored sending message requires Encrypted Link recovery".into());
        }
    }
}

pub(super) fn validate_encrypted_link_snapshots(
    records: &HashMap<PeerStorageKey, EncryptedLinkStateRecord>,
    local_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    for ((counterparty, counterparty_receiver_path), record) in records {
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
            if snapshot.local_receiver_path() != local_receiver_path
                || snapshot.remote_receiver_path() != counterparty_receiver_path
            {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "Encrypted Link snapshot receiver paths do not match peer {counterparty}/{counterparty_receiver_path}"
                ), source: None });
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
            if snapshot.local_receiver_path() != local_receiver_path
                || snapshot.remote_receiver_path() != counterparty_receiver_path
            {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "Encrypted Link Handshake snapshot receiver paths do not match peer {counterparty}/{counterparty_receiver_path}"
                ), source: None });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_linked_peer_records(
    records: &HashMap<PeerStorageKey, LinkedPeerRecord>,
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

pub(super) fn validate_public_endpoint_records(
    records: &HashMap<String, PublicEndpointRecord>,
) -> Result<()> {
    for record in records.values() {
        PaymentEndpointIdentifier::new(&record.identifier)?;
        match record.status {
            PublicationStatus::NotPublished => {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "public endpoint record '{}' cannot be not-published",
                        record.identifier
                    ),
                    source: None,
                });
            }
            PublicationStatus::PendingPublication | PublicationStatus::Published => {
                if record.payload.is_none() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has no payload for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
                if record.last_error.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has an error for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
            }
            PublicationStatus::PendingRemoval | PublicationStatus::Removed => {
                if record.last_error.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "public endpoint record '{}' has an error for status {:?}",
                            record.identifier, record.status
                        ),
                        source: None,
                    });
                }
                if record.status == PublicationStatus::Removed && record.payload.is_some() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "removed public endpoint record '{}' still has a payload",
                            record.identifier
                        ),
                        source: None,
                    });
                }
            }
            PublicationStatus::Failed => {
                if record.last_error.is_none() {
                    return Err(PaykitSdkError::Protocol {
                        context: format!(
                            "failed public endpoint record '{}' has no error",
                            record.identifier
                        ),
                        source: None,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_contact_records(
    records: &HashMap<PubkyPublicKey, ContactRecord>,
) -> Result<()> {
    for record in records.values() {
        if let Some(profile) = record.profile.as_ref() {
            profile.validate()?;
        }
        if let Some(label) = record.label.as_deref() {
            crate::ContactUpdate {
                public_key: record.public_key.clone(),
                receiver_paths: record.receiver_paths.clone(),
                label: Some(label.to_owned()),
            }
            .validate()?;
        } else {
            crate::ContactUpdate {
                public_key: record.public_key.clone(),
                receiver_paths: record.receiver_paths.clone(),
                label: None,
            }
            .validate()?;
        }
        validate_contact_marker_state(record)?;
    }
    Ok(())
}

fn validate_contact_marker_state(record: &ContactRecord) -> Result<()> {
    use crate::PublicationStatus::{
        Failed, NotPublished, PendingPublication, PendingRemoval, Published, Removed,
    };

    if record.public_contact_published_at.is_some() && record.public_contact_removed_at.is_some() {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "local contact {} has inconsistent public contact marker timestamps",
                record.public_key
            ),
            source: None,
        });
    }

    let active_marker_receiver = record
        .public_contact_marker_receiver_path
        .as_ref()
        .is_some_and(|receiver_path| record.contains_receiver_path(receiver_path));

    let invalid = match record.public_contact_marker_status {
        NotPublished => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
                || record.public_contact_marker_receiver_path.is_some()
        }
        PendingPublication => record.public_contact_last_error.is_some() || !active_marker_receiver,
        Published => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
                || !active_marker_receiver
        }
        PendingRemoval => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
                || !active_marker_receiver
        }
        Removed => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_none()
                || record.public_contact_last_error.is_some()
                || record.public_contact_marker_receiver_path.is_some()
        }
        Failed => record.public_contact_last_error.is_none() || !active_marker_receiver,
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "local contact {} has inconsistent public contact marker state",
                record.public_key
            ),
            source: None,
        });
    }
    Ok(())
}

pub(super) fn validate_payment_endpoint_reservations(
    records: &HashMap<
        (PubkyPublicKey, PaykitReceiverPath, String),
        PaymentEndpointReservationRecord,
    >,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    let outbound_by_id = outbound_private_messages
        .iter()
        .map(|record| (record.outbound_message_id, record))
        .collect::<HashMap<_, _>>();

    for record in records.values() {
        validate_reservation_id(&record.reservation_id)?;
        let identifier = PaymentEndpointIdentifier::new(&record.identifier)?;
        let outbound = outbound_by_id
            .get(&record.outbound_message_id)
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' references missing outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            })?;
        if outbound.counterparty != record.counterparty
            || outbound.counterparty_receiver_path != record.counterparty_receiver_path
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' peer does not match outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            });
        }
        if outbound.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' references non-list outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            });
        }
        // SECURITY / REDACTION: name only local ids, never the parse error
        // text (it could echo the outbound payload into restore diagnostics).
        let private_list = parse_private_payment_list_json(&outbound.raw_json).map_err(|_| {
            PaykitSdkError::Protocol {
                context: format!(
                    "Payment Endpoint Reservation '{}' outbound Private Payment List {} is unparseable",
                    record.reservation_id, record.outbound_message_id
                ),
                source: None,
            }
        })?;
        let payload = private_list.get(&identifier).ok_or_else(|| {
            PaykitSdkError::Protocol { context: format!(
                "Payment Endpoint Reservation '{}' identifier is missing from outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            ), source: None }
        })?;
        let payload_hash = reservation_payload_hash(payload.as_str());
        if record.payload_hash != payload_hash {
            return Err(PaykitSdkError::Protocol { context: format!(
                "Payment Endpoint Reservation '{}' payload hash does not match outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            ), source: None });
        }
    }
    Ok(())
}

pub(super) fn validate_outbound_private_messages(
    records: &[OutboundPrivateMessageRecord],
    local_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    for record in records {
        validate_outbound_private_status(record)?;
        if matches!(
            record.status,
            OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::RecoveryRequired
        ) {
            continue;
        }
        if outbound_record_is_acceptable_parked(record) {
            // A well-formed unknown-kind record is a parked queue entry
            // written by a newer build: restore preserves it verbatim so the
            // runtime can keep it parked at the head of its peer's queue.
            // Restore stays strict when the kind column diverges from the
            // payload body kind and FAILS below, while the runtime parks that
            // divergence safely -- a deliberate asymmetry: restore validates
            // a whole snapshot up front, whereas the flush path must never
            // destroy unknown outbound intent it encounters live.
            continue;
        }
        validate_queued_outbound_private_message(record)?;
        let kind = validate_outbound_private_message(&record.raw_json)?;
        if kind == PrivateMessageKind::ReceiptAccess.as_str() {
            // SECURITY / REDACTION: name only the local outbound id, never
            // the parse error text (it could echo the outbound payload).
            let access = paykit_lib::parse_receipt_access_json(&record.raw_json).map_err(|_| {
                PaykitSdkError::Protocol {
                    context: format!(
                        "outbound Receipt Access message {} payload is unparseable",
                        record.outbound_message_id
                    ),
                    source: None,
                }
            })?;
            if !access.has_location_for_receiver(local_receiver_path) {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                    "outbound Receipt Access message {} location does not match local receiver {}",
                    record.outbound_message_id, local_receiver_path
                ),
                    source: None,
                });
            }
        }
    }
    Ok(())
}

/// Whether a restored outbound record is an acceptable parked unknown-kind
/// queue entry: a well-formed envelope with an unrecognized body kind whose
/// `kind` column matches that body kind exactly.
///
/// Inspection reads only `raw_json`, so its `parsed_kind` for an unknown-kind
/// payload is the top-level `kind` field of the payload document itself --
/// the same body-authoritative field the stamping invariant on
/// `OutboundPrivateMessageRecord::kind` mirrors into the column.
fn outbound_record_is_acceptable_parked(record: &OutboundPrivateMessageRecord) -> bool {
    let inspection = paykit_lib::inspect_private_application_message(&record.raw_json);
    inspection.structure == paykit_lib::PrivateMessageStructure::UnknownKind
        && inspection.parsed_kind.as_deref() == Some(record.kind.as_str())
}

fn validate_outbound_private_status(record: &OutboundPrivateMessageRecord) -> Result<()> {
    let invalid = match record.status {
        OutboundPrivateMessageStatus::Pending => {
            record.attempt_count != 0
                || record.last_attempt_at.is_some()
                || record.sent_at.is_some()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Sending => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_some()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Sent => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_none()
                || record.last_error.is_some()
        }
        OutboundPrivateMessageStatus::Failed => {
            record.attempt_count == 0
                || record.last_attempt_at.is_none()
                || record.sent_at.is_some()
                || record.last_error.is_none()
        }
        OutboundPrivateMessageStatus::Invalid | OutboundPrivateMessageStatus::RecoveryRequired => {
            record.sent_at.is_some() || record.last_error.is_none()
        }
        OutboundPrivateMessageStatus::Superseded => {
            record.sent_at.is_some() || record.last_error.is_some()
        }
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "outbound Private Application Message {} has inconsistent {:?} status metadata",
                record.outbound_message_id, record.status
            ),
            source: None,
        });
    }
    Ok(())
}

// pub(crate) (not pub(super)) so the test-only re-export in backup/mod.rs can
// expose it to the frozen classification fixture replay.
pub(crate) fn validate_private_stream_items(records: &[PrivateStreamItemRecord]) -> Result<()> {
    for record in records {
        let (parsed_version, parsed_kind, known_kind) = private_message_header(&record.raw_json)?;
        let mut classification =
            classify_private_application_message(&private_application_message_from_raw(
                record.raw_json.clone(),
                parsed_version,
                parsed_kind.clone(),
            ));
        enforce_receipt_access_receiver_scope(
            &mut classification,
            &record.counterparty_receiver_path,
        );
        if record.parsed_version != parsed_version {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parsed version metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parsed_kind.as_deref() != parsed_kind.as_deref() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parsed kind metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.known_paykit_kind.as_deref() != known_kind.map(PrivateMessageKind::as_str) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale known kind metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_status != classification.status {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parse status metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_error.as_deref() != classification.parse_error.as_deref() {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} has stale parse error metadata",
                    record.stream_item_id
                ),
                source: None,
            });
        }
        if record.parse_status == PrivateStreamParseStatus::Valid {
            let Some(kind) = known_kind else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "private stream item {} is marked valid without a recognized Paykit kind",
                        record.stream_item_id
                    ),
                    source: None,
                });
            };
            validate_valid_private_stream_body(record, kind)?;
        }
    }
    Ok(())
}

pub(super) fn validate_event_dedup_records(
    records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), EventDedupRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        validate_event_dedup_membership(record)?;
        let Some(first) = stream_by_id.get(&record.first_stream_item_id) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' references missing first stream item {}",
                    record.event_id, record.first_stream_item_id
                ),
                source: None,
            });
        };
        if first.counterparty != record.counterparty
            || first.counterparty_receiver_path != record.counterparty_receiver_path
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' peer does not match first stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        if payload_hash(&first.raw_json) != record.payload_hash {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' payload hash does not match first stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        validate_event_dedup_stream_item(record, first, EventDedupeItemKind::First)?;
        for stream_item_id in &record.duplicate_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' references missing stream item {}",
                        record.event_id, stream_item_id
                    ),
                    source: None,
                });
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Duplicate)?;
        }
        for stream_item_id in &record.conflicting_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' references missing stream item {}",
                        record.event_id, stream_item_id
                    ),
                    source: None,
                });
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Conflict)?;
        }
    }
    Ok(())
}

fn validate_event_dedup_membership(record: &EventDedupRecord) -> Result<()> {
    let mut seen = HashSet::new();
    seen.insert(record.first_stream_item_id);
    for stream_item_id in record
        .duplicate_stream_item_ids
        .iter()
        .chain(record.conflicting_stream_item_ids.iter())
    {
        if !seen.insert(*stream_item_id) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' references stream item {} more than once",
                    record.event_id, stream_item_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EventDedupeItemKind {
    First,
    Duplicate,
    Conflict,
}

fn validate_event_dedup_stream_item(
    record: &EventDedupRecord,
    item: &PrivateStreamItemRecord,
    item_kind: EventDedupeItemKind,
) -> Result<()> {
    if item.counterparty != record.counterparty {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' counterparty does not match stream item {}",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    }
    if item.counterparty_receiver_path != record.counterparty_receiver_path {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' counterparty receiver does not match stream item {}",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    }

    let mut classification =
        classify_private_application_message(&private_application_message_from_raw(
            item.raw_json.clone(),
            item.parsed_version,
            item.parsed_kind.clone(),
        ));
    enforce_receipt_access_receiver_scope(&mut classification, &item.counterparty_receiver_path);
    let Some(event) = classification.event else {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' references non-event stream item {}",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    };
    if event.event_id != record.event_id {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Event dedupe record '{}' does not match stream item {} event header",
                record.event_id, item.stream_item_id
            ),
            source: None,
        });
    }

    let item_hash = payload_hash(&item.raw_json);
    match item_kind {
        EventDedupeItemKind::First | EventDedupeItemKind::Duplicate => {
            if event.event_kind != record.event_kind {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                    "Event dedupe record '{}' same-payload stream item {} has different event kind",
                    record.event_id, item.stream_item_id
                ),
                    source: None,
                });
            }
            if item_hash != record.payload_hash {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "Event dedupe record '{}' same-payload stream item {} has different payload hash",
                    record.event_id, item.stream_item_id
                ), source: None });
            }
        }
        EventDedupeItemKind::Conflict => {
            if item_hash == record.payload_hash {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Event dedupe record '{}' conflict stream item {} has same payload hash",
                        record.event_id, item.stream_item_id
                    ),
                    source: None,
                });
            }
        }
    }

    Ok(())
}

pub(super) fn validate_receipt_access_records(
    records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptAccessRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        validate_receipt_access_retrieval_status(record)?;
        let Some(item) = stream_by_id.get(&record.stream_item_id) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' references missing stream item {}",
                    record.event_id, record.stream_item_id
                ),
                source: None,
            });
        };
        if item.counterparty != record.counterparty
            || item.counterparty_receiver_path != record.counterparty_receiver_path
            || item.receive_batch_id != record.receive_batch_id
            || item.known_paykit_kind.as_deref() != Some(PrivateMessageKind::ReceiptAccess.as_str())
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' does not match its stream item",
                    record.event_id
                ),
                source: None,
            });
        }
        let event =
            paykit_lib::parse_receipt_access_event_message(&private_application_message_from_raw(
                item.raw_json.clone(),
                item.parsed_version,
                item.parsed_kind.clone(),
            ))
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' stream item is not parseable",
                    record.event_id
                ),
                source: None,
            })?;
        let Some(access) = event.parsed_access() else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' stream item is malformed",
                    record.event_id
                ),
                source: None,
            });
        };
        if access.event_id.as_str() != record.event_id
            || access.receipt_id.as_str() != record.receipt_id
            || access.payment_reference.as_str() != record.payment_reference
            || access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || access
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || access.location != record.location
            || access.key.as_str() != record.key
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' does not match parsed stream payload",
                    record.event_id
                ),
                source: None,
            });
        }
        if !access.has_location_for_receiver(&record.counterparty_receiver_path) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access record '{}' location does not match counterparty receiver {}",
                    record.event_id, record.counterparty_receiver_path
                ),
                source: None,
            });
        }
    }
    Ok(())
}

fn validate_receipt_access_retrieval_status(record: &ReceiptAccessRecord) -> Result<()> {
    match record.retrieval_status {
        ReceiptRetrievalStatus::Pending => {
            if record.retrieval_attempted_at.is_some()
                || record.retrieved_at.is_some()
                || record.last_retrieval_error.is_some()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "pending Receipt Access record '{}' has retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
        ReceiptRetrievalStatus::Retrieved => {
            if record.retrieval_attempted_at.is_none()
                || record.retrieved_at.is_none()
                || record.last_retrieval_error.is_some()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "retrieved Receipt Access record '{}' has inconsistent retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
        ReceiptRetrievalStatus::NotFound | ReceiptRetrievalStatus::Failed => {
            if record.retrieval_attempted_at.is_none()
                || record.retrieved_at.is_some()
                || record.last_retrieval_error.is_none()
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "failed Receipt Access record '{}' has inconsistent retrieval metadata",
                        record.event_id
                    ),
                    source: None,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn validate_required_private_stream_indexes(
    stream_items: &[PrivateStreamItemRecord],
    event_dedup_records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), EventDedupRecord>,
    receipt_access_records: &HashMap<
        (PubkyPublicKey, PaykitReceiverPath, String),
        ReceiptAccessRecord,
    >,
) -> Result<()> {
    for item in stream_items {
        let mut classification =
            classify_private_application_message(&private_application_message_from_raw(
                item.raw_json.clone(),
                item.parsed_version,
                item.parsed_kind.clone(),
            ));
        enforce_receipt_access_receiver_scope(
            &mut classification,
            &item.counterparty_receiver_path,
        );
        let Some(event) = classification.event else {
            continue;
        };
        let key = (
            item.counterparty.clone(),
            item.counterparty_receiver_path.clone(),
            event.event_id.clone(),
        );
        let Some(dedupe) = event_dedup_records.get(&key) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} is missing required Event dedupe record '{}'",
                    item.stream_item_id, event.event_id
                ),
                source: None,
            });
        };
        if !event_dedup_record_contains_stream_event(dedupe, item, &event.event_kind) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Event dedupe record '{}' does not include private stream item {}",
                    event.event_id, item.stream_item_id
                ),
                source: None,
            });
        }
        if classification.receipt_access.is_some()
            && dedupe.first_stream_item_id == item.stream_item_id
            && !receipt_access_records.contains_key(&key)
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt Access event '{}' is missing required Receipt Access record",
                    event.event_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

fn event_dedup_record_contains_stream_event(
    record: &EventDedupRecord,
    item: &PrivateStreamItemRecord,
    event_kind: &str,
) -> bool {
    let item_hash = payload_hash(&item.raw_json);
    if record.first_stream_item_id == item.stream_item_id {
        return event_kind == record.event_kind && item_hash == record.payload_hash;
    }
    if record
        .duplicate_stream_item_ids
        .contains(&item.stream_item_id)
    {
        return event_kind == record.event_kind && item_hash == record.payload_hash;
    }
    if record
        .conflicting_stream_item_ids
        .contains(&item.stream_item_id)
    {
        return item_hash != record.payload_hash;
    }
    false
}

pub(super) fn validate_receipt_records(
    records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptRecord>,
    access_records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptAccessRecord>,
    expected_recipient: Option<&PubkyPublicKey>,
) -> Result<()> {
    for record in records.values() {
        ReceiptId::new(&record.receipt_id)?;
        if let Some(expected_recipient) = expected_recipient {
            if &record.recipient_public_key != expected_recipient {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt record '{}' recipient does not match backup identity",
                        record.receipt_id
                    ),
                    source: None,
                });
            }
        }
        if let Some(identifier) = record.payment_endpoint_identifier.as_ref() {
            PaymentEndpointIdentifier::new(identifier)?;
        }
        let access_key = (
            record.issuer.clone(),
            record.issuer_receiver_path.clone(),
            record.receipt_access_event_id.clone(),
        );
        let Some(access) = access_records.get(&access_key) else {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt record '{}' references missing Receipt Access event '{}'",
                    record.receipt_id, record.receipt_access_event_id
                ),
                source: None,
            });
        };
        if access.receipt_id != record.receipt_id
            || access.payment_reference != record.payment_reference
            || access.payment_request_id != record.payment_request_id
            || access.billing_period != record.billing_period
            || access.location != record.location
            || receipt_access_key_hash(&access.key) != record.receipt_access_key_hash
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt record '{}' does not match its Receipt Access record",
                    record.receipt_id
                ),
                source: None,
            });
        }
    }
    Ok(())
}

pub(super) fn validate_receipt_issuance_records(
    records: &HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptIssuanceRecord>,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
    local_receiver_path: &PaykitReceiverPath,
) -> Result<()> {
    let outbound_by_id = outbound_private_messages
        .iter()
        .map(|record| (record.outbound_message_id, record))
        .collect::<HashMap<_, _>>();
    let mut receipt_ids = HashSet::new();

    for record in records.values() {
        if !receipt_ids.insert(record.receipt_id.clone()) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' is duplicated across counterparties",
                    record.receipt_id
                ),
                source: None,
            });
        }
        validate_receipt_issuance_status(record)?;
        ReceiptId::new(&record.receipt_id)?;
        if let Some(identifier) = record.payment_endpoint_identifier.as_ref() {
            PaymentEndpointIdentifier::new(identifier)?;
        }

        // SECURITY / REDACTION: name only the local receipt id, never the
        // parse error text (it could echo the stored access payload).
        let access = paykit_lib::parse_receipt_access_json(&record.access_json).map_err(|_| {
            PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' outbound payload is unparseable",
                    record.receipt_id
                ),
                source: None,
            }
        })?;
        if access.event_id.as_str() != record.receipt_access_event_id
            || access.receipt_id.as_str() != record.receipt_id
            || access.payment_reference.as_str() != record.payment_reference
            || access
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || access
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || access.location != record.location
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' does not match Receipt Access payload",
                    record.receipt_id
                ),
                source: None,
            });
        }
        if !access.has_location_for_receiver(local_receiver_path) {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' location does not match local receiver {}",
                    record.receipt_id, local_receiver_path
                ),
                source: None,
            });
        }

        let receipt =
            paykit_lib::decrypt_receipt(&record.encrypted_receipt, &access.key, &access.location)
                .map_err(|_| PaykitSdkError::Protocol {
                // The lib error can describe the stored encrypted receipt
                // (and its `source` can carry envelope parse detail), and
                // this context crosses the FFI from backup restore; keep it
                // static and drop the cause.
                context: "stored encrypted receipt is invalid".into(),
                source: None,
            })?;
        let recipient = PubkyPublicKey::from_public_key(&receipt.recipient_public_key);
        if recipient != record.counterparty
            || receipt.receipt_id.as_str() != record.receipt_id
            || receipt.payment_reference.as_str() != record.payment_reference
            || receipt
                .payment_request_id
                .as_ref()
                .map(|id| id.as_str().to_owned())
                != record.payment_request_id
            || receipt
                .billing_period
                .as_ref()
                .map(BillingPeriodRecord::from)
                != record.billing_period
            || receipt
                .payment_endpoint_identifier
                .as_ref()
                .map(|identifier| identifier.as_str().to_owned())
                != record.payment_endpoint_identifier
            || receipt.amount.as_ref().map(AmountRecord::from) != record.amount
        {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "Receipt issuance record '{}' does not match encrypted Receipt",
                    record.receipt_id
                ),
                source: None,
            });
        }

        if let Some(outbound_message_id) = record.outbound_message_id {
            let Some(outbound) = outbound_by_id.get(&outbound_message_id) else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt issuance record '{}' references missing outbound message {}",
                        record.receipt_id, outbound_message_id
                    ),
                    source: None,
                });
            };
            if outbound.counterparty != record.counterparty
                || outbound.counterparty_receiver_path != record.counterparty_receiver_path
                || outbound.kind != PrivateMessageKind::ReceiptAccess.as_str()
                || outbound.raw_json != record.access_json
            {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "Receipt issuance record '{}' does not match outbound message {}",
                        record.receipt_id, outbound_message_id
                    ),
                    source: None,
                });
            }
        }
    }

    Ok(())
}

fn validate_receipt_issuance_status(record: &ReceiptIssuanceRecord) -> Result<()> {
    if record.updated_at < record.created_at
        || record
            .stored_at
            .is_some_and(|stored_at| stored_at < record.created_at)
        || record
            .access_queued_at
            .is_some_and(|queued_at| queued_at < record.created_at)
    {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Receipt issuance record '{}' has inconsistent timestamps",
                record.receipt_id
            ),
            source: None,
        });
    }

    let invalid = match record.status {
        ReceiptIssuanceStatus::PendingStorage => {
            record.stored_at.is_some()
                || record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::Stored => {
            record.stored_at.is_none()
                || record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::AccessQueued => {
            record.stored_at.is_none()
                || record.access_queued_at.is_none()
                || record.outbound_message_id.is_none()
                || record.last_error.is_some()
        }
        ReceiptIssuanceStatus::Failed => {
            record.access_queued_at.is_some()
                || record.outbound_message_id.is_some()
                || record.last_error.is_none()
        }
    };
    if invalid {
        return Err(PaykitSdkError::Protocol {
            context: format!(
                "Receipt issuance record '{}' has inconsistent {:?} status metadata",
                record.receipt_id, record.status
            ),
            source: None,
        });
    }
    Ok(())
}

fn validate_valid_private_stream_body(
    record: &PrivateStreamItemRecord,
    kind: PrivateMessageKind,
) -> Result<()> {
    // The typed parsers below take the stored envelope columns but classify
    // from `raw_json` alone (the body's kind is authoritative), so no
    // synthetic envelope metadata is constructed here.
    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            paykit_lib::parse_private_payment_list_json(&record.raw_json)?;
        }
        PrivateMessageKind::ReceiptAccess => {
            let event = paykit_lib::parse_receipt_access_event_message(
                &private_application_message_from_raw(
                    record.raw_json.clone(),
                    record.parsed_version,
                    record.parsed_kind.clone(),
                ),
            )
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} Receipt Access payload does not match its kind",
                    record.stream_item_id
                ),
                source: None,
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol {
                    context: error.to_owned(),
                    source: None,
                });
            }
            let Some(access) = event.parsed_access() else {
                return Err(PaykitSdkError::Protocol {
                    context: format!(
                        "private stream item {} Receipt Access payload is malformed",
                        record.stream_item_id
                    ),
                    source: None,
                });
            };
            if !access.has_location_for_receiver(&record.counterparty_receiver_path) {
                return Err(PaykitSdkError::Protocol { context: format!(
                    "private stream item {} Receipt Access location does not match counterparty receiver {}",
                    record.stream_item_id, record.counterparty_receiver_path
                ), source: None });
            }
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            let event = paykit_lib::parse_payment_request_event_message(
                &private_application_message_from_raw(
                    record.raw_json.clone(),
                    record.parsed_version,
                    record.parsed_kind.clone(),
                ),
            )
            .ok_or_else(|| PaykitSdkError::Protocol {
                context: format!(
                    "private stream item {} Payment Request payload does not match its kind",
                    record.stream_item_id
                ),
                source: None,
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol {
                    context: error.to_owned(),
                    source: None,
                });
            }
        }
    }
    Ok(())
}
