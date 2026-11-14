//! Durable caller identity for an explicitly requested empty-list withdrawal.
use crate::{PaykitAppId, PubkyPublicKey};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Original context for one retained empty Private Payment List.
///
/// Persist this value with the caller's cleanup operation. It is a correlation
/// handle, not publication authority: every field is checked against the stored
/// intent and the current original session before a retry can publish.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivatePaymentListWithdrawal {
    /// Original owner identity.
    pub owner: PubkyPublicKey,
    /// Original local Paykit App.
    pub app_id: PaykitAppId,
    /// Original counterparty.
    pub counterparty: PubkyPublicKey,
    /// Exact original outbound history row.
    pub outbound_message_id: u64,
    /// Opaque digest binding the original grant, signer, block, and packet.
    pub original_binding: String,
}

impl fmt::Debug for PrivatePaymentListWithdrawal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivatePaymentListWithdrawal(<redacted>)")
    }
}

/// Witness that the exact withdrawal was published and durably acknowledged.
///
/// This is not peer receipt, reservation cancellation, or permission to erase
/// credentials. The caller must match the complete original withdrawal handle.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Debug)]
pub struct PrivatePaymentListWithdrawalAcknowledgement {
    /// Exact original withdrawal acknowledged by this operation.
    pub withdrawal: PrivatePaymentListWithdrawal,
}
