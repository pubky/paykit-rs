//! Payment Request lifecycle records.

use serde::{Deserialize, Serialize};

/// Local SDK lifecycle state for a Payment Request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaymentRequestLifecycleState {
    /// Proposal has been received or sent.
    Proposed,
    /// Proposal expired before acceptance.
    ProposalExpired,
    /// Payer accepted the request.
    Accepted,
    /// Payer rejected the request.
    Rejected,
    /// Request was canceled.
    Canceled,
    /// Payment Proof was submitted.
    ProofSubmitted,
    /// Recurring request is active.
    ActiveRecurring,
    /// Automatic execution is paused.
    Paused,
    /// Local state requires recovery.
    RecoveryRequired,
    /// Conflicting events made local state invalid.
    InvalidConflict,
}
