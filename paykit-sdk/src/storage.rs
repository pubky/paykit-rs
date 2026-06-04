use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    endpoints::EndpointPublicationStatus,
    identity::{IdentityState, PubkyPublicKey},
    linked_peers::LinkedPeerState,
    outbound_private::OutboundPrivateMessageStatus,
    private_stream::PrivateStreamParseStatus,
    receipts::{ReceiptAccessRecord, ReceiptRecord},
    PaykitSdkError, Result,
};

/// Durable storage boundary for Paykit SDK.
#[async_trait]
pub trait StorageAdapter: Send + Sync {
    /// Run an atomic storage transaction.
    async fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: FnOnce(&mut dyn StorageTransaction) -> Result<T> + Send;

    /// Load the current identity state.
    async fn load_identity_state(&self) -> Result<Option<IdentityState>> {
        self.transaction(|tx| Ok(tx.load_identity_state())).await
    }

    /// Save the current identity state atomically.
    async fn save_identity_state(&self, state: IdentityState) -> Result<()> {
        self.transaction(move |tx| {
            tx.save_identity_state(state);
            Ok(())
        })
        .await
    }
}

/// Mutable operations available inside one storage transaction.
pub trait StorageTransaction {
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

    /// List SDK-managed public Payment Endpoint records.
    fn public_endpoint_records(&self) -> Vec<PublicEndpointRecord>;

    /// Save one SDK-managed public Payment Endpoint record.
    fn save_public_endpoint_record(&mut self, record: PublicEndpointRecord);

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

    /// Claim the next outbound private message for sending, preserving FIFO.
    fn claim_next_outbound_private_message(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
    ) -> Option<OutboundPrivateMessageRecord>;

    /// Save updates for one existing outbound private message record.
    fn save_outbound_private_message(&mut self, record: OutboundPrivateMessageRecord);

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

/// Durable Linked Peer state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPeerRecord {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Current local relationship/link state.
    pub state: LinkedPeerState,
    /// Last successful sync time.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// Last private receive time.
    pub last_private_receive_at: Option<DateTime<Utc>>,
    /// Consecutive failure count for recovery/retry policy.
    pub failure_count: u32,
}

/// Durable SDK-managed public Payment Endpoint publication record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicEndpointRecord {
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Last payload the SDK tried to publish.
    pub payload: Option<String>,
    /// Current publication status.
    pub status: EndpointPublicationStatus,
    /// Last status update time.
    pub updated_at: DateTime<Utc>,
    /// Last sync error, when available.
    pub last_error: Option<String>,
}

/// Durable Encrypted Link snapshot state.
///
/// Snapshot bytes contain Noise key and counter material. Store them encrypted
/// at rest and avoid logging them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedLinkStateRecord {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Serialized active link snapshot.
    pub link_snapshot: Option<Vec<u8>>,
    /// Serialized in-progress handshake snapshot.
    pub handshake_snapshot: Option<Vec<u8>>,
    /// Local role for the in-progress handshake.
    pub handshake_role: Option<crate::EncryptedLinkHandshakeRole>,
    /// Local snapshot generation.
    pub generation: u64,
    /// Last checkpoint time.
    pub checkpointed_at: DateTime<Utc>,
}

impl fmt::Debug for EncryptedLinkStateRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncryptedLinkStateRecord")
            .field("counterparty", &self.counterparty)
            .field(
                "link_snapshot",
                &self
                    .link_snapshot
                    .as_ref()
                    .map(|snapshot| format!("<redacted:{} bytes>", snapshot.len())),
            )
            .field(
                "handshake_snapshot",
                &self
                    .handshake_snapshot
                    .as_ref()
                    .map(|snapshot| format!("<redacted:{} bytes>", snapshot.len())),
            )
            .field("handshake_role", &self.handshake_role)
            .field("generation", &self.generation)
            .field("checkpointed_at", &self.checkpointed_at)
            .finish()
    }
}

/// Storage-backed lease for one peer link operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerLinkOperationLease {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Assigned lease id.
    pub lease_id: u64,
    /// Claim time.
    pub claimed_at: DateTime<Utc>,
    /// Expiry time after which another worker may retry.
    pub expires_at: DateTime<Utc>,
}

