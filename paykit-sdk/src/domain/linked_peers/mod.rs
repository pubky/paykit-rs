//! Linked Peer state records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    storage::{
        require_peer_link_operation_lease, EncryptedLinkStateRecord, LinkedPeerRecord,
        PeerLinkOperationLease, StorageAdapter, StorageTransaction,
    },
    PaykitReceiverPath, PaykitSdkError, PubkyPublicKey, Result,
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

/// Result of starting or advancing an Encrypted Link Handshake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPeerHandshakeReport {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Current Linked Peer state after the operation.
    pub state: LinkedPeerState,
    /// Current Encrypted Link state generation.
    pub generation: u64,
    /// In-progress handshake role, when a handshake remains pending.
    pub handshake_role: Option<EncryptedLinkHandshakeRole>,
}

pub(crate) struct RecoveryRequiredMark {
    pub new_episode: bool,
}

struct GenerationCheckedHandshakeState {
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    expected_generation: u64,
    now: DateTime<Utc>,
}

/// Load the durable Linked Peer record for a counterparty.
pub async fn load_linked_peer<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<Option<LinkedPeerRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.linked_peer(counterparty, counterparty_receiver_path)))
        .await
}

pub(crate) fn default_linked_peer(
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
) -> LinkedPeerRecord {
    LinkedPeerRecord {
        counterparty,
        counterparty_receiver_path,
        state: LinkedPeerState::NotLinked,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        local_recovery_marker_last_error: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
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

fn counterparty_receiver_path_for_operation(
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: Option<&PaykitReceiverPath>,
    lease: Option<&PeerLinkOperationLease>,
) -> Result<PaykitReceiverPath> {
    if let Some(lease) = lease {
        return Ok(lease.counterparty_receiver_path.clone());
    }

    counterparty_receiver_path
        .cloned()
        .ok_or_else(|| PaykitSdkError::Policy {
            context: format!(
                "counterparty {counterparty} receiver path is required for private link state"
            ),
            source: None,
        })
}

fn report_current_link_state(
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    peer: &mut LinkedPeerRecord,
    link_state: &EncryptedLinkStateRecord,
    now: DateTime<Utc>,
) -> LinkedPeerHandshakeReport {
    if link_state.link_snapshot.is_some() {
        peer.state = LinkedPeerState::Linked;
        peer.last_sync_at = Some(now);
        peer.failure_count = 0;
        LinkedPeerHandshakeReport {
            counterparty,
            counterparty_receiver_path,
            state: LinkedPeerState::Linked,
            generation: link_state.generation,
            handshake_role: None,
        }
    } else if link_state.handshake_snapshot.is_some() {
        peer.state = LinkedPeerState::Linking;
        peer.last_sync_at = Some(now);
        peer.failure_count = 0;
        LinkedPeerHandshakeReport {
            counterparty,
            counterparty_receiver_path,
            state: LinkedPeerState::Linking,
            generation: link_state.generation,
            handshake_role: link_state.handshake_role,
        }
    } else {
        peer.state = LinkedPeerState::RecoveryRequired;
        if peer.last_sync_at.is_none() {
            peer.last_sync_at = Some(now);
        }
        LinkedPeerHandshakeReport {
            counterparty,
            counterparty_receiver_path,
            state: LinkedPeerState::RecoveryRequired,
            generation: link_state.generation,
            handshake_role: None,
        }
    }
}

/// Save a Linked Peer state update.
#[cfg(test)]
pub(crate) async fn save_linked_peer_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    state: LinkedPeerState,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    save_linked_peer_state_inner(
        storage,
        counterparty,
        state,
        Some(counterparty_receiver_path),
        None,
        now,
    )
    .await
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
    save_linked_peer_state_inner(storage, counterparty, state, None, Some(lease), now).await
}

async fn save_linked_peer_state_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    counterparty_receiver_path: Option<PaykitReceiverPath>,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let counterparty_receiver_path = counterparty_receiver_path_for_operation(
                &counterparty,
                counterparty_receiver_path.as_ref(),
                lease.as_ref(),
            )?;
            if lease.is_none()
                && tx
                    .peer_link_operation_lease(&counterparty, &counterparty_receiver_path)
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
                .linked_peer(&counterparty, &counterparty_receiver_path)
                .unwrap_or_else(|| {
                    default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
                });
            if record.state == LinkedPeerState::Blocked && state != LinkedPeerState::Blocked {
                return Err(PaykitSdkError::Policy {
                    context: format!("Linked Peer {counterparty} is blocked"),
                    source: None,
                });
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
        })
        .await
}

/// Mark a Linked Peer as requiring recovery.
pub(crate) async fn mark_recovery_required_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<RecoveryRequiredMark>
where
    S: StorageAdapter,
{
    mark_recovery_required_inner(storage, counterparty, None, Some(lease), now).await
}

