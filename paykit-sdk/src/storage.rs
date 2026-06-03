use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    endpoints::EndpointPublicationStatus,
    identity::{IdentityState, PubkyPublicKey},
    linked_peers::LinkedPeerState,
    private_stream::PrivateStreamParseStatus,
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

    /// Allocate a receive batch id.
    fn allocate_receive_batch_id(&mut self) -> u64;

    /// Insert one private stream item and return its assigned id.
    fn insert_private_stream_item(&mut self, item: NewPrivateStreamItem) -> u64;

    /// List private stream items for a counterparty.
    fn private_stream_items(&self, counterparty: &PubkyPublicKey) -> Vec<PrivateStreamItemRecord>;

    /// Load an Event Message dedupe record.
    fn event_dedup_record(&self, event_id: &str) -> Option<EventDedupRecord>;

    /// Save an Event Message dedupe record.
    fn save_event_dedup_record(&mut self, record: EventDedupRecord);
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedLinkStateRecord {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Serialized active link snapshot.
    pub link_snapshot: Option<Vec<u8>>,
    /// Serialized in-progress handshake snapshot.
    pub handshake_snapshot: Option<Vec<u8>>,
    /// Local snapshot generation.
    pub generation: u64,
    /// Last checkpoint time.
    pub checkpointed_at: DateTime<Utc>,
}

/// New private stream item before storage assigns an id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewPrivateStreamItem {
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Receive batch id assigned by the SDK runtime.
    pub receive_batch_id: u64,
    /// Raw JSON payload.
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

/// Durable private stream item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateStreamItemRecord {
    /// Assigned stream item id.
    pub stream_item_id: u64,
    /// Counterparty public key.
    pub counterparty: PubkyPublicKey,
    /// Receive batch id assigned by the SDK runtime.
    pub receive_batch_id: u64,
    /// Raw JSON payload.
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
    /// Append-only private stream items.
    pub private_stream_items: Vec<PrivateStreamItemRecord>,
    /// Next receive batch id.
    pub next_receive_batch_id: u64,
    /// Next private stream item id.
    pub next_private_stream_item_id: u64,
    /// Event dedupe records by Event ID.
    pub event_dedup_records: HashMap<String, EventDedupRecord>,
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

    fn event_dedup_record(&self, event_id: &str) -> Option<EventDedupRecord> {
        self.state.event_dedup_records.get(event_id).cloned()
    }

    fn save_event_dedup_record(&mut self, record: EventDedupRecord) {
        self.state
            .event_dedup_records
            .insert(record.event_id.clone(), record);
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;

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

                    let stream_item_id = tx.insert_private_stream_item(NewPrivateStreamItem {
                        counterparty: counterparty.clone(),
                        receive_batch_id: 7,
                        raw_json: r#"{"version":1,"kind":"paykit.test"}"#.into(),
                        parsed_version: Some(1),
                        parsed_kind: Some("paykit.test".into()),
                        known_paykit_kind: None,
                        parse_status: PrivateStreamParseStatus::UnknownKind,
                        parse_error: None,
                        received_at: timestamp(),
                    });

                    tx.save_event_dedup_record(EventDedupRecord {
                        event_id: "650e8400-e29b-41d4-a716-446655440000".into(),
                        event_kind: "paykit.test".into(),
                        payload_hash: "hash".into(),
                        first_stream_item_id: stream_item_id,
                        duplicate_stream_item_ids: Vec::new(),
                        conflicting_stream_item_ids: Vec::new(),
                    });

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
        assert_eq!(snapshot.private_stream_items.len(), 1);
        assert_eq!(snapshot.event_dedup_records.len(), 1);
        assert_eq!(snapshot.next_private_stream_item_id, 1);
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
