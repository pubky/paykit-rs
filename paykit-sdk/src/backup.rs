//! SDK backup and restore state.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{
    contacts::ContactRecord,
    endpoint_reservations::reservation_payload_hash,
    identity::{IdentityState, PubkyIdentityCapability, PubkyPublicKey},
    linked_peers::LinkedPeerState,
    outbound_private::validate_queued_outbound_private_message,
    private_stream::{
        classify_private_application_message, payload_hash, PrivateStreamParseStatus,
    },
    receipts::{receipt_access_key_hash, ReceiptAccessRecord, ReceiptRecord},
    storage::{
        EncryptedLinkStateRecord, EventDedupRecord, LinkedPeerRecord, OutboundPrivateMessageRecord,
        PaymentEndpointReservationRecord, PrivateStreamItemRecord, PublicEndpointRecord,
        StorageAdapter, StorageState,
    },
    PaykitSdkError, Result,
};
use paykit_lib::{
    parse_private_payment_list_json, PaymentEndpointIdentifier, PrivateApplicationMessage,
    PrivateMessageKind, ReceiptId,
};

/// Current SDK backup schema version.
pub const SDK_BACKUP_VERSION: u32 = 1;

/// Versioned SDK-managed backup payload.
///
/// This payload includes private SDK recovery data such as Encrypted Link
/// snapshots, raw Private Application Messages, outbound payloads, and Receipt
/// Decryption Keys. Store and transport it with caller-managed encryption.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkBackupState {
    /// Backup schema version.
    pub version: u32,
    /// Current identity state.
    pub identity_state: Option<IdentityState>,
    /// Linked Peer records.
    pub linked_peers: Vec<LinkedPeerRecord>,
    /// Local contact records.
    pub contact_records: Vec<ContactRecord>,
    /// Public Payment Endpoint records.
    pub public_endpoint_records: Vec<PublicEndpointRecord>,
    /// Payment Endpoint Reservation records.
    pub payment_endpoint_reservations: Vec<PaymentEndpointReservationRecord>,
    /// Encrypted Link state records.
    pub encrypted_link_states: Vec<EncryptedLinkStateRecord>,
    /// Outbound Private Application Message records.
    pub outbound_private_messages: Vec<OutboundPrivateMessageRecord>,
    /// Private stream item records.
    pub private_stream_items: Vec<PrivateStreamItemRecord>,
    /// Event Message dedupe records.
    pub event_dedup_records: Vec<EventDedupRecord>,
    /// Receipt Access records.
    pub receipt_access_records: Vec<ReceiptAccessRecord>,
    /// Decrypted Receipt records.
    pub receipt_records: Vec<ReceiptRecord>,
    /// Next outbound Private Application Message id.
    pub next_outbound_private_message_id: u64,
    /// Next private receive batch id.
    pub next_receive_batch_id: u64,
    /// Next private stream item id.
    pub next_private_stream_item_id: u64,
}

impl fmt::Debug for SdkBackupState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdkBackupState")
            .field("version", &self.version)
            .field("identity_state", &self.identity_state)
            .field("linked_peers", &self.linked_peers.len())
            .field("contact_records", &self.contact_records.len())
            .field(
                "public_endpoint_records",
                &self.public_endpoint_records.len(),
            )
            .field(
                "payment_endpoint_reservations",
                &self.payment_endpoint_reservations.len(),
            )
            .field("encrypted_link_states", &self.encrypted_link_states.len())
            .field(
                "outbound_private_messages",
                &self.outbound_private_messages.len(),
            )
            .field("private_stream_items", &self.private_stream_items.len())
            .field("event_dedup_records", &self.event_dedup_records.len())
            .field("receipt_access_records", &self.receipt_access_records.len())
            .field("receipt_records", &self.receipt_records.len())
            .field(
                "next_outbound_private_message_id",
                &self.next_outbound_private_message_id,
            )
            .field("next_receive_batch_id", &self.next_receive_batch_id)
            .field(
                "next_private_stream_item_id",
                &self.next_private_stream_item_id,
            )
            .finish()
    }
}

/// Report returned after restoring SDK-managed backup state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    /// Restored backup schema version.
    pub version: u32,
    /// Whether identity state was restored.
    pub restored_identity: bool,
    /// Number of restored Linked Peer records.
    pub linked_peers: usize,
    /// Number of restored local contact records.
    pub contact_records: usize,
    /// Number of restored public Payment Endpoint records.
    pub public_endpoint_records: usize,
    /// Number of restored Payment Endpoint Reservation records.
    pub payment_endpoint_reservations: usize,
    /// Number of restored Encrypted Link state records.
    pub encrypted_link_states: usize,
    /// Number of restored outbound Private Application Message records.
    pub outbound_private_messages: usize,
    /// Number of restored private stream item records.
    pub private_stream_items: usize,
    /// Number of restored Event Message dedupe records.
    pub event_dedup_records: usize,
    /// Number of restored Receipt Access records.
    pub receipt_access_records: usize,
    /// Number of restored decrypted Receipt records.
    pub receipt_records: usize,
    /// Counterparties marked recovery-required during restore.
    pub recovery_required_peers: Vec<PubkyPublicKey>,
}

/// Backup-validated storage replacement payload.
///
/// This type is intentionally opaque to callers. It prevents arbitrary full
/// storage replacement through [`StorageTransaction`](crate::StorageTransaction)
/// while still letting storage adapters apply validated backup state.
pub struct ValidatedStorageState {
    state: StorageState,
}

impl ValidatedStorageState {
    pub(crate) fn new(state: StorageState) -> Self {
        Self { state }
    }

    /// Consume the validated wrapper and return the storage state.
    pub fn into_storage_state(self) -> StorageState {
        self.state
    }
}

/// Export SDK-managed state from storage.
pub async fn export_backup_state<S>(storage: &S) -> Result<SdkBackupState>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(SdkBackupState::from_storage_state(
                tx.export_storage_state(),
            ))
        })
        .await
}

pub(crate) async fn restore_backup_state<S>(
    storage: &S,
    backup: SdkBackupState,
) -> Result<RestoreReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let current_identity = tx.load_identity_state();
            let current_next_peer_link_operation_lease_id =
                tx.export_storage_state().next_peer_link_operation_lease_id;
            let (state, report) = backup.into_storage_state(
                current_identity.as_ref(),
                current_next_peer_link_operation_lease_id,
            )?;
            tx.replace_storage_state(state);
            Ok(report)
        })
        .await
}

