use std::{any::Any, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

mod in_memory;
mod queue;
mod records;

pub use in_memory::{run_storage_state_transaction, InMemoryStorage};
pub use records::{
    EncryptedLinkStateRecord, EventDedupRecord, LinkedPeerRecord, NewOutboundPrivateMessage,
    NewPrivateStreamItem, OutboundPrivateMessageRecord, PaymentEndpointReservationRecord,
    PeerLinkOperationLease, PrivateStreamItemClassificationUpdate, PrivateStreamItemRecord,
    PublicEndpointRecord, StorageState,
};

pub(crate) use queue::{
    is_parked_unknown_kind_outbound_message, outbound_private_queue_head_is_claimable,
};
pub(crate) use records::NewPrivateStreamItemDetails;

use crate::{
    backup::ValidatedStorageState,
    domain::contacts::ContactRecord,
    domain::receipts::{ReceiptAccessRecord, ReceiptIssuanceRecord, ReceiptRecord},
    identity::{IdentityState, PubkyPublicKey},
    PaykitReceiverPath, PaykitSdkError, Result,
};

/// Erased storage transaction callback for boxed storage adapters.
pub type StorageTransactionCallback<'a> =
    Box<dyn FnOnce(&mut dyn StorageTransaction) -> Result<Box<dyn Any + Send>> + Send + 'a>;

/// Compatibility generation of SDK storage state semantics.
///
/// This versions the meaning of persisted SDK storage state, not its record
/// layout: generation bumps guard semantic changes that an older reader would
/// mishandle -- redacted parse-error categories in persisted classification,
/// classification normalization, and parked unknown-kind outbound queue
/// heads. New Private Message kinds do NOT bump the generation. Generation 2
/// code reads state persisted under generation 1 or 2 and always writes
/// generation 2.
///
/// The upgrade is lazy: adapters persist only on state change, so a blob
/// whose state is never mutated keeps its stored generation byte for byte.
/// Opening the app does not by itself re-stamp generation-1 state, and
/// rollback to a generation-1 binary remains possible until the first real
/// state change writes generation 2.
///
/// Custom durable Rust [`StorageAdapter`] implementations MUST persist a
/// generation marker equal to this constant alongside their state and MUST
/// refuse to open state persisted under a higher generation. Rollback to an
/// older binary is unsupported for adapters without that fence. The built-in
/// FFI state and backup envelopes enforce this through their envelope version
/// checks.
pub const SDK_STORAGE_STATE_GENERATION: u32 = 2;

/// Oldest storage-state generation this build still reads.
///
/// Internal compatibility bound. It is `pub` only so `paykit-ffi` shares one
/// lockstep min-read bound with this crate; it is not part of the documented
/// public API and may change without notice.
#[doc(hidden)]
pub const SDK_STORAGE_STATE_MIN_READ_GENERATION: u32 = 1;

/// Durable storage boundary for Paykit SDK.
///
/// Production adapters must provide atomic transactions with monotonic id
/// allocation, stable FIFO ordering for outbound/private-stream records, and
/// lease-aware writes. The SDK assumes all mutation methods called inside one
/// transaction either commit together or roll back together.
///
/// # Contract revisions
///
/// - Revision 1: the original transaction contract.
/// - Revision 2: [`StorageTransaction`] gains the four classification
///   normalization mutators (see that trait's contract revisions), and custom
///   durable adapters take on the generation fence duty documented on
///   [`SDK_STORAGE_STATE_GENERATION`]: persist a generation marker equal to
///   that constant and refuse to open state persisted under a higher one.
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
///
/// # Contract revisions
///
/// - Revision 1: the original method set.
/// - Revision 2: adds the four narrowly scoped classification normalization
///   mutators [`update_private_stream_item_classification`](Self::update_private_stream_item_classification),
///   [`remove_event_dedup_record`](Self::remove_event_dedup_record),
///   [`remove_receipt_access_record`](Self::remove_receipt_access_record), and
///   [`remove_receipt_record`](Self::remove_receipt_record). Implementations
///   backed by custom durable storage must also honor the generation fence
///   documented on [`SDK_STORAGE_STATE_GENERATION`].
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
    fn linked_peer(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Option<LinkedPeerRecord>;

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
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Vec<PaymentEndpointReservationRecord>;

    /// Load one Payment Endpoint Reservation record.
    fn payment_endpoint_reservation(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord>;

    /// Save one Payment Endpoint Reservation record.
    fn save_payment_endpoint_reservation(&mut self, record: PaymentEndpointReservationRecord);

    /// Remove one Payment Endpoint Reservation record.
    fn remove_payment_endpoint_reservation(
        &mut self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord>;

    /// Load one Encrypted Link state record.
    fn encrypted_link_state(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Option<EncryptedLinkStateRecord>;

    /// Save one Encrypted Link state record.
    fn save_encrypted_link_state(&mut self, record: EncryptedLinkStateRecord);

    /// Claim exclusive local work on one peer's Encrypted Link.
    fn claim_peer_link_operation(
        &mut self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Option<PeerLinkOperationLease>;

    /// Load the active peer link operation lease.
    fn peer_link_operation_lease(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Option<PeerLinkOperationLease>;

    /// Release a previously claimed peer link operation.
    fn release_peer_link_operation(
        &mut self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        lease_id: u64,
    );

    /// Insert one outbound private message and return its assigned record.
    fn insert_outbound_private_message(
        &mut self,
        message: NewOutboundPrivateMessage,
    ) -> OutboundPrivateMessageRecord;

    /// List outbound private messages that should be attempted.
    fn queued_outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Vec<OutboundPrivateMessageRecord>;

    /// List all outbound private messages for one counterparty.
    fn outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
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
        counterparty_receiver_path: &PaykitReceiverPath,
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
    fn private_stream_items(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Vec<PrivateStreamItemRecord>;

    /// Update the derived classification of one existing private stream item.
    ///
    /// This exists for classification normalization of derived data only; it
    /// is not a general mutation surface. It never changes the raw payload or
    /// the immutable source context of the item. Returns
    /// [`PaykitSdkError::Storage`] when no item with the given stream item id
    /// exists, so normalization fails closed instead of silently skipping.
    fn update_private_stream_item_classification(
        &mut self,
        update: PrivateStreamItemClassificationUpdate,
    ) -> Result<()>;

    /// Load an Event Message dedupe record.
    fn event_dedup_record(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        event_id: &str,
    ) -> Option<EventDedupRecord>;

    /// Save an Event Message dedupe record.
    fn save_event_dedup_record(&mut self, record: EventDedupRecord);

    /// Remove one Event Message dedupe record.
    ///
    /// This exists for classification normalization of derived dedupe indexes
    /// only; it is not a general mutation surface. Returns the removed
    /// record, or `None` when no record exists for the Event ID.
    fn remove_event_dedup_record(
        &mut self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        event_id: &str,
    ) -> Option<EventDedupRecord>;

    /// Save one indexed Receipt Access record.
    fn save_receipt_access_record(&mut self, record: ReceiptAccessRecord);

    /// List indexed Receipt Access records for one counterparty.
    fn receipt_access_records(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Vec<ReceiptAccessRecord>;

    /// Load the latest indexed Receipt Access record for a receipt.
    fn receipt_access_record_by_receipt_id(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        receipt_id: &str,
    ) -> Option<ReceiptAccessRecord>;

    /// Remove one indexed Receipt Access record.
    ///
    /// This exists for classification normalization of derived Receipt Access
    /// indexes only; it is not a general mutation surface. Returns the
    /// removed record, or `None` when no record exists for the Event ID.
    fn remove_receipt_access_record(
        &mut self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        event_id: &str,
    ) -> Option<ReceiptAccessRecord>;

    /// Save one decrypted Receipt record.
    fn save_receipt_record(&mut self, record: ReceiptRecord);

    /// Load one decrypted Receipt record.
    fn receipt_record(
        &self,
        issuer: &PubkyPublicKey,
        issuer_receiver_path: &PaykitReceiverPath,
        receipt_id: &str,
    ) -> Option<ReceiptRecord>;

    /// Remove one decrypted Receipt record.
    ///
    /// This exists for classification normalization of cached Receipts whose
    /// Receipt Access record was removed or rewritten; it is not a general
    /// mutation surface. Returns the removed record, or `None` when no record
    /// exists for the Receipt ID.
    fn remove_receipt_record(
        &mut self,
        issuer: &PubkyPublicKey,
        issuer_receiver_path: &PaykitReceiverPath,
        receipt_id: &str,
    ) -> Option<ReceiptRecord>;

    /// Save one local receipt issuance record.
    fn save_receipt_issuance_record(&mut self, record: ReceiptIssuanceRecord);

    /// List receipt issuance records for one counterparty.
    fn receipt_issuance_records(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
    ) -> Vec<ReceiptIssuanceRecord>;

    /// Load one receipt issuance record.
    fn receipt_issuance_record(
        &self,
        counterparty: &PubkyPublicKey,
        counterparty_receiver_path: &PaykitReceiverPath,
        receipt_id: &str,
    ) -> Option<ReceiptIssuanceRecord>;

    /// Load one receipt issuance record by Receipt ID across counterparties.
    fn receipt_issuance_record_by_receipt_id(
        &self,
        receipt_id: &str,
    ) -> Option<ReceiptIssuanceRecord>;
}

pub(crate) fn require_peer_link_operation_lease(
    tx: &dyn StorageTransaction,
    lease: &PeerLinkOperationLease,
) -> Result<()> {
    match tx.peer_link_operation_lease(&lease.counterparty, &lease.counterparty_receiver_path) {
        Some(active) if active.lease_id == lease.lease_id => Ok(()),
        _ => Err(PaykitSdkError::Policy {
            context: format!(
                "peer link operation lease {} is no longer active for counterparty {}",
                lease.lease_id, lease.counterparty
            ),
            source: None,
        }),
    }
}

#[cfg(test)]
mod tests;
