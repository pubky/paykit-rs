use std::{
    any::Any,
    collections::{HashMap, HashSet},
    sync::{Arc, Barrier, Mutex},
};

use chrono::{TimeZone, Utc};
use paykit_sdk::storage::{
    PaymentEndpointReservationRecord, PeerLinkOperationLease, PublicEndpointRecord,
};
use paykit_sdk::storage::{StorageAdapter, StorageState};
use paykit_sdk::PaykitSdkError;
use paykit_sdk::{
    ContactRecord, IdentityState, LinkedPeerState, OutboundPrivateMessageStatus, PaykitAppId,
    PaykitProfile, PrivateStreamParseStatus, PubkyPublicKey, PublicationStatus,
};
use sha2::{Digest, Sha256};

use crate::errors::storage_error;
use crate::storage::{
    decode_backup_state, decode_storage_state, encode_backup_state, encode_storage_state,
    FfiSdkStorage,
};
use crate::*;

fn next_test_revision(current: Option<&str>) -> String {
    let next = current
        .and_then(|revision| revision.strip_prefix("revision-"))
        .and_then(|revision| revision.parse::<u64>().ok())
        .unwrap_or_default()
        .saturating_add(1);
    format!("revision-{next}")
}

fn payment_payload_hash(payload: &str) -> String {
    Sha256::digest(payload.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn outbound_state(outbound_ids: &[u64], next_outbound_id: u64) -> StorageState {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let local_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let app_id = PaykitAppId::new("bitkit").unwrap();
    let outbound_private_messages = outbound_ids
        .iter()
        .map(|outbound_message_id| paykit_sdk::storage::OutboundPrivateMessageRecord {
            outbound_message_id: *outbound_message_id,
            counterparty: counterparty.clone(),
            app_id: app_id.clone(),
            kind: "paykit.private_payment_list".into(),
            raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"btc-lightning-bolt11":"private-outbound-secret"}}"#.into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: now,
            updated_at: now,
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        })
        .collect();
    let capabilities = paykit_sdk::PaykitAppCapabilities {
        private_payments: true,
        payment_requests: true,
        receipts: true,
        outgoing_payments: true,
    };

    StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(local_key),
            initialized_at: now,
        }),
        registered_paykit_apps: HashSet::from([app_id.clone()]),
        registered_paykit_app_capabilities: HashMap::from([(app_id, capabilities)]),
        outbound_private_messages,
        next_outbound_private_message_id: next_outbound_id,
        ..StorageState::default()
    }
}

