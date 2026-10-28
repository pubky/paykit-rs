use std::sync::{Arc, Mutex};

use super::queue::{
    next_claimable_outbound_private_message, supersede_outdated_private_payment_lists,
};
use super::*;
use crate::OutboundPrivateMessageStatus;

fn storage_id_exhausted() -> PaykitSdkError {
    PaykitSdkError::Storage {
        context: "SDK storage id space is exhausted".into(),
        source: None,
    }
}

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

    /// Create in-memory storage from an existing logical state for tests.
    pub fn from_state(state: StorageState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_registered_apps(
        app_ids: impl IntoIterator<Item = paykit_lib::PaykitAppId>,
    ) -> Self {
        let mut state = StorageState::default();
        for app_id in app_ids {
            state.registered_paykit_apps.insert(app_id.clone());
            state.registered_paykit_app_capabilities.insert(
                app_id,
                paykit_lib::PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            );
        }
        Self::from_state(state)
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
    async fn load_local_identity_state(&self) -> Result<Option<IdentityState>> {
        self.load_identity_state().await
    }

    async fn transaction_erased<'a>(
        &self,
        f: StorageTransactionCallback<'a>,
    ) -> Result<Box<dyn std::any::Any + Send>> {
        let mut guard = self.state.lock().map_err(|err| PaykitSdkError::Storage {
            context: "in-memory storage lock poisoned".into(),
            source: Some(anyhow::anyhow!(err.to_string())),
        })?;
        let (updated_state, value) = run_storage_state_transaction(guard.clone(), f)?;
        *guard = updated_state;
        Ok(value)
    }
}

/// Run one logical SDK storage transaction against an owned storage state.
pub fn run_storage_state_transaction(
    state: StorageState,
    f: StorageTransactionCallback<'_>,
) -> Result<(StorageState, Box<dyn std::any::Any + Send>)> {
    let mut transaction = StorageStateTransaction { state };
    let value = f(&mut transaction)?;
    Ok((transaction.state, value))
}

struct StorageStateTransaction {
    state: StorageState,
}

fn advance_private_intake_cursor(
    state: &mut StorageState,
    cursor: PrivateIntakeCursorRecord,
) -> Result<()> {
    if cursor.homeserver.is_empty() {
        return Err(PaykitSdkError::Storage {
            context: "private intake cursor has an empty homeserver".into(),
            source: None,
        });
    }
    if let Some(existing) = state.private_intake_cursors.get(&cursor.subscribed_user) {
        if existing.homeserver != cursor.homeserver {
            return Err(PaykitSdkError::Storage {
                context: "private intake cursor homeserver changed without reconciliation".into(),
                source: None,
            });
        }
        if existing.event_id > cursor.event_id {
            return Err(PaykitSdkError::Storage {
                context: "private intake cursor regressed".into(),
                source: None,
            });
        }
    }
    state
        .private_intake_cursors
        .insert(cursor.subscribed_user.clone(), cursor);
    Ok(())
}

impl StorageTransaction for StorageStateTransaction {
    fn paykit_noise_public_key(&self) -> Option<crate::PubkyPublicKey> {
        self.state.paykit_noise_public_key.clone()
    }

    fn save_paykit_noise_public_key(&mut self, public_key: crate::PubkyPublicKey) {
        self.state.paykit_noise_public_key = Some(public_key);
    }

    fn allowance_accounting_state(&self) -> Option<crate::AllowanceAccountingState> {
        self.state.allowance_accounting.clone()
    }
    fn save_allowance_accounting_state(&mut self, state: crate::AllowanceAccountingState) {
        self.state.allowance_accounting = Some(state);
    }
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