impl SdkBackupState {
    pub(crate) fn from_storage_state(state: StorageState) -> Self {
        let mut linked_peers = state.linked_peers.into_values().collect::<Vec<_>>();
        linked_peers
            .sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));

        let mut contact_records = state.contact_records.into_values().collect::<Vec<_>>();
        contact_records
            .sort_by(|left, right| left.public_key.as_str().cmp(right.public_key.as_str()));

        let mut public_endpoint_records = state
            .public_endpoint_records
            .into_values()
            .collect::<Vec<_>>();
        public_endpoint_records.sort_by(|left, right| left.identifier.cmp(&right.identifier));

        let mut payment_endpoint_reservations = state
            .payment_endpoint_reservations
            .into_values()
            .collect::<Vec<_>>();
        payment_endpoint_reservations.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.reservation_id.cmp(&right.reservation_id))
        });

        let mut encrypted_link_states = state
            .encrypted_link_states
            .into_values()
            .collect::<Vec<_>>();
        encrypted_link_states
            .sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));

        let mut event_dedup_records = state.event_dedup_records.into_values().collect::<Vec<_>>();
        event_dedup_records.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.event_id.cmp(&right.event_id))
        });

        let mut receipt_access_records = state
            .receipt_access_records
            .into_values()
            .collect::<Vec<_>>();
        receipt_access_records.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.event_id.cmp(&right.event_id))
        });

        let mut receipt_records = state.receipt_records.into_values().collect::<Vec<_>>();
        receipt_records.sort_by(|left, right| {
            left.issuer
                .as_str()
                .cmp(right.issuer.as_str())
                .then(left.receipt_id.cmp(&right.receipt_id))
        });

        Self {
            version: SDK_BACKUP_VERSION,
            identity_state: state.identity_state,
            linked_peers,
            contact_records,
            public_endpoint_records,
            payment_endpoint_reservations,
            encrypted_link_states,
            outbound_private_messages: state.outbound_private_messages,
            private_stream_items: state.private_stream_items,
            event_dedup_records,
            receipt_access_records,
            receipt_records,
            next_outbound_private_message_id: state.next_outbound_private_message_id,
            next_receive_batch_id: state.next_receive_batch_id,
            next_private_stream_item_id: state.next_private_stream_item_id,
        }
    }

    fn into_storage_state(
        self,
        current_identity: Option<&IdentityState>,
        next_peer_link_operation_lease_id: u64,
    ) -> Result<(ValidatedStorageState, RestoreReport)> {
        self.validate(current_identity)?;

        let mut identity_state = self.identity_state;
        preserve_current_sign_out_generation(&mut identity_state, current_identity);
        let mut linked_peers = keyed_by_counterparty(self.linked_peers, "Linked Peer")?;
        let contact_records = keyed_by_tuple(
            self.contact_records,
            |record| record.public_key.clone(),
            "local contact",
        )?;
        validate_contact_records(&contact_records)?;
        let public_endpoint_records = keyed_by_string(
            self.public_endpoint_records,
            |record| record.identifier.clone(),
            "public Payment Endpoint",
        )?;
        validate_public_endpoint_records(&public_endpoint_records)?;
        let payment_endpoint_reservations = keyed_by_tuple(
            self.payment_endpoint_reservations,
            |record| (record.counterparty.clone(), record.reservation_id.clone()),
            "Payment Endpoint Reservation",
        )?;
        let encrypted_link_states =
            keyed_by_counterparty(self.encrypted_link_states, "Encrypted Link state")?;
        validate_encrypted_link_snapshots(&encrypted_link_states)?;
        let outbound_private_messages = unique_outbound_messages(self.outbound_private_messages)?;
        validate_outbound_private_messages(&outbound_private_messages)?;
        validate_payment_endpoint_reservations(
            &payment_endpoint_reservations,
            &outbound_private_messages,
        )?;
        let private_stream_items = unique_private_stream_items(self.private_stream_items)?;
        validate_private_stream_items(&private_stream_items)?;
        let event_dedup_records = keyed_by_tuple(
            self.event_dedup_records,
            |record| (record.counterparty.clone(), record.event_id.clone()),
            "Event dedupe",
        )?;
        validate_event_dedup_records(&event_dedup_records, &private_stream_items)?;
        let receipt_access_records = keyed_by_tuple(
            self.receipt_access_records,
            |record| (record.counterparty.clone(), record.event_id.clone()),
            "Receipt Access",
        )?;
        validate_receipt_access_records(&receipt_access_records, &private_stream_items)?;
        let receipt_records = keyed_by_tuple(
            self.receipt_records,
            |record| (record.issuer.clone(), record.receipt_id.clone()),
            "Receipt",
        )?;
        validate_receipt_records(&receipt_records, &receipt_access_records)?;

        let recovery_counterparties = recovery_counterparties(RecoverySources {
            linked_peers: &linked_peers,
            payment_endpoint_reservations: &payment_endpoint_reservations,
            encrypted_link_states: &encrypted_link_states,
            outbound_private_messages: &outbound_private_messages,
            private_stream_items: &private_stream_items,
            event_dedup_records: &event_dedup_records,
            receipt_access_records: &receipt_access_records,
            receipt_records: &receipt_records,
        });
        let recovery_required_peers =
            mark_restored_peers_recovery_required(&mut linked_peers, &recovery_counterparties);

        let next_outbound_private_message_id = self
            .next_outbound_private_message_id
            .max(next_outbound_id(&outbound_private_messages));
        let next_receive_batch_id = self
            .next_receive_batch_id
            .max(next_receive_batch_id(&private_stream_items));
        let next_private_stream_item_id = self
            .next_private_stream_item_id
            .max(next_private_stream_item_id(&private_stream_items));

        let report = RestoreReport {
            version: self.version,
            restored_identity: identity_state.is_some(),
            linked_peers: linked_peers.len(),
            contact_records: contact_records.len(),
            public_endpoint_records: public_endpoint_records.len(),
            payment_endpoint_reservations: payment_endpoint_reservations.len(),
            encrypted_link_states: encrypted_link_states.len(),
            outbound_private_messages: outbound_private_messages.len(),
            private_stream_items: private_stream_items.len(),
            event_dedup_records: event_dedup_records.len(),
            receipt_access_records: receipt_access_records.len(),
            receipt_records: receipt_records.len(),
            recovery_required_peers,
        };

        let state = StorageState {
            identity_state,
            linked_peers,
            contact_records,
            public_endpoint_records,
            payment_endpoint_reservations,
            encrypted_link_states,
            peer_link_operation_leases: HashMap::new(),
            next_peer_link_operation_lease_id,
            outbound_private_messages,
            next_outbound_private_message_id,
            private_stream_items,
            next_receive_batch_id,
            next_private_stream_item_id,
            event_dedup_records,
            receipt_access_records,
            receipt_records,
        };

        Ok((ValidatedStorageState::new(state), report))
    }

    fn validate(&self, current_identity: Option<&IdentityState>) -> Result<()> {
        if self.version != SDK_BACKUP_VERSION {
            return Err(PaykitSdkError::Protocol(format!(
                "unsupported SDK backup version {}",
                self.version
            )));
        }

        if let Some(current_public_key) =
            current_identity.and_then(|state| state.public_key.as_ref())
        {
            let backup_public_key = self
                .identity_state
                .as_ref()
                .and_then(|state| state.public_key.as_ref());
            if backup_public_key != Some(current_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "backup identity does not match current local identity".into(),
                    source: None,
                });
            }
        }

        let backup_public_key = self
            .identity_state
            .as_ref()
            .and_then(|state| state.public_key.as_ref());
        if backup_public_key.is_none() && self.has_identity_scoped_state() {
            return Err(PaykitSdkError::Protocol(
                "backup has SDK state but no local public identity".into(),
            ));
        }

        Ok(())
    }

    pub(crate) fn identity_public_key(&self) -> Option<&PubkyPublicKey> {
        self.identity_state
            .as_ref()
            .and_then(|state| state.public_key.as_ref())
    }

    pub(crate) fn has_identity_scoped_state(&self) -> bool {
        !self.linked_peers.is_empty()
            || !self.contact_records.is_empty()
            || !self.public_endpoint_records.is_empty()
            || !self.payment_endpoint_reservations.is_empty()
            || !self.encrypted_link_states.is_empty()
            || !self.outbound_private_messages.is_empty()
            || !self.private_stream_items.is_empty()
            || !self.event_dedup_records.is_empty()
            || !self.receipt_access_records.is_empty()
            || !self.receipt_records.is_empty()
    }

    pub(crate) fn has_private_identity_scoped_state(&self) -> bool {
        !self.linked_peers.is_empty()
            || !self.payment_endpoint_reservations.is_empty()
            || !self.encrypted_link_states.is_empty()
            || !self.outbound_private_messages.is_empty()
            || !self.private_stream_items.is_empty()
            || !self.event_dedup_records.is_empty()
            || !self.receipt_access_records.is_empty()
            || !self.receipt_records.is_empty()
    }
}