fn assert_invalid_state_blob(error: PaykitFfiError) {
    match error {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "invalid_state_blob");
            assert_eq!(context, "SDK state blob failed validation");
            assert!(!context.contains("private-outbound-secret"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_storage_state_blob_round_trips() {
    let state = StorageState::default();
    let encoded = encode_storage_state(&state).unwrap();
    let decoded = decode_storage_state(&encoded).unwrap();

    assert_eq!(decoded, state);
}

#[test]
fn test_state_blob_rejects_duplicate_outbound_ids_with_redacted_error() {
    let state = outbound_state(&[0, 0], 1);
    let encoded = encode_storage_state(&state).unwrap();

    let error = decode_storage_state(&encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[test]
fn test_state_blob_snapshot_rejects_out_of_range_outbound_id_with_redacted_error() {
    let state = outbound_state(&[7], 7);
    let snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(encode_storage_state(&state).unwrap())),
        revision: "revision-sensitive-value".into(),
    };
    let encoded = encode_sdk_state_blob_snapshot(snapshot).unwrap();

    let error = decode_sdk_state_blob_snapshot(encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[test]
fn test_state_blob_round_trips_exhausted_id_counter() {
    let state = StorageState {
        next_outbound_private_message_id: u64::MAX,
        ..StorageState::default()
    };

    let decoded = decode_storage_state(&encode_storage_state(&state).unwrap()).unwrap();

    assert_eq!(decoded, state);
}

#[test]
fn test_state_revision_validates_loaded_snapshot() {
    struct InvalidSnapshotStore;

    impl FfiSdkStateBlobStore for InvalidSnapshotStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(Some(FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(
                    encode_storage_state(&StorageState::default()).unwrap(),
                )),
                revision: String::new(),
            }))
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            _expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            unreachable!("state revision lookup must not write")
        }
    }

    let storage = FfiSdkStorage {
        store: Arc::new(InvalidSnapshotStore),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    match storage.state_revision().unwrap_err() {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "invalid_state_blob");
            assert_eq!(context, "load SDK state blob: invalid_state_blob");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_state_revision_redacts_callback_error_context() {
    struct FailingStore;

    impl FfiSdkStateBlobStore for FailingStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Err(storage_error(
                "platform_load_failed",
                "sensitive platform path and metadata",
            ))
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            _expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            unreachable!("state revision lookup must not write")
        }
    }

    let storage = FfiSdkStorage {
        store: Arc::new(FailingStore),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let error = storage.state_revision().unwrap_err();
    match error {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "platform_load_failed");
            assert_eq!(context, "load SDK state blob: platform_load_failed");
            assert!(!context.contains("sensitive"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_state_blob_transaction_rejects_exhausted_id_counter() {
    struct ExhaustedStateStore {
        snapshot: FfiSdkStateBlobSnapshot,
    }

    impl FfiSdkStateBlobStore for ExhaustedStateStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(Some(self.snapshot.clone()))
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            _expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            panic!("failed allocation must not write SDK state")
        }
    }

    let state = StorageState {
        next_receive_batch_id: u64::MAX,
        ..StorageState::default()
    };
    let storage = FfiSdkStorage {
        store: Arc::new(ExhaustedStateStore {
            snapshot: FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(encode_storage_state(&state).unwrap())),
                revision: "revision-1".into(),
            },
        }),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let error = storage
        .transaction_erased(Box::new(|tx| {
            tx.allocate_receive_batch_id()?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap_err();

    assert!(matches!(error, PaykitSdkError::Storage { .. }));
}

#[test]
fn test_state_blob_rejects_duplicate_active_lease_ids() {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let first_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let second_counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let mut state = StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: now,
        }),
        next_peer_link_operation_lease_id: 1,
        ..StorageState::default()
    };
    for counterparty in [first_counterparty, second_counterparty] {
        state.peer_link_operation_leases.insert(
            counterparty.clone(),
            PeerLinkOperationLease {
                counterparty,
                lease_id: 0,
                claimed_at: now,
                expires_at: now + chrono::Duration::minutes(1),
            },
        );
    }
    let encoded = encode_storage_state(&state).unwrap();

    let error = decode_storage_state(&encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[test]
fn test_state_blob_rejects_registered_app_without_capabilities() {
    let mut state = outbound_state(&[], 0);
    state.registered_paykit_app_capabilities.clear();
    let encoded = encode_storage_state(&state).unwrap();

    let error = decode_storage_state(&encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[test]
fn test_state_blob_rejects_capabilities_for_unknown_app() {
    let mut state = outbound_state(&[], 0);
    let unknown_app = PaykitAppId::new("unknown-app").unwrap();
    state.registered_paykit_app_capabilities.insert(
        unknown_app,
        paykit_sdk::PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    );
    let encoded = encode_storage_state(&state).unwrap();

    let error = decode_storage_state(&encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[test]
fn test_state_blob_rejects_deliverable_message_from_retired_app() {
    let mut state = outbound_state(&[0], 1);
    let app_id = PaykitAppId::new("bitkit").unwrap();
    state.registered_paykit_apps.remove(&app_id);
    state.retired_paykit_apps.insert(app_id);
    let encoded = encode_storage_state(&state).unwrap();

    let error = decode_storage_state(&encoded).unwrap_err();

    assert_invalid_state_blob(error);
}

#[tokio::test]
async fn test_storage_rejects_decodable_invalid_loaded_state_with_redacted_error() {
    struct InvalidStateStore {
        snapshot: FfiSdkStateBlobSnapshot,
    }

    impl FfiSdkStateBlobStore for InvalidStateStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(Some(self.snapshot.clone()))
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            _expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            unreachable!("invalid loaded state must not reach a write")
        }
    }

    let state = outbound_state(&[3], 3);
    let storage = FfiSdkStorage {
        store: Arc::new(InvalidStateStore {
            snapshot: FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(encode_storage_state(&state).unwrap())),
                revision: "revision-sensitive-value".into(),
            },
        }),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let error = storage
        .transaction_erased(Box::new(|_| Ok(Box::new(()) as Box<dyn Any + Send>)))
        .await
        .unwrap_err();

    match PaykitFfiError::from(error) {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "invalid_state_blob");
            assert_eq!(context, "load SDK state blob: invalid_state_blob");
            assert!(!context.contains("private-outbound-secret"));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[tokio::test]
async fn test_storage_and_backup_blobs_round_trip_private_sync_records() {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let local_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let assert_round_trip = |label: &str, state: &StorageState| {
        let encoded = encode_storage_state(state).unwrap();
        let decoded = decode_storage_state(&encoded)
            .unwrap_or_else(|err| panic!("{label} did not decode: {err}"));
        assert_eq!(&decoded, state, "{label} decoded to a different state");
    };
    let outbound = paykit_sdk::storage::OutboundPrivateMessageRecord {
        outbound_message_id: 0,
        counterparty: counterparty.clone(),
        app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
        kind: "paykit.private_payment_list".into(),
        raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{"btc-lightning-bolt11":"ln-private"}}"#.into(),
        status: OutboundPrivateMessageStatus::Pending,
        attempt_count: 0,
        created_at: now,
        updated_at: now,
        last_attempt_at: None,
        sent_at: None,
        last_error: None,
    };
    let mut state = StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(local_key.clone()),
            initialized_at: now,
        }),
        ..StorageState::default()
    };
    let app_id = paykit_sdk::PaykitAppId::new("bitkit").unwrap();
    state.registered_paykit_apps.insert(app_id.clone());
    state.registered_paykit_app_capabilities.insert(
        app_id.clone(),
        paykit_sdk::PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    );
    assert_round_trip("identity", &state);

    state.retired_paykit_apps =
        HashSet::from([paykit_sdk::PaykitAppId::new("removed-app").unwrap()]);
    assert_round_trip("retired_app", &state);

    state.contact_records = HashMap::from([(
        counterparty.clone(),
        ContactRecord {
            public_key: counterparty.clone(),
            label: None,
            profile: Some(PaykitProfile {
                display_name: Some("Bob".into()),
                image_uri: None,
                extra: Some(serde_json::Map::from_iter([(
                    "role".into(),
                    serde_json::Value::String("merchant".into()),
                )])),
            }),
            profile_fetched_at: Some(now),
            created_at: now,
            updated_at: now,
            public_contact_marker_status: PublicationStatus::NotPublished,
            public_contact_published_at: None,
            public_contact_removed_at: None,
            public_contact_last_error: None,
        },
    )]);
    assert_round_trip("contact", &state);

    state.linked_peers = HashMap::from([(
        counterparty.clone(),
        paykit_sdk::storage::LinkedPeerRecord {
            counterparty: counterparty.clone(),
            state: LinkedPeerState::Linking,
            last_sync_at: None,
            last_private_receive_at: None,
            failure_count: 0,
            local_recovery_attempt_id: None,
            local_recovery_marker_created_at: None,
            local_recovery_marker_last_error: None,
            remote_recovery_attempt_id: None,
            remote_recovery_marker_observed_at: None,
        },
    )]);
    assert_round_trip("linked_peer", &state);

    state.encrypted_link_states = HashMap::from([(
        counterparty.clone(),
        paykit_sdk::storage::EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            link_snapshot: None,
            handshake_snapshot: None,
            handshake_role: None,
            generation: 1,
            checkpointed_at: now,
        },
    )]);
    assert_round_trip("encrypted_link", &state);

    state.peer_link_operation_leases = HashMap::from([(
        counterparty.clone(),
        PeerLinkOperationLease {
            counterparty: counterparty.clone(),
            lease_id: 0,
            claimed_at: now,
            expires_at: now + chrono::Duration::minutes(1),
        },
    )]);
    state.next_peer_link_operation_lease_id = 1;
    assert_round_trip("lease", &state);

    state.outbound_private_messages = vec![outbound.clone()];
    state.next_outbound_private_message_id = 1;
    assert_round_trip("outbound", &state);

    state.payment_endpoint_reservations = HashMap::from([(
        (counterparty.clone(), app_id.clone(), "reservation-1".into()),
        PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            app_id,
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: payment_payload_hash("ln-private"),
            outbound_message_id: outbound.outbound_message_id,
            attribution: HashMap::from([("payment_hash".into(), "hash-1".into())]),
            expires_at: None,
            cancellation_started_at: None,
            created_at: now,
        },
    )]);
    assert_round_trip("reservation", &state);

    state.private_stream_items = vec![paykit_sdk::storage::PrivateStreamItemRecord {
        stream_item_id: 0,
        counterparty: counterparty.clone(),
        receive_batch_id: 0,
        raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","app_id":"bitkit","payment_endpoints":{}}"#.into(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.private_payment_list".into()),
        parsed_app_id: Some("bitkit".into()),
        known_paykit_kind: Some("paykit.private_payment_list".into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: None,
        received_at: now,
    }];
    state.next_receive_batch_id = 1;
    state.next_private_stream_item_id = 1;
    assert_round_trip("private_stream", &state);

    state.receipt_records = HashMap::from([(
        (counterparty.clone(), "receipt-1".into()),
        paykit_sdk::ReceiptRecord {
            issuer: counterparty,
            app_id: paykit_sdk::PaykitAppId::new("bitkit").unwrap(),
            receipt_access_event_id: "event-1".into(),
            receipt_access_key_hash: "key-hash".into(),
            receipt_id: "receipt-1".into(),
            payment_reference: "reference-1".into(),
            payment_request_id: None,
            billing_period: None,
            recipient_public_key: local_key,
            payment_endpoint_identifier: None,
            amount: None,
            metadata: serde_json::Map::from_iter([(
                "order".into(),
                serde_json::json!({"id": 7, "items": ["coffee"]}),
            )]),
            location: "/pub/paykit/v0/receipts/receipt-1".into(),
            retrieved_at: now,
        },
    )]);
    let backup =
        paykit_sdk::export_backup_state(&paykit_sdk::storage::InMemoryStorage::from_state(state))
            .await
            .unwrap();
    let encoded = encode_backup_state(&backup).unwrap();
    assert_eq!(decode_backup_state(&encoded).unwrap(), backup);
}

#[test]
fn test_pubky_public_key_helpers_accept_raw_or_app_key() {
    let raw = "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io";
    let app_key = format!("pubky{raw}");

    assert_eq!(normalize_pubky_public_key(raw.into()).unwrap(), app_key);
    assert_eq!(raw_pubky_public_key(app_key.clone()).unwrap(), raw);
    assert!(redacted_pubky_public_key(app_key)
        .unwrap()
        .starts_with("pubky"));
}

#[test]
fn test_homeserver_public_key_parser_accepts_emitted_app_key() {
    let raw_homeserver = "8jsf5bm1ck3r7sn6pfx4q9mgqq5xn8fi6sizw6pxgjc8zs1bt4io";
    let app_homeserver = normalize_pubky_public_key(raw_homeserver.into()).unwrap();

    assert!(PubkyPublicKey::new(app_homeserver.clone()).is_err());
    assert_eq!(
        crate::session::parse_public_key(app_homeserver)
            .unwrap()
            .as_str(),
        raw_homeserver
    );
}

#[tokio::test]
async fn test_state_blob_save_error_preserves_code() {
    struct SaveFailStore {
        error: PaykitFfiError,
    }

    impl FfiSdkStateBlobStore for SaveFailStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(None)
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            _expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            Err(self.error.clone())
        }
    }

    for (code, provider_context) in [
        ("stale_revision", "state blob revision changed"),
        ("atomic_write_failed", "state blob write failed"),
    ] {
        let storage = FfiSdkStorage {
            store: Arc::new(SaveFailStore {
                error: storage_error(code, provider_context),
            }),
            transaction_lock: Arc::new(Mutex::new(())),
        };

        let err = storage
            .transaction_erased(Box::new(|tx| {
                tx.allocate_receive_batch_id()?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            }))
            .await
            .unwrap_err();

        match PaykitFfiError::from(err) {
            PaykitFfiError::Storage {
                code: actual_code,
                context: actual_context,
            } => {
                assert_eq!(actual_code, code);
                assert_eq!(actual_context, format!("save SDK state blob: {code}"));
                assert!(!actual_context.contains(provider_context));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

#[test]
fn test_state_blob_snapshot_encoding_round_trips() {
    let snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(
            encode_storage_state(&StorageState::default()).unwrap(),
        )),
        revision: "revision-1".into(),
    };

    let encoded = encode_sdk_state_blob_snapshot(snapshot.clone()).unwrap();
    let decoded = decode_sdk_state_blob_snapshot(encoded).unwrap();

    assert_eq!(decoded.revision, snapshot.revision);
    assert_eq!(decoded.blob.export_bytes(), snapshot.blob.export_bytes());
}

#[test]
fn test_state_blob_snapshot_encoding_rejects_empty_revision() {
    let snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(
            encode_storage_state(&StorageState::default()).unwrap(),
        )),
        revision: String::new(),
    };

    let error = encode_sdk_state_blob_snapshot(snapshot).unwrap_err();

    assert_invalid_state_blob(error);
}