/// New outbound private message before storage assigns an id.
///
/// The payload may contain private Paykit secrets. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct NewOutboundPrivateMessage {
    counterparty: PubkyPublicKey,
    kind: String,
    raw_json: String,
    created_at: DateTime<Utc>,
}

impl NewOutboundPrivateMessage {
    pub(crate) fn new(
        counterparty: PubkyPublicKey,
        kind: String,
        raw_json: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            counterparty,
            kind,
            raw_json,
            created_at,
        }
    }

    /// Counterparty public key.
    pub fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }

    /// Private Message Kind string.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Exact outbound JSON payload to send.
    pub fn raw_json(&self) -> &str {
        &self.raw_json
    }

    /// Queue time.
    pub fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

impl fmt::Debug for NewOutboundPrivateMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewOutboundPrivateMessage")
            .field("counterparty", &self.counterparty)
            .field("kind", &self.kind)
            .field(
                "raw_json",
                &format!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("created_at", &self.created_at)
            .finish()
    }
}

/// Durable outbound private message.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPrivateMessageRecord {
    /// Assigned outbound message id.
    pub outbound_message_id: u64,
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Private Message Kind string.
    pub kind: String,
    /// Exact outbound JSON payload to send.
    pub raw_json: String,
    /// Delivery status.
    pub status: OutboundPrivateMessageStatus,
    /// Number of send attempts.
    pub attempt_count: u32,
    /// Queue time.
    pub created_at: DateTime<Utc>,
    /// Last status update time.
    pub updated_at: DateTime<Utc>,
    /// Last send attempt time.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// Successful send time.
    pub sent_at: Option<DateTime<Utc>>,
    /// Last send error, when available.
    pub last_error: Option<String>,
}

impl fmt::Debug for OutboundPrivateMessageRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundPrivateMessageRecord")
            .field("outbound_message_id", &self.outbound_message_id)
            .field("counterparty", &self.counterparty)
            .field("kind", &self.kind)
            .field(
                "raw_json",
                &format!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("status", &self.status)
            .field("attempt_count", &self.attempt_count)
            .field("created_at", &self.created_at)
            .field("updated_at", &self.updated_at)
            .field("last_attempt_at", &self.last_attempt_at)
            .field("sent_at", &self.sent_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

impl OutboundPrivateMessageRecord {
    fn from_new(outbound_message_id: u64, message: NewOutboundPrivateMessage) -> Self {
        Self {
            outbound_message_id,
            counterparty: message.counterparty,
            kind: message.kind,
            raw_json: message.raw_json,
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: message.created_at,
            updated_at: message.created_at,
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        }
    }
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

/// New private stream item before storage assigns an id.
///
/// The payload may contain private Paykit secrets. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct NewPrivateStreamItem {
    counterparty: PubkyPublicKey,
    receive_batch_id: u64,
    raw_json: String,
    parsed_version: Option<u32>,
    parsed_kind: Option<String>,
    known_paykit_kind: Option<String>,
    parse_status: PrivateStreamParseStatus,
    parse_error: Option<String>,
    received_at: DateTime<Utc>,
}

impl NewPrivateStreamItem {
    pub(crate) fn new(details: NewPrivateStreamItemDetails) -> Self {
        Self {
            counterparty: details.counterparty,
            receive_batch_id: details.receive_batch_id,
            raw_json: details.raw_json,
            parsed_version: details.parsed_version,
            parsed_kind: details.parsed_kind,
            known_paykit_kind: details.known_paykit_kind,
            parse_status: details.parse_status,
            parse_error: details.parse_error,
            received_at: details.received_at,
        }
    }

    /// Counterparty public key.
    pub fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }

    /// Receive batch id assigned by the SDK runtime.
    pub fn receive_batch_id(&self) -> u64 {
        self.receive_batch_id
    }

    /// Raw plaintext payload.
    pub fn raw_json(&self) -> &str {
        &self.raw_json
    }

    /// Parsed Private Application Message version.
    pub fn parsed_version(&self) -> Option<u32> {
        self.parsed_version
    }

    /// Parsed Private Application Message kind.
    pub fn parsed_kind(&self) -> Option<&str> {
        self.parsed_kind.as_deref()
    }

    /// Whether the kind is a known Paykit kind.
    pub fn known_paykit_kind(&self) -> Option<&str> {
        self.known_paykit_kind.as_deref()
    }

    /// Parse status.
    pub fn parse_status(&self) -> PrivateStreamParseStatus {
        self.parse_status.clone()
    }

    /// Parse error, when available.
    pub fn parse_error(&self) -> Option<&str> {
        self.parse_error.as_deref()
    }

    /// Receive time.
    pub fn received_at(&self) -> DateTime<Utc> {
        self.received_at
    }
}

pub(crate) struct NewPrivateStreamItemDetails {
    pub(crate) counterparty: PubkyPublicKey,
    pub(crate) receive_batch_id: u64,
    pub(crate) raw_json: String,
    pub(crate) parsed_version: Option<u32>,
    pub(crate) parsed_kind: Option<String>,
    pub(crate) known_paykit_kind: Option<String>,
    pub(crate) parse_status: PrivateStreamParseStatus,
    pub(crate) parse_error: Option<String>,
    pub(crate) received_at: DateTime<Utc>,
}

impl fmt::Debug for NewPrivateStreamItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewPrivateStreamItem")
            .field("counterparty", &self.counterparty)
            .field("receive_batch_id", &self.receive_batch_id)
            .field(
                "raw_json",
                &format!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("parsed_version", &self.parsed_version)
            .field("parsed_kind", &self.parsed_kind)
            .field("known_paykit_kind", &self.known_paykit_kind)
            .field("parse_status", &self.parse_status)
            .field("parse_error", &self.parse_error)
            .field("received_at", &self.received_at)
            .finish()
    }
}

