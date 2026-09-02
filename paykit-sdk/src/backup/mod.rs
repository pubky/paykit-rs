//! SDK backup and restore state.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::contacts::ContactRecord,
    domain::endpoint_reservations::{reservation_payload_hash, validate_reservation_id},
    domain::linked_peers::LinkedPeerState,
    domain::outbound_private::validate_queued_outbound_private_message,
    domain::payment_requests::{
        derive_payment_request_records_from_parts, payment_request_record_blocks_app_removal,
        PaymentRequestLifecycleState, PaymentRequestLocalRole,
    },
    domain::private_lists::counterparties_with_shared_private_payment_lists,
    domain::private_stream::{
        classify_private_application_message, payload_hash, PrivateStreamParseStatus,
    },
    domain::publication::PublicationStatus,
    domain::receipts::{
        receipt_access_key_hash, receipt_record_matches_access, ReceiptAccessRecord,
        ReceiptIssuanceRecord, ReceiptIssuanceStatus, ReceiptRecord, ReceiptRetrievalStatus,
    },
    domain::records::{AmountRecord, BillingPeriodRecord},
    identity::{IdentityState, PubkyPublicKey},
    storage::{
        EncryptedLinkStateRecord, EventDedupRecord, LinkedPeerRecord, OutboundPrivateMessageRecord,
        PaymentEndpointReservationRecord, PaymentRequestExecutionClaim, PrivateStreamItemRecord,
        PublicEndpointRecord, StorageAdapter, StorageState, ValidatedStorageState,
    },
    OutboundPrivateMessageStatus, PaykitSdkError, Result,
};
use paykit_lib::{
    parse_private_payment_list_json, PaymentEndpointIdentifier, PrivateApplicationMessage,
    PrivateMessageKind, ReceiptId,
};

pub(crate) mod validation;

