use paykit_sdk::PaykitStateObservation;

use crate::{
    session::app_public_key, FfiContactRecord, FfiLinkedPeerRecord, FfiPaykitSdk,
    FfiPaymentRequestRecord, FfiReceiptRecord, PaykitFfiError,
};

/// Stored Paykit data for display, never for payment authorization or execution.
#[derive(uniffi::Record, Clone)]
pub struct FfiPaykitStateObservation {
    /// Identity owning this observation; discard cached views on identity changes.
    pub public_key: String,
    /// Paykit key generation; discard cached views when the generation changes.
    pub key_generation: u64,
    /// RFC3339 local observation time, not a remote freshness guarantee.
    pub observed_at: String,
    /// Previously stored Contact Records.
    pub contacts: Vec<FfiContactRecord>,
    /// Previously stored link states, not current peer reachability.
    pub linked_peers: Vec<FfiLinkedPeerRecord>,
    /// Previously stored Payment Requests from non-blocked counterparties.
    pub payment_requests: Vec<FfiPaymentRequestRecord>,
    /// Previously decrypted Receipts with valid stored access.
    pub receipts: Vec<FfiReceiptRecord>,
}

impl std::fmt::Debug for FfiPaykitStateObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FfiPaykitStateObservation")
            .field("public_key", &"<redacted>")
            .field("key_generation", &self.key_generation)
            .field("observed_at", &self.observed_at)
            .field("contacts", &self.contacts.len())
            .field("linked_peers", &self.linked_peers.len())
            .field("payment_requests", &self.payment_requests.len())
            .field("receipts", &self.receipts.len())
            .finish()
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Refresh display observations together through one shared-state operation.
    ///
    /// Requires an initialized identity and live private-link-capable session.
    /// Call from a coalesced refresh worker; display the app's last cached view
    /// without waiting for this operation. The result does not authorize payment,
    /// refresh peer messages or confirm current remote app permissions. Discard
    /// cached views when session access, identity or key generation changes.
    pub async fn observe_state(&self) -> Result<FfiPaykitStateObservation, PaykitFfiError> {
        self.runtime.observe_state().await?.try_into()
    }
}

impl TryFrom<PaykitStateObservation> for FfiPaykitStateObservation {
    type Error = PaykitFfiError;

    fn try_from(value: PaykitStateObservation) -> Result<Self, Self::Error> {
        Ok(Self {
            public_key: app_public_key(&value.public_key),
            key_generation: value.key_generation,
            observed_at: value.observed_at.to_rfc3339(),
            contacts: value.contacts.into_iter().map(Into::into).collect(),
            linked_peers: value.linked_peers.into_iter().map(Into::into).collect(),
            payment_requests: value
                .payment_requests
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            receipts: value
                .receipts
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
        })
    }
}