#[tokio::test]
async fn test_encoded_state_blob_snapshot_store_supports_repeated_transactions() {
    #[derive(Default)]
    struct EncodedSnapshotStore {
        data: Mutex<Option<Vec<u8>>>,
    }

    impl FfiSdkStateBlobStore for EncodedSnapshotStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            self.data
                .lock()
                .unwrap()
                .clone()
                .map(decode_sdk_state_blob_snapshot)
                .transpose()
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut data = self.data.lock().unwrap();
            let current_revision = data
                .clone()
                .map(decode_sdk_state_blob_snapshot)
                .transpose()?
                .map(|snapshot| snapshot.revision);
            assert_eq!(current_revision, expected_revision);

            let revision = next_test_revision(current_revision.as_deref());
            let snapshot = FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            };
            *data = Some(encode_sdk_state_blob_snapshot(snapshot)?);
            Ok(revision)
        }
    }

    let storage = FfiSdkStorage {
        store: Arc::new(EncodedSnapshotStore::default()),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    storage
        .transaction_erased(Box::new(|tx| {
            tx.allocate_receive_batch_id()?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap();
    storage
        .transaction_erased(Box::new(|tx| {
            tx.allocate_receive_batch_id()?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap();
}

#[tokio::test]
async fn test_storage_rejects_unchanged_revision_after_write() {
    struct UnchangedRevisionStore {
        snapshot: FfiSdkStateBlobSnapshot,
    }

    impl FfiSdkStateBlobStore for UnchangedRevisionStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(Some(self.snapshot.clone()))
        }

        fn save_state_blob_atomically(
            &self,
            _blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            Ok(expected_revision.unwrap())
        }
    }

    let storage = FfiSdkStorage {
        store: Arc::new(UnchangedRevisionStore {
            snapshot: FfiSdkStateBlobSnapshot {
                blob: Arc::new(FfiSdkStateBlob::new(
                    encode_storage_state(&StorageState::default()).unwrap(),
                )),
                revision: "revision-1".into(),
            },
        }),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let result = storage
        .transaction_erased(Box::new(|tx| {
            tx.allocate_receive_batch_id()?;
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await;

    assert!(matches!(result, Err(PaykitSdkError::Storage { .. })));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_storage_adapters_reject_stale_writer() {
    struct ConcurrentStore {
        snapshot: Mutex<FfiSdkStateBlobSnapshot>,
        load_barrier: Barrier,
    }

    impl FfiSdkStateBlobStore for ConcurrentStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            let snapshot = self.snapshot.lock().unwrap().clone();
            self.load_barrier.wait();
            Ok(Some(snapshot))
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut snapshot = self.snapshot.lock().unwrap();
            if Some(snapshot.revision.clone()) != expected_revision {
                return Err(storage_error(
                    "stale_revision",
                    "state blob revision changed",
                ));
            }
            let revision = next_test_revision(Some(&snapshot.revision));
            *snapshot = FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            };
            Ok(revision)
        }
    }

    let store = Arc::new(ConcurrentStore {
        snapshot: Mutex::new(FfiSdkStateBlobSnapshot {
            blob: Arc::new(FfiSdkStateBlob::new(
                encode_storage_state(&StorageState::default()).unwrap(),
            )),
            revision: "revision-1".into(),
        }),
        load_barrier: Barrier::new(2),
    });
    let first = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };
    let second = FfiSdkStorage {
        store,
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let first_write = tokio::spawn(async move {
        first
            .transaction_erased(Box::new(|tx| {
                tx.allocate_receive_batch_id()?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            }))
            .await
    });
    let second_write = tokio::spawn(async move {
        second
            .transaction_erased(Box::new(|tx| {
                tx.allocate_receive_batch_id()?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            }))
            .await
    });
    let (first_result, second_result) = tokio::join!(first_write, second_write);
    let results = [first_result.unwrap(), second_result.unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    assert!(results.iter().filter_map(|result| result.as_ref().err()).any(
        |error| matches!(error, PaykitSdkError::Storage { context, .. } if context.contains("stale_revision"))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_storage_adapters_commit_one_app_owned_endpoint() {
    struct ConcurrentStore {
        snapshot: Mutex<FfiSdkStateBlobSnapshot>,
        load_barrier: Barrier,
    }

    impl FfiSdkStateBlobStore for ConcurrentStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            let snapshot = self.snapshot.lock().unwrap().clone();
            self.load_barrier.wait();
            Ok(Some(snapshot))
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut snapshot = self.snapshot.lock().unwrap();
            if Some(snapshot.revision.clone()) != expected_revision {
                return Err(storage_error(
                    "stale_revision",
                    "state blob revision changed",
                ));
            }
            let revision = next_test_revision(Some(&snapshot.revision));
            *snapshot = FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            };
            Ok(revision)
        }
    }

    let app_id = PaykitAppId::new("bitkit").unwrap();
    let mut state = StorageState {
        identity_state: Some(IdentityState {
            public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::random().public_key(),
            )),
            initialized_at: Utc::now(),
        }),
        ..StorageState::default()
    };
    state.registered_paykit_apps.insert(app_id.clone());
    state.registered_paykit_app_capabilities.insert(
        app_id.clone(),
        paykit_sdk::PaykitAppCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        },
    );
    let store = Arc::new(ConcurrentStore {
        snapshot: Mutex::new(FfiSdkStateBlobSnapshot {
            blob: Arc::new(FfiSdkStateBlob::new(encode_storage_state(&state).unwrap())),
            revision: "revision-1".into(),
        }),
        load_barrier: Barrier::new(2),
    });
    let first = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };
    let second = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };
    let write = |storage: FfiSdkStorage, identifier: &'static str| {
        let app_id = app_id.clone();
        tokio::spawn(async move {
            storage
                .transaction_erased(Box::new(move |tx| {
                    tx.save_public_endpoint_record(PublicEndpointRecord {
                        app_id,
                        identifier: identifier.into(),
                        payload: Some("endpoint-payload".into()),
                        status: PublicationStatus::Published,
                        updated_at: Utc::now(),
                        last_error: None,
                    });
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                }))
                .await
        })
    };
    let (first_result, second_result) = tokio::join!(
        write(first, "btc-lightning-bolt11"),
        write(second, "btc-onchain")
    );
    let results = [first_result.unwrap(), second_result.unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let final_snapshot = store.snapshot.lock().unwrap().clone();
    let final_state = decode_storage_state(&final_snapshot.blob.export_bytes()).unwrap();
    assert_eq!(final_state.public_endpoint_records.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_concurrent_storage_adapters_reject_stale_first_writer() {
    struct InitialWriteStore {
        snapshot: Mutex<Option<FfiSdkStateBlobSnapshot>>,
        load_barrier: Barrier,
    }

    impl FfiSdkStateBlobStore for InitialWriteStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            let snapshot = self.snapshot.lock().unwrap().clone();
            self.load_barrier.wait();
            Ok(snapshot)
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut snapshot = self.snapshot.lock().unwrap();
            let current_revision = snapshot.as_ref().map(|snapshot| snapshot.revision.clone());
            if current_revision != expected_revision {
                return Err(storage_error(
                    "stale_revision",
                    "state blob revision changed",
                ));
            }
            let revision = "revision-1".to_owned();
            *snapshot = Some(FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            });
            Ok(revision)
        }
    }

    let store = Arc::new(InitialWriteStore {
        snapshot: Mutex::new(None),
        load_barrier: Barrier::new(2),
    });
    let first = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };
    let second = FfiSdkStorage {
        store,
        transaction_lock: Arc::new(Mutex::new(())),
    };
    let write = |storage: FfiSdkStorage| {
        tokio::spawn(async move {
            storage
                .transaction_erased(Box::new(|tx| {
                    tx.allocate_receive_batch_id()?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                }))
                .await
        })
    };
    let (first_result, second_result) = tokio::join!(write(first), write(second));
    let results = [first_result.unwrap(), second_result.unwrap()];

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
}

