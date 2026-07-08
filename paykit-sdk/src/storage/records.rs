use std::{collections::HashMap, fmt};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    domain::contacts::ContactRecord,
    domain::linked_peers::LinkedPeerState,
    domain::outbound_private::OutboundPrivateMessageStatus,
    domain::private_stream::PrivateStreamParseStatus,
    domain::publication::PublicationStatus,
    domain::receipts::{ReceiptAccessRecord, ReceiptIssuanceRecord, ReceiptRecord},
    identity::{IdentityState, PubkyPublicKey},
    PaykitReceiverPath,
};

/// Durable Linked Peer state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedPeerRecord {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Current local relationship/link state.
    pub state: LinkedPeerState,
    /// Last successful sync time.
    pub last_sync_at: Option<DateTime<Utc>>,
    /// Last private receive time.
    pub last_private_receive_at: Option<DateTime<Utc>>,
    /// Consecutive failure count for recovery/retry policy.
    pub failure_count: u32,
    /// Locally published Encrypted Link recovery attempt id.
    pub local_recovery_attempt_id: Option<String>,
    /// Creation time for the local recovery marker payload.
    pub local_recovery_marker_created_at: Option<DateTime<Utc>>,
    /// Last local recovery marker publish/remove error, when available.
    #[serde(default)]
    pub local_recovery_marker_last_error: Option<String>,
    /// Latest counterparty recovery attempt id already observed.
    pub remote_recovery_attempt_id: Option<String>,
    /// Time the counterparty recovery marker was observed.
    pub remote_recovery_marker_observed_at: Option<DateTime<Utc>>,
}

/// Durable SDK-managed public Payment Endpoint publication record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicEndpointRecord {
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Last payload the SDK tried to publish.
    pub payload: Option<String>,
    /// Current publication status.
    pub status: PublicationStatus,
    /// Last status update time.
    pub updated_at: DateTime<Utc>,
    /// Last sync error, when available.
    pub last_error: Option<String>,
}

impl fmt::Debug for PublicEndpointRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublicEndpointRecord")
            .field("identifier", &self.identifier)
            .field(
                "payload",
                &self
                    .payload
                    .as_ref()
                    .map(|payload| format!("<redacted:{} bytes>", payload.len())),
            )
            .field("status", &self.status)
            .field("updated_at", &self.updated_at)
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// Durable SDK-managed Payment Endpoint Reservation record.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEndpointReservationRecord {
    /// Adapter-stable reservation id.
    pub reservation_id: String,
    /// Counterparty the reserved endpoint is intended for.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
    /// Payment Endpoint Identifier.
    pub identifier: String,
    /// Hash of the reserved endpoint payload.
    pub payload_hash: String,
    /// Latest outbound message id that queued this reservation for sharing.
    pub outbound_message_id: u64,
    /// Adapter-provided attribution metadata.
    pub attribution: HashMap<String, String>,
    /// Optional reservation expiry.
    pub expires_at: Option<DateTime<Utc>>,
    /// Time at which adapter cancellation was claimed for this reservation.
    #[serde(default)]
    pub cancellation_started_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Durable Encrypted Link snapshot state.
///
/// Snapshot bytes contain Noise key and counter material. Store them encrypted
/// at rest and avoid logging them.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedLinkStateRecord {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
    counterparty_receiver_path: PaykitReceiverPath,
    kind: String,
    raw_json: String,
    created_at: DateTime<Utc>,
}

impl NewOutboundPrivateMessage {
    pub(crate) fn new(
        counterparty: PubkyPublicKey,
        counterparty_receiver_path: PaykitReceiverPath,
        kind: String,
        raw_json: String,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            counterparty,
            counterparty_receiver_path,
            kind,
            raw_json,
            created_at,
        }
    }

    /// Counterparty public key.
    pub fn counterparty(&self) -> &PubkyPublicKey {
        &self.counterparty
    }

    /// Counterparty receiver/runtime folder.
    pub fn counterparty_receiver_path(&self) -> &PaykitReceiverPath {
        &self.counterparty_receiver_path
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
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
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
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
        let last_error = self
            .last_error
            .as_ref()
            .map(|error| format!("<redacted:{} bytes>", error.len()));
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
            .field("last_error", &last_error)
            .finish()
    }
}