pub(crate) fn mark_recovery_required_in_transaction<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
) -> Result<RecoveryRequiredMark>
where
    T: StorageTransaction + ?Sized,
{
    mark_recovery_required_in_transaction_inner(
        tx,
        counterparty,
        counterparty_receiver_path,
        now,
        true,
    )
}

pub(crate) fn mark_recovery_required_for_marker_in_transaction<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
) -> Result<RecoveryRequiredMark>
where
    T: StorageTransaction + ?Sized,
{
    mark_recovery_required_in_transaction_inner(
        tx,
        counterparty,
        counterparty_receiver_path,
        now,
        false,
    )
}

fn mark_recovery_required_in_transaction_inner<T>(
    tx: &mut T,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
    now: DateTime<Utc>,
    bump_existing_episode: bool,
) -> Result<RecoveryRequiredMark>
where
    T: StorageTransaction + ?Sized,
{
    let mut record = tx
        .linked_peer(counterparty, counterparty_receiver_path)
        .unwrap_or_else(|| {
            default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
        });
    ensure_not_blocked(&record)?;
    let new_episode = record.state != LinkedPeerState::RecoveryRequired;
    record.state = LinkedPeerState::RecoveryRequired;
    if new_episode || record.last_sync_at.is_none() {
        record.last_sync_at = Some(now);
    }
    if new_episode || bump_existing_episode {
        record.failure_count = record.failure_count.saturating_add(1);
    }
    tx.save_linked_peer(record);
    if let Some(link_state) = tx.encrypted_link_state(counterparty, counterparty_receiver_path) {
        tx.save_encrypted_link_state(EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: link_state.counterparty_receiver_path,
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: link_state.generation.saturating_add(1),
            checkpointed_at: now,
        });
    }
    Ok(RecoveryRequiredMark { new_episode })
}

async fn mark_recovery_required_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: Option<PaykitReceiverPath>,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<RecoveryRequiredMark>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let counterparty_receiver_path = counterparty_receiver_path_for_operation(
                &counterparty,
                counterparty_receiver_path.as_ref(),
                lease.as_ref(),
            )?;
            if lease.is_none()
                && tx
                    .peer_link_operation_lease(&counterparty, &counterparty_receiver_path)
                    .is_some_and(|active_lease| active_lease.expires_at > now)
            {
                return Err(PaykitSdkError::Policy {
                    context: format!(
                        "counterparty {counterparty} receiver {} already has an active link operation",
                        counterparty_receiver_path
                    ),
                    source: None,
                });
            }
            mark_recovery_required_in_transaction(
                tx,
                &counterparty,
                &counterparty_receiver_path,
                now,
            )
        })
        .await
}

/// Persist an in-progress Encrypted Link Handshake snapshot.
#[cfg(test)]
pub(crate) async fn save_link_handshake_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
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
        Some(counterparty_receiver_path),
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
        None,
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
    counterparty_receiver_path: Option<PaykitReceiverPath>,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let counterparty_receiver_path = counterparty_receiver_path_for_operation(
                &counterparty,
                counterparty_receiver_path.as_ref(),
                lease.as_ref(),
            )?;
            let mut peer = tx
                .linked_peer(&counterparty, &counterparty_receiver_path)
                .unwrap_or_else(|| {
                    default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
                });
            ensure_not_blocked(&peer)?;
            peer.state = LinkedPeerState::Linking;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let existing = tx.encrypted_link_state(&counterparty, &counterparty_receiver_path);
            let generation = existing
                .as_ref()
                .map(|record| record.generation.saturating_add(1))
                .unwrap_or_default();
            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: counterparty_receiver_path.clone(),
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
                counterparty_receiver_path,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        })
        .await
}

/// Persist an in-progress handshake only if the stored generation is unchanged.
#[cfg(test)]
pub(crate) async fn save_link_handshake_state_if_generation<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    expected_generation: u64,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    save_link_handshake_state_if_generation_inner(
        storage,
        counterparty,
        Some(counterparty_receiver_path),
        GenerationCheckedHandshakeState {
            handshake_role,
            handshake_snapshot,
            expected_generation,
            now,
        },
        None,
    )
    .await
}

/// Persist an in-progress handshake if generation and lease still match.
pub(crate) async fn save_link_handshake_state_if_generation_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
    expected_generation: u64,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    save_link_handshake_state_if_generation_inner(
        storage,
        counterparty,
        None,
        GenerationCheckedHandshakeState {
            handshake_role,
            handshake_snapshot,
            expected_generation,
            now,
        },
        Some(lease),
    )
    .await
}

