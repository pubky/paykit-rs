//! Linked Peer state records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    storage::{EncryptedLinkStateRecord, LinkedPeerRecord, StorageAdapter},
    PubkyPublicKey, Result,
};

/// Local relationship state for a counterparty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkedPeerState {
    /// The counterparty is not known locally.
    Unknown,
    /// The counterparty is known but no active Encrypted Link exists.
    Known,
    /// An Encrypted Link is established.
    Linked,
    /// Local state cannot safely continue without recovery.
    RecoveryRequired,
    /// Local policy blocks this peer.
    Blocked,
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

/// Save a Linked Peer state update.
pub async fn save_linked_peer_state<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    state: LinkedPeerState,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let mut record = tx.linked_peer(&counterparty).unwrap_or(LinkedPeerRecord {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::Unknown,
                last_sync_at: None,
                last_private_receive_at: None,
                failure_count: 0,
            });
            record.state = state;
            record.last_sync_at = Some(now);
            tx.save_linked_peer(record.clone());
            Ok(record)
        })
        .await
}

/// Mark a Linked Peer as requiring recovery.
pub async fn mark_recovery_required<S>(
    storage: &S,
    counterparty: PubkyPublicKey,
    now: DateTime<Utc>,
) -> Result<LinkedPeerRecord>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let mut record = tx.linked_peer(&counterparty).unwrap_or(LinkedPeerRecord {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::Unknown,
                last_sync_at: None,
                last_private_receive_at: None,
                failure_count: 0,
            });
            record.state = LinkedPeerState::RecoveryRequired;
            record.last_sync_at = Some(now);
            record.failure_count = record.failure_count.saturating_add(1);
            tx.save_linked_peer(record.clone());
            Ok(record)
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

/// Save the durable Encrypted Link state for a counterparty.
pub async fn save_encrypted_link_state<S>(
    storage: &S,
    record: EncryptedLinkStateRecord,
) -> Result<EncryptedLinkStateRecord>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            tx.save_encrypted_link_state(record.clone());
            Ok(record)
        })
        .await
}
