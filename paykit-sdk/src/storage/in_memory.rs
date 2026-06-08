use std::sync::{Arc, Mutex};

use super::queue::{
    is_claimable_outbound_private_message, supersede_outdated_private_payment_lists,
};
use super::*;
use crate::OutboundPrivateMessageStatus;

/// In-memory SDK storage implementation for tests and examples.
///
/// This storage is not durable and must not be used for production SDK state.
#[derive(Clone, Debug, Default)]
pub struct InMemoryStorage {
    state: Arc<Mutex<StorageState>>,
}

impl InMemoryStorage {
    /// Create empty in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a copy of the current storage state.
    pub fn snapshot(&self) -> Result<StorageState> {
        Ok(self
            .state
            .lock()
            .map_err(|err| PaykitSdkError::Storage {
                context: "in-memory storage lock poisoned".into(),
                source: Some(anyhow::anyhow!(err.to_string())),
            })?
            .clone())
    }
}

#[async_trait]
impl StorageAdapter for InMemoryStorage {
    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        let mut guard = self.state.lock().map_err(|err| PaykitSdkError::Storage {
            context: "in-memory storage lock poisoned".into(),
            source: Some(anyhow::anyhow!(err.to_string())),
        })?;
        let mut transaction = InMemoryStorageTransaction {
            state: guard.clone(),
        };

        let value = f(&mut transaction)?;
        *guard = transaction.state;
        Ok(value)
    }
}

struct InMemoryStorageTransaction {
    state: StorageState,
}

impl StorageTransaction for InMemoryStorageTransaction {
    fn export_storage_state(&self) -> StorageState {
        self.state.clone()
    }

    fn replace_storage_state(&mut self, state: ValidatedStorageState) {
        self.state = state.into_storage_state();
    }

    fn load_identity_state(&self) -> Option<IdentityState> {
        self.state.identity_state.clone()
    }

    fn save_identity_state(&mut self, state: IdentityState) {
        self.state.identity_state = Some(state);
    }

    fn clear_identity_scoped_state(&mut self) {
        self.clear_private_identity_scoped_state();
        self.state.contact_records.clear();
        self.state.public_endpoint_records.clear();
    }

    fn clear_private_identity_scoped_state(&mut self) {
        self.state.linked_peers.clear();
        self.state.encrypted_link_states.clear();
        self.state.peer_link_operation_leases.clear();
        self.state.payment_endpoint_reservations.clear();
        self.state.outbound_private_messages.clear();
        self.state.private_stream_items.clear();
        self.state.event_dedup_records.clear();
        self.state.receipt_access_records.clear();
        self.state.receipt_records.clear();
    }

    fn linked_peer(&self, counterparty: &PubkyPublicKey) -> Option<LinkedPeerRecord> {
        self.state.linked_peers.get(counterparty).cloned()
    }

    fn save_linked_peer(&mut self, record: LinkedPeerRecord) {
        self.state
            .linked_peers
            .insert(record.counterparty.clone(), record);
    }

