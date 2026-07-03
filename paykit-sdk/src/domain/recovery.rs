//! Encrypted Link recovery marker state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::linked_peers::LinkedPeerState,
    storage::{LinkedPeerRecord, StorageAdapter},
    PaykitReceiverId, PubkyPublicKey, Result,
};

/// Public recovery marker state tracked for one Linked Peer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedLinkRecoveryMarkerReport {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_id: PaykitReceiverId,
    /// Current Linked Peer state.
    pub state: LinkedPeerState,
    /// Locally published recovery attempt id.
    pub local_attempt_id: Option<String>,
    /// Creation time for the local marker payload.
    pub local_marker_created_at: Option<DateTime<Utc>>,
    /// Last local marker publish/remove error, when available.
    pub local_marker_last_error: Option<String>,
    /// Latest observed counterparty recovery attempt id.
    pub remote_attempt_id: Option<String>,
    /// Time the counterparty marker was observed.
    pub remote_marker_observed_at: Option<DateTime<Utc>>,
    /// Whether this operation observed a new counterparty marker.
    pub remote_marker_changed: bool,
}

impl EncryptedLinkRecoveryMarkerReport {
    pub(crate) fn from_peer(peer: &LinkedPeerRecord, remote_marker_changed: bool) -> Self {
        Self {
            counterparty: peer.counterparty.clone(),
            counterparty_receiver_id: peer.counterparty_receiver_id.clone(),
            state: peer.state.clone(),
            local_attempt_id: peer.local_recovery_attempt_id.clone(),
            local_marker_created_at: peer.local_recovery_marker_created_at,
            local_marker_last_error: peer.local_recovery_marker_last_error.clone(),
            remote_attempt_id: peer.remote_recovery_attempt_id.clone(),
            remote_marker_observed_at: peer.remote_recovery_marker_observed_at,
            remote_marker_changed,
        }
    }
}

pub(crate) async fn recovery_marker_report<S>(
    storage: &S,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_id: &PaykitReceiverId,
) -> Result<Option<EncryptedLinkRecoveryMarkerReport>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx
                .linked_peer(counterparty, counterparty_receiver_id)
                .as_ref()
                .map(|peer| EncryptedLinkRecoveryMarkerReport::from_peer(peer, false)))
        })
        .await
}