#[test]
fn test_storage_revision_contract_rejects_aba_revision_reuse() {
    struct RevisionState {
        snapshot: Option<FfiSdkStateBlobSnapshot>,
        remaining_revisions: Vec<String>,
        used_revisions: HashSet<String>,
    }

    struct CasRevisionStore {
        state: Mutex<RevisionState>,
    }

    impl CasRevisionStore {
        fn new(revisions: &[&str]) -> Self {
            Self {
                state: Mutex::new(RevisionState {
                    snapshot: None,
                    remaining_revisions: revisions
                        .iter()
                        .map(|revision| (*revision).to_owned())
                        .collect(),
                    used_revisions: HashSet::new(),
                }),
            }
        }
    }

    impl FfiSdkStateBlobStore for CasRevisionStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(self.state.lock().unwrap().snapshot.clone())
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut state = self.state.lock().unwrap();
            let current_revision = state
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision.clone());
            if current_revision != expected_revision {
                return Err(storage_error(
                    "stale_revision",
                    "state blob revision changed",
                ));
            }
            let revision = state.remaining_revisions.remove(0);
            if !state.used_revisions.insert(revision.clone()) {
                return Err(storage_error(
                    "reused_revision",
                    "state blob revision was already used",
                ));
            }
            state.snapshot = Some(FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            });
            Ok(revision)
        }
    }

    let store = CasRevisionStore::new(&["revision-a", "revision-b", "revision-a"]);
    let first_revision = store
        .save_state_blob_atomically(Arc::new(FfiSdkStateBlob::new(vec![1])), None)
        .unwrap();
    let second_revision = store
        .save_state_blob_atomically(
            Arc::new(FfiSdkStateBlob::new(vec![2])),
            Some(first_revision.clone()),
        )
        .unwrap();

    assert_ne!(first_revision, second_revision);
    let reused_revision = store.save_state_blob_atomically(
        Arc::new(FfiSdkStateBlob::new(vec![3])),
        Some(second_revision),
    );
    assert!(matches!(
        reused_revision,
        Err(PaykitFfiError::Storage { .. })
    ));
}

