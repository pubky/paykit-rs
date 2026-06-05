//! Linked Peer state records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    storage::{
        require_peer_link_operation_lease, EncryptedLinkStateRecord, LinkedPeerRecord,
        PeerLinkOperationLease, StorageAdapter,
    },
    PaykitSdkError, PubkyPublicKey, Result,
};

/// Local role for an in-progress Encrypted Link Handshake.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncryptedLinkHandshakeRole {
    /// Local peer initiated the handshake.
    Initiator,
    /// Local peer accepted a handshake initiated by the counterparty.
    Responder,
}

/// Local relationship state for a counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkedPeerState {
    /// The counterparty is not known locally.
    Unknown,
    /// The counterparty is known but no active Encrypted Link exists.
    Known,
    /// An Encrypted Link Handshake is in progress.
    Linking,
    /// An Encrypted Link is established.
    Linked,
    /// Local state cannot safely continue without recovery.
    RecoveryRequired,
    /// Local policy blocks this peer.
    Blocked,
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

pub(crate) struct RecoveryRequiredMark {
    pub new_episode: bool,
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

fn default_linked_peer(counterparty: PubkyPublicKey) -> LinkedPeerRecord {
    LinkedPeerRecord {
        counterparty,
        state: LinkedPeerState::Unknown,
        last_sync_at: None,
        last_private_receive_at: None,
        failure_count: 0,
        local_recovery_attempt_id: None,
        local_recovery_marker_created_at: None,
        remote_recovery_attempt_id: None,
        remote_recovery_marker_observed_at: None,
    }
}

fn ensure_not_blocked(peer: &LinkedPeerRecord) -> Result<()> {
    if peer.state == LinkedPeerState::Blocked {
        return Err(PaykitSdkError::Policy(format!(
            "Linked Peer {} is blocked",
            peer.counterparty
        )));
    }
    Ok(())
}

fn report_current_link_state(
    counterparty: PubkyPublicKey,
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
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let mut record = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            if record.state == LinkedPeerState::Blocked && state != LinkedPeerState::Blocked {
                return Err(PaykitSdkError::Policy(format!(
                    "Linked Peer {counterparty} is blocked"
                )));
            }
            record.state = state;
            record.last_sync_at = Some(now);
            if matches!(
                record.state,
                LinkedPeerState::Known | LinkedPeerState::Linking | LinkedPeerState::Linked
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
    mark_recovery_required_inner(storage, counterparty, Some(lease), now).await
}

async fn mark_recovery_required_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
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
            let mut record = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&record)?;
            let new_episode = record.state != LinkedPeerState::RecoveryRequired;
            record.state = LinkedPeerState::RecoveryRequired;
            if new_episode || record.last_sync_at.is_none() {
                record.last_sync_at = Some(now);
            }
            record.failure_count = record.failure_count.saturating_add(1);
            tx.save_linked_peer(record.clone());
            if let Some(link_state) = tx.encrypted_link_state(&counterparty) {
                tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                    counterparty: counterparty.clone(),
                    link_snapshot: None,
                    handshake_snapshot: None,
                    handshake_role: None,
                    generation: link_state.generation.saturating_add(1),
                    checkpointed_at: now,
                });
            }
            Ok(RecoveryRequiredMark { new_episode })
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
    storage
        .transaction(move |tx| {
            if let Some(lease) = lease.as_ref() {
                require_peer_link_operation_lease(tx, lease)?;
            }
            let mut peer = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&peer)?;
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
        })
        .await
}

/// Persist an in-progress handshake only if the stored generation is unchanged.
#[cfg(test)]
pub(crate) async fn save_link_handshake_state_if_generation<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
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
        handshake_role,
        handshake_snapshot,
        expected_generation,
        None,
        now,
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
        handshake_role,
        handshake_snapshot,
        expected_generation,
        Some(lease),
        now,
    )
    .await
}