/// Durable private stream item.
///
/// The payload may contain private Paykit secrets. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateStreamItemRecord {
    /// Assigned stream item id.
    pub stream_item_id: u64,
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Receive batch id assigned by the SDK runtime.
    pub receive_batch_id: u64,
    /// Raw plaintext payload.
    pub raw_json: String,
    /// Parsed Private Application Message version.
    pub parsed_version: Option<u32>,
    /// Parsed Private Application Message kind.
    pub parsed_kind: Option<String>,
    /// Whether the kind is a known Paykit kind.
    pub known_paykit_kind: Option<String>,
    /// Parse status.
    pub parse_status: PrivateStreamParseStatus,
    /// Parse error, when available.
    pub parse_error: Option<String>,
    /// Receive time.
    pub received_at: DateTime<Utc>,
}

impl fmt::Debug for PrivateStreamItemRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateStreamItemRecord")
            .field("stream_item_id", &self.stream_item_id)
            .field("counterparty", &self.counterparty)
            .field("receive_batch_id", &self.receive_batch_id)
            .field(
                "raw_json",
                &format!("<redacted:{} bytes>", self.raw_json.len()),
            )
            .field("parsed_version", &self.parsed_version)
            .field("parsed_kind", &self.parsed_kind)
            .field("known_paykit_kind", &self.known_paykit_kind)
            .field("parse_status", &self.parse_status)
            .field("parse_error", &self.parse_error)
            .field("received_at", &self.received_at)
            .finish()
    }
}

impl PrivateStreamItemRecord {
    fn from_new(stream_item_id: u64, item: NewPrivateStreamItem) -> Self {
        Self {
            stream_item_id,
            counterparty: item.counterparty,
            receive_batch_id: item.receive_batch_id,
            raw_json: item.raw_json,
            parsed_version: item.parsed_version,
            parsed_kind: item.parsed_kind,
            known_paykit_kind: item.known_paykit_kind,
            parse_status: item.parse_status,
            parse_error: item.parse_error,
            received_at: item.received_at,
        }
    }
}

/// Event Message dedupe/conflict record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDedupRecord {
    /// Counterparty that sent the event.
    pub counterparty: PubkyPublicKey,
    /// Event ID.
    pub event_id: String,
    /// Event kind.
    pub event_kind: String,
    /// Hash of the exact payload bytes.
    pub payload_hash: String,
    /// First stream item that carried the event.
    pub first_stream_item_id: u64,
    /// Duplicate stream items with the same payload.
    pub duplicate_stream_item_ids: Vec<u64>,
    /// Conflicting stream items that reused the Event ID with a different payload.
    pub conflicting_stream_item_ids: Vec<u64>,
}