#[test]
fn test_blob_debug_redacts_bytes() {
    let state = FfiSdkStateBlob::new(vec![1, 2, 3]);
    let backup = FfiSdkBackupBlob::new(vec![4, 5, 6, 7]);
    let secret = FfiPubkyLocalSecretKey::new(vec![8; 32]);
    let paykit_secret = FfiPaykitIdentitySecretKey::new(vec![9; 32], 2).unwrap();
    let payment_payload = FfiPaymentPayload::new("bc1qexample".into());
    let attribution = FfiReservationAttribution::new(HashMap::from([(
        "backend_reference".into(),
        "internal-reservation-1".into(),
    )]));

    assert_eq!(format!("{state:?}"), "FfiSdkStateBlob(<redacted:3 bytes>)");
    assert_eq!(
        format!("{backup:?}"),
        "FfiSdkBackupBlob(<redacted:4 bytes>)"
    );
    assert_eq!(
        format!("{secret:?}"),
        "FfiPubkyLocalSecretKey(<redacted:32 bytes>)"
    );
    assert_eq!(
        format!("{paykit_secret:?}"),
        "FfiPaykitIdentitySecretKey { bytes: <redacted:32 bytes>, key_generation: 2 }"
    );
    assert_eq!(
        format!("{payment_payload:?}"),
        "FfiPaymentPayload(<redacted:11 bytes>)"
    );
    assert_eq!(
        format!("{attribution:?}"),
        "FfiReservationAttribution(<redacted:1 fields>)"
    );
}