fn preserve_current_sign_out_generation(
    backup_identity: &mut Option<IdentityState>,
    current_identity: Option<&IdentityState>,
) {
    match (backup_identity, current_identity) {
        (Some(backup_identity), Some(current_identity))
            if backup_identity.public_key == current_identity.public_key =>
        {
            backup_identity.sign_out_generation = backup_identity
                .sign_out_generation
                .max(current_identity.sign_out_generation);
        }
        (backup_identity @ None, Some(current_identity))
            if current_identity.capability == PubkyIdentityCapability::SignedOut =>
        {
            *backup_identity = Some(current_identity.clone());
        }
        _ => {}
    }
}

fn keyed_by_counterparty<T>(records: Vec<T>, label: &str) -> Result<HashMap<PubkyPublicKey, T>>
where
    T: HasCounterparty,
{
    keyed_by_tuple(records, |record| record.counterparty().clone(), label)
}

trait HasCounterparty {
    fn counterparty(&self) -> &PubkyPublicKey;
}

impl HasCounterparty for LinkedPeerRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }
}

impl HasCounterparty for EncryptedLinkStateRecord {
    fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }
}

fn keyed_by_string<T, F>(records: Vec<T>, key: F, label: &str) -> Result<HashMap<String, T>>
where
    F: Fn(&T) -> String,
{
    keyed_by_tuple(records, key, label)
}

fn keyed_by_tuple<K, T, F>(records: Vec<T>, key: F, label: &str) -> Result<HashMap<K, T>>
where
    K: Eq + std::hash::Hash + fmt::Debug,
    F: Fn(&T) -> K,
{
    let mut keyed = HashMap::new();
    for record in records {
        let key = key(&record);
        if keyed.insert(key, record).is_some() {
            return Err(PaykitSdkError::Protocol(format!(
                "duplicate {label} backup key"
            )));
        }
    }
    Ok(keyed)
}

fn unique_outbound_messages(
    mut records: Vec<OutboundPrivateMessageRecord>,
) -> Result<Vec<OutboundPrivateMessageRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.outbound_message_id) {
            return Err(PaykitSdkError::Protocol(format!(
                "duplicate outbound Private Application Message id {}",
                record.outbound_message_id
            )));
        }
    }
    records.sort_by_key(|record| record.outbound_message_id);
    Ok(records)
}

fn unique_private_stream_items(
    mut records: Vec<PrivateStreamItemRecord>,
) -> Result<Vec<PrivateStreamItemRecord>> {
    let mut ids = HashSet::new();
    for record in &records {
        if !ids.insert(record.stream_item_id) {
            return Err(PaykitSdkError::Protocol(format!(
                "duplicate private stream item id {}",
                record.stream_item_id
            )));
        }
    }
    records.sort_by_key(|record| record.stream_item_id);
    Ok(records)
}