/// In-memory storage state used for tests and examples.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageState {
    /// Current identity state.
    pub identity_state: Option<IdentityState>,
    /// Linked Peer records by counterparty.
    pub linked_peers: HashMap<PubkyPublicKey, LinkedPeerRecord>,
    /// SDK-managed public endpoint records by identifier.
    pub public_endpoint_records: HashMap<String, PublicEndpointRecord>,
    /// Encrypted Link state records by counterparty.
    pub encrypted_link_states: HashMap<PubkyPublicKey, EncryptedLinkStateRecord>,
    /// Active peer link operation leases by counterparty.
    pub peer_link_operation_leases: HashMap<PubkyPublicKey, PeerLinkOperationLease>,
    /// Next peer link operation lease id.
    pub next_peer_link_operation_lease_id: u64,
    /// Append-only outbound private message records.
    pub outbound_private_messages: Vec<OutboundPrivateMessageRecord>,
    /// Next outbound private message id.
    pub next_outbound_private_message_id: u64,
    /// Append-only private stream items.
    pub private_stream_items: Vec<PrivateStreamItemRecord>,
    /// Next receive batch id.
    pub next_receive_batch_id: u64,
    /// Next private stream item id.
    pub next_private_stream_item_id: u64,
    /// Event dedupe records by counterparty and Event ID.
    pub event_dedup_records: HashMap<(PubkyPublicKey, String), EventDedupRecord>,
    /// Receipt Access records by counterparty and Event ID.
    pub receipt_access_records: HashMap<(PubkyPublicKey, String), ReceiptAccessRecord>,
    /// Decrypted Receipt records by issuer and Receipt ID.
    pub receipt_records: HashMap<(PubkyPublicKey, String), ReceiptRecord>,
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
    async fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        T: Send,
        F: FnOnce(&mut dyn StorageTransaction) -> Result<T> + Send,
    {
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
    fn load_identity_state(&self) -> Option<IdentityState> {
        self.state.identity_state.clone()
    }

    fn save_identity_state(&mut self, state: IdentityState) {
        self.state.identity_state = Some(state);
    }

    fn clear_identity_scoped_state(&mut self) {
        self.clear_private_identity_scoped_state();
        self.state.public_endpoint_records.clear();
    }

    fn clear_private_identity_scoped_state(&mut self) {
        self.state.linked_peers.clear();
        self.state.encrypted_link_states.clear();
        self.state.peer_link_operation_leases.clear();
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

    fn public_endpoint_records(&self) -> Vec<PublicEndpointRecord> {
        self.state
            .public_endpoint_records
            .values()
            .cloned()
            .collect()
    }

    fn save_public_endpoint_record(&mut self, record: PublicEndpointRecord) {
        self.state
            .public_endpoint_records
            .insert(record.identifier.clone(), record);
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

    fn claim_next_outbound_private_message(
        &mut self,
        counterparty: &PubkyPublicKey,
        now: DateTime<Utc>,
        stale_before: DateTime<Utc>,
    ) -> Option<OutboundPrivateMessageRecord> {
        let mut indexes = self
            .state
            .outbound_private_messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                &message.counterparty == counterparty
                    && !matches!(
                        message.status,
                        OutboundPrivateMessageStatus::Sent | OutboundPrivateMessageStatus::Invalid
                    )
            })
            .map(|(index, message)| (index, message.outbound_message_id))
            .collect::<Vec<_>>();
        indexes.sort_by_key(|(_, outbound_message_id)| *outbound_message_id);

        let (index, _) = indexes.first().copied()?;
        let message = &mut self.state.outbound_private_messages[index];
        if !is_claimable_outbound_private_message(message, stale_before) {
            return None;
        }

        message.status = OutboundPrivateMessageStatus::Sending;
        message.attempt_count = message.attempt_count.saturating_add(1);
        message.last_attempt_at = Some(now);
        message.updated_at = now;
        message.last_error = None;
        Some(message.clone())
    }

    fn save_outbound_private_message(&mut self, record: OutboundPrivateMessageRecord) {
        if let Some(existing) = self
            .state
            .outbound_private_messages
            .iter_mut()
            .find(|message| message.outbound_message_id == record.outbound_message_id)
        {
            *existing = record;
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

fn is_claimable_outbound_private_message(
    message: &OutboundPrivateMessageRecord,
    stale_before: DateTime<Utc>,
) -> bool {
    match message.status {
        OutboundPrivateMessageStatus::Pending | OutboundPrivateMessageStatus::Failed => true,
        OutboundPrivateMessageStatus::Sending => match message.last_attempt_at {
            Some(last_attempt_at) => last_attempt_at <= stale_before,
            None => true,
        },
        OutboundPrivateMessageStatus::Sent | OutboundPrivateMessageStatus::Invalid => false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        outbound_private::{
            claim_next_outbound_private_message, mark_outbound_failed, mark_outbound_invalid,
            mark_outbound_sent,
        },
        queued_outbound_private_messages,
    };

    fn timestamp() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 3, 12, 0, 0).unwrap()
    }

    fn counterparty() -> PubkyPublicKey {
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key())
    }

    fn public_endpoint_record(identifier: &str) -> PublicEndpointRecord {
        PublicEndpointRecord {
            identifier: identifier.into(),
            payload: Some("payload".into()),
            status: EndpointPublicationStatus::Published,
            updated_at: timestamp(),
            last_error: None,
        }
    }

    fn outbound_private_message(counterparty: PubkyPublicKey) -> NewOutboundPrivateMessage {
        NewOutboundPrivateMessage::new(
            counterparty,
            "paykit.private_payment_list".into(),
            r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into(),
            timestamp(),
        )
    }

    fn receipt_access_record(counterparty: PubkyPublicKey) -> ReceiptAccessRecord {
        ReceiptAccessRecord {
            counterparty,
            stream_item_id: 0,
            receive_batch_id: 0,
            event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            location: "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000".into(),
            key: "receipt-secret".into(),
            retrieval_status: crate::ReceiptRetrievalStatus::Pending,
            retrieval_attempted_at: None,
            retrieved_at: None,
            last_retrieval_error: None,
            received_at: timestamp(),
        }
    }

    fn receipt_record(issuer: PubkyPublicKey) -> ReceiptRecord {
        ReceiptRecord {
            issuer,
            receipt_access_event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
            receipt_access_key_hash: "sha256:test".into(),
            receipt_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            payment_reference: "invoice-2026-0001".into(),
            payment_request_id: None,
            billing_period: None,
            recipient_public_key: PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            ),
            payment_endpoint_identifier: None,
            amount: None,
            metadata: serde_json::Map::new(),
            location: "/pub/paykit/v0/private/receipts/550e8400-e29b-41d4-a716-446655440000".into(),
            retrieved_at: timestamp(),
        }
    }

    #[test]
    fn test_sensitive_storage_debug_is_redacted() {
        let counterparty = counterparty();
        let link_state = EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: Some(vec![1, 2, 3]),
            handshake_snapshot: Some(vec![4, 5, 6]),
            handshake_role: None,
            generation: 0,
            checkpointed_at: timestamp(),
        };
        let outbound = OutboundPrivateMessageRecord::from_new(
            0,
            outbound_private_message(counterparty.clone()),
        );
        let stream = PrivateStreamItemRecord::from_new(
            0,
            NewPrivateStreamItem::new(NewPrivateStreamItemDetails {
                counterparty,
                receive_batch_id: 0,
                raw_json: r#"{"key":"secret"}"#.into(),
                parsed_version: Some(1),
                parsed_kind: Some("paykit.receipt_access".into()),
                known_paykit_kind: Some("paykit.receipt_access".into()),
                parse_status: PrivateStreamParseStatus::Valid,
                parse_error: None,
                received_at: timestamp(),
            }),
        );
        let receipt_access = receipt_access_record(stream.counterparty.clone());
        let receipt = receipt_record(receipt_access.counterparty.clone());

        let debug =
            format!("{link_state:?} {outbound:?} {stream:?} {receipt_access:?} {receipt:?}");
        assert!(debug.contains("<redacted:"));
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("receipt-secret"));
        assert!(!debug.contains("[1, 2, 3]"));
    }

    #[tokio::test]
    async fn test_transaction_commits_records() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let stream_item_id = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_linked_peer(LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(timestamp()),
                        last_private_receive_at: Some(timestamp()),
                        failure_count: 0,
                    });
                    tx.save_public_endpoint_record(public_endpoint_record("btc-lightning-bolt11"));
                    tx.insert_outbound_private_message(outbound_private_message(
                        counterparty.clone(),
                    ));

                    let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem::new(
                        NewPrivateStreamItemDetails {
                            counterparty: counterparty.clone(),
                            receive_batch_id: 7,
                            raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                            parsed_version: Some(1),
                            parsed_kind: Some("paykit.test".into()),
                            known_paykit_kind: None,
                            parse_status: PrivateStreamParseStatus::UnknownKind,
                            parse_error: None,
                            received_at: timestamp(),
                        },
                    ));

                    tx.save_event_dedup_record(EventDedupRecord {
                        counterparty: counterparty.clone(),
                        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
                        event_kind: "paykit.test".into(),
                        payload_hash: "hash".into(),
                        first_stream_item_id: stream_item_id,
                        duplicate_stream_item_ids: Vec::new(),
                        conflicting_stream_item_ids: Vec::new(),
                    });

                    tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                    tx.save_receipt_record(receipt_record(counterparty));

                    Ok(stream_item_id)
                }
            })
            .await
            .unwrap();

        let snapshot = storage.snapshot().unwrap();
        assert_eq!(stream_item_id, 0);
        assert_eq!(
            snapshot.linked_peers[&counterparty].state,
            LinkedPeerState::Linked
        );
        assert_eq!(snapshot.public_endpoint_records.len(), 1);
        assert_eq!(snapshot.outbound_private_messages.len(), 1);
        assert_eq!(snapshot.private_stream_items.len(), 1);
        assert_eq!(snapshot.event_dedup_records.len(), 1);
        assert_eq!(snapshot.receipt_access_records.len(), 1);
        assert_eq!(snapshot.receipt_records.len(), 1);
        assert_eq!(snapshot.next_private_stream_item_id, 1);
        assert_eq!(snapshot.next_outbound_private_message_id, 1);
    }

    #[tokio::test]
    async fn test_save_outbound_private_message_updates_existing_only() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_outbound_private_message(OutboundPrivateMessageRecord {
                        outbound_message_id: 99,
                        counterparty,
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
                    });
                    Ok(())
                }
            })
            .await
            .unwrap();

        assert!(storage
            .snapshot()
            .unwrap()
            .outbound_private_messages
            .is_empty());
    }

    #[tokio::test]
    async fn test_invalid_outbound_private_message_does_not_block_later_records() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let (first, second) = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    let first = tx.insert_outbound_private_message(outbound_private_message(
                        counterparty.clone(),
                    ));
                    let second =
                        tx.insert_outbound_private_message(outbound_private_message(counterparty));
                    Ok((first, second))
                }
            })
            .await
            .unwrap();
        storage
            .transaction({
                let invalid = mark_outbound_invalid(
                    first,
                    "invalid private message JSON".into(),
                    timestamp(),
                );
                move |tx| {
                    tx.save_outbound_private_message(invalid);
                    Ok(())
                }
            })
            .await
            .unwrap();

        let claimed = claim_next_outbound_private_message(
            &storage,
            &counterparty,
            timestamp(),
            timestamp() - chrono::Duration::seconds(60),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(claimed.outbound_message_id, second.outbound_message_id);
        assert_eq!(claimed.status, OutboundPrivateMessageStatus::Sending);
        let queued = queued_outbound_private_messages(&storage, &counterparty)
            .await
            .unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].outbound_message_id, second.outbound_message_id);
    }

    #[tokio::test]
    async fn test_peer_link_operation_lease_blocks_until_released() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let first = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();
        let blocked = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    ))
                }
            })
            .await
            .unwrap();
        assert!(blocked.is_none());

        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.release_peer_link_operation(&counterparty, first.lease_id);
                    Ok(())
                }
            })
            .await
            .unwrap();
        let second = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    ))
                }
            })
            .await
            .unwrap();

        assert!(second.is_some());
    }

    #[tokio::test]
    async fn test_clear_identity_scoped_state_preserves_identity_only() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let identity = IdentityState {
            public_key: Some(counterparty.clone()),
            capability: crate::PubkyIdentityCapability::PrivateLinkCapable,
            local_secret_available: true,
            initialized_at: timestamp(),
            sign_out_generation: 1,
        };

        storage
            .transaction({
                let counterparty = counterparty.clone();
                let identity = identity.clone();
                move |tx| {
                    tx.save_identity_state(identity);
                    tx.save_linked_peer(LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(timestamp()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });
                    tx.save_public_endpoint_record(public_endpoint_record("btc-lightning-bolt11"));
                    tx.insert_outbound_private_message(outbound_private_message(
                        counterparty.clone(),
                    ));
                    tx.insert_private_stream_item(NewPrivateStreamItem::new(
                        NewPrivateStreamItemDetails {
                            counterparty: counterparty.clone(),
                            receive_batch_id: 0,
                            raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                            parsed_version: Some(1),
                            parsed_kind: Some("paykit.test".into()),
                            known_paykit_kind: None,
                            parse_status: PrivateStreamParseStatus::UnknownKind,
                            parse_error: None,
                            received_at: timestamp(),
                        },
                    ));
                    tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                    tx.save_receipt_record(receipt_record(counterparty));
                    tx.clear_identity_scoped_state();
                    Ok(())
                }
            })
            .await
            .unwrap();

        let snapshot = storage.snapshot().unwrap();
        assert_eq!(snapshot.identity_state, Some(identity));
        assert!(snapshot.linked_peers.is_empty());
        assert!(snapshot.public_endpoint_records.is_empty());
        assert!(snapshot.encrypted_link_states.is_empty());
        assert!(snapshot.outbound_private_messages.is_empty());
        assert!(snapshot.private_stream_items.is_empty());
        assert!(snapshot.event_dedup_records.is_empty());
        assert!(snapshot.receipt_access_records.is_empty());
        assert!(snapshot.receipt_records.is_empty());
    }

    #[tokio::test]
    async fn test_clear_private_identity_scoped_state_preserves_public_endpoints() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();
        let identity = IdentityState {
            public_key: Some(counterparty.clone()),
            capability: crate::PubkyIdentityCapability::PrivateLinkCapable,
            local_secret_available: true,
            initialized_at: timestamp(),
            sign_out_generation: 1,
        };

        storage
            .transaction({
                let counterparty = counterparty.clone();
                let identity = identity.clone();
                move |tx| {
                    tx.save_identity_state(identity);
                    tx.save_linked_peer(LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(timestamp()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });
                    tx.save_public_endpoint_record(public_endpoint_record("btc-lightning-bolt11"));
                    tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(60),
                    );
                    tx.allocate_receive_batch_id();
                    tx.insert_outbound_private_message(outbound_private_message(
                        counterparty.clone(),
                    ));
                    tx.insert_private_stream_item(NewPrivateStreamItem::new(
                        NewPrivateStreamItemDetails {
                            counterparty: counterparty.clone(),
                            receive_batch_id: 0,
                            raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                            parsed_version: Some(1),
                            parsed_kind: Some("paykit.test".into()),
                            known_paykit_kind: None,
                            parse_status: PrivateStreamParseStatus::UnknownKind,
                            parse_error: None,
                            received_at: timestamp(),
                        },
                    ));
                    tx.save_receipt_access_record(receipt_access_record(counterparty.clone()));
                    tx.save_receipt_record(receipt_record(counterparty));
                    tx.clear_private_identity_scoped_state();
                    Ok(())
                }
            })
            .await
            .unwrap();

        let snapshot = storage.snapshot().unwrap();
        assert_eq!(snapshot.identity_state, Some(identity));
        assert_eq!(snapshot.public_endpoint_records.len(), 1);
        assert!(snapshot.linked_peers.is_empty());
        assert!(snapshot.encrypted_link_states.is_empty());
        assert!(snapshot.outbound_private_messages.is_empty());
        assert!(snapshot.private_stream_items.is_empty());
        assert!(snapshot.event_dedup_records.is_empty());
        assert!(snapshot.receipt_access_records.is_empty());
        assert!(snapshot.receipt_records.is_empty());
        assert_eq!(snapshot.next_peer_link_operation_lease_id, 1);
        assert_eq!(snapshot.next_outbound_private_message_id, 1);
        assert_eq!(snapshot.next_receive_batch_id, 1);
        assert_eq!(snapshot.next_private_stream_item_id, 1);
    }

    #[tokio::test]
    async fn test_peer_link_operation_lease_can_be_reclaimed_after_expiry() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let first = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();
        let second = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();

        assert_ne!(first.lease_id, second.lease_id);
        assert_eq!(
            storage
                .transaction({
                    let counterparty = counterparty.clone();
                    move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
                })
                .await
                .unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn test_peer_link_operation_stale_release_keeps_newer_lease() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let first = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp(),
                        timestamp() + chrono::Duration::seconds(10),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();
        let second = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx.claim_peer_link_operation(
                        &counterparty,
                        timestamp() + chrono::Duration::seconds(11),
                        timestamp() + chrono::Duration::seconds(71),
                    ))
                }
            })
            .await
            .unwrap()
            .unwrap();

        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.release_peer_link_operation(&counterparty, first.lease_id);
                    Ok(())
                }
            })
            .await
            .unwrap();

        assert_eq!(
            storage
                .transaction({
                    let counterparty = counterparty.clone();
                    move |tx| Ok(tx.peer_link_operation_lease(&counterparty))
                })
                .await
                .unwrap(),
            Some(second)
        );
    }

    #[tokio::test]
    async fn test_stale_peer_link_lease_cannot_overwrite_outbound_status() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let (record, first_lease) = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    let record = tx.insert_outbound_private_message(outbound_private_message(
                        counterparty.clone(),
                    ));
                    let lease = tx
                        .claim_peer_link_operation(
                            &counterparty,
                            timestamp(),
                            timestamp() + chrono::Duration::seconds(10),
                        )
                        .unwrap();
                    Ok((record, lease))
                }
            })
            .await
            .unwrap();
        let active_lease = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    Ok(tx
                        .claim_peer_link_operation(
                            &counterparty,
                            timestamp() + chrono::Duration::seconds(11),
                            timestamp() + chrono::Duration::seconds(71),
                        )
                        .unwrap())
                }
            })
            .await
            .unwrap();
        let sent = mark_outbound_sent(record.clone(), timestamp() + chrono::Duration::seconds(12));
        storage
            .transaction({
                let sent = sent.clone();
                let active_lease = active_lease.clone();
                move |tx| {
                    require_peer_link_operation_lease(tx, &active_lease)?;
                    tx.save_outbound_private_message(sent);
                    Ok(())
                }
            })
            .await
            .unwrap();

        let failed = mark_outbound_failed(
            record,
            "late failed send".into(),
            timestamp() + chrono::Duration::seconds(13),
        );
        let stale_result: Result<()> = storage
            .transaction({
                let failed = failed.clone();
                move |tx| {
                    require_peer_link_operation_lease(tx, &first_lease)?;
                    tx.save_outbound_private_message(failed);
                    Ok(())
                }
            })
            .await;

        assert!(matches!(stale_result, Err(PaykitSdkError::Policy(_))));
        let snapshot = storage.snapshot().unwrap();
        assert_eq!(
            snapshot.outbound_private_messages[0].status,
            OutboundPrivateMessageStatus::Sent
        );
        assert!(snapshot.outbound_private_messages[0].last_error.is_none());
    }

    #[tokio::test]
    async fn test_transaction_rolls_back_on_error() {
        let storage = InMemoryStorage::new();
        let counterparty = counterparty();

        let result: Result<()> = storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_linked_peer(LinkedPeerRecord {
                        counterparty: counterparty.clone(),
                        state: LinkedPeerState::Linked,
                        last_sync_at: Some(timestamp()),
                        last_private_receive_at: None,
                        failure_count: 0,
                    });

                    Err(PaykitSdkError::Storage {
                        context: "forced rollback".into(),
                        source: None,
                    })
                }
            })
            .await;

        assert!(result.is_err());
        let snapshot = storage.snapshot().unwrap();
        assert!(snapshot.linked_peers.is_empty());
    }
}
