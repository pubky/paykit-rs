use std::{any::Any, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

mod in_memory;
mod queue;
mod records;

pub use in_memory::InMemoryStorage;
pub use records::{
    EncryptedLinkStateRecord, EventDedupRecord, LinkedPeerRecord, NewOutboundPrivateMessage,
    NewPrivateStreamItem, OutboundPrivateMessageRecord, PaymentEndpointReservationRecord,
    PeerLinkOperationLease, PrivateStreamItemRecord, PublicEndpointRecord, StorageState,
};

pub(crate) use queue::outbound_private_queue_head_is_claimable;
pub(crate) use records::NewPrivateStreamItemDetails;

use crate::{
    backup::ValidatedStorageState,
    contacts::ContactRecord,
    identity::{IdentityState, PubkyPublicKey},
    receipts::{ReceiptAccessRecord, ReceiptRecord},
    PaykitSdkError, Result,
};

/// Erased storage transaction callback for boxed storage adapters.
pub type StorageTransactionCallback<'a> =
    Box<dyn FnOnce(&mut dyn StorageTransaction) -> Result<Box<dyn Any + Send>> + Send + 'a>;

/// Durable storage boundary for Paykit SDK.
///
/// Production adapters must provide atomic transactions with monotonic id
/// allocation, stable FIFO ordering for outbound/private-stream records, and
/// lease-aware writes. The SDK assumes all mutation methods called inside one
/// transaction either commit together or roll back together.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Run an atomic storage transaction through an object-safe erased callback.
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>>;

    /// Run an atomic storage transaction.
    async fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        Self: Sized,
        T: Send + 'static,
        F: FnOnce(&mut dyn StorageTransaction) -> Result<T> + Send,
    {
        let result = self
            .transaction_erased(Box::new(move |tx| {
                Ok(Box::new(f(tx)?) as Box<dyn Any + Send>)
            }))
            .await?;
        result
            .downcast::<T>()
            .map(|value| *value)
            .map_err(|_| PaykitSdkError::Storage {
                context: "storage transaction result type mismatch".into(),
                source: None,
            })
    }

    /// Load the current identity state.
    async fn load_identity_state(&self) -> Result<Option<IdentityState>> {
        let result = self
            .transaction_erased(Box::new(|tx| {
                Ok(Box::new(tx.load_identity_state()) as Box<dyn Any + Send>)
            }))
            .await?;
        result
            .downcast::<Option<IdentityState>>()
            .map(|value| *value)
            .map_err(|_| PaykitSdkError::Storage {
                context: "storage transaction result type mismatch".into(),
                source: None,
            })
    }

    /// Save the current identity state atomically.
    async fn save_identity_state(&self, state: IdentityState) -> Result<()> {
        self.transaction_erased(Box::new(move |tx| {
            tx.save_identity_state(state);
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .map(|_| ())
    }
}

#[async_trait]
impl<T> StorageAdapter for Box<T>
where
    T: StorageAdapter + ?Sized,
{
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        (**self).transaction_erased(f).await
    }
}

#[async_trait]
impl<T> StorageAdapter for Arc<T>
where
    T: StorageAdapter + ?Sized,
{
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn Any + Send>> {
        (**self).transaction_erased(f).await
    }
}

/// Mutable operations available inside one storage transaction.
pub trait StorageTransaction {
    /// Export the full logical SDK storage state.
    fn export_storage_state(&self) -> StorageState;

    /// Replace the full logical SDK storage state after backup validation.
    fn replace_storage_state(&mut self, state: ValidatedStorageState);

    /// Load the current identity state.
    fn load_identity_state(&self) -> Option<IdentityState>;

    /// Save the current identity state.
    fn save_identity_state(&mut self, state: IdentityState);

    /// Clear SDK-managed state that belongs to one local identity.
    fn clear_identity_scoped_state(&mut self);

    /// Clear private SDK-managed state that depends on local secret-key access.
    fn clear_private_identity_scoped_state(&mut self);

    /// Load one Linked Peer record.
    fn linked_peer(&self, counterparty: &PubkyPublicKey) -> Option<LinkedPeerRecord>;

    /// Save one Linked Peer record.
    fn save_linked_peer(&mut self, record: LinkedPeerRecord);

    /// List local contact records.
    fn contact_records(&self) -> Vec<ContactRecord>;

    /// Load one local contact record.
    fn contact_record(&self, public_key: &PubkyPublicKey) -> Option<ContactRecord>;

    /// Save one local contact record.
    fn save_contact_record(&mut self, record: ContactRecord);

    /// Remove one local contact record.
    fn remove_contact_record(&mut self, public_key: &PubkyPublicKey) -> Option<ContactRecord>;

    /// List SDK-managed public Payment Endpoint records.
    fn public_endpoint_records(&self) -> Vec<PublicEndpointRecord>;

    /// Save one SDK-managed public Payment Endpoint record.
    fn save_public_endpoint_record(&mut self, record: PublicEndpointRecord);

    /// List Payment Endpoint Reservation records for one counterparty.
    fn payment_endpoint_reservations(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<PaymentEndpointReservationRecord>;

    /// Load one Payment Endpoint Reservation record.
    fn payment_endpoint_reservation(
        &self,
        counterparty: &PubkyPublicKey,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord>;

    /// Save one Payment Endpoint Reservation record.
    fn save_payment_endpoint_reservation(&mut self, record: PaymentEndpointReservationRecord);

    /// Remove one Payment Endpoint Reservation record.
    fn remove_payment_endpoint_reservation(
        &mut self,
        counterparty: &PubkyPublicKey,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord>;

    /// Load one Encrypted Link state record.
    fn encrypted_link_state(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<EncryptedLinkStateRecord>;

    /// Save one Encrypted Link state record.
    fn save_encrypted_link_state(&mut self, record: EncryptedLinkStateRecord);

    /// Claim exclusive local work on one peer's Encrypted Link.
    fn claim_peer_link_operation(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Option<PeerLinkOperationLease>;

    /// Load the active peer link operation lease.
    fn peer_link_operation_lease(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<PeerLinkOperationLease>;

    /// Release a previously claimed peer link operation.
    fn release_peer_link_operation(&mut self, counterparty: &PubkyPublicKey, lease_id: u64);

    /// Insert one outbound private message and return its assigned record.
    fn insert_outbound_private_message(
        &mut self,
        message: NewOutboundPrivateMessage,
    ) -> OutboundPrivateMessageRecord;

    /// List outbound private messages that should be attempted.
    fn queued_outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<OutboundPrivateMessageRecord>;

    /// List all outbound private messages for one counterparty.
    fn outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<OutboundPrivateMessageRecord>;

    /// Claim the next retryable outbound private message for sending.
    ///
    /// Event Messages are claimed FIFO. Private Payment Lists are Latest-State
    /// Messages, so older claimable unsent lists should be marked
    /// [`crate::OutboundPrivateMessageStatus::Superseded`] before claiming.
    /// Stale [`crate::OutboundPrivateMessageStatus::Sending`] records can be
    /// reclaimed only at the queue head so the same message is retried before
    /// later private messages advance the Encrypted Link.
    fn claim_next_outbound_private_message(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
        failed_retry_after: DateTime<Utc>,
    ) -> Option<OutboundPrivateMessageRecord>;

    /// Save updates for one existing outbound private message record.
    fn save_outbound_private_message(&mut self, record: OutboundPrivateMessageRecord)
        -> Result<()>;

    /// Allocate a receive batch id.
    fn allocate_receive_batch_id(&mut self) -> u64;

    /// Insert one private stream item and return its assigned id.
    fn insert_private_stream_item(&mut self, item: NewPrivateStreamItem) -> u64;

    /// List private stream items for a counterparty.
    fn private_stream_items(&self, counterparty: &PubkyPublicKey) -> Vec<PrivateStreamItemRecord>;

    /// Load an Event Message dedupe record.
    fn event_dedup_record(
        &self,
        counterparty: &PubkyPublicKey,
        event_id: &str,
    ) -> Option<EventDedupRecord>;

    /// Save an Event Message dedupe record.
    fn save_event_dedup_record(&mut self, record: EventDedupRecord);

    /// Save one indexed Receipt Access record.
    fn save_receipt_access_record(&mut self, record: ReceiptAccessRecord);

    /// List indexed Receipt Access records for one counterparty.
    fn receipt_access_records(&self, counterparty: &PubkyPublicKey) -> Vec<ReceiptAccessRecord>;

    /// Load the latest indexed Receipt Access record for a receipt.
    fn receipt_access_record_by_receipt_id(
        &self,
        counterparty: &PubkyPublicKey,
        receipt_id: &str,
    ) -> Option<ReceiptAccessRecord>;

    /// Save one decrypted Receipt record.
    fn save_receipt_record(&mut self, record: ReceiptRecord);

    /// Load one decrypted Receipt record.
    fn receipt_record(&self, issuer: &PubkyPublicKey, receipt_id: &str) -> Option<ReceiptRecord>;
}

pub(crate) fn require_peer_link_operation_lease(
    tx: &dyn StorageTransaction,
    lease: &PeerLinkOperationLease,
) -> Result<()> {
    match tx.peer_link_operation_lease(&lease.counterparty) {
        Some(active) if active.lease_id == lease.lease_id => Ok(()),
        _ => Err(PaykitSdkError::Policy(format!(
            "peer link operation lease {} is no longer active for counterparty {}",
            lease.lease_id, lease.counterparty
        ))),
    }
}

#[cfg(test)]
mod tests;