fn next_outbound_id(records: &[OutboundPrivateMessageRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.outbound_message_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

fn next_receive_batch_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.receive_batch_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

fn next_private_stream_item_id(records: &[PrivateStreamItemRecord]) -> u64 {
    records
        .iter()
        .map(|record| record.stream_item_id.saturating_add(1))
        .max()
        .unwrap_or_default()
}

fn mark_restored_peers_recovery_required(
    linked_peers: &mut HashMap<PubkyPublicKey, LinkedPeerRecord>,
    recovery_counterparties: &HashSet<PubkyPublicKey>,
) -> Vec<PubkyPublicKey> {
    for counterparty in recovery_counterparties {
        linked_peers
            .entry(counterparty.clone())
            .or_insert_with(|| LinkedPeerRecord {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::RecoveryRequired,
                last_sync_at: None,
                last_private_receive_at: None,
                failure_count: 0,
                local_recovery_attempt_id: None,
                local_recovery_marker_created_at: None,
                remote_recovery_attempt_id: None,
                remote_recovery_marker_observed_at: None,
            });
    }

    let mut peers = Vec::new();
    for record in linked_peers.values_mut() {
        if record.state != LinkedPeerState::Blocked
            && (recovery_counterparties.contains(&record.counterparty)
                || matches!(
                    record.state,
                    LinkedPeerState::Linked | LinkedPeerState::Linking
                ))
        {
            record.state = LinkedPeerState::RecoveryRequired;
        }
        if record.state == LinkedPeerState::RecoveryRequired {
            peers.push(record.counterparty.clone());
        }
    }
    peers.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    peers
}

struct RecoverySources<'a> {
    linked_peers: &'a HashMap<PubkyPublicKey, LinkedPeerRecord>,
    payment_endpoint_reservations:
        &'a HashMap<(PubkyPublicKey, String), PaymentEndpointReservationRecord>,
    encrypted_link_states: &'a HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
    outbound_private_messages: &'a [OutboundPrivateMessageRecord],
    private_stream_items: &'a [PrivateStreamItemRecord],
    event_dedup_records: &'a HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    receipt_access_records: &'a HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
    receipt_records: &'a HashMap<(PubkyPublicKey, String), ReceiptRecord>,
}

fn recovery_counterparties(sources: RecoverySources<'_>) -> HashSet<PubkyPublicKey> {
    let mut counterparties = HashSet::new();
    for record in sources.linked_peers.values() {
        if matches!(
            record.state,
            LinkedPeerState::Linked | LinkedPeerState::Linking
        ) {
            counterparties.insert(record.counterparty.clone());
        }
    }
    counterparties.extend(
        sources
            .payment_endpoint_reservations
            .values()
            .map(|record| record.counterparty.clone()),
    );
    counterparties.extend(sources.encrypted_link_states.keys().cloned());
    counterparties.extend(
        sources
            .outbound_private_messages
            .iter()
            .map(|record| record.counterparty.clone()),
    );
    counterparties.extend(
        sources
            .private_stream_items
            .iter()
            .map(|record| record.counterparty.clone()),
    );
    counterparties.extend(
        sources
            .event_dedup_records
            .values()
            .map(|record| record.counterparty.clone()),
    );
    counterparties.extend(
        sources
            .receipt_access_records
            .values()
            .map(|record| record.counterparty.clone()),
    );
    counterparties.extend(
        sources
            .receipt_records
            .values()
            .map(|record| record.issuer.clone()),
    );
    counterparties
}

fn validate_encrypted_link_snapshots(
    records: &HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
) -> Result<()> {
    for (counterparty, record) in records {
        let expected_recipient = counterparty.to_public_key()?;
        if let Some(snapshot_bytes) = record.link_snapshot.as_ref() {
            let snapshot = paykit_lib::EncryptedLinkSnapshot::deserialize(snapshot_bytes)
                .map_err(PaykitSdkError::from)?;
            if snapshot.recipient() != &expected_recipient {
                return Err(PaykitSdkError::Protocol(format!(
                    "Encrypted Link snapshot recipient does not match counterparty {counterparty}"
                )));
            }
        }
        if let Some(snapshot_bytes) = record.handshake_snapshot.as_ref() {
            let snapshot = paykit_lib::EncryptedLinkHandshakeSnapshot::deserialize(snapshot_bytes)
                .map_err(PaykitSdkError::from)?;
            if snapshot.recipient() != &expected_recipient {
                return Err(PaykitSdkError::Protocol(format!(
                    "Encrypted Link Handshake snapshot recipient does not match counterparty {counterparty}"
                )));
            }
        }
    }
    Ok(())
}

fn validate_public_endpoint_records(records: &HashMap<String, PublicEndpointRecord>) -> Result<()> {
    for record in records.values() {
        PaymentEndpointIdentifier::new(&record.identifier)?;
    }
    Ok(())
}

fn validate_contact_records(records: &HashMap<PubkyPublicKey, ContactRecord>) -> Result<()> {
    for record in records.values() {
        if let Some(profile) = record.profile.as_ref() {
            profile.validate()?;
        }
        if let Some(label) = record.label.as_deref() {
            crate::ContactUpdate {
                public_key: record.public_key.clone(),
                label: Some(label.to_owned()),
            }
            .validate()?;
        }
        validate_contact_marker_state(record)?;
    }
    Ok(())
}

fn validate_contact_marker_state(record: &ContactRecord) -> Result<()> {
    use crate::PublicContactMarkerStatus::{
        Failed, NotPublished, PendingPublication, PendingRemoval, Published, Removed,
    };

    if record.public_contact_published_at.is_some() && record.public_contact_removed_at.is_some() {
        return Err(PaykitSdkError::Protocol(format!(
            "local contact {} has inconsistent public contact marker timestamps",
            record.public_key
        )));
    }

    let invalid = match record.public_contact_marker_status {
        NotPublished => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        PendingPublication => record.public_contact_last_error.is_some(),
        Published => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        PendingRemoval => {
            record.public_contact_published_at.is_none()
                || record.public_contact_removed_at.is_some()
                || record.public_contact_last_error.is_some()
        }
        Removed => {
            record.public_contact_published_at.is_some()
                || record.public_contact_removed_at.is_none()
                || record.public_contact_last_error.is_some()
        }
        Failed => record.public_contact_last_error.is_none(),
    };
    if invalid {
        return Err(PaykitSdkError::Protocol(format!(
            "local contact {} has inconsistent public contact marker state",
            record.public_key
        )));
    }
    Ok(())
}

fn validate_payment_endpoint_reservations(
    records: &HashMap<(PubkyPublicKey, String), PaymentEndpointReservationRecord>,
    outbound_private_messages: &[OutboundPrivateMessageRecord],
) -> Result<()> {
    let outbound_by_id = outbound_private_messages
        .iter()
        .map(|record| (record.outbound_message_id, record))
        .collect::<HashMap<_, _>>();

    for record in records.values() {
        if record.reservation_id.trim().is_empty() {
            return Err(PaykitSdkError::Protocol(
                "Payment Endpoint Reservation id must not be empty".into(),
            ));
        }
        let identifier = PaymentEndpointIdentifier::new(&record.identifier)?;
        let outbound = outbound_by_id
            .get(&record.outbound_message_id)
            .ok_or_else(|| {
                PaykitSdkError::Protocol(format!(
                    "Payment Endpoint Reservation '{}' references missing outbound message {}",
                    record.reservation_id, record.outbound_message_id
                ))
            })?;
        if outbound.counterparty != record.counterparty {
            return Err(PaykitSdkError::Protocol(format!(
                "Payment Endpoint Reservation '{}' counterparty does not match outbound message {}",
                record.reservation_id, record.outbound_message_id
            )));
        }
        if outbound.kind != PrivateMessageKind::PrivatePaymentList.as_str() {
            return Err(PaykitSdkError::Protocol(format!(
                "Payment Endpoint Reservation '{}' references non-list outbound message {}",
                record.reservation_id, record.outbound_message_id
            )));
        }
        let private_list = parse_private_payment_list_json(&outbound.raw_json)
            .map_err(|err| PaykitSdkError::Protocol(err.to_string()))?;
        let payload = private_list.get(&identifier).ok_or_else(|| {
            PaykitSdkError::Protocol(format!(
                "Payment Endpoint Reservation '{}' identifier is missing from outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            ))
        })?;
        let payload_hash = reservation_payload_hash(payload.as_str());
        if record.payload_hash != payload_hash {
            return Err(PaykitSdkError::Protocol(format!(
                "Payment Endpoint Reservation '{}' payload hash does not match outbound Private Payment List {}",
                record.reservation_id, record.outbound_message_id
            )));
        }
    }
    Ok(())
}

fn validate_outbound_private_messages(records: &[OutboundPrivateMessageRecord]) -> Result<()> {
    for record in records {
        validate_queued_outbound_private_message(record)?;
    }
    Ok(())
}

fn validate_private_stream_items(records: &[PrivateStreamItemRecord]) -> Result<()> {
    for record in records {
        let (parsed_version, parsed_kind, known_kind) = private_message_header(&record.raw_json)?;
        let classification =
            classify_private_application_message(&private_application_message_from_raw(
                record.raw_json.clone(),
                parsed_version,
                parsed_kind.clone(),
            ));
        if record.parsed_version != parsed_version {
            return Err(PaykitSdkError::Protocol(format!(
                "private stream item {} has stale parsed version metadata",
                record.stream_item_id
            )));
        }
        if record.parsed_kind.as_deref() != parsed_kind.as_deref() {
            return Err(PaykitSdkError::Protocol(format!(
                "private stream item {} has stale parsed kind metadata",
                record.stream_item_id
            )));
        }
        if record.known_paykit_kind.as_deref() != known_kind.map(PrivateMessageKind::as_str) {
            return Err(PaykitSdkError::Protocol(format!(
                "private stream item {} has stale known kind metadata",
                record.stream_item_id
            )));
        }
        if record.parse_status != classification.status {
            return Err(PaykitSdkError::Protocol(format!(
                "private stream item {} has stale parse status metadata",
                record.stream_item_id
            )));
        }
        if record.parse_status == PrivateStreamParseStatus::Valid {
            let Some(kind) = known_kind else {
                return Err(PaykitSdkError::Protocol(format!(
                    "private stream item {} is marked valid without a recognized Paykit kind",
                    record.stream_item_id
                )));
            };
            validate_valid_private_stream_body(record, kind)?;
        }
    }
    Ok(())
}

fn validate_event_dedup_records(
    records: &HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        let Some(first) = stream_by_id.get(&record.first_stream_item_id) else {
            return Err(PaykitSdkError::Protocol(format!(
                "Event dedupe record '{}' references missing first stream item {}",
                record.event_id, record.first_stream_item_id
            )));
        };
        if first.counterparty != record.counterparty {
            return Err(PaykitSdkError::Protocol(format!(
                "Event dedupe record '{}' counterparty does not match first stream item",
                record.event_id
            )));
        }
        if payload_hash(&first.raw_json) != record.payload_hash {
            return Err(PaykitSdkError::Protocol(format!(
                "Event dedupe record '{}' payload hash does not match first stream item",
                record.event_id
            )));
        }
        validate_event_dedup_stream_item(record, first, EventDedupeItemKind::First)?;
        for stream_item_id in &record.duplicate_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol(format!(
                    "Event dedupe record '{}' references missing stream item {}",
                    record.event_id, stream_item_id
                )));
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Duplicate)?;
        }
        for stream_item_id in &record.conflicting_stream_item_ids {
            let Some(item) = stream_by_id.get(stream_item_id) else {
                return Err(PaykitSdkError::Protocol(format!(
                    "Event dedupe record '{}' references missing stream item {}",
                    record.event_id, stream_item_id
                )));
            };
            validate_event_dedup_stream_item(record, item, EventDedupeItemKind::Conflict)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EventDedupeItemKind {
    First,
    Duplicate,
    Conflict,
}

fn validate_event_dedup_stream_item(
    record: &EventDedupRecord,
    item: &PrivateStreamItemRecord,
    item_kind: EventDedupeItemKind,
) -> Result<()> {
    if item.counterparty != record.counterparty {
        return Err(PaykitSdkError::Protocol(format!(
            "Event dedupe record '{}' counterparty does not match stream item {}",
            record.event_id, item.stream_item_id
        )));
    }

    let classification =
        classify_private_application_message(&private_application_message_from_raw(
            item.raw_json.clone(),
            item.parsed_version,
            item.parsed_kind.clone(),
        ));
    let Some(event) = classification.event else {
        return Err(PaykitSdkError::Protocol(format!(
            "Event dedupe record '{}' references non-event stream item {}",
            record.event_id, item.stream_item_id
        )));
    };
    if event.event_id != record.event_id || event.event_kind != record.event_kind {
        return Err(PaykitSdkError::Protocol(format!(
            "Event dedupe record '{}' does not match stream item {} event header",
            record.event_id, item.stream_item_id
        )));
    }

    let item_hash = payload_hash(&item.raw_json);
    match item_kind {
        EventDedupeItemKind::First | EventDedupeItemKind::Duplicate => {
            if item_hash != record.payload_hash {
                return Err(PaykitSdkError::Protocol(format!(
                    "Event dedupe record '{}' same-payload stream item {} has different payload hash",
                    record.event_id, item.stream_item_id
                )));
            }
        }
        EventDedupeItemKind::Conflict => {
            if item_hash == record.payload_hash {
                return Err(PaykitSdkError::Protocol(format!(
                    "Event dedupe record '{}' conflict stream item {} has same payload hash",
                    record.event_id, item.stream_item_id
                )));
            }
        }
    }

    Ok(())
}

fn validate_receipt_access_records(
    records: &HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
    stream_items: &[PrivateStreamItemRecord],
) -> Result<()> {
    let stream_by_id = stream_items
        .iter()
        .map(|item| (item.stream_item_id, item))
        .collect::<HashMap<_, _>>();
    for record in records.values() {
        let Some(item) = stream_by_id.get(&record.stream_item_id) else {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt Access record '{}' references missing stream item {}",
                record.event_id, record.stream_item_id
            )));
        };
        if item.counterparty != record.counterparty
            || item.receive_batch_id != record.receive_batch_id
            || item.known_paykit_kind.as_deref() != Some(PrivateMessageKind::ReceiptAccess.as_str())
        {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt Access record '{}' does not match its stream item",
                record.event_id
            )));
        }
        let event = paykit_lib::parse_receipt_access_event_message(&private_application_message(
            item,
            PrivateMessageKind::ReceiptAccess,
        ))
        .ok_or_else(|| {
            PaykitSdkError::Protocol(format!(
                "Receipt Access record '{}' stream item is not parseable",
                record.event_id
            ))
        })?;
        let Some(access) = event.parsed_access() else {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt Access record '{}' stream item is malformed",
                record.event_id
            )));
        };
        if access.event_id.as_str() != record.event_id
            || access.receipt_id.as_str() != record.receipt_id
            || access.payment_reference.as_str() != record.payment_reference
            || access.location != record.location
            || access.key.as_str() != record.key
        {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt Access record '{}' does not match parsed stream payload",
                record.event_id
            )));
        }
    }
    Ok(())
}