async fn save_link_handshake_state_if_generation_inner<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    handshake_role: EncryptedLinkHandshakeRole,
    handshake_snapshot: Vec<u8>,
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
            let mut peer = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&peer)?;

            if let Some(existing) = tx.encrypted_link_state(&counterparty) {
                if existing.generation != expected_generation {
                    let report =
                        report_current_link_state(counterparty.clone(), &mut peer, &existing, now);
                    tx.save_linked_peer(peer);
                    return Ok(report);
                }
            }

            peer.state = LinkedPeerState::Linking;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: None,
                handshake_snapshot: Some(handshake_snapshot),
                handshake_role: Some(handshake_role),
                generation: expected_generation.saturating_add(1),
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
    storage
        .transaction(move |tx| {
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
            Ok(LinkedPeerHandshakeReport {
                counterparty,
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
            let mut peer = tx
                .linked_peer(&counterparty)
                .unwrap_or_else(|| default_linked_peer(counterparty.clone()));
            ensure_not_blocked(&peer)?;

            if let Some(existing) = tx.encrypted_link_state(&counterparty) {
                if existing.generation != expected_generation {
                    let report =
                        report_current_link_state(counterparty.clone(), &mut peer, &existing, now);
                    tx.save_linked_peer(peer);
                    return Ok(report);
                }
            }

            peer.state = LinkedPeerState::Linked;
            peer.last_sync_at = Some(now);
            peer.failure_count = 0;

            let link_state = EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
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
) -> Result<Option<EncryptedLinkStateRecord>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| Ok(tx.encrypted_link_state(counterparty)))
        .await
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::storage::InMemoryStorage;

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 4, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    #[tokio::test]
    async fn test_save_link_handshake_state_marks_peer_linking() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let report = save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(report.state, LinkedPeerState::Linking);
        assert_eq!(
            report.handshake_role,
            Some(EncryptedLinkHandshakeRole::Initiator)
        );
        let peer = load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::Linking);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(link_state.handshake_snapshot, Some(vec![1, 2, 3]));
        assert!(link_state.link_snapshot.is_none());
    }

    #[tokio::test]
    async fn test_save_linked_peer_link_state_clears_pending_handshake() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Responder,
            vec![1, 2, 3],
            timestamp(),
        )
        .await
        .unwrap();

        let report =
            save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
                .await
                .unwrap();

        assert_eq!(report.state, LinkedPeerState::Linked);
        assert_eq!(report.handshake_role, None);
        assert_eq!(report.generation, 1);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
        assert!(link_state.handshake_snapshot.is_none());
        assert!(link_state.handshake_role.is_none());
    }

    #[tokio::test]
    async fn test_mark_recovery_required_clears_active_link_snapshot() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
            .await
            .unwrap();

        let mark = mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
            .await
            .unwrap();

        assert!(mark.new_episode);
        let peer = load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert!(link_state.link_snapshot.is_none());
        assert!(link_state.handshake_snapshot.is_none());
        assert!(link_state.handshake_role.is_none());
        assert_eq!(link_state.generation, 1);
    }

    #[tokio::test]
    async fn test_mark_recovery_required_clears_handshake_snapshot() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Responder,
            vec![1, 2, 3],
            timestamp(),
        )
        .await
        .unwrap();

        let mark = mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
            .await
            .unwrap();

        assert!(mark.new_episode);
        let peer = load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert!(link_state.link_snapshot.is_none());
        assert!(link_state.handshake_snapshot.is_none());
        assert!(link_state.handshake_role.is_none());
        assert_eq!(link_state.generation, 1);
    }

    #[tokio::test]
    async fn test_save_link_handshake_state_rejects_blocked_peer() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_linked_peer_state(
            &storage,
            counterparty.clone(),
            LinkedPeerState::Blocked,
            timestamp(),
        )
        .await
        .unwrap();

        let result = save_link_handshake_state(
            &storage,
            counterparty,
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
            timestamp(),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
    }

    #[tokio::test]
    async fn test_generation_checked_handshake_save_keeps_newer_link() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
            timestamp(),
        )
        .await
        .unwrap();
        save_linked_peer_link_state(&storage, counterparty.clone(), vec![4, 5, 6], timestamp())
            .await
            .unwrap();

        let report = save_link_handshake_state_if_generation(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![7, 8, 9],
            0,
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(report.state, LinkedPeerState::Linked);
        assert_eq!(report.generation, 1);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(link_state.link_snapshot, Some(vec![4, 5, 6]));
        assert!(link_state.handshake_snapshot.is_none());
    }

    #[tokio::test]
    async fn test_generation_checked_handshake_save_preserves_recovery_required() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_link_handshake_state(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
            timestamp(),
        )
        .await
        .unwrap();
        mark_recovery_required_inner(&storage, counterparty.clone(), None, timestamp())
            .await
            .unwrap();

        let report = save_link_handshake_state_if_generation(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![7, 8, 9],
            0,
            timestamp(),
        )
        .await
        .unwrap();

        assert_eq!(report.state, LinkedPeerState::RecoveryRequired);
        assert_eq!(report.generation, 1);
        assert_eq!(report.handshake_role, None);
        let peer = load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::RecoveryRequired);
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert!(link_state.link_snapshot.is_none());
        assert!(link_state.handshake_snapshot.is_none());
    }

    #[tokio::test]
    async fn test_lease_checked_handshake_save_rejects_stale_lease() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let first_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();
        let active_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();

        let result = save_link_handshake_state_with_lease(
            &storage,
            counterparty.clone(),
            EncryptedLinkHandshakeRole::Initiator,
            vec![1, 2, 3],
            first_lease,
            timestamp() + chrono::Duration::seconds(12),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        let link_state = load_encrypted_link_state(&storage, &counterparty)
            .await
            .unwrap();
        assert!(link_state.is_none());
        assert_eq!(
            storage
                .transaction({
                    let counterparty = counterparty.clone();
                    move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
                })
                .await
                .unwrap(),
            Some(active_lease)
        );
    }

    #[tokio::test]
    async fn test_lease_checked_recovery_rejects_stale_lease() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        save_linked_peer_state(
            &storage,
            counterparty.clone(),
            LinkedPeerState::Linked,
            timestamp(),
        )
        .await
        .unwrap();

        let first_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();
        let active_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();

        let result = mark_recovery_required_with_lease(
            &storage,
            counterparty.clone(),
            first_lease,
            timestamp() + chrono::Duration::seconds(12),
        )
        .await;

        assert!(matches!(result, Err(PaykitSdkError::Policy(_))));
        let peer = load_linked_peer(&storage, &counterparty)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.state, LinkedPeerState::Linked);
        assert_eq!(
            storage
                .transaction({
                    let counterparty = counterparty.clone();
                    move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
                })
                .await
                .unwrap(),
            Some(active_lease)
        );
    }
}
