//! SDK backup and restore state.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{
    domain::contacts::ContactRecord,
    domain::endpoint_reservations::{reservation_payload_hash, validate_reservation_id},
    domain::linked_peers::LinkedPeerState,
    domain::outbound_private::{
        validate_outbound_private_message, validate_queued_outbound_private_message,
    },
    domain::private_stream::{
        classify_private_application_message, enforce_receipt_access_receiver_scope,
        normalize::compute_private_stream_normalization, payload_hash,
        private_application_message_from_raw, private_message_header, PrivateStreamParseStatus,
    },
    domain::publication::PublicationStatus,
    domain::receipts::{
        receipt_access_key_hash, ReceiptAccessRecord, ReceiptIssuanceRecord, ReceiptIssuanceStatus,
        ReceiptRecord, ReceiptRetrievalStatus,
    },
    domain::records::{AmountRecord, BillingPeriodRecord},
    identity::{IdentityState, PubkyPublicKey},
    storage::{
        EncryptedLinkStateRecord, EventDedupRecord, LinkedPeerRecord, OutboundPrivateMessageRecord,
        PaymentEndpointReservationRecord, PrivateStreamItemRecord, PublicEndpointRecord,
        StorageAdapter, StorageState,
    },
    OutboundPrivateMessageStatus, PaykitSdkError, Result,
};
use paykit_lib::{
    parse_private_payment_list_json, PaykitReceiverPath, PaymentEndpointIdentifier,
    PrivateApplicationMessage, PrivateMessageKind, ReceiptId,
};

mod validation;

use validation::*;

// Test-only crate-wide access for the frozen classification fixture replay.
#[cfg(test)]
pub(crate) use validation::validate_private_stream_items;

type PeerStorageKey = (PubkyPublicKey, PaykitReceiverPath);

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
    /// Local Paykit receiver/runtime folder that exported this backup.
    pub local_receiver_path: PaykitReceiverPath,
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
    /// Local receipt issuance records.
    pub receipt_issuance_records: Vec<ReceiptIssuanceRecord>,
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
            .field("local_receiver_path", &self.local_receiver_path)
            .field(
                "identity_state",
                &self.identity_state.as_ref().map(|_| "<redacted>"),
            )
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
                "receipt_issuance_records",
                &self.receipt_issuance_records.len(),
            )
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
    /// Number of restored local receipt issuance records.
    pub receipt_issuance_records: usize,
    /// Receiver-scoped peers restored as recovery-required.
    pub recovery_required_peers: Vec<RestoreRecoveryRequiredPeer>,
}

/// Receiver-scoped peer that was restored as recovery-required.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreRecoveryRequiredPeer {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
pub async fn export_backup_state<S>(
    storage: &S,
    local_receiver_path: PaykitReceiverPath,
) -> Result<SdkBackupState>
where
    S: StorageAdapter,
{
    storage
        .transaction(|tx| {
            Ok(SdkBackupState::from_storage_state(
                tx.export_storage_state(),
                local_receiver_path,
            ))
        })
        .await
}

#[cfg(test)]
pub(crate) async fn restore_backup_state<S>(
    storage: &S,
    backup: SdkBackupState,
) -> Result<RestoreReport>
where
    S: StorageAdapter,
{
    restore_backup_state_with_identity(storage, backup, test_receiver_path(), None).await
}

#[cfg(test)]
fn test_receiver_path() -> PaykitReceiverPath {
    PaykitReceiverPath::new("bitkit/wallet").unwrap()
}

pub(crate) async fn restore_backup_state_with_identity<S>(
    storage: &S,
    backup: SdkBackupState,
    local_receiver_path: PaykitReceiverPath,
    trusted_identity: Option<IdentityState>,
) -> Result<RestoreReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let stored_identity = tx.load_identity_state();
            let current_identity = trusted_identity.as_ref().or(stored_identity.as_ref());
            let current_next_peer_link_operation_lease_id =
                tx.export_storage_state().next_peer_link_operation_lease_id;
            let (state, report) = backup.into_storage_state(
                current_identity,
                &local_receiver_path,
                current_next_peer_link_operation_lease_id,
            )?;
            tx.replace_storage_state(state);
            Ok(report)
        })
        .await
}