fn validate_receipt_records(
    records: &HashMap<(PubkyPublicKey, String), ReceiptRecord>,
    access_records: &HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
) -> Result<()> {
    for record in records.values() {
        ReceiptId::new(&record.receipt_id)?;
        if let Some(identifier) = record.payment_endpoint_identifier.as_ref() {
            PaymentEndpointIdentifier::new(identifier)?;
        }
        let access_key = (
            record.issuer.clone(),
            record.receipt_access_event_id.clone(),
        );
        let Some(access) = access_records.get(&access_key) else {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt record '{}' references missing Receipt Access event '{}'",
                record.receipt_id, record.receipt_access_event_id
            )));
        };
        if access.receipt_id != record.receipt_id
            || access.payment_reference != record.payment_reference
            || access.payment_request_id != record.payment_request_id
            || access.billing_period != record.billing_period
            || access.location != record.location
            || receipt_access_key_hash(&access.key) != record.receipt_access_key_hash
        {
            return Err(PaykitSdkError::Protocol(format!(
                "Receipt record '{}' does not match its Receipt Access record",
                record.receipt_id
            )));
        }
    }
    Ok(())
}

fn private_message_header(
    raw_json: &str,
) -> Result<(Option<u32>, Option<String>, Option<PrivateMessageKind>)> {
    let value = match serde_json::from_str::<serde_json::Value>(raw_json) {
        Ok(value) => value,
        Err(_) => return Ok((None, None, None)),
    };
    let parsed_version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u8::try_from(version).ok())
        .map(u32::from);
    let parsed_kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let known_kind = parsed_kind.as_deref().and_then(PrivateMessageKind::parse);
    Ok((parsed_version, parsed_kind, known_kind))
}