    fn authorized_paykit_apps(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Option<HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>> {
        self.state.authorized_paykit_apps.get(counterparty).cloned()
    }

    fn save_authorized_paykit_apps(
        &mut self,
        counterparty: PubkyPublicKey,
        apps: HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>,
    ) {
        self.state.authorized_paykit_apps.insert(counterparty, apps);
    }

    fn paykit_app_is_registered(&self, app_id: &paykit_lib::PaykitAppId) -> bool {
        self.state.registered_paykit_apps.contains(app_id)
    }

    fn paykit_app_capabilities(
        &self,
        app_id: &paykit_lib::PaykitAppId,
    ) -> Option<paykit_lib::PaykitAppCapabilities> {
        self.state
            .registered_paykit_app_capabilities
            .get(app_id)
            .copied()
    }

    fn paykit_app_is_retired(&self, app_id: &paykit_lib::PaykitAppId) -> bool {
        self.state.retired_paykit_apps.contains(app_id)
    }

    fn retire_paykit_app(&mut self, app_id: paykit_lib::PaykitAppId) {
        self.state.registered_paykit_apps.remove(&app_id);
        self.state.retired_paykit_apps.insert(app_id);
    }

    fn save_paykit_app_capabilities(
        &mut self,
        app_id: &paykit_lib::PaykitAppId,
        capabilities: paykit_lib::PaykitAppCapabilities,
    ) {
        self.state
            .registered_paykit_app_capabilities
            .insert(app_id.clone(), capabilities);
    }

    fn activate_paykit_app(&mut self, app_id: &paykit_lib::PaykitAppId) {
        self.state.registered_paykit_apps.insert(app_id.clone());
        self.state.retired_paykit_apps.remove(app_id);
    }

    fn public_endpoint_records(&self) -> Vec<PublicEndpointRecord> {
        let mut records = self
            .state
            .public_endpoint_records
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.app_id
                .as_str()
                .cmp(right.app_id.as_str())
                .then_with(|| left.identifier.cmp(&right.identifier))
        });
        records
    }

    fn save_public_endpoint_record(&mut self, record: PublicEndpointRecord) {
        self.state
            .public_endpoint_records
            .insert((record.app_id.clone(), record.identifier.clone()), record);
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
        app_id: &paykit_lib::PaykitAppId,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord> {
        self.state
            .payment_endpoint_reservations
            .get(&(
                counterparty.clone(),
                app_id.clone(),
                reservation_id.to_owned(),
            ))
            .cloned()
    }

    fn save_payment_endpoint_reservation(&mut self, record: PaymentEndpointReservationRecord) {
        self.state.payment_endpoint_reservations.insert(
            (
                record.counterparty.clone(),
                record.app_id.clone(),
                record.reservation_id.clone(),
            ),
            record,
        );
    }

    fn remove_payment_endpoint_reservation(
        &mut self,
        counterparty: &PubkyPublicKey,
        app_id: &paykit_lib::PaykitAppId,
        reservation_id: &str,
    ) -> Option<PaymentEndpointReservationRecord> {
        self.state.payment_endpoint_reservations.remove(&(
            counterparty.clone(),
            app_id.clone(),
            reservation_id.to_owned(),
        ))
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

    fn buffer_private_intake_packet(
        &mut self,
        packet: BufferedPrivatePacketRecord,
    ) -> Result<PrivateIntakeBufferOutcome> {
        if packet.ciphertext.len() != PRIVATE_INTAKE_PACKET_BYTES {
            return Err(PaykitSdkError::Storage {
                context: "private intake packet has invalid ciphertext length".into(),
                source: None,
            });
        }
        let link = self
            .state
            .encrypted_link_states
            .get(&packet.counterparty)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake packet has no Encrypted Link state".into(),
                source: None,
            })?;
        if link.generation != packet.link_generation {
            return Err(PaykitSdkError::Storage {
                context: "private intake packet generation does not match Encrypted Link".into(),
                source: None,
            });
        }

        if let Some(existing) = self.state.private_intake_peers.get(&packet.counterparty) {
            if existing.link_generation != packet.link_generation {
                return Err(PaykitSdkError::Storage {
                    context: "private intake peer generation changed with buffered state".into(),
                    source: None,
                });
            }
            if let Some(buffered) = existing
                .buffered_packets
                .iter()
                .find(|buffered| buffered.receive_counter == packet.receive_counter)
            {
                if buffered.ciphertext != packet.ciphertext {
                    return Err(PaykitSdkError::Storage {
                        context: "private intake receive slot changed ciphertext".into(),
                        source: None,
                    });
                }
                return Ok(PrivateIntakeBufferOutcome::AlreadyBuffered);
            }
            if let Some(tail) = existing.buffered_packets.last() {
                let expected =
                    tail.receive_counter
                        .checked_add(1)
                        .ok_or_else(|| PaykitSdkError::Storage {
                            context: "private intake receive counter is exhausted".into(),
                            source: None,
                        })?;
                if packet.receive_counter != expected {
                    return Err(PaykitSdkError::Storage {
                        context: "private intake packet is not the next ordered receive slot"
                            .into(),
                        source: None,
                    });
                }
            }
        }

        let peer_count = self
            .state
            .private_intake_peers
            .get(&packet.counterparty)
            .map_or(0, |record| record.buffered_packets.len());
        let global_count = self
            .state
            .private_intake_peers
            .values()
            .try_fold(0usize, |total, record| {
                total.checked_add(record.buffered_packets.len())
            })
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake packet count overflow".into(),
                source: None,
            })?;
        let peer_bytes = peer_count
            .checked_mul(PRIVATE_INTAKE_PACKET_BYTES)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake peer byte count overflow".into(),
                source: None,
            })?;
        let global_bytes = global_count
            .checked_mul(PRIVATE_INTAKE_PACKET_BYTES)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake global byte count overflow".into(),
                source: None,
            })?;
        if peer_count >= PRIVATE_INTAKE_MAX_PACKETS_PER_PEER
            || global_count >= PRIVATE_INTAKE_MAX_PACKETS_GLOBAL
            || peer_bytes
                .checked_add(packet.ciphertext.len())
                .is_none_or(|bytes| bytes > PRIVATE_INTAKE_MAX_BYTES_PER_PEER)
            || global_bytes
                .checked_add(packet.ciphertext.len())
                .is_none_or(|bytes| bytes > PRIVATE_INTAKE_MAX_BYTES_GLOBAL)
        {
            return Ok(PrivateIntakeBufferOutcome::CapacityReached);
        }

        if let Some(cursor) = packet.source_cursor.clone() {
            advance_private_intake_cursor(&mut self.state, cursor)?;
        }
        let peer = self
            .state
            .private_intake_peers
            .entry(packet.counterparty.clone())
            .or_insert_with(|| PrivateIntakePeerRecord {
                counterparty: packet.counterparty.clone(),
                link_generation: packet.link_generation,
                buffered_packets: Vec::new(),
                cancelled_handshake_through_generation: None,
            });
        peer.buffered_packets.push(packet);
        Ok(PrivateIntakeBufferOutcome::Inserted)
    }

    fn advance_private_intake_cursor(&mut self, cursor: PrivateIntakeCursorRecord) -> Result<()> {
        advance_private_intake_cursor(&mut self.state, cursor)
    }

    fn reconcile_private_intake_cursor_homeserver(
        &mut self,
        subscribed_user: &PubkyPublicKey,
        homeserver: &str,
    ) {
        if self
            .state
            .private_intake_cursors
            .get(subscribed_user)
            .is_some_and(|cursor| cursor.homeserver != homeserver)
        {
            self.state.private_intake_cursors.remove(subscribed_user);
        }
    }

    fn remove_buffered_private_intake_packet(
        &mut self,
        counterparty: &PubkyPublicKey,
        link_generation: u64,
        receive_counter: u64,
    ) -> Result<BufferedPrivatePacketRecord> {
        let peer = self
            .state
            .private_intake_peers
            .get_mut(counterparty)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake peer state is missing".into(),
                source: None,
            })?;
        if peer.link_generation != link_generation {
            return Err(PaykitSdkError::Storage {
                context: "private intake dequeue generation mismatch".into(),
                source: None,
            });
        }
        if peer
            .buffered_packets
            .first()
            .is_none_or(|packet| packet.receive_counter != receive_counter)
        {
            return Err(PaykitSdkError::Storage {
                context: "private intake dequeue must match the ordered queue head".into(),
                source: None,
            });
        }
        Ok(peer.buffered_packets.remove(0))
    }

    fn cancel_private_intake_handshake_generation(
        &mut self,
        counterparty: &PubkyPublicKey,
        generation: u64,
    ) -> Result<()> {
        let link = self
            .state
            .encrypted_link_states
            .get_mut(counterparty)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake cancellation has no Encrypted Link state".into(),
                source: None,
            })?;
        if link.generation != generation {
            return Err(PaykitSdkError::Storage {
                context: "private intake cancellation generation mismatch".into(),
                source: None,
            });
        }
        let next_generation = generation
            .checked_add(1)
            .ok_or_else(|| PaykitSdkError::Storage {
                context: "private intake cancellation generation is exhausted".into(),
                source: None,
            })?;
        link.link_snapshot = None;
        link.handshake_snapshot = None;
        link.handshake_role = None;
        link.generation = next_generation;
        let peer = self
            .state
            .private_intake_peers
            .entry(counterparty.clone())
            .or_insert_with(|| PrivateIntakePeerRecord {
                counterparty: counterparty.clone(),
                link_generation: next_generation,
                buffered_packets: Vec::new(),
                cancelled_handshake_through_generation: None,
            });
        if peer.link_generation != generation && peer.link_generation != next_generation {
            return Err(PaykitSdkError::Storage {
                context: "private intake cancellation conflicts with buffered generation".into(),
                source: None,
            });
        }
        peer.link_generation = next_generation;
        peer.buffered_packets.clear();
        peer.cancelled_handshake_through_generation = Some(
            peer.cancelled_handshake_through_generation
                .map_or(generation, |current| current.max(generation)),
        );
        Ok(())
    }

    fn claim_peer_link_operation(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<PeerLinkOperationLease>> {
        if let Some(existing) = self.state.peer_link_operation_leases.get(counterparty) {
            if existing.expires_at > now {
                return Ok(None);
            }
        }

        let next_lease_id = self
            .state
            .next_peer_link_operation_lease_id
            .checked_add(1)
            .ok_or_else(storage_id_exhausted)?;
        let lease = PeerLinkOperationLease {
            counterparty: counterparty.clone(),
            lease_id: self.state.next_peer_link_operation_lease_id,
            claimed_at: now,
            expires_at,
        };
        self.state.next_peer_link_operation_lease_id = next_lease_id;
        self.state
            .peer_link_operation_leases
            .insert(counterparty.clone(), lease.clone());
        Ok(Some(lease))
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

    fn claim_paykit_app_operation(
        &mut self,
        app_id: &paykit_lib::PaykitAppId,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<PaykitAppOperationLease>> {
        if self
            .state
            .paykit_app_operation_leases
            .get(app_id)
            .is_some_and(|existing| existing.expires_at > now)
        {
            return Ok(None);
        }
        let next_lease_id = self
            .state
            .next_paykit_app_operation_lease_id
            .checked_add(1)
            .ok_or_else(storage_id_exhausted)?;
        let lease = PaykitAppOperationLease {
            app_id: app_id.clone(),
            lease_id: self.state.next_paykit_app_operation_lease_id,
            claimed_at: now,
            expires_at,
        };
        self.state.next_paykit_app_operation_lease_id = next_lease_id;
        self.state
            .paykit_app_operation_leases
            .insert(app_id.clone(), lease.clone());
        Ok(Some(lease))
    }

    fn paykit_app_operation_lease(
        &self,
        app_id: &paykit_lib::PaykitAppId,
    ) -> Option<PaykitAppOperationLease> {
        self.state.paykit_app_operation_leases.get(app_id).cloned()
    }

    fn release_paykit_app_operation(&mut self, app_id: &paykit_lib::PaykitAppId, lease_id: u64) {
        if self
            .state
            .paykit_app_operation_leases
            .get(app_id)
            .is_some_and(|lease| lease.lease_id == lease_id)
        {
            self.state.paykit_app_operation_leases.remove(app_id);
        }
    }

    fn renew_paykit_app_operation(
        &mut self,
        app_id: &paykit_lib::PaykitAppId,
        lease_id: u64,
        expires_at: DateTime<Utc>,
    ) -> bool {
        let Some(lease) = self.state.paykit_app_operation_leases.get_mut(app_id) else {
            return false;
        };
        if lease.lease_id != lease_id {
            return false;
        }
        lease.expires_at = lease.expires_at.max(expires_at);
        true
    }

    fn claim_private_intake_supervisor(
        &mut self,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<Option<PrivateIntakeSupervisorLease>> {
        if self
            .state
            .private_intake_supervisor_lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at > now)
        {
            return Ok(None);
        }
        let lease_id = self.state.next_private_intake_supervisor_lease_id;
        self.state.next_private_intake_supervisor_lease_id =
            lease_id.checked_add(1).ok_or_else(storage_id_exhausted)?;
        let lease = PrivateIntakeSupervisorLease {
            lease_id,
            claimed_at: now,
            expires_at,
        };
        self.state.private_intake_supervisor_lease = Some(lease.clone());
        Ok(Some(lease))
    }

    fn renew_private_intake_supervisor(
        &mut self,
        lease_id: u64,
        expires_at: DateTime<Utc>,
    ) -> bool {
        let Some(lease) = self.state.private_intake_supervisor_lease.as_mut() else {
            return false;
        };
        if lease.lease_id != lease_id {
            return false;
        }
        lease.expires_at = lease.expires_at.max(expires_at);
        true
    }

    fn release_private_intake_supervisor(&mut self, lease_id: u64) {
        if self
            .state
            .private_intake_supervisor_lease
            .as_ref()
            .is_some_and(|lease| lease.lease_id == lease_id)
        {
            self.state.private_intake_supervisor_lease = None;
        }
    }

    fn payment_request_execution_claim(
        &self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &str,
    ) -> Option<PaymentRequestExecutionClaim> {
        self.state
            .payment_request_execution_claims
            .get(&(counterparty.clone(), payment_request_id.to_owned()))
            .cloned()
    }

    fn has_payment_request_execution_claims(&self, counterparty: &PubkyPublicKey) -> bool {
        self.state
            .payment_request_execution_claims
            .keys()
            .any(|(owner, _)| owner == counterparty)
    }

    fn save_payment_request_execution_claim(&mut self, claim: PaymentRequestExecutionClaim) {
        self.state.payment_request_execution_claims.insert(
            (claim.counterparty.clone(), claim.payment_request_id.clone()),
            claim,
        );
    }

    fn remove_payment_request_execution_claim(
        &mut self,
        counterparty: &PubkyPublicKey,
        payment_request_id: &str,
    ) -> Option<PaymentRequestExecutionClaim> {
        self.state
            .payment_request_execution_claims
            .remove(&(counterparty.clone(), payment_request_id.to_owned()))
    }

    fn insert_outbound_private_message(
        &mut self,
        message: NewOutboundPrivateMessage,
    ) -> Result<OutboundPrivateMessageRecord> {
        let outbound_message_id = self.state.next_outbound_private_message_id;
        self.state.next_outbound_private_message_id = outbound_message_id
            .checked_add(1)
            .ok_or_else(storage_id_exhausted)?;
        let record = OutboundPrivateMessageRecord::from_new(outbound_message_id, message);
        self.state.outbound_private_messages.push(record.clone());
        Ok(record)
    }

    fn queued_outbound_private_messages(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<OutboundPrivateMessageRecord> {
        let mut messages = self
            .state
            .outbound_private_messages
            .iter()
            .filter(|message| &message.counterparty == counterparty && message.is_queued())
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
        supersede_outdated_private_payment_lists(&mut self.state, counterparty, now);

        let messages = self.outbound_private_messages(counterparty);
        let outbound_message_id = next_claimable_outbound_private_message(
            &messages,
            &self.state.registered_paykit_apps,
            &self.state.retired_paykit_apps,
            stale_before,
            failed_retry_after,
        )?;
        let message = self
            .state
            .outbound_private_messages
            .iter_mut()
            .find(|message| message.outbound_message_id == outbound_message_id)?;

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

    fn allocate_receive_batch_id(&mut self) -> Result<u64> {
        let receive_batch_id = self.state.next_receive_batch_id;
        self.state.next_receive_batch_id = receive_batch_id
            .checked_add(1)
            .ok_or_else(storage_id_exhausted)?;
        Ok(receive_batch_id)
    }

    fn insert_private_stream_item(&mut self, item: NewPrivateStreamItem) -> Result<u64> {
        let stream_item_id = self.state.next_private_stream_item_id;
        self.state.next_private_stream_item_id = stream_item_id
            .checked_add(1)
            .ok_or_else(storage_id_exhausted)?;
        self.state
            .private_stream_items
            .push(PrivateStreamItemRecord::from_new(stream_item_id, item));
        Ok(stream_item_id)
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

    fn save_receipt_issuance_record(&mut self, record: ReceiptIssuanceRecord) {
        self.state.receipt_issuance_records.insert(
            (record.counterparty.clone(), record.receipt_id.clone()),
            record,
        );
    }

    fn receipt_issuance_records(
        &self,
        counterparty: &PubkyPublicKey,
    ) -> Vec<ReceiptIssuanceRecord> {
        let mut records = self
            .state
            .receipt_issuance_records
            .values()
            .filter(|record| &record.counterparty == counterparty)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.created_at);
        records
    }

    fn receipt_issuance_record(
        &self,
        counterparty: &PubkyPublicKey,
        receipt_id: &str,
    ) -> Option<ReceiptIssuanceRecord> {
        self.state
            .receipt_issuance_records
            .get(&(counterparty.clone(), receipt_id.to_owned()))
            .cloned()
    }

    fn receipt_issuance_record_by_receipt_id(
        &self,
        receipt_id: &str,
    ) -> Option<ReceiptIssuanceRecord> {
        self.state
            .receipt_issuance_records
            .values()
            .find(|record| record.receipt_id == receipt_id)
            .cloned()
    }
}
