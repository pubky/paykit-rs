//! Linked Peer state records.

use serde::{Deserialize, Serialize};

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