fn validate_valid_private_stream_body(
    record: &PrivateStreamItemRecord,
    kind: PrivateMessageKind,
) -> Result<()> {
    match kind {
        PrivateMessageKind::PrivatePaymentList => {
            paykit_lib::parse_private_payment_list_json(&record.raw_json)?;
        }
        PrivateMessageKind::ReceiptAccess => {
            let event = paykit_lib::parse_receipt_access_event_message(
                &private_application_message(record, kind),
            )
            .ok_or_else(|| {
                PaykitSdkError::Protocol(format!(
                    "private stream item {} Receipt Access payload does not match its kind",
                    record.stream_item_id
                ))
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol(error.to_owned()));
            }
        }
        PrivateMessageKind::PaymentRequest
        | PrivateMessageKind::PaymentRequestAcceptance
        | PrivateMessageKind::PaymentRequestRejection
        | PrivateMessageKind::PaymentRequestCancellation
        | PrivateMessageKind::PaymentProof => {
            let event = paykit_lib::parse_payment_request_event_message(
                &private_application_message(record, kind),
            )
            .ok_or_else(|| {
                PaykitSdkError::Protocol(format!(
                    "private stream item {} Payment Request payload does not match its kind",
                    record.stream_item_id
                ))
            })?;
            if let Some(error) = event.validation_error() {
                return Err(PaykitSdkError::Protocol(error.to_owned()));
            }
        }
    }
    Ok(())
}

fn private_application_message(
    record: &PrivateStreamItemRecord,
    kind: PrivateMessageKind,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: record
            .parsed_version
            .and_then(|version| u8::try_from(version).ok()),
        kind: Some(kind.as_str().to_owned()),
        raw_json: record.raw_json.clone(),
    }
}