impl SdkBackupState {
    pub(crate) fn from_storage_state(
        state: StorageState,
        local_receiver_path: PaykitReceiverPath,
    ) -> Self {
        let mut linked_peers = state.linked_peers.into_values().collect::<Vec<_>>();
        linked_peers.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
        });

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
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
                .then(left.reservation_id.cmp(&right.reservation_id))
        });

        let mut encrypted_link_states = state
            .encrypted_link_states
            .into_values()
            .collect::<Vec<_>>();
        encrypted_link_states.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
        });

        let mut event_dedup_records = state.event_dedup_records.into_values().collect::<Vec<_>>();
        event_dedup_records.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
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
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
                .then(left.event_id.cmp(&right.event_id))
        });

        let mut receipt_records = state.receipt_records.into_values().collect::<Vec<_>>();
        receipt_records.sort_by(|left, right| {
            left.issuer
                .as_str()
                .cmp(right.issuer.as_str())
                .then(
                    left.issuer_receiver_path
                        .as_str()
                        .cmp(right.issuer_receiver_path.as_str()),
                )
                .then(left.receipt_id.cmp(&right.receipt_id))
        });

        let mut receipt_issuance_records = state
            .receipt_issuance_records
            .into_values()
            .collect::<Vec<_>>();
        receipt_issuance_records.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(
                    left.counterparty_receiver_path
                        .as_str()
                        .cmp(right.counterparty_receiver_path.as_str()),
                )
                .then(left.receipt_id.cmp(&right.receipt_id))
        });

        Self {
            version: SDK_BACKUP_VERSION,
            local_receiver_path,
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
            receipt_issuance_records,
            next_outbound_private_message_id: state.next_outbound_private_message_id,
            next_receive_batch_id: state.next_receive_batch_id,
            next_private_stream_item_id: state.next_private_stream_item_id,
        }
    }

    fn into_storage_state(
        self,
        current_identity: Option<&IdentityState>,
        local_receiver_path: &PaykitReceiverPath,
        next_peer_link_operation_lease_id: u64,
    ) -> Result<(ValidatedStorageState, RestoreReport)> {
        self.validate(current_identity, local_receiver_path)?;

        let mut identity_state = self.identity_state;
        preserve_current_sign_out_generation(&mut identity_state, current_identity);
        let mut linked_peers = keyed_by_peer(self.linked_peers, "Linked Peer")?;
        validate_linked_peer_records(&linked_peers)?;
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
            |record| {
                (
                    record.counterparty.clone(),
                    record.counterparty_receiver_path.clone(),
                    record.reservation_id.clone(),
                )
            },
            "Payment Endpoint Reservation",
        )?;
        // Normalize derived private stream classification and index state from
        // raw message data before any private-message re-validation, so
        // backups written by an older classifier generation restore instead of
        // failing byte-for-byte derived-state comparison. Cached Receipt
        // records orphaned by that reconciliation are dropped the same way.
        // Immutable source context stays validated: the uniqueness checks
        // here still reject, and every check below runs against the
        // normalized state.
        let mut private_stream_items = unique_private_stream_items(self.private_stream_items)?;
        let event_dedup_records = keyed_by_tuple(
            self.event_dedup_records,
            |record| {
                (
                    record.counterparty.clone(),
                    record.counterparty_receiver_path.clone(),
                    record.event_id.clone(),
                )
            },
            "Event dedupe",
        )?;
        let receipt_access_records = keyed_by_tuple(
            self.receipt_access_records,
            |record| {
                (
                    record.counterparty.clone(),
                    record.counterparty_receiver_path.clone(),
                    record.event_id.clone(),
                )
            },
            "Receipt Access",
        )?;
        let mut receipt_records = keyed_by_tuple(
            self.receipt_records,
            |record| {
                (
                    record.issuer.clone(),
                    record.issuer_receiver_path.clone(),
                    record.receipt_id.clone(),
                )
            },
            "Receipt",
        )?;
        let normalization = compute_private_stream_normalization(
            &private_stream_items,
            &event_dedup_records,
            &receipt_access_records,
            &receipt_records,
        );
        for update in normalization.item_updates {
            // Updates are derived from these items, so every id resolves; a
            // skipped update would still fail stream-item validation below.
            let Some(item) = private_stream_items
                .iter_mut()
                .find(|item| item.stream_item_id == update.stream_item_id)
            else {
                continue;
            };
            item.parsed_version = update.parsed_version;
            item.parsed_kind = update.parsed_kind;
            item.known_paykit_kind = update.known_paykit_kind;
            item.parse_status = update.parse_status;
            item.parse_error = update.parse_error;
        }
        let event_dedup_records = normalization.expected_event_dedup_records;
        let receipt_access_records = normalization.expected_receipt_access_records;
        for key in &normalization.removed_receipt_record_keys {
            receipt_records.remove(key);
        }
        let mut encrypted_link_states =
            keyed_by_peer(self.encrypted_link_states, "Encrypted Link state")?;
        validate_encrypted_link_snapshots(&encrypted_link_states, local_receiver_path)?;
        let mut outbound_private_messages =
            unique_outbound_messages(self.outbound_private_messages)?;
        validate_outbound_private_messages(&outbound_private_messages, &self.local_receiver_path)?;
        validate_payment_endpoint_reservations(
            &payment_endpoint_reservations,
            &outbound_private_messages,
        )?;
        validate_private_stream_items(&private_stream_items)?;
        validate_event_dedup_records(&event_dedup_records, &private_stream_items)?;
        validate_receipt_access_records(&receipt_access_records, &private_stream_items)?;
        validate_required_private_stream_indexes(
            &private_stream_items,
            &event_dedup_records,
            &receipt_access_records,
        )?;
        let expected_receipt_recipient = identity_state
            .as_ref()
            .and_then(|identity| identity.local_pubky_public_key.as_ref());
        validate_receipt_records(
            &receipt_records,
            &receipt_access_records,
            expected_receipt_recipient,
        )?;
        let receipt_issuance_records = keyed_by_tuple(
            self.receipt_issuance_records,
            |record| {
                (
                    record.counterparty.clone(),
                    record.counterparty_receiver_path.clone(),
                    record.receipt_id.clone(),
                )
            },
            "Receipt issuance",
        )?;
        validate_receipt_issuance_records(
            &receipt_issuance_records,
            &outbound_private_messages,
            &self.local_receiver_path,
        )?;

        let recovery_required_peers = reconcile_restored_linked_peers(
            &mut linked_peers,
            &encrypted_link_states,
            &outbound_private_messages,
        );
        clear_recovery_required_link_snapshots(
            &mut encrypted_link_states,
            &recovery_required_peers,
        );
        mark_restored_sending_outbound_recovery_required(
            &mut outbound_private_messages,
            &recovery_required_peers,
        );

        let next_outbound_private_message_id = self
            .next_outbound_private_message_id
            .max(next_outbound_id(&outbound_private_messages));
        let next_receive_batch_id = self
            .next_receive_batch_id
            .max(next_receive_batch_id(&private_stream_items));
        let next_private_stream_item_id = self
            .next_private_stream_item_id
            .max(next_private_stream_item_id(&private_stream_items));

        let recovery_required_report_peers = recovery_required_peers
            .iter()
            .map(
                |(counterparty, counterparty_receiver_path)| RestoreRecoveryRequiredPeer {
                    counterparty: counterparty.clone(),
                    counterparty_receiver_path: counterparty_receiver_path.clone(),
                },
            )
            .collect();

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
            receipt_issuance_records: receipt_issuance_records.len(),
            recovery_required_peers: recovery_required_report_peers,
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
            receipt_issuance_records,
        };

        Ok((ValidatedStorageState::new(state), report))
    }

    fn validate(
        &self,
        current_identity: Option<&IdentityState>,
        local_receiver_path: &PaykitReceiverPath,
    ) -> Result<()> {
        if self.version != SDK_BACKUP_VERSION {
            return Err(PaykitSdkError::Protocol {
                context: format!("unsupported SDK backup version {}", self.version),
                source: None,
            });
        }
        if &self.local_receiver_path != local_receiver_path {
            return Err(PaykitSdkError::Protocol {
                context: format!(
                    "backup receiver path '{}' does not match local receiver path '{}'",
                    self.local_receiver_path, local_receiver_path
                ),
                source: None,
            });
        }

        if let Some(current_public_key) =
            current_identity.and_then(|state| state.local_pubky_public_key.as_ref())
        {
            let backup_public_key = self
                .identity_state
                .as_ref()
                .and_then(|state| state.local_pubky_public_key.as_ref());
            if backup_public_key != Some(current_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "backup identity does not match current local identity".into(),
                    source: None,
                });
            }
        }

        if let Some(current_receiver_noise_public_key) =
            current_identity.and_then(|state| state.local_receiver_noise_public_key.as_ref())
        {
            let backup_receiver_noise_public_key = self
                .identity_state
                .as_ref()
                .and_then(|state| state.local_receiver_noise_public_key.as_ref());
            if backup_receiver_noise_public_key != Some(current_receiver_noise_public_key) {
                return Err(PaykitSdkError::Identity {
                    context: "backup receiver Noise key does not match current receiver".into(),
                    source: None,
                });
            }
        }

        let backup_public_key = self
            .identity_state
            .as_ref()
            .and_then(|state| state.local_pubky_public_key.as_ref());
        if backup_public_key.is_none() && self.has_identity_scoped_state() {
            return Err(PaykitSdkError::Protocol {
                context: "backup has SDK state but no local public identity".into(),
                source: None,
            });
        }

        Ok(())
    }

    pub(crate) fn local_pubky_public_key(&self) -> Option<&PubkyPublicKey> {
        self.identity_state
            .as_ref()
            .and_then(|state| state.local_pubky_public_key.as_ref())
    }

    pub(crate) fn local_receiver_noise_public_key(&self) -> Option<&PubkyPublicKey> {
        self.identity_state
            .as_ref()
            .and_then(|state| state.local_receiver_noise_public_key.as_ref())
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
            || !self.receipt_issuance_records.is_empty()
    }
}

#[cfg(test)]
mod tests;
