//! Linked Peer state records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::outbound_private::{mark_outbound_recovery_required, OutboundPrivateMessageStatus},
    storage::{
        require_peer_link_operation_lease, retry_storage_transaction, EncryptedLinkStateRecord,
        LinkedPeerRecord, PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

/// Local role for an in-progress Encrypted Link Handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum EncryptedLinkHandshakeRole {
    /// Local peer initiated the handshake.
    Initiator,
    /// Local peer accepted a handshake initiated by the counterparty.
    Responder,
}

/// Local relationship state for a counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LinkedPeerState {
    /// The SDK tracks this counterparty, but no active Encrypted Link exists.
    NotLinked,
    /// An Encrypted Link Handshake is in progress.
    Linking,
    /// An Encrypted Link is established.
    Linked,
    /// Local state cannot safely continue without recovery.
    RecoveryRequired,
    /// Local policy blocks this peer.
    Blocked,
}

/// Decide whether private automation may proceed on one exact Encrypted Link.
///
/// This is the single readiness rule shared by every private-message command;
/// callers read `peer_state` and `has_active_link` inside their own storage
/// transaction so the decision and any append stay atomic.
pub(crate) fn require_private_automation_ready(
    peer_state: Option<LinkedPeerState>,
    has_active_link: bool,
    counterparty: &PubkyPublicKey,
) -> Result<()> {
    match peer_state {
        Some(LinkedPeerState::Linked) if has_active_link => Ok(()),
        Some(LinkedPeerState::Linking) => Err(PaykitSdkError::RecoveryRequired {
            context: format!(
                "Encrypted Link Handshake is still in progress for counterparty {counterparty}"
            ),
            source: None,
        }),
        Some(LinkedPeerState::RecoveryRequired) => Err(PaykitSdkError::RecoveryRequired {
            context: format!("Encrypted Link recovery is required for counterparty {counterparty}"),
            source: None,
        }),
        Some(LinkedPeerState::Blocked) => Err(PaykitSdkError::Policy {
            context: format!("counterparty {counterparty} is blocked"),
            source: None,
        }),
        _ => Err(PaykitSdkError::RecoveryRequired {
            context: format!("no active Encrypted Link snapshot for counterparty {counterparty}"),
            source: None,
        }),
    }
}

pub(crate) fn require_recovery_context(
    peer: &LinkedPeerRecord,
    context: &paykit_lib::EncryptedLinkRecoveryContext,
) -> Result<()> {
    if context.local_attempt_id() != peer.local_recovery_attempt_id.as_deref()
        || context.remote_attempt_id() != peer.remote_recovery_attempt_id.as_deref()
    {
        return Err(PaykitSdkError::RecoveryRequired {
            context: format!(
                "Encrypted Link recovery context changed for counterparty {}",
                peer.counterparty
            ),
            source: None,
        });
    }
    Ok(())
}

/// Result of starting or advancing an Encrypted Link Handshake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPeerHandshakeReport {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Current Linked Peer state after the operation.
    pub state: LinkedPeerState,
    /// Current Encrypted Link state generation.
    pub generation: u64,
    /// In-progress handshake role, when a handshake remains pending.
    pub handshake_role: Option<EncryptedLinkHandshakeRole>,
}

/// Load the durable Linked Peer record for a counterparty.
pub async fn load_linked_peer<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Option<LinkedPeerRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.linked_peer(counterparty)))
        .await
}

pub(crate) fn default_linked_peer(counterparty: PubkyPublicKey) -> LinkedPeerRecord {
    LinkedPeerRecord {
        counterparty,
        state: LinkedPeerState::NotLinked,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
        noise_key_authorization: None,
    }
}

fn ensure_not_blocked(peer: &LinkedPeerRecord) -> Result<()> {
    if peer.state == LinkedPeerState::Blocked {
        return Err(PaykitSdkError::Policy {
            context: format!("Linked Peer {} is blocked", peer.counterparty),
            source: None,
        });
    }
    Ok(())
}

/// Save a Linked Peer state update.
#[cfg(test)]
pub(crate) async fn save_linked_peer_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    save_linked_peer_state_inner(storage, counterparty, state, None, now).await
}