    fn contact_records(&self) -> Vec<ContactRecord> {
        let mut records = self
            .state
            .contact_records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.public_key.as_str().cmp(right.public_key.as_str()));
        records
    }

    fn contact_record(&self, public_key: &PubkyPublicKey) -> Option<ContactRecord> {
        self.state.contact_records.get(public_key).cloned()
    }

    fn save_contact_record(&mut self, record: ContactRecord) {
        self.state
            .contact_records
            .insert(record.public_key.clone(), record);
    }

    fn remove_contact_record(&mut self, public_key: &PubkyPublicKey) -> Option<ContactRecord> {
        self.state.contact_records.remove(public_key)
    }

    fn public_endpoint_records(&self) -> Vec<PublicEndpointRecord> {
        let mut records = self
            .state
            .public_endpoint_records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.identifier.cmp(&right.identifier));
        records
    }

    fn save_public_endpoint_record(&mut self, record: PublicEndpointRecord) {
        self.state
            .public_endpoint_records
            .insert(record.identifier.clone(), record);
    }

    fn payment_endpoint_reservations(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<PaymentEndpointReservationRecord> {
        let mut records = self
            .state
            .payment_endpoint_reservations
            .values()
            .filter(|record| &record.counterparty == counterparty)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.created_at);
        records
    }

    fn payment_endpoint_reservation(
        &self,
        counterparty: &PubkyPublicKey,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord> {
        self.state
            .payment_endpoint_reservations
            .get(&(counterparty.clone(), reservation_id.to_owned()))
            .cloned()
    }

    fn save_payment_endpoint_reservation(&mut self, record: PaymentEndpointReservationRecord) {
        self.state.payment_endpoint_reservations.insert(
            (record.counterparty.clone(), record.reservation_id.clone()),
            record,
        );
    }

    fn remove_payment_endpoint_reservation(
        &mut self,
        counterparty: &PubkyPublicKey,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord> {
        self.state
            .payment_endpoint_reservations
            .remove(&(counterparty.clone(), reservation_id.to_owned()))
    }

    fn encrypted_link_state(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<EncryptedLinkStateRecord> {
        self.state.encrypted_link_states.get(counterparty).cloned()
    }

    fn save_encrypted_link_state(&mut self, record: EncryptedLinkStateRecord) {
        self.state
            .encrypted_link_states
            .insert(record.counterparty.clone(), record);
    }

    fn claim_peer_link_operation(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Option<PeerLinkOperationLease> {
        if let Some(existing) = self.state.peer_link_operation_leases.get(counterparty) {
            if existing.expires_at > now {
                return None;
            }
        }

        let lease = PeerLinkOperationLease {
            counterparty: counterparty.clone(),
            lease_id: self.state.next_peer_link_operation_lease_id,
            claimed_at: now,
            expires_at,
        };
        self.state.next_peer_link_operation_lease_id += 1;
        self.state
            .peer_link_operation_leases
            .insert(counterparty.clone(), lease.clone());
        Some(lease)
    }

    fn peer_link_operation_lease(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<PeerLinkOperationLease> {
        self.state
            .peer_link_operation_leases
            .get(counterparty)
            .cloned()
    }

    fn release_peer_link_operation(&mut self, counterparty: &PubkyPublicKey, lease_id: u64) {
        if self
            .state
            .peer_link_operation_leases
            .get(counterparty)
            .is_some_and(|lease| lease.lease_id == lease_id)
        {
            self.state.peer_link_operation_leases.remove(counterparty);
        }
    }

    fn insert_outbound_private_message(
        &mut self,
        message: NewOutboundPrivateMessage,
    ) -> OutboundPrivateMessageRecord {
        let outbound_message_id = self.state.next_outbound_private_message_id;
        self.state.next_outbound_private_message_id += 1;
        let record = OutboundPrivateMessageRecord::from_new(outbound_message_id, message);
        self.state.outbound_private_messages.push(record.clone());
        record
    }

    fn queued_outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<OutboundPrivateMessageRecord> {
        let mut messages = self
            .state
            .outbound_private_messages
            .iter()
            .filter(|message| {
                &message.counterparty == counterparty
                    && matches!(
                        message.status,
                        OutboundPrivateMessageStatus::Pending
                            | OutboundPrivateMessageStatus::Sending
                            | OutboundPrivateMessageStatus::Failed
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.outbound_message_id);
        messages
    }

    fn outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<OutboundPrivateMessageRecord> {
        let mut messages = self
            .state
            .outbound_private_messages
            .iter()
            .filter(|message| &message.counterparty == counterparty)
            .cloned()
            .collect::<Vec<_>>();
        messages.sort_by_key(|message| message.outbound_message_id);
        messages
    }

    fn claim_next_outbound_private_message(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
        failed_retry_after: DateTime<Utc>,
    ) -> Option<OutboundPrivateMessageRecord> {
        supersede_outdated_private_payment_lists(
            &mut self.state,
            counterparty,
            now,
            stale_before,
            failed_retry_after,
        );

        let mut indexes = self
            .state
            .outbound_private_messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                &message.counterparty == counterparty
                    && !matches!(
                        message.status,
                        OutboundPrivateMessageStatus::Sent
                            | OutboundPrivateMessageStatus::Invalid
                            | OutboundPrivateMessageStatus::RecoveryRequired
                            | OutboundPrivateMessageStatus::Superseded
                    )
            })
            .map(|(index, message)| (index, message.outbound_message_id))
            .collect::<Vec<_>>();
        indexes.sort_by_key(|(_, outbound_message_id)| *outbound_message_id);

        let (index, _) = indexes.first().copied()?;
        let message = &mut self.state.outbound_private_messages[index];
        if !is_claimable_outbound_private_message(message, stale_before, failed_retry_after) {
            return None;
        }

        message.status = OutboundPrivateMessageStatus::Sending;
        message.attempt_count = message.attempt_count.saturating_add(1);
        message.last_attempt_at = Some(now);
        message.updated_at = now;
        message.last_error = None;
        Some(message.clone())
    }

    fn save_outbound_private_message(
        &mut self,
        record: OutboundPrivateMessageRecord,
    ) -> Result<()> {
        if let Some(existing) = self
            .state
            .outbound_private_messages
            .iter_mut()
            .find(|message| message.outbound_message_id == record.outbound_message_id)
        {
            *existing = record;
            Ok(())
        } else {
            Err(PaykitSdkError::Storage {
                context: format!(
                    "outbound private message {} does not exist",
                    record.outbound_message_id
                ),
                source: None,
            })
        }
    }

    fn allocate_receive_batch_id(&mut self) -> u64 {
        let receive_batch_id = self.state.next_receive_batch_id;
        self.state.next_receive_batch_id += 1;
        receive_batch_id
    }

    fn insert_private_stream_item(&mut self, item: NewPrivateStreamItem) -> u64 {
        let stream_item_id = self.state.next_private_stream_item_id;
        self.state.next_private_stream_item_id += 1;
        self.state
            .private_stream_items
            .push(PrivateStreamItemRecord::from_new(stream_item_id, item));
        stream_item_id
    }

    fn private_stream_items(&self, counterparty: &PubkyPublicKey) -> Vec<PrivateStreamItemRecord> {
        self.state
            .private_stream_items
            .iter()
            .filter(|item| &item.counterparty == counterparty)
            .cloned()
            .collect()
    }

    fn event_dedup_record(
        &self,
        counterparty: &PubkyPublicKey,
        event_id: &str,
    ) -> Option<EventDedupRecord> {
        self.state
            .event_dedup_records
            .get(&(counterparty.clone(), event_id.to_owned()))
            .cloned()
    }

    fn save_event_dedup_record(&mut self, record: EventDedupRecord) {
        self.state.event_dedup_records.insert(
            (record.counterparty.clone(), record.event_id.clone()),
            record,
        );
    }

    fn save_receipt_access_record(&mut self, record: ReceiptAccessRecord) {
        self.state.receipt_access_records.insert(
            (record.counterparty.clone(), record.event_id.clone()),
            record,
        );
    }

    fn receipt_access_records(&self, counterparty: &PubkyPublicKey) -> Vec<ReceiptAccessRecord> {
        let mut records = self
            .state
            .receipt_access_records
            .values()
            .filter(|record| &record.counterparty == counterparty)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.stream_item_id);
        records
    }

    fn receipt_access_record_by_receipt_id(
        &self,
        counterparty: &PubkyPublicKey,
        receipt_id: &str,
    ) -> Option<ReceiptAccessRecord> {
        self.state
            .receipt_access_records
            .values()
            .filter(|record| {
                &record.counterparty == counterparty && record.receipt_id == receipt_id
            })
            .max_by_key(|record| record.stream_item_id)
            .cloned()
    }

    fn save_receipt_record(&mut self, record: ReceiptRecord) {
        self.state
            .receipt_records
            .insert((record.issuer.clone(), record.receipt_id.clone()), record);
    }

    fn receipt_record(&self, issuer: &PubkyPublicKey, receipt_id: &str) -> Option<ReceiptRecord> {
        self.state
            .receipt_records
            .get(&(issuer.clone(), receipt_id.to_owned()))
            .cloned()
    }
}