use validation::*;

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
    /// Applications retired until explicitly published again.
    pub retired_paykit_apps: Vec<paykit_lib::PaykitAppId>,
    /// Public Payment Endpoint records.
    pub public_endpoint_records: Vec<PublicEndpointRecord>,
    /// Payment Endpoint Reservation records.
    pub payment_endpoint_reservations: Vec<PaymentEndpointReservationRecord>,
    /// Durable ownership claims for unresolved Payment Request execution.
    pub payment_request_execution_claims: Vec<PaymentRequestExecutionClaim>,
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
            .field(
                "identity_state",
                &self.identity_state.as_ref().map(|_| "<redacted>"),
            )
            .field("linked_peers", &self.linked_peers.len())
            .field("contact_records", &self.contact_records.len())
            .field("retired_paykit_apps", &self.retired_paykit_apps.len())
            .field(
                "public_endpoint_records",
                &self.public_endpoint_records.len(),
            )
            .field(
                "payment_endpoint_reservations",
                &self.payment_endpoint_reservations.len(),
            )
            .field(
                "payment_request_execution_claims",
                &self.payment_request_execution_claims.len(),
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
    /// Number of restored Contact Records.
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
    /// Counterparties restored as recovery-required.
    pub recovery_required_peers: Vec<PubkyPublicKey>,
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

#[cfg(test)]
pub(crate) async fn restore_backup_state<S>(
    storage: &S,
    backup: SdkBackupState,
) -> Result<RestoreReport>
where
    S: StorageAdapter,
{
    restore_backup_state_with_identity(storage, backup, None, DateTime::<Utc>::MIN_UTC).await
}

pub(crate) async fn restore_backup_state_with_identity<S>(
    storage: &S,
    backup: SdkBackupState,
    trusted_identity: Option<IdentityState>,
    now: DateTime<Utc>,
) -> Result<RestoreReport>
where
    S: StorageAdapter,
{
    storage
        .transaction(move |tx| {
            let current_state = tx.export_storage_state();
            if current_state
                .peer_link_operation_leases
                .values()
                .any(|lease| lease.expires_at > now)
                || current_state
                    .paykit_app_operation_leases
                    .values()
                    .any(|lease| lease.expires_at > now)
            {
                return Err(PaykitSdkError::Policy {
                    context: "cannot restore backup while shared SDK work is in progress".into(),
                    source: None,
                });
            }
            let stored_identity = tx.load_identity_state();
            if let (Some(stored), Some(trusted)) =
                (stored_identity.as_ref(), trusted_identity.as_ref())
            {
                if stored.public_key.is_some()
                    && trusted.public_key.is_some()
                    && stored.public_key != trusted.public_key
                {
                    return Err(PaykitSdkError::Identity {
                        context: "backup identity does not match this SDK state backing".into(),
                        source: None,
                    });
                }
            }
            if !storage_state_is_empty_except_identity(&current_state) {
                return Err(PaykitSdkError::Policy {
                    context: "cannot restore a backup over existing SDK-managed state".into(),
                    source: None,
                });
            }
            let current_identity = match (stored_identity.as_ref(), trusted_identity.as_ref()) {
                (Some(stored), Some(trusted)) if stored.public_key == trusted.public_key => {
                    Some(stored)
                }
                (_, Some(trusted)) => Some(trusted),
                (Some(stored), None) => Some(stored),
                (None, None) => None,
            };
            let current_next_peer_link_operation_lease_id =
                current_state.next_peer_link_operation_lease_id;
            let current_next_paykit_app_operation_lease_id =
                current_state.next_paykit_app_operation_lease_id;
            let (state, report) = backup.into_storage_state(
                current_identity,
                current_next_peer_link_operation_lease_id,
                current_next_paykit_app_operation_lease_id,
            )?;
            tx.replace_storage_state(state);
            Ok(report)
        })
        .await
}

fn storage_state_is_empty_except_identity(state: &StorageState) -> bool {
    let mut empty = StorageState {
        identity_state: state.identity_state.clone(),
        peer_link_operation_leases: state.peer_link_operation_leases.clone(),
        paykit_app_operation_leases: state.paykit_app_operation_leases.clone(),
        ..StorageState::default()
    };
    empty.next_peer_link_operation_lease_id = state.next_peer_link_operation_lease_id;
    empty.next_paykit_app_operation_lease_id = state.next_paykit_app_operation_lease_id;
    state == &empty
}

impl SdkBackupState {
    pub(crate) fn from_storage_state(state: StorageState) -> Self {
        let mut linked_peers = state.linked_peers.into_values().collect::<Vec<_>>();
        linked_peers
            .sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));

        let mut contact_records = state.contact_records.into_values().collect::<Vec<_>>();
        contact_records
            .sort_by(|left, right| left.public_key.as_str().cmp(right.public_key.as_str()));

        let mut retired_paykit_apps = state.retired_paykit_apps.into_iter().collect::<Vec<_>>();
        retired_paykit_apps.sort_by(|left, right| left.as_str().cmp(right.as_str()));

        let mut public_endpoint_records = state
            .public_endpoint_records
            .into_values()
            .collect::<Vec<_>>();
        public_endpoint_records.sort_by(|left, right| {
            left.app_id
                .as_str()
                .cmp(right.app_id.as_str())
                .then(left.identifier.cmp(&right.identifier))
        });

        let mut payment_endpoint_reservations = state
            .payment_endpoint_reservations
            .into_values()
            .collect::<Vec<_>>();
        payment_endpoint_reservations.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.app_id.as_str().cmp(right.app_id.as_str()))
                .then(left.reservation_id.cmp(&right.reservation_id))
        });

        let mut encrypted_link_states = state
            .encrypted_link_states
            .into_values()
            .collect::<Vec<_>>();
        encrypted_link_states
            .sort_by(|left, right| left.counterparty.as_str().cmp(right.counterparty.as_str()));

        let mut payment_request_execution_claims = state
            .payment_request_execution_claims
            .into_values()
            .collect::<Vec<_>>();
        payment_request_execution_claims.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.payment_request_id.cmp(&right.payment_request_id))
        });

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

        let mut receipt_issuance_records = state
            .receipt_issuance_records
            .into_values()
            .collect::<Vec<_>>();
        receipt_issuance_records.sort_by(|left, right| {
            left.counterparty
                .as_str()
                .cmp(right.counterparty.as_str())
                .then(left.receipt_id.cmp(&right.receipt_id))
        });

        Self {
            version: SDK_BACKUP_VERSION,
            identity_state: state.identity_state,
            linked_peers,
            contact_records,
            retired_paykit_apps,
            public_endpoint_records,
            payment_endpoint_reservations,
            payment_request_execution_claims,
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
        next_peer_link_operation_lease_id: u64,
        next_paykit_app_operation_lease_id: u64,
    ) -> Result<(ValidatedStorageState, RestoreReport)> {
        self.validate(current_identity)?;

        let identity_state = self.identity_state;
        let mut linked_peers = keyed_by_counterparty(self.linked_peers, "Linked Peer")?;
        validate_linked_peer_records(&linked_peers)?;
        let contact_records = keyed_by_tuple(
            self.contact_records,
            |record| record.public_key.clone(),
            "Contact Record",
        )?;
        validate_contact_records(&contact_records)?;
        let retired_paykit_apps = keyed_by_tuple(
            self.retired_paykit_apps,
            |app_id| app_id.clone(),
            "retired Paykit App",
        )?
        .into_keys()
        .collect();
        let public_endpoint_records = keyed_by_tuple(
            self.public_endpoint_records,
            |record| (record.app_id.clone(), record.identifier.clone()),
            "public Payment Endpoint",
        )?;
        validate_public_endpoint_records(&public_endpoint_records)?;
        let payment_endpoint_reservations = keyed_by_tuple(
            self.payment_endpoint_reservations,
            |record| {
                (
                    record.counterparty.clone(),
                    record.app_id.clone(),
                    record.reservation_id.clone(),
                )
            },
            "Payment Endpoint Reservation",
        )?;
        let mut encrypted_link_states =
            keyed_by_counterparty(self.encrypted_link_states, "Encrypted Link state")?;
        validate_encrypted_link_snapshots(&encrypted_link_states)?;
        let mut outbound_private_messages =
            unique_outbound_messages(self.outbound_private_messages)?;
        validate_outbound_private_messages(&outbound_private_messages)?;
        validate_retired_app_outbound_messages(&retired_paykit_apps, &outbound_private_messages)?;
        validate_retired_app_private_payment_lists(
            &retired_paykit_apps,
            &outbound_private_messages,
        )?;
        validate_payment_endpoint_reservations(
            &payment_endpoint_reservations,
            &outbound_private_messages,
        )?;
        let payment_request_execution_claims = keyed_by_tuple(
            self.payment_request_execution_claims,
            |record| {
                (
                    record.counterparty.clone(),
                    record.payment_request_id.clone(),
                )
            },
            "Payment Request execution claim",
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
        validate_required_private_stream_indexes(
            &private_stream_items,
            &event_dedup_records,
            &receipt_access_records,
        )?;
        let receipt_records = keyed_by_tuple(
            self.receipt_records,
            |record| (record.issuer.clone(), record.receipt_id.clone()),
            "Receipt",
        )?;
        let expected_receipt_recipient = identity_state
            .as_ref()
            .and_then(|identity| identity.public_key.as_ref());
        validate_receipt_records(
            &receipt_records,
            &receipt_access_records,
            &event_dedup_records,
            expected_receipt_recipient,
        )?;
        let receipt_issuance_records = keyed_by_tuple(
            self.receipt_issuance_records,
            |record| (record.counterparty.clone(), record.receipt_id.clone()),
            "Receipt issuance",
        )?;
        validate_receipt_issuance_records(&receipt_issuance_records, &outbound_private_messages)?;
        validate_retired_app_payment_requests(
            &retired_paykit_apps,
            &private_stream_items,
            &outbound_private_messages,
            &event_dedup_records,
        )?;
        validate_retired_app_receipt_issuance(
            &retired_paykit_apps,
            &receipt_issuance_records,
            &outbound_private_messages,
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
            recovery_required_peers,
        };

        let state = StorageState {
            identity_state,
            linked_peers,
            contact_records,
            authorized_paykit_apps: HashMap::new(),
            registered_paykit_apps: HashSet::new(),
            registered_paykit_app_capabilities: HashMap::new(),
            retired_paykit_apps,
            public_endpoint_records,
            payment_endpoint_reservations,
            encrypted_link_states,
            peer_link_operation_leases: HashMap::new(),
            next_peer_link_operation_lease_id,
            paykit_app_operation_leases: HashMap::new(),
            next_paykit_app_operation_lease_id,
            payment_request_execution_claims,
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

        validation::validate_storage_state(&state)?;
        Ok((ValidatedStorageState::new(state), report))
    }

    fn validate(&self, current_identity: Option<&IdentityState>) -> Result<()> {
        if self.version != SDK_BACKUP_VERSION {
            return Err(PaykitSdkError::Protocol {
                context: format!("unsupported SDK backup version {}", self.version),
                source: None,
            });
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
            return Err(PaykitSdkError::Protocol {
                context: "backup has SDK state but no local public identity".into(),
                source: None,
            });
        }

        Ok(())
    }

    pub(crate) fn local_public_key(&self) -> Option<&PubkyPublicKey> {
        self.identity_state
            .as_ref()
            .and_then(|state| state.public_key.as_ref())
    }
    pub(crate) fn has_identity_scoped_state(&self) -> bool {
        !self.linked_peers.is_empty()
            || !self.contact_records.is_empty()
            || !self.retired_paykit_apps.is_empty()
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

    pub(crate) fn has_private_state(&self) -> bool {
        !self.linked_peers.is_empty()
            || !self.retired_paykit_apps.is_empty()
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