pub(crate) async fn save_linked_peer_state_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    save_linked_peer_state_inner(storage, counterparty, state, Some(lease), now).await
}

async fn save_linked_peer_state_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let lease = lease.clone();
        let state = state.clone();
        move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            } else if tx
                .peer_link_operation_lease(&counterparty)
                .is_some_and(|active_lease| active_lease.expires_at > now)
            {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "peer link operation already in progress for counterparty {counterparty}"
                    ),
                    source: None,
                });
            }
            let mut record = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            if record.state == LinkedPeerState::Blocked && state != LinkedPeerState::Blocked {
                return Err(PaykitSdkError::Policy {
                    context: format!("Linked Peer {counterparty} is blocked"),
                    source: None,
                });
            }
            if matches!(state, LinkedPeerState::Linking | LinkedPeerState::Linked)
                && record.state == state
                && record.failure_count == 0
            {
                return Ok(record);
            }
            record.state = state;
            record.last_sync_at = Some(now);
            if matches!(
                record.state,
                LinkedPeerState::NotLinked | LinkedPeerState::Linking | LinkedPeerState::Linked
            ) {
                record.failure_count = 0;
            }
            tx.save_linked_peer(record.clone());
            Ok(record)
        }
    })
    .await
}

/// Mark a Linked Peer as requiring recovery.
pub(crate) async fn mark_recovery_required_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<()>
where
    S: StorageAdapter,
{
    mark_recovery_required_inner(storage, counterparty, Some(lease), now).await
}

pub(crate) fn mark_recovery_required_in_transaction<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<()>
where
    T: StorageTransaction + ?Sized,
{
    mark_recovery_required_in_transaction_inner(tx, counterparty, now, true, None)
}

pub(crate) fn mark_recovery_required_for_marker_in_transaction<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    remote_attempt_id: Option<&str>,
) -> Result<()>
where
    T: StorageTransaction + ?Sized,
{
    mark_recovery_required_in_transaction_inner(tx, counterparty, now, false, remote_attempt_id)
}

fn mark_recovery_required_in_transaction_inner<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
    bump_existing_episode: bool,
    remote_attempt_id: Option<&str>,
) -> Result<()>
where
    T: StorageTransaction + ?Sized,
{
    let mut record = tx
        .linked_peer(counterparty)
        .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
    ensure_not_blocked(&record)?;
    let new_episode = record.state != LinkedPeerState::RecoveryRequired;
    if let Some(attempt_id) = remote_attempt_id {
        // Following the peer must not rotate our own attempt and trigger a loop.
        record.remote_recovery_attempt_id = Some(attempt_id.to_owned());
        record.remote_recovery_marker_observed_at = Some(now);
    } else if new_episode || record.local_recovery_attempt_id.is_none() {
        let marker = paykit_lib::EncryptedLinkRecoveryMarker::new_v4(
            now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        )?;
        record.local_recovery_attempt_id = Some(marker.attempt_id().to_owned());
        record.local_recovery_marker_created_at = Some(now);
        record.local_recovery_marker_last_error = None;
    }
    record.state = LinkedPeerState::RecoveryRequired;
    if new_episode || record.last_sync_at.is_none() {
        record.last_sync_at = Some(now);
    }
    if new_episode || bump_existing_episode {
        record.failure_count = record.failure_count.saturating_add(1);
    }
    tx.save_linked_peer(record);
    if let Some(link_state) = tx.encrypted_link_state(counterparty) {
        tx.save_encrypted_link_state(EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: link_state.generation.saturating_add(1),
            checkpointed_at: now,
        });
    }
    for message in tx.outbound_private_messages(counterparty) {
        if (message.status == OutboundPrivateMessageStatus::Sent && message.is_unconfirmed_event())
            || matches!(
                message.status,
                OutboundPrivateMessageStatus::Pending
                    | OutboundPrivateMessageStatus::Sending
                    | OutboundPrivateMessageStatus::Failed
                    | OutboundPrivateMessageStatus::RecoveryRequired
            )
        {
            tx.save_outbound_private_message(mark_outbound_recovery_required(
                message,
                "Encrypted Link recovery is required".into(),
                now,
            ))?;
        }
    }
    Ok(())
}