impl OutboundPrivateMessageRecord {
    pub(super) fn from_new(outbound_message_id: u64, message: NewOutboundPrivateMessage) -> Self {
        Self {
            outbound_message_id,
            counterparty: message.counterparty,
            counterparty_receiver_path: message.counterparty_receiver_path,
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

/// New private stream item before storage assigns an id.
///
/// The payload may contain private Paykit secrets. `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct NewPrivateStreamItem {
    counterparty: PubkyPublicKey,
    counterparty_receiver_path: PaykitReceiverPath,
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
            counterparty_receiver_path: details.counterparty_receiver_path,
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

    /// Counterparty receiver/runtime folder.
    pub fn counterparty_receiver_path(&self) -> &PaykitReceiverPath {
        &self.counterparty_receiver_path
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
    pub(crate) counterparty_receiver_path: PaykitReceiverPath,
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
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
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
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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
            .field(
                "counterparty_receiver_path",
                &self.counterparty_receiver_path,
            )
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
    pub(super) fn from_new(stream_item_id: u64, item: NewPrivateStreamItem) -> Self {
        Self {
            stream_item_id,
            counterparty: item.counterparty,
            counterparty_receiver_path: item.counterparty_receiver_path,
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
    /// Counterparty receiver/runtime folder.
    pub counterparty_receiver_path: PaykitReceiverPath,
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

/// Logical SDK storage state used by snapshots, tests, and backup/restore.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageState {
    /// Current identity state.
    pub identity_state: Option<IdentityState>,
    /// Linked Peer records by counterparty and receiver path.
    pub linked_peers: HashMap<(PubkyPublicKey, PaykitReceiverPath), LinkedPeerRecord>,
    /// Local contact records by public key and receiver path.
    pub contact_records: HashMap<(PubkyPublicKey, PaykitReceiverPath), ContactRecord>,
    /// SDK-managed public endpoint records by identifier.
    pub public_endpoint_records: HashMap<String, PublicEndpointRecord>,
    /// SDK-managed Payment Endpoint Reservation records by counterparty, receiver path, and reservation id.
    pub payment_endpoint_reservations:
        HashMap<(PubkyPublicKey, PaykitReceiverPath, String), PaymentEndpointReservationRecord>,
    /// Encrypted Link state records by counterparty and receiver path.
    pub encrypted_link_states:
        HashMap<(PubkyPublicKey, PaykitReceiverPath), EncryptedLinkStateRecord>,
    /// Active peer link operation leases by counterparty and receiver path.
    pub peer_link_operation_leases:
        HashMap<(PubkyPublicKey, PaykitReceiverPath), PeerLinkOperationLease>,
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
    /// Event dedupe records by counterparty, receiver path, and Event ID.
    pub event_dedup_records:
        HashMap<(PubkyPublicKey, PaykitReceiverPath, String), EventDedupRecord>,
    /// Receipt Access records by counterparty, receiver path, and Event ID.
    pub receipt_access_records:
        HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptAccessRecord>,
    /// Decrypted Receipt records by issuer, receiver path, and Receipt ID.
    pub receipt_records: HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptRecord>,
    /// Local receipt issuance records by counterparty, receiver path, and Receipt ID.
    pub receipt_issuance_records:
        HashMap<(PubkyPublicKey, PaykitReceiverPath, String), ReceiptIssuanceRecord>,
}

impl fmt::Debug for StorageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageState")
            .field(
                "identity_state",
                &self.identity_state.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "linked_peers",
                &format_args!("{} records", self.linked_peers.len()),
            )
            .field(
                "contact_records",
                &format_args!("{} records", self.contact_records.len()),
            )
            .field(
                "public_endpoint_records",
                &format_args!("{} records", self.public_endpoint_records.len()),
            )
            .field(
                "payment_endpoint_reservations",
                &format_args!("{} records", self.payment_endpoint_reservations.len()),
            )
            .field(
                "encrypted_link_states",
                &format_args!("{} records", self.encrypted_link_states.len()),
            )
            .field(
                "peer_link_operation_leases",
                &format_args!("{} records", self.peer_link_operation_leases.len()),
            )
            .field(
                "next_peer_link_operation_lease_id",
                &self.next_peer_link_operation_lease_id,
            )
            .field(
                "outbound_private_messages",
                &format_args!("{} records", self.outbound_private_messages.len()),
            )
            .field(
                "next_outbound_private_message_id",
                &self.next_outbound_private_message_id,
            )
            .field(
                "private_stream_items",
                &format_args!("{} records", self.private_stream_items.len()),
            )
            .field("next_receive_batch_id", &self.next_receive_batch_id)
            .field(
                "next_private_stream_item_id",
                &self.next_private_stream_item_id,
            )
            .field(
                "event_dedup_records",
                &format_args!("{} records", self.event_dedup_records.len()),
            )
            .field(
                "receipt_access_records",
                &format_args!("{} records", self.receipt_access_records.len()),
            )
            .field(
                "receipt_records",
                &format_args!("{} records", self.receipt_records.len()),
            )
            .field(
                "receipt_issuance_records",
                &format_args!("{} records", self.receipt_issuance_records.len()),
            )
            .finish()
    }
}
