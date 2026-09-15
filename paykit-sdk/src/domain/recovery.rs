//! Encrypted Link recovery marker state.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::linked_peers::LinkedPeerState,
    storage::{LinkedPeerRecord, StorageAdapter},
    PubkyPublicKey, Result,
};

/// Public recovery marker state tracked for one Linked Peer.
///
/// `local_marker_last_error` mirrors the raw publish/remove failure string from
/// the durable Linked Peer record, which can embed private storage paths or
/// payload material. `Debug` redacts it to length only, matching the
/// `LinkedPeerRecord` idiom, so `format!("{report:?}")` cannot leak it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedLinkRecoveryMarkerReport {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
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

impl fmt::Debug for EncryptedLinkRecoveryMarkerReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedLinkRecoveryMarkerReport")
            .field("counterparty", &self.counterparty)
            .field("state", &self.state)
            .field("local_attempt_id", &self.local_attempt_id)
            .field("local_marker_created_at", &self.local_marker_created_at)
            .field(
                "local_marker_last_error",
                &self
                    .local_marker_last_error
                    .as_ref()
                    .map(|error| format!("<redacted:{} bytes>", error.len())),
            )
            .field("remote_attempt_id", &self.remote_attempt_id)
            .field("remote_marker_observed_at", &self.remote_marker_observed_at)
            .field("remote_marker_changed", &self.remote_marker_changed)
            .finish()
    }
}

impl EncryptedLinkRecoveryMarkerReport {
    pub(crate) fn from_peer(peer: &LinkedPeerRecord, remote_marker_changed: bool) -> Self {
        Self {
            counterparty: peer.counterparty.clone(),
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
) -> Result<Option<EncryptedLinkRecoveryMarkerReport>>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(tx
                .linked_peer(counterparty)
                .as_ref()
                .map(|peer| EncryptedLinkRecoveryMarkerReport::from_peer(peer, false)))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    #[test]
    fn test_recovery_marker_report_debug_redacts_local_marker_error() {
        // The sentinel stands in for a raw publish/remove failure string that may
        // embed private storage paths or payload material. It must never survive
        // into Debug output, including via the `from_peer` copy path.
        let sentinel = "recovery-marker-error-secret";
        let peer = LinkedPeerRecord {
            counterparty: counterparty(),
            state: LinkedPeerState::Linked,
            last_sync_at: None,
            last_private_receive_at: None,
            failure_count: 0,
            local_recovery_attempt_id: None,
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: Some(sentinel.to_string()),
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        };

        let report = EncryptedLinkRecoveryMarkerReport::from_peer(&peer, true);
        // The raw error is retained on the value (only Debug redacts it).
        assert_eq!(report.local_marker_last_error.as_deref(), Some(sentinel));

        let debug = format!("{report:?}");
        assert!(!debug.contains(sentinel));
        assert!(debug.contains("<redacted:"));
    }
}