async fn mark_recovery_required_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<()>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let lease = lease.clone();
        move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            } else if tx.peer_link_operation_lease(&counterparty).is_some() {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "peer link operation already in progress for counterparty {counterparty}"
                    ),
                    source: None,
                });
            }
            mark_recovery_required_in_transaction(tx, &counterparty, now)
        }
    })
    .await
}

/// Persist an in-progress Encrypted Link Handshake snapshot.
#[cfg(test)]
pub(crate) async fn save_link_handshake_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    save_link_handshake_state_inner(
        storage,
        counterparty,
        handshake_role,
        handshake_snapshot,
        None,
        now,
    )
    .await
}

/// Persist an in-progress handshake only if the peer link lease is active.
pub(crate) async fn save_link_handshake_state_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    save_link_handshake_state_inner(
        storage,
        counterparty,
        handshake_role,
        handshake_snapshot,
        Some(lease),
        now,
    )
    .await
}

async fn save_link_handshake_state_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let handshake_snapshot = handshake_snapshot.clone();
        let lease = lease.clone();
        move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let mut peer = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&peer)?;
            if lease.is_some() {
                let snapshot =
                    paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(&handshake_snapshot)?;
                require_recovery_context(&peer, snapshot.recovery_context())?;
            }
            peer.state = LinkedPeerState::Linking;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let existing = tx.encrypted_link_state(&counterparty);
            let generation = existing
                .as_ref()
                .map(|record| record.generation.saturating_add(1))
                .unwrap_or_default();
            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: None,
                handshake_snapshot: Some(handshake_snapshot),
                handshake_role: Some(handshake_role),
                generation,
                checkpointed_at: now,
            };

            tx.save_linked_peer(peer.clone());
            tx.save_encrypted_link_state(link_state.clone());
            Ok(LinkedPeerHandshakeReport {
                counterparty,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        }
    })
    .await
}

/// Persist an established Encrypted Link snapshot.
#[cfg(test)]
pub(crate) async fn save_linked_peer_link_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    link_snapshot: Vec<u8>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    retry_storage_transaction(storage, || {
        let counterparty = counterparty.clone();
        let link_snapshot = link_snapshot.clone();
        move |tx| {
            let mut peer = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&peer)?;
            peer.state = LinkedPeerState::Linked;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let existing = tx.encrypted_link_state(&counterparty);
            let generation = existing
                .as_ref()
                .map(|record| record.generation.saturating_add(1))
                .unwrap_or_default();
            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: Some(link_snapshot),
                handshake_snapshot: None,
                handshake_role: None,
                generation,
                checkpointed_at: now,
            };

            tx.save_linked_peer(peer.clone());
            tx.save_encrypted_link_state(link_state.clone());
            requeue_recovery_required_outbound_messages(tx, &counterparty, now)?;
            Ok(LinkedPeerHandshakeReport {
                counterparty,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        }
    })
    .await
}

pub(crate) enum HandshakeCheckpointSave {
    Applied(LinkedPeerHandshakeReport),
    Stale,
}

pub(crate) enum HandshakeCheckpointUpdate {
    Pending(paykit_lib::EncryptedLinkHandshakeSnapshot),
    Complete(paykit_lib::EncryptedLinkSnapshot),
}