async fn save_link_handshake_state_if_generation_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: Option<PaykitReceiverPath>,
    handshake: GenerationCheckedHandshakeState,
    lease: Option<PeerLinkOperationLease>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let counterparty_receiver_path = counterparty_receiver_path_for_operation(
                &counterparty,
                counterparty_receiver_path.as_ref(),
                lease.as_ref(),
            )?;
            let mut peer = tx
                .linked_peer(&counterparty, &counterparty_receiver_path)
                .unwrap_or_else(|| {
                    default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
                });
            ensure_not_blocked(&peer)?;

            if let Some(existing) =
                tx.encrypted_link_state(&counterparty, &counterparty_receiver_path)
            {
                if existing.generation != handshake.expected_generation {
                    let report = report_current_link_state(
                        counterparty.clone(),
                        counterparty_receiver_path.clone(),
                        &mut peer,
                        &existing,
                        handshake.now,
                    );
                    tx.save_linked_peer(peer);
                    return Ok(report);
                }
            }

            peer.state = LinkedPeerState::Linking;
            peer.last_sync_at = Some(handshake.now);
            peer.failure_count = 0;

            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: counterparty_receiver_path.clone(),
                link_snapshot: None,
                handshake_snapshot: Some(handshake.handshake_snapshot),
                handshake_role: Some(handshake.handshake_role),
                generation: handshake.expected_generation.saturating_add(1),
                checkpointed_at: handshake.now,
            };

            tx.save_linked_peer(peer.clone());
            tx.save_encrypted_link_state(link_state.clone());
            Ok(LinkedPeerHandshakeReport {
                counterparty,
                counterparty_receiver_path,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        })
        .await
}

/// Persist an established Encrypted Link snapshot.
#[cfg(test)]
pub(crate) async fn save_linked_peer_link_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
    link_snapshot: Vec<u8>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let mut peer = tx
                .linked_peer(&counterparty, &counterparty_receiver_path)
                .unwrap_or_else(|| {
                    default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
                });
            ensure_not_blocked(&peer)?;
            peer.state = LinkedPeerState::Linked;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let existing = tx.encrypted_link_state(&counterparty, &counterparty_receiver_path);
            let generation = existing
                .as_ref()
                .map(|record| record.generation.saturating_add(1))
                .unwrap_or_default();
            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: counterparty_receiver_path.clone(),
                link_snapshot: Some(link_snapshot),
                handshake_snapshot: None,
                handshake_role: None,
                generation,
                checkpointed_at: now,
            };

            tx.save_linked_peer(peer.clone());
            tx.save_encrypted_link_state(link_state.clone());
            Ok(LinkedPeerHandshakeReport {
                counterparty,
                counterparty_receiver_path,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        })
        .await
}

/// Persist an established link if generation and lease still match.
pub(crate) async fn save_linked_peer_link_state_if_generation_with_lease<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    link_snapshot: Vec<u8>,
    expected_generation: u64,
    lease: PeerLinkOperationLease,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    save_linked_peer_link_state_if_generation_inner(
        storage,
        counterparty,
        link_snapshot,
        expected_generation,
        Some(lease),
        now,
    )
    .await
}

async fn save_linked_peer_link_state_if_generation_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    link_snapshot: Vec<u8>,
    expected_generation: u64,
    lease: Option<PeerLinkOperationLease>,
    now: DateTime<Utc>,
) -> Result<LinkedPeerHandshakeReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let counterparty_receiver_path =
                counterparty_receiver_path_for_operation(&counterparty, None, lease.as_ref())?;
            let mut peer = tx
                .linked_peer(&counterparty, &counterparty_receiver_path)
                .unwrap_or_else(|| {
                    default_linked_peer(counterparty.clone(), counterparty_receiver_path.clone())
                });
            ensure_not_blocked(&peer)?;

            if let Some(existing) =
                tx.encrypted_link_state(&counterparty, &counterparty_receiver_path)
            {
                if existing.generation != expected_generation {
                    let report = report_current_link_state(
                        counterparty.clone(),
                        counterparty_receiver_path.clone(),
                        &mut peer,
                        &existing,
                        now,
                    );
                    tx.save_linked_peer(peer);
                    return Ok(report);
                }
            }

            peer.state = LinkedPeerState::Linked;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                counterparty_receiver_path: counterparty_receiver_path.clone(),
                link_snapshot: Some(link_snapshot),
                handshake_snapshot: None,
                handshake_role: None,
                generation: expected_generation.saturating_add(1),
                checkpointed_at: now,
            };

            tx.save_linked_peer(peer.clone());
            tx.save_encrypted_link_state(link_state.clone());
            Ok(LinkedPeerHandshakeReport {
                counterparty,
                counterparty_receiver_path,
                state: peer.state,
                generation: link_state.generation,
                handshake_role: link_state.handshake_role,
            })
        })
        .await
}

/// Load the durable Encrypted Link state for a counterparty.
pub async fn load_encrypted_link_state<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) -> Result<Option<EncryptedLinkStateRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.encrypted_link_state(counterparty, counterparty_receiver_path)))
        .await
}

#[cfg(test)]
mod tests;