fn private_application_message_from_raw(
    raw_json: String,
    parsed_version: Option<u32>,
    parsed_kind: Option<String>,
) -> PrivateApplicationMessage {
    PrivateApplicationMessage {
        version: parsed_version.and_then(|version| u8::try_from(version).ok()),
        kind: parsed_kind,
        raw_json,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        identity::PubkyIdentityCapability, outbound_private::OutboundPrivateMessageStatus,
        private_stream::PrivateStreamParseStatus, storage::InMemoryStorage,
    };

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn public_key() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn identity(public_key: PubkyPublicKey) -> IdentityState {
        IdentityState {
            public_key: Some(public_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            local_secret_available: true,
            initialized_at: timestamp(),
            sign_out_generation: 0,
        }
    }

    fn signed_out_identity(sign_out_generation: u64) -> IdentityState {
        IdentityState {
            public_key: None,
            capability: PubkyIdentityCapability::SignedOut,
            local_secret_available: false,
            initialized_at: timestamp(),
            sign_out_generation,
        }
    }

    fn contact_record(public_key: PubkyPublicKey) -> ContactRecord {
        ContactRecord {
            public_key,
            label: Some("Alice".into()),
            profile: None,
            profile_fetched_at: None,
            created_at: timestamp(),
            updated_at: timestamp(),
            public_contact_marker_status: crate::PublicContactMarkerStatus::NotPublished,
            public_contact_published_at: None,
            public_contact_removed_at: None,
            public_contact_last_error: None,
        }
    }

    fn payment_request_json(event_id: &str) -> String {
        format!(
            r#"{{"version":1,"kind":"paykit.payment_request","event_id":"{event_id}","payment_request_id":"550e8400-e29b-41d4-a716-446655440000","request":{{"payment_request_id":"550e8400-e29b-41d4-a716-446655440000","terms":{{"amount":{{"value":"1","asset":"btc"}},"payment_reference":"invoice-2026-0001","proposal_expires_at":null,"recurrence":null,"accepted_payment_endpoint_identifiers":["btc-lightning-bolt11"],"metadata":{{}}}}}}}}"#
        )
    }

    fn private_payment_list_outbound(
        counterparty: PubkyPublicKey,
        outbound_message_id: u64,
        payload: &str,
    ) -> OutboundPrivateMessageRecord {
        OutboundPrivateMessageRecord {
            outbound_message_id,
            counterparty,
            kind: PrivateMessageKind::PrivatePaymentList.as_str().into(),
            raw_json: format!(
                r#"{{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{{"btc-lightning-bolt11":"{payload}"}}}}"#
            ),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: timestamp(),
            updated_at: timestamp(),
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        }
    }

    #[tokio::test]
    async fn test_export_backup_state_redacts_debug() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_identity_state(identity(counterparty.clone()));
                    tx.save_encrypted_link_state(EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                        link_snapshot: Some(vec![1, 2, 3]),
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 0,
                        checkpointed_at: timestamp(),
                    });
                    tx.insert_outbound_private_message(crate::storage::NewOutboundPrivateMessage::new(
                        counterparty,
                        "paykit.private_payment_list".into(),
                        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"btc-lightning-bolt11":"ln-private-payload-marker"}}"#.into(),
                        timestamp(),
                    ));
                    Ok(())
                }
            })
            .await
            .unwrap();

        let backup = export_backup_state(&storage).await.unwrap();
        let debug = format!("{backup:?}");

        assert!(!debug.contains("ln-private-payload-marker"));
        assert!(!debug.contains("[1, 2, 3]"));
        assert_eq!(backup.outbound_private_messages.len(), 1);
    }

    #[tokio::test]
    async fn test_restore_backup_state_marks_restored_links_recovery_required() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: vec![LinkedPeerRecord {
                counterparty: counterparty.clone(),
                state: LinkedPeerState::Linked,
                last_sync_at: Some(timestamp()),
                last_private_receive_at: None,
                failure_count: 0,
                local_recovery_attempt_id: None,
                local_recovery_marker_created_at: None,
                remote_recovery_attempt_id: None,
                remote_recovery_marker_observed_at: None,
            }],
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: vec![EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: None,
                handshake_snapshot: None,
                handshake_role: None,
                generation: 0,
                checkpointed_at: timestamp(),
            }],
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let report = restore_backup_state(&storage, backup).await.unwrap();
        let restored = storage.snapshot().unwrap();

        assert_eq!(report.recovery_required_peers, vec![counterparty.clone()]);
        assert_eq!(
            restored.linked_peers.get(&counterparty).unwrap().state,
            LinkedPeerState::RecoveryRequired
        );
        assert!(restored.peer_link_operation_leases.is_empty());
    }

    #[tokio::test]
    async fn test_backup_state_round_trips_contact_records() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let contact_public_key = public_key();
        storage
            .transaction({
                let local_public_key = local_public_key.clone();
                let contact_public_key = contact_public_key.clone();
                move |tx| {
                    tx.save_identity_state(identity(local_public_key));
                    tx.save_contact_record(contact_record(contact_public_key));
                    Ok(())
                }
            })
            .await
            .unwrap();

        let backup = export_backup_state(&storage).await.unwrap();
        let restore_storage = InMemoryStorage::new();
        let report = restore_backup_state(&restore_storage, backup)
            .await
            .unwrap();
        let restored = restore_storage.snapshot().unwrap();

        assert_eq!(report.contact_records, 1);
        assert_eq!(
            restored.contact_records[&contact_public_key]
                .label
                .as_deref(),
            Some("Alice")
        );
        assert!(report.recovery_required_peers.is_empty());
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_inconsistent_contact_marker_state() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let contact_public_key = public_key();
        let mut contact = contact_record(contact_public_key);
        contact.public_contact_published_at = Some(timestamp());
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(local_public_key)),
            linked_peers: Vec::new(),
            contact_records: vec![contact],
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_dual_contact_marker_timestamps() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let contact_public_key = public_key();
        let mut contact = contact_record(contact_public_key)
            .mark_public_contact_published(timestamp())
            .mark_public_contact_removed(timestamp());
        contact.public_contact_published_at = Some(timestamp());
        contact.public_contact_marker_status = crate::PublicContactMarkerStatus::Failed;
        contact.public_contact_last_error = Some("failed".into());
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(local_public_key)),
            linked_peers: Vec::new(),
            contact_records: vec![contact],
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_accepts_pending_contact_marker_removal() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let contact_public_key = public_key();
        let contact = contact_record(contact_public_key)
            .mark_public_contact_published(timestamp())
            .mark_public_contact_removal_pending(timestamp());
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(local_public_key)),
            linked_peers: Vec::new(),
            contact_records: vec![contact],
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let report = restore_backup_state(&storage, backup).await.unwrap();

        assert_eq!(report.contact_records, 1);
    }

    #[tokio::test]
    async fn test_restore_backup_state_preserves_next_peer_lease_id() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    let lease = tx
                        .claim_peer_link_operation(
                            &counterparty,
                            timestamp(),
                            timestamp() + chrono::Duration::seconds(60),
                        )
                        .unwrap();
                    assert_eq!(lease.lease_id, 0);
                    Ok(())
                }
            })
            .await
            .unwrap();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: None,
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        restore_backup_state(&storage, backup).await.unwrap();
        let snapshot = storage.snapshot().unwrap();

        assert!(snapshot.peer_link_operation_leases.is_empty());
        assert_eq!(snapshot.next_peer_link_operation_lease_id, 1);
    }

    #[tokio::test]
    async fn test_restore_backup_state_marks_link_state_without_peer_recovery_required() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: vec![EncryptedLinkStateRecord {
                counterparty: counterparty.clone(),
                link_snapshot: None,
                handshake_snapshot: None,
                handshake_role: None,
                generation: 0,
                checkpointed_at: timestamp(),
            }],
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        restore_backup_state(&storage, backup).await.unwrap();
        let restored = storage.snapshot().unwrap();

        assert_eq!(
            restored.linked_peers.get(&counterparty).unwrap().state,
            LinkedPeerState::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn test_restore_backup_state_marks_private_stream_only_peer_recovery_required() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: vec![PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json:
                    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                        .into(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.private_payment_list".into()),
                known_paykit_kind: Some("paykit.private_payment_list".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            }],
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 2,
        };

        restore_backup_state(&storage, backup).await.unwrap();
        let restored = storage.snapshot().unwrap();

        assert_eq!(
            restored.linked_peers.get(&counterparty).unwrap().state,
            LinkedPeerState::RecoveryRequired
        );
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_malformed_link_snapshot() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: vec![EncryptedLinkStateRecord {
                counterparty,
                link_snapshot: Some(vec![1, 2, 3]),
                handshake_snapshot: None,
                handshake_role: None,
                generation: 0,
                checkpointed_at: timestamp(),
            }],
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_records_without_identity() {
        let storage = InMemoryStorage::new();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: None,
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: vec![PublicEndpointRecord {
                identifier: "btc-lightning-bolt11".into(),
                payload: Some("ln".into()),
                status: crate::EndpointPublicationStatus::Published,
                updated_at: timestamp(),
                last_error: None,
            }],
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_invalid_public_endpoint_record() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(local_public_key)),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: vec![PublicEndpointRecord {
                identifier: "private".into(),
                payload: Some("ln".into()),
                status: crate::EndpointPublicationStatus::Published,
                updated_at: timestamp(),
                last_error: None,
            }],
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_stale_private_stream_metadata() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: vec![PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty,
                receive_batch_id: 0,
                raw_json:
                    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                        .into(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.receipt_access".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            }],
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 2,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_stale_private_stream_parse_status() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: vec![PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty,
                receive_batch_id: 0,
                raw_json:
                    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                        .into(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.private_payment_list".into()),
                known_paykit_kind: Some("paykit.private_payment_list".into()),
                parse_status: PrivateStreamParseStatus::MalformedRecognized,
                parse_error: Some("stale".into()),
                received_at: timestamp(),
            }],
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 2,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_stale_dedupe_event_header() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let raw_json = payment_request_json("650e8400-e29b-41d4-a716-446655440000");
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: vec![PrivateStreamItemRecord {
                stream_item_id: 1,
                counterparty: counterparty.clone(),
                receive_batch_id: 0,
                raw_json: raw_json.clone(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.payment_request".into()),
                known_paykit_kind: Some("paykit.payment_request".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            }],
            event_dedup_records: vec![EventDedupRecord {
                counterparty,
                event_id: "750e8400-e29b-41d4-a716-446655440000".into(),
                event_kind: "paykit.payment_request".into(),
                payload_hash: payload_hash(&raw_json),
                first_stream_item_id: 1,
                duplicate_stream_item_ids: Vec::new(),
                conflicting_stream_item_ids: Vec::new(),
            }],
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 2,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_wrong_identity() {
        let storage = InMemoryStorage::new();
        let current = public_key();
        let backup_public_key = public_key();
        storage
            .save_identity_state(identity(current))
            .await
            .unwrap();

        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(backup_public_key)),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Identity { .. })));
    }

    #[tokio::test]
    async fn test_restore_backup_state_preserves_current_sign_out_generation() {
        let storage = InMemoryStorage::new();
        let local_public_key = public_key();
        let mut current_identity = identity(local_public_key.clone());
        current_identity.sign_out_generation = 7;
        storage.save_identity_state(current_identity).await.unwrap();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(local_public_key.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        restore_backup_state(&storage, backup).await.unwrap();

        assert_eq!(
            storage
                .snapshot()
                .unwrap()
                .identity_state
                .unwrap()
                .sign_out_generation,
            7
        );
    }

    #[tokio::test]
    async fn test_restore_identity_less_backup_preserves_signed_out_generation() {
        let storage = InMemoryStorage::new();
        storage
            .save_identity_state(signed_out_identity(7))
            .await
            .unwrap();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: None,
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        restore_backup_state(&storage, backup).await.unwrap();

        let identity = storage.snapshot().unwrap().identity_state.unwrap();
        assert_eq!(identity.capability, PubkyIdentityCapability::SignedOut);
        assert_eq!(identity.sign_out_generation, 7);
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_orphan_endpoint_reservation() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
                reservation_id: "reservation-1".into(),
                counterparty,
                identifier: "btc-lightning-bolt11".into(),
                payload_hash: reservation_payload_hash("ln-private"),
                outbound_message_id: 7,
                attribution: HashMap::new(),
                expires_at: None,
                created_at: timestamp(),
            }],
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_mismatched_endpoint_reservation_payload() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: vec![PaymentEndpointReservationRecord {
                reservation_id: "reservation-1".into(),
                counterparty: counterparty.clone(),
                identifier: "btc-lightning-bolt11".into(),
                payload_hash: reservation_payload_hash("different-payload"),
                outbound_message_id: 7,
                attribution: HashMap::new(),
                expires_at: None,
                created_at: timestamp(),
            }],
            encrypted_link_states: Vec::new(),
            outbound_private_messages: vec![private_payment_list_outbound(
                counterparty,
                7,
                "ln-private",
            )],
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 0,
            next_private_stream_item_id: 0,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_rejects_receipt_key_hash_mismatch() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let receipt_id = "550e8400-e29b-41d4-a716-446655440000";
        let access = ReceiptAccessRecord {
            counterparty: counterparty.clone(),
            stream_item_id: 0,
            receive_batch_id: 0,
            event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            receipt_id: receipt_id.into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            location: format!("/pub/paykit/v0/private/receipts/{receipt_id}"),
            key: "receipt-secret".into(),
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        };
        let receipt = ReceiptRecord {
            issuer: counterparty.clone(),
            receipt_access_event_id: access.event_id.clone(),
            receipt_access_key_hash: receipt_access_key_hash("wrong-secret"),
            receipt_id: receipt_id.into(),
            payment_reference: access.payment_reference.clone(),
            payment_request_id: None,
            billing_period: None,
            recipient_public_key: counterparty.clone(),
            payment_endpoint_identifier: None,
            amount: None,
            metadata: serde_json::Map::new(),
            location: access.location.clone(),
            retrieved_at: timestamp(),
        };
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty)),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: Vec::new(),
            private_stream_items: Vec::new(),
            event_dedup_records: Vec::new(),
            receipt_access_records: vec![access],
            receipt_records: vec![receipt],
            next_outbound_private_message_id: 0,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 1,
        };

        let result = restore_backup_state(&storage, backup).await;

        assert!(matches!(result, Err(PaykitSdkError::Protocol(_))));
    }

    #[tokio::test]
    async fn test_restore_backup_state_advances_counters() {
        let storage = InMemoryStorage::new();
        let counterparty = public_key();
        let backup = SdkBackupState {
            version: SDK_BACKUP_VERSION,
            identity_state: Some(identity(counterparty.clone())),
            linked_peers: Vec::new(),
            contact_records: Vec::new(),
            public_endpoint_records: Vec::new(),
            payment_endpoint_reservations: Vec::new(),
            encrypted_link_states: Vec::new(),
            outbound_private_messages: vec![OutboundPrivateMessageRecord {
                outbound_message_id: 7,
                counterparty: counterparty.clone(),
                kind: "paykit.private_payment_list".into(),
                raw_json:
                    r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                        .into(),
                status: OutboundPrivateMessageStatus::Pending,
                attempt_count: 0,
                created_at: timestamp(),
                updated_at: timestamp(),
                last_attempt_at: None,
                sent_at: None,
                last_error: None,
            }],
            private_stream_items: vec![PrivateStreamItemRecord {
                stream_item_id: 9,
                counterparty,
                receive_batch_id: 3,
                raw_json: "{}".into(),
                parsed_version: None,
                parsed_kind: None,
                known_paykit_kind: None,
                parse_status: PrivateStreamParseStatus::InvalidJson,
                parse_error: Some("invalid".into()),
                received_at: timestamp(),
            }],
            event_dedup_records: Vec::new(),
            receipt_access_records: Vec::new(),
            receipt_records: Vec::new(),
            next_outbound_private_message_id: 1,
            next_receive_batch_id: 1,
            next_private_stream_item_id: 1,
        };

        restore_backup_state(&storage, backup).await.unwrap();
        let restored = storage.snapshot().unwrap();

        assert_eq!(restored.next_outbound_private_message_id, 8);
        assert_eq!(restored.next_receive_batch_id, 4);
        assert_eq!(restored.next_private_stream_item_id, 10);
    }
}