pub(crate) fn handshake_checkpoint_is_current(
    tx: &dyn StorageTransaction,
    peer: &LinkedPeerRecord,
    state: &EncryptedLinkStateRecord,
    lease: &PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> bool {
    peer.counterparty == state.counterparty
        && lease.counterparty == state.counterparty
        && lease.expires_at > now
        && tx.peer_link_operation_lease(&state.counterparty).as_ref() == Some(lease)
        && tx.linked_peer(&state.counterparty).as_ref() == Some(peer)
        && tx.encrypted_link_state(&state.counterparty).as_ref() == Some(state)
}

/// Save pending publication or acknowledged completion against its exact source.
/// The caller must check current identity/key/authorization in this transaction;
/// Applied proves checkpoint persistence, not permission to publish by itself.
pub(crate) fn save_handshake_checkpoint_in_transaction(
    tx: &mut dyn StorageTransaction,
    expected_peer: &LinkedPeerRecord,
    expected_state: &EncryptedLinkStateRecord,
    update: &HandshakeCheckpointUpdate,
    lease: &PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<HandshakeCheckpointSave> {
    let counterparty = &expected_state.counterparty;
    if !handshake_checkpoint_is_current(tx, expected_peer, expected_state, lease, now) {
        return Ok(HandshakeCheckpointSave::Stale);
    }
    let (recipient, recovery) = match update {
        HandshakeCheckpointUpdate::Pending(snapshot) => {
            (snapshot.recipient(), snapshot.recovery_context())
        }
        HandshakeCheckpointUpdate::Complete(snapshot) => {
            (snapshot.recipient(), snapshot.recovery_context())
        }
    };
    if expected_peer.state != LinkedPeerState::Linking
        || expected_state.link_snapshot.is_some()
        || expected_state.handshake_snapshot.is_none()
        || expected_state.handshake_role.is_none()
        || PubkyPublicKey::from_public_key(recipient) != *counterparty
    {
        return Err(PaykitSdkError::Policy {
            context: "staged handshake requires the current linking checkpoint".into(),
            source: None,
        });
    }
    require_recovery_context(expected_peer, recovery)?;
    let mut state = expected_state.clone();
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| PaykitSdkError::Policy {
            context: "handshake checkpoint generation exhausted".into(),
            source: None,
        })?;
    state.checkpointed_at = now;
    let mut peer = expected_peer.clone();
    match update {
        HandshakeCheckpointUpdate::Pending(snapshot) => {
            state.handshake_snapshot = Some(snapshot.serialize())
        }
        HandshakeCheckpointUpdate::Complete(snapshot) => {
            state.link_snapshot = Some(snapshot.serialize());
            state.handshake_snapshot = None;
            state.handshake_role = None;
            peer.state = LinkedPeerState::Linked;
            requeue_recovery_required_outbound_messages(tx, counterparty, now)?;
        }
    }
    peer.failure_count = 0;
    peer.last_sync_at = Some(now);
    let report = LinkedPeerHandshakeReport {
        counterparty: counterparty.clone(),
        state: peer.state.clone(),
        generation: state.generation,
        handshake_role: state.handshake_role,
    };
    tx.save_encrypted_link_state(state);
    tx.save_linked_peer(peer);
    Ok(HandshakeCheckpointSave::Applied(report))
}

pub(crate) fn requeue_recovery_required_outbound_messages(
    tx: &mut dyn StorageTransaction,
    counterparty: &PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<()> {
    for mut message in tx.outbound_private_messages(counterparty) {
        if message.status != OutboundPrivateMessageStatus::RecoveryRequired {
            continue;
        }
        if message.confirmed_at.is_some() {
            tx.save_outbound_private_message(crate::domain::outbound_private::mark_outbound_sent(
                message, now,
            ))?;
            continue;
        }
        if !message.is_delivery_confirmation()
            && (!tx.paykit_app_is_registered(&message.app_id)
                || tx.paykit_app_is_retired(&message.app_id))
        {
            continue;
        }
        message.status = OutboundPrivateMessageStatus::Pending;
        message.updated_at = now;
        // Recovery cannot prove that a previous publication missed the peer.
        // Keep attempt evidence for both Event Messages and Private Payment Lists.
        if !message.is_unconfirmed_event() {
            message.sent_at = None;
        }
        message.last_error = None;
        message.prepared_send = None;
        tx.save_outbound_private_message(message)?;
    }
    crate::domain::payment_requests::release_resolved_payment_execution_claims(tx, now)
}

/// Load the durable Encrypted Link state for a counterparty.
pub async fn load_encrypted_link_state<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
) -> Result<Option<EncryptedLinkStateRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.encrypted_link_state(counterparty)))
        .await
}

#[cfg(test)]
mod tests;
