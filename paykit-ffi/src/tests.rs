use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, Mutex},
};

use chrono::{TimeZone, Utc};
use paykit_sdk::storage::{PaymentEndpointReservationRecord, PeerLinkOperationLease};
use paykit_sdk::storage::{StorageAdapter, StorageState};
use paykit_sdk::PaykitSdkConfig;
use paykit_sdk::{
    ContactRecord, EncryptedLinkHandshakeRole, IdentityState, LinkedPeerState,
    OutboundPrivateMessageStatus, PaykitProfile, PrivateStreamParseStatus, PubkyPublicKey,
    PublicationStatus,
};

use crate::errors::storage_error;
use crate::storage::{
    decode_backup_state, decode_storage_state, encode_backup_state, encode_storage_state,
    FfiSdkStorage,
};
use crate::*;

fn receiver_noise_public_key() -> PubkyPublicKey {
    PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&[7; 32]).public_key())
}

#[test]
fn test_default_config_round_trips_to_sdk_config() {
    let ffi = default_config("bitkit/wallet".into()).unwrap();
    let sdk = PaykitSdkConfig::try_from(ffi.clone()).unwrap();
    let round_trip = FfiPaykitSdkConfig::from(sdk);

    assert_eq!(ffi, round_trip);
}

#[test]
fn test_default_pubky_client_config_uses_production() {
    let config = default_pubky_client_config();

    assert!(config.local_testnet_host.is_none());
    assert!(pubky_from_config(&config).is_ok());
}

#[test]
fn test_pubky_client_config_accepts_local_testnet() {
    let mut config = default_pubky_client_config();
    config.local_testnet_host = Some("10.0.2.2".into());

    let result = pubky_from_config(&config);
    assert!(
        result.is_ok(),
        "expected local testnet client, got: {result:?}"
    );
}

#[test]
fn test_pubky_client_config_rejects_invalid_local_testnet_host() {
    for host in ["", " not-a-host", "not a host", "::1"] {
        let mut config = default_pubky_client_config();
        config.local_testnet_host = Some(host.into());

        let err = pubky_from_config(&config).unwrap_err();

        assert!(
            err.to_string().contains("local testnet host is invalid"),
            "expected validation error for {host:?}, got: {err}"
        );
    }
}

#[test]
fn test_required_capabilities_include_custom_namespace_scope() {
    let mut config = default_config("bitkit/wallet".into()).unwrap();
    config.public_contact_sharing = FfiPublicContactSharingPolicy::ConfiguredPublicNamespace;
    config.profile_namespace = "bitkit.to".into();

    let capabilities = required_session_capabilities(config).unwrap();

    assert!(capabilities.contains("/pub/paykit/v0/bitkit/wallet/:rw"));
    assert!(capabilities.contains("/pub/paykit/v0/private/bitkit/wallet/:rw"));
    assert!(capabilities.contains("/pub/bitkit.to/bitkit/wallet/:rw"));
}

#[test]
fn test_required_capabilities_validate_config() {
    let mut config = default_config("bitkit/wallet".into()).unwrap();
    config.profile_namespace = "pubky.app".into();

    let err = required_session_capabilities(config).unwrap_err();

    assert!(
        err.to_string().contains("profile namespace"),
        "expected profile namespace validation error, got: {err}"
    );
}

#[tokio::test]
async fn test_pubky_auth_companion_claim_reports_invalid_auth_url() {
    let bootstrap = FfiPubkySessionBootstrap::new().unwrap();
    let error = bootstrap
        .approve_auth_with_companion_claim(
            "https://example.com/not-pubky-auth".into(),
            "/pub/example/account/:rw".into(),
            Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
            FfiPubkyAuthCompanionClaim {
                query_parameter: "x-example-claim".into(),
                claim_type: "account-export-v1".into(),
                unsigned_payload: vec![1, 2, 3],
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        FfiPubkyAuthCompanionClaimApprovalError::InvalidAuthUrl { .. }
    ));
}

#[tokio::test]
async fn test_pubky_auth_companion_claim_reports_invalid_claim() {
    let bootstrap = FfiPubkySessionBootstrap::new().unwrap();
    let error = bootstrap
        .approve_auth_with_companion_claim(
            "pubkyauth://signin".into(),
            "/pub/example/account/:rw".into(),
            Arc::new(FfiPubkyLocalSecretKey::new(vec![7; 32])),
            FfiPubkyAuthCompanionClaim {
                query_parameter: "x-example|claim".into(),
                claim_type: "account-export-v1".into(),
                unsigned_payload: vec![],
            },
        )
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        FfiPubkyAuthCompanionClaimApprovalError::InvalidClaim { .. }
    ));
}

#[test]
fn test_pubky_auth_companion_claim_debug_redacts_unsigned_payload() {
    let claim = FfiPubkyAuthCompanionClaim {
        query_parameter: "x-example-claim".into(),
        claim_type: "account-export-v1".into(),
        unsigned_payload: vec![222, 173, 190, 239],
    };

    let debug = format!("{claim:?}");

    assert!(debug.contains("x-example-claim"));
    assert!(debug.contains("account-export-v1"));
    assert!(debug.contains("<redacted:4 bytes>"));
    assert!(!debug.contains("[222, 173, 190, 239]"));
}

#[test]
fn test_pubky_auth_companion_claim_unexpected_error_is_delivery_neutral() {
    let error = FfiPubkyAuthCompanionClaimApprovalError::Unexpected {
        reason: "unrecognized SDK companion claim approval failure".into(),
    };

    let display = error.to_string();

    assert!(display.contains("unexpected"));
    assert!(!display.contains("after companion delivery"));
}

#[test]
fn test_storage_state_blob_round_trips() {
    let state = StorageState::default();
    let encoded = encode_storage_state(&state).unwrap();
    let decoded = decode_storage_state(&encoded).unwrap();

    assert_eq!(decoded, state);
}

#[test]
fn test_storage_state_blob_round_trips_private_sync_records() {
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
        counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        kind: "paykit.private_payment_list".into(),
        raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{"btc-lightning-bolt11":"ln-private"}}"#.into(),
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
            local_pubky_public_key: Some(local_key),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: now,
            sign_out_generation: 0,
        }),
        ..StorageState::default()
    };
    assert_round_trip("identity", &state);

    let receiver_path = paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap();

    state.contact_records = HashMap::from([(
        counterparty.clone(),
        ContactRecord {
            public_key: counterparty.clone(),
            receiver_paths: vec![receiver_path.clone()],
            label: None,
            profile: Some(PaykitProfile {
                display_name: Some("Bob".into()),
                image_uri: None,
                extra: None,
            }),
            profile_fetched_at: Some(now),
            created_at: now,
            updated_at: now,
            public_contact_marker_status: PublicationStatus::NotPublished,
            public_contact_marker_receiver_path: None,
            public_contact_published_at: None,
            public_contact_removed_at: None,
            public_contact_last_error: None,
        },
    )]);
    assert_round_trip("contact", &state);

    state.linked_peers = HashMap::from([(
        (counterparty.clone(), receiver_path.clone()),
        paykit_sdk::storage::LinkedPeerRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path.clone(),
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
        (counterparty.clone(), receiver_path.clone()),
        paykit_sdk::storage::EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path.clone(),
            link_snapshot: None,
            handshake_snapshot: Some(vec![1, 2, 3, 4, 5]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
            generation: 1,
            checkpointed_at: now,
        },
    )]);
    assert_round_trip("encrypted_link", &state);

    state.peer_link_operation_leases = HashMap::from([(
        (counterparty.clone(), receiver_path.clone()),
        PeerLinkOperationLease {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path.clone(),
            lease_id: 0,
            claimed_at: now,
            expires_at: now,
        },
    )]);
    state.next_peer_link_operation_lease_id = 1;
    assert_round_trip("lease", &state);

    state.outbound_private_messages = vec![outbound.clone()];
    state.next_outbound_private_message_id = 1;
    assert_round_trip("outbound", &state);

    state.payment_endpoint_reservations = HashMap::from([(
        (
            counterparty.clone(),
            receiver_path.clone(),
            "reservation-1".into(),
        ),
        PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path.clone(),
            identifier: "btc-lightning-bolt11".into(),
            payload_hash: "payload-hash".into(),
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
        counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        receive_batch_id: 0,
        raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
            .into(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.private_payment_list".into()),
        known_paykit_kind: Some("paykit.private_payment_list".into()),
        parse_status: PrivateStreamParseStatus::Valid,
        parse_error: None,
        received_at: now,
    }];
    state.next_receive_batch_id = 1;
    state.next_private_stream_item_id = 1;
    assert_round_trip("private_stream", &state);
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

    for (code, context) in [
        ("stale_revision", "state blob revision changed"),
        ("atomic_write_failed", "state blob write failed"),
    ] {
        let storage = FfiSdkStorage {
            store: Arc::new(SaveFailStore {
                error: storage_error(code, context),
            }),
            transaction_lock: Arc::new(Mutex::new(())),
        };

        let err = storage
            .transaction_erased(Box::new(|tx| {
                tx.allocate_receive_batch_id();
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
                assert_eq!(actual_context, context);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

#[test]
fn test_state_blob_snapshot_encoding_round_trips() {
    let snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(vec![1, 2, 3])),
        revision: "revision-1".into(),
    };

    let encoded = encode_sdk_state_blob_snapshot(snapshot.clone()).unwrap();
    let decoded = decode_sdk_state_blob_snapshot(encoded).unwrap();

    assert_eq!(decoded.revision, snapshot.revision);
    assert_eq!(decoded.blob.export_bytes(), snapshot.blob.export_bytes());
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

            let revision = format!("revision-{}", blob.export_bytes().len());
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
            tx.allocate_receive_batch_id();
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap();
    storage
        .transaction_erased(Box::new(|tx| {
            tx.allocate_receive_batch_id();
            Ok(Box::new(()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap();
}

/// Snapshot-envelope store shared by tests that seed persisted state and then
/// observe what the SDK writes back.
struct SeededSnapshotStore {
    data: Mutex<Option<Vec<u8>>>,
}

impl FfiSdkStateBlobStore for SeededSnapshotStore {
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

        let revision = format!("revision-{}", blob.export_bytes().len());
        let snapshot = FfiSdkStateBlobSnapshot {
            blob,
            revision: revision.clone(),
        };
        *data = Some(encode_sdk_state_blob_snapshot(snapshot)?);
        Ok(revision)
    }
}

struct OfflineSessionProvider;

impl FfiSdkPubkySessionProvider for OfflineSessionProvider {
    fn load_session_access(&self) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError> {
        Ok(None)
    }

    fn public_storage_available(&self) -> Result<bool, PaykitFfiError> {
        Ok(false)
    }

    fn clear_session_access(&self) -> Result<(), PaykitFfiError> {
        Ok(())
    }
}

#[tokio::test]
async fn test_first_transaction_load_normalizes_stale_state() {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let local_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let raw_json = r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#;
    let mut stale_state = StorageState {
        identity_state: Some(IdentityState {
            local_pubky_public_key: Some(local_key.clone()),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: now,
            sign_out_generation: 0,
        }),
        ..StorageState::default()
    };
    // Valid Private Payment List payload stored with stale derived
    // classification, as an older classifier generation could have left it.
    stale_state.private_stream_items = vec![paykit_sdk::storage::PrivateStreamItemRecord {
        stream_item_id: 0,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        receive_batch_id: 0,
        raw_json: raw_json.into(),
        parsed_version: Some(9),
        parsed_kind: Some("paykit.stale".into()),
        known_paykit_kind: None,
        parse_status: PrivateStreamParseStatus::InvalidJson,
        parse_error: Some("stale serde detail".into()),
        received_at: now,
    }];
    stale_state.next_receive_batch_id = 1;
    stale_state.next_private_stream_item_id = 1;

    let seeded_snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(
            encode_storage_state(&stale_state).unwrap(),
        )),
        revision: "revision-0".into(),
    };
    let store = Arc::new(SeededSnapshotStore {
        data: Mutex::new(Some(
            encode_sdk_state_blob_snapshot(seeded_snapshot).unwrap(),
        )),
    });
    let sdk = FfiPaykitSdk::new(
        store.clone(),
        Arc::new(OfflineSessionProvider),
        default_config("bitkit/wallet".into()).unwrap(),
    )
    .unwrap();

    sdk.initialize().await.unwrap();

    let saved = store.data.lock().unwrap().clone().unwrap();
    let snapshot = decode_sdk_state_blob_snapshot(saved).unwrap();
    assert_ne!(snapshot.revision, "revision-0");
    let normalized = decode_storage_state(&snapshot.blob.export_bytes()).unwrap();
    let item = &normalized.private_stream_items[0];
    assert_eq!(item.raw_json, raw_json);
    assert_eq!(item.counterparty, counterparty);
    assert_eq!(item.received_at, now);
    assert_eq!(item.parsed_version, Some(1));
    assert_eq!(
        item.parsed_kind.as_deref(),
        Some("paykit.private_payment_list")
    );
    assert_eq!(
        item.known_paykit_kind.as_deref(),
        Some("paykit.private_payment_list")
    );
    assert_eq!(item.parse_status, PrivateStreamParseStatus::Valid);
    assert_eq!(item.parse_error, None);
    assert_eq!(
        normalized
            .identity_state
            .as_ref()
            .unwrap()
            .local_pubky_public_key,
        Some(local_key)
    );
}

// ---------------------------------------------------------------------------
// Envelope generation fence fixtures.
//
// The structs and helpers below replicate the exact postcard layout the
// generation-1 envelopes and decoders used. DO NOT MODIFY them to make a
// failing test pass: a diff here means the persisted layout or the fence
// semantics changed, which requires a deliberate generation bump, not a
// fixture update.
// ---------------------------------------------------------------------------

/// Test-only envelope mirror. Postcard is positional, so this local struct is
/// byte-compatible with the private envelope in `crate::storage`.
#[derive(serde::Serialize, serde::Deserialize)]
struct TestStateEnvelope {
    version: u32,
    state: StorageState,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TestBackupEnvelope {
    version: u32,
    backup: paykit_sdk::SdkBackupState,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TestSnapshotEnvelope {
    version: u32,
    revision: String,
    blob: Vec<u8>,
}

/// Frozen envelope encoder; with `version` 1 it reproduces exactly what the
/// last pre-safeguard (generation-1) binary wrote.
fn encode_state_envelope(version: u32, state: &StorageState) -> Vec<u8> {
    postcard::to_allocvec(&TestStateEnvelope {
        version,
        state: state.clone(),
    })
    .unwrap()
}

fn encode_backup_envelope(version: u32, backup: &paykit_sdk::SdkBackupState) -> Vec<u8> {
    postcard::to_allocvec(&TestBackupEnvelope {
        version,
        backup: backup.clone(),
    })
    .unwrap()
}

/// Frozen replica of the generation-1 strict state decoder (`!= 1` check),
/// standing in for a pre-safeguard binary. DO NOT MODIFY.
fn decode_state_envelope_v1_strict(bytes: &[u8]) -> Result<StorageState, String> {
    let envelope: TestStateEnvelope = postcard::from_bytes(bytes).map_err(|err| err.to_string())?;
    if envelope.version != 1 {
        return Err(format!(
            "unsupported SDK state blob version {}, expected 1",
            envelope.version
        ));
    }
    Ok(envelope.state)
}

/// Frozen replica of the generation-1 strict backup decoder. DO NOT MODIFY.
fn decode_backup_envelope_v1_strict(bytes: &[u8]) -> Result<paykit_sdk::SdkBackupState, String> {
    let envelope: TestBackupEnvelope =
        postcard::from_bytes(bytes).map_err(|err| err.to_string())?;
    if envelope.version != 1 {
        return Err(format!(
            "unsupported SDK backup blob version {}, expected 1",
            envelope.version
        ));
    }
    Ok(envelope.backup)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

const FIXTURE_MALFORMED_LIST_JSON: &str = r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":"legacy-serde-detail"}"#;

/// Deterministic fixture state for the frozen envelope bytes.
///
/// Every map holds at most one entry because postcard serializes HashMap
/// entries in iteration order; the Vec fields carry the record content. All
/// keys and timestamps are fixed so the encoded bytes never vary.
fn envelope_fixture_state() -> StorageState {
    let now = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let counterparty =
        PubkyPublicKey::from_public_key(&pubky::Keypair::from_secret(&[11; 32]).public_key());
    let receiver_path = paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap();
    let mut state = StorageState {
        identity_state: Some(IdentityState {
            local_pubky_public_key: Some(PubkyPublicKey::from_public_key(
                &pubky::Keypair::from_secret(&[9; 32]).public_key(),
            )),
            local_receiver_noise_public_key: Some(receiver_noise_public_key()),
            initialized_at: now,
            sign_out_generation: 0,
        }),
        ..StorageState::default()
    };
    state.encrypted_link_states = HashMap::from([(
        (counterparty.clone(), receiver_path.clone()),
        paykit_sdk::storage::EncryptedLinkStateRecord {
            counterparty: counterparty.clone(),
            counterparty_receiver_path: receiver_path.clone(),
            link_snapshot: None,
            handshake_snapshot: Some(vec![1, 2, 3]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
            generation: 1,
            checkpointed_at: now,
        },
    )]);
    state.outbound_private_messages = vec![paykit_sdk::storage::OutboundPrivateMessageRecord {
        outbound_message_id: 0,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path.clone(),
        kind: "paykit.private_payment_list".into(),
        raw_json: r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
            .into(),
        status: OutboundPrivateMessageStatus::Pending,
        attempt_count: 0,
        created_at: now,
        updated_at: now,
        last_attempt_at: None,
        sent_at: None,
        last_error: None,
    }];
    state.next_outbound_private_message_id = 1;
    // A recognized-kind item whose body is malformed, stored with the
    // serde-detail parse error a generation-1 classifier persisted.
    // Normalization must rewrite it to the frozen redacted category.
    state.private_stream_items = vec![paykit_sdk::storage::PrivateStreamItemRecord {
        stream_item_id: 0,
        counterparty,
        counterparty_receiver_path: receiver_path,
        receive_batch_id: 0,
        raw_json: FIXTURE_MALFORMED_LIST_JSON.into(),
        parsed_version: Some(1),
        parsed_kind: Some("paykit.private_payment_list".into()),
        known_paykit_kind: Some("paykit.private_payment_list".into()),
        parse_status: PrivateStreamParseStatus::MalformedRecognized,
        parse_error: Some(
            "invalid type: string \"legacy-serde-detail\", expected a map at line 1 column 76"
                .into(),
        ),
        received_at: now,
    }];
    state.next_receive_batch_id = 1;
    state.next_private_stream_item_id = 1;
    state
}

fn envelope_fixture_backup(version: u32) -> paykit_sdk::SdkBackupState {
    let state = envelope_fixture_state();
    paykit_sdk::SdkBackupState {
        version,
        local_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap(),
        identity_state: state.identity_state.clone(),
        linked_peers: Vec::new(),
        contact_records: Vec::new(),
        public_endpoint_records: Vec::new(),
        payment_endpoint_reservations: Vec::new(),
        encrypted_link_states: state.encrypted_link_states.values().cloned().collect(),
        outbound_private_messages: state.outbound_private_messages.clone(),
        private_stream_items: state.private_stream_items.clone(),
        event_dedup_records: Vec::new(),
        receipt_access_records: Vec::new(),
        receipt_records: Vec::new(),
        receipt_issuance_records: Vec::new(),
        next_outbound_private_message_id: 1,
        next_receive_batch_id: 1,
        next_private_stream_item_id: 1,
    }
}

// Frozen generation-1 envelope bytes (hex), generated once from
// `envelope_fixture_state()` / `envelope_fixture_backup(1)`. A mismatch means
// the postcard layout of persisted records changed, which is a compatibility
// break requiring a conscious generation decision, not a fixture update.
const STATE_ENVELOPE_V1_HEX: &str = "0101013439776d3165716e34776464697333383578646773796d3762356765393534397a70636a686d64737a796d69636f70786a366163790134376a66676161396e75746a7969787a696b623774676d736639676b77713769717a3439387a72316e6435696731666e673465737914323032362d30312d30325430333a30343a30355a000000000001346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c6574346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c657400010301020301000114323032362d30312d30325430333a30343a30355a00000100346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c65741b7061796b69742e707269766174655f7061796d656e745f6c697374497b2276657273696f6e223a312c226b696e64223a227061796b69742e707269766174655f7061796d656e745f6c697374222c227061796d656e745f656e64706f696e7473223a7b7d7d000014323032362d30312d30325430333a30343a30355a14323032362d30312d30325430333a30343a30355a000000010100346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c6574005c7b2276657273696f6e223a312c226b696e64223a227061796b69742e707269766174655f7061796d656e745f6c697374222c227061796d656e745f656e64706f696e7473223a226c65676163792d73657264652d64657461696c227d0101011b7061796b69742e707269766174655f7061796d656e745f6c697374011b7061796b69742e707269766174655f7061796d656e745f6c69737401014e696e76616c696420747970653a20737472696e6720226c65676163792d73657264652d64657461696c222c2065787065637465642061206d6170206174206c696e65203120636f6c756d6e20373614323032362d30312d30325430333a30343a30355a010100000000";
const BACKUP_ENVELOPE_V1_HEX: &str = "01010d6269746b69742f77616c6c657401013439776d3165716e34776464697333383578646773796d3762356765393534397a70636a686d64737a796d69636f70786a366163790134376a66676161396e75746a7969787a696b623774676d736639676b77713769717a3439387a72316e6435696731666e673465737914323032362d30312d30325430333a30343a30355a000000000001346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c657400010301020301000114323032362d30312d30325430333a30343a30355a0100346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c65741b7061796b69742e707269766174655f7061796d656e745f6c697374497b2276657273696f6e223a312c226b696e64223a227061796b69742e707269766174655f7061796d656e745f6c697374222c227061796d656e745f656e64706f696e7473223a7b7d7d000014323032362d30312d30325430333a30343a30355a14323032362d30312d30325430333a30343a30355a0000000100346334393868633363786a6e7567636937757766383639706f6b7a34686d35613479347334633563617371783570796563656837790d6269746b69742f77616c6c6574005c7b2276657273696f6e223a312c226b696e64223a227061796b69742e707269766174655f7061796d656e745f6c697374222c227061796d656e745f656e64706f696e7473223a226c65676163792d73657264652d64657461696c227d0101011b7061796b69742e707269766174655f7061796d656e745f6c697374011b7061796b69742e707269766174655f7061796d656e745f6c69737401014e696e76616c696420747970653a20737472696e6720226c65676163792d73657264652d64657461696c222c2065787065637465642061206d6170206174206c696e65203120636f6c756d6e20373614323032362d30312d30325430333a30343a30355a00000000010101";

#[test]
fn test_state_blob_decode_accepts_generation_1() {
    let fixture = envelope_fixture_state();
    assert_eq!(
        to_hex(&encode_state_envelope(1, &fixture)),
        STATE_ENVELOPE_V1_HEX,
        "generation-1 state envelope bytes changed; StorageState postcard \
         layout is a compatibility surface - decide on a generation bump \
         instead of updating this fixture"
    );

    let decoded = decode_storage_state(&from_hex(STATE_ENVELOPE_V1_HEX)).unwrap();

    assert_eq!(decoded, fixture);
}

#[test]
fn test_state_blob_reencoded_gen1_is_stamped_generation_2() {
    let fixture = envelope_fixture_state();
    let decoded = decode_storage_state(&encode_state_envelope(1, &fixture)).unwrap();

    let reencoded = encode_storage_state(&decoded).unwrap();

    let envelope: TestStateEnvelope = postcard::from_bytes(&reencoded).unwrap();
    assert_eq!(envelope.version, 2);
    assert_eq!(decode_storage_state(&reencoded).unwrap(), fixture);
}

#[test]
fn test_state_blob_decode_rejects_generation_3() {
    let err =
        decode_storage_state(&encode_state_envelope(3, &envelope_fixture_state())).unwrap_err();

    let text = err.to_string();
    assert!(
        text.contains("unsupported SDK state blob version 3"),
        "{text}"
    );
    assert!(text.contains("expected 1 through 2"), "{text}");
    // The rejection must not echo blob content.
    assert!(!text.contains("paykit.private_payment_list"), "{text}");
    assert!(!text.contains("legacy-serde-detail"), "{text}");
}

#[test]
fn test_pre_safeguard_state_reader_rejects_generation_2() {
    let encoded = encode_storage_state(&envelope_fixture_state()).unwrap();

    let err = decode_state_envelope_v1_strict(&encoded).unwrap_err();

    assert!(
        err.contains("unsupported SDK state blob version 2"),
        "{err}"
    );
}

#[test]
fn test_backup_blob_decode_accepts_generation_1() {
    let fixture = envelope_fixture_backup(1);
    assert_eq!(
        to_hex(&encode_backup_envelope(1, &fixture)),
        BACKUP_ENVELOPE_V1_HEX,
        "generation-1 backup envelope bytes changed; SdkBackupState postcard \
         layout is a compatibility surface - decide on a version bump instead \
         of updating this fixture"
    );

    let decoded = decode_backup_state(&from_hex(BACKUP_ENVELOPE_V1_HEX)).unwrap();

    assert_eq!(decoded, fixture);
}

#[test]
fn test_backup_blob_reencoded_gen1_is_stamped_generation_2() {
    let fixture = envelope_fixture_backup(1);
    let decoded = decode_backup_state(&encode_backup_envelope(1, &fixture)).unwrap();

    let reencoded = encode_backup_state(&decoded).unwrap();

    let envelope: TestBackupEnvelope = postcard::from_bytes(&reencoded).unwrap();
    assert_eq!(envelope.version, 2);
    assert_eq!(decode_backup_state(&reencoded).unwrap(), fixture);
}

#[test]
fn test_backup_blob_decode_rejects_generation_3() {
    let err =
        decode_backup_state(&encode_backup_envelope(3, &envelope_fixture_backup(1))).unwrap_err();

    match &err {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "unsupported_backup_blob_version");
            assert!(
                context.contains("unsupported SDK backup blob version 3"),
                "{context}"
            );
            assert!(context.contains("expected 1 through 2"), "{context}");
            assert!(!context.contains("legacy-serde-detail"), "{context}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_pre_safeguard_backup_reader_rejects_generation_2() {
    let encoded = encode_backup_state(&envelope_fixture_backup(1)).unwrap();

    let err = decode_backup_envelope_v1_strict(&encoded).unwrap_err();

    assert!(
        err.contains("unsupported SDK backup blob version 2"),
        "{err}"
    );
}

#[test]
fn test_state_snapshot_envelope_stamps_generation_2_and_rejects_generation_3() {
    let snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(vec![1, 2, 3])),
        revision: "revision-1".into(),
    };

    let encoded = encode_sdk_state_blob_snapshot(snapshot.clone()).unwrap();
    let envelope: TestSnapshotEnvelope = postcard::from_bytes(&encoded).unwrap();
    assert_eq!(envelope.version, 2);
    let decoded = decode_sdk_state_blob_snapshot(encoded).unwrap();
    assert_eq!(decoded.revision, snapshot.revision);
    assert_eq!(decoded.blob.export_bytes(), snapshot.blob.export_bytes());

    let forged = postcard::to_allocvec(&TestSnapshotEnvelope {
        version: 3,
        revision: "revision-1".into(),
        blob: vec![1, 2, 3],
    })
    .unwrap();
    let err = decode_sdk_state_blob_snapshot(forged).unwrap_err();
    match &err {
        PaykitFfiError::Storage { code, context } => {
            assert_eq!(code, "unsupported_state_snapshot_blob_version");
            assert!(
                context.contains("unsupported SDK state snapshot blob version 3"),
                "{context}"
            );
            assert!(context.contains("expected 1 through 2"), "{context}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn test_decode_rejects_truncated_and_empty_state_blob() {
    assert!(decode_storage_state(&[]).is_err());

    let encoded = encode_storage_state(&envelope_fixture_state()).unwrap();
    assert!(decode_storage_state(&encoded[..1]).is_err());
    assert!(decode_storage_state(&encoded[..encoded.len() / 2]).is_err());
}

#[tokio::test]
async fn test_generation_1_state_normalizes_then_reencodes_as_generation_2() {
    let fixture = envelope_fixture_state();
    let seeded_snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(encode_state_envelope(1, &fixture))),
        revision: "revision-0".into(),
    };
    let store = Arc::new(SeededSnapshotStore {
        data: Mutex::new(Some(
            encode_sdk_state_blob_snapshot(seeded_snapshot).unwrap(),
        )),
    });
    let sdk = FfiPaykitSdk::new(
        store.clone(),
        Arc::new(OfflineSessionProvider),
        default_config("bitkit/wallet".into()).unwrap(),
    )
    .unwrap();

    sdk.initialize().await.unwrap();

    let saved = store.data.lock().unwrap().clone().unwrap();
    let snapshot = decode_sdk_state_blob_snapshot(saved).unwrap();
    assert_ne!(snapshot.revision, "revision-0");
    let bytes = snapshot.blob.export_bytes();
    let envelope: TestStateEnvelope = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(
        envelope.version, 2,
        "re-encoded state must stamp generation 2"
    );
    let item = &envelope.state.private_stream_items[0];
    assert_eq!(item.raw_json, FIXTURE_MALFORMED_LIST_JSON);
    assert_eq!(
        item.parse_status,
        PrivateStreamParseStatus::MalformedRecognized
    );
    assert_eq!(
        item.parse_error.as_deref(),
        Some("invalid private message structure"),
        "legacy serde detail must normalize to the frozen redacted category"
    );
}

/// The generation upgrade is lazy: the adapter saves only on state change, so
/// a generation-1 blob whose state a transaction leaves untouched keeps its
/// stored bytes (and generation stamp) byte-for-byte, preserving rollback to
/// a generation-1 binary until the first real state change.
#[tokio::test]
async fn test_unchanged_generation_1_blob_is_not_restamped() {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let state = StorageState {
        outbound_private_messages: vec![paykit_sdk::storage::OutboundPrivateMessageRecord {
            outbound_message_id: 0,
            counterparty,
            counterparty_receiver_path: paykit_sdk::PaykitReceiverPath::new("bitkit/wallet")
                .unwrap(),
            kind: "paykit.private_payment_list".into(),
            raw_json:
                r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#
                    .into(),
            status: OutboundPrivateMessageStatus::Pending,
            attempt_count: 0,
            created_at: now,
            updated_at: now,
            last_attempt_at: None,
            sent_at: None,
            last_error: None,
        }],
        next_outbound_private_message_id: 1,
        ..StorageState::default()
    };

    let seeded_snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(encode_state_envelope(1, &state))),
        revision: "revision-0".into(),
    };
    let seeded_bytes = encode_sdk_state_blob_snapshot(seeded_snapshot).unwrap();
    let store = Arc::new(SeededSnapshotStore {
        data: Mutex::new(Some(seeded_bytes.clone())),
    });
    let storage = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let exported = storage
        .transaction_erased(Box::new(move |tx| {
            Ok(Box::new(tx.export_storage_state()) as Box<dyn Any + Send>)
        }))
        .await
        .unwrap()
        .downcast::<StorageState>()
        .unwrap();

    assert_eq!(*exported, state);
    // Nothing was written back: the stored blob is the exact seeded bytes,
    // still carrying the generation-1 stamp.
    assert_eq!(store.data.lock().unwrap().clone(), Some(seeded_bytes));
    let stored = store.load_state_blob().unwrap().unwrap();
    let envelope: TestStateEnvelope = postcard::from_bytes(&stored.blob.export_bytes()).unwrap();
    assert_eq!(envelope.version, 1, "gen-1 blob must not be re-stamped");
}

#[test]
fn test_blob_debug_redacts_bytes() {
    let state = FfiSdkStateBlob::new(vec![1, 2, 3]);
    let backup = FfiSdkBackupBlob::new(vec![4, 5, 6, 7]);
    let secret = FfiPubkyLocalSecretKey::new(vec![8; 32]);
    let receiver_noise_secret = FfiReceiverNoiseSecretKey::random();
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
    assert_eq!(receiver_noise_secret.export_bytes().len(), 32);
    assert_eq!(
        format!("{receiver_noise_secret:?}"),
        "FfiReceiverNoiseSecretKey(<redacted:32 bytes>)"
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

#[test]
fn test_session_access_exports_receiver_noise_secret_key() {
    let receiver_noise_secret_key = Arc::new(FfiReceiverNoiseSecretKey::random());
    let expected = receiver_noise_secret_key.export_bytes();
    let access =
        FfiPubkySessionAccess::new("session-secret".into(), None, receiver_noise_secret_key);

    assert_eq!(
        access.export_receiver_noise_secret_key().export_bytes(),
        expected
    );
}

#[test]
fn test_pubky_secret_key_derivation_matches_pubky_core_seed() {
    let seed = vec![3; 64];
    let secret = pubky_secret_key_from_bip39_seed(seed).unwrap();

    assert_eq!(secret.export_bytes(), vec![3; 32]);
}

#[test]
fn test_pubky_secret_key_derivation_matches_pubky_core_mnemonic() {
    let mnemonic = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    let secret = pubky_secret_key_from_bip39_mnemonic(mnemonic.into()).unwrap();

    assert_eq!(
        hex::encode(secret.export_bytes()),
        "5eb00bbddcf069084889a8ab9155568165f5c453ccb85e70811aaed6f6da5fc1"
    );
}

#[tokio::test]
#[ignore = "requires a live Pubky homeserver session"]
async fn test_ffi_session_provider_reimports_repeatedly() {
    #[derive(Default)]
    struct MemoryStore {
        snapshot: Mutex<Option<FfiSdkStateBlobSnapshot>>,
    }

    impl FfiSdkStateBlobStore for MemoryStore {
        fn load_state_blob(&self) -> Result<Option<FfiSdkStateBlobSnapshot>, PaykitFfiError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }

        fn save_state_blob_atomically(
            &self,
            blob: Arc<FfiSdkStateBlob>,
            expected_revision: Option<String>,
        ) -> Result<String, PaykitFfiError> {
            let mut snapshot = self.snapshot.lock().unwrap();
            let current_revision = snapshot.as_ref().map(|snapshot| snapshot.revision.clone());
            assert_eq!(current_revision, expected_revision);
            let revision = format!("revision-{}", blob.export_bytes().len());
            *snapshot = Some(FfiSdkStateBlobSnapshot {
                blob,
                revision: revision.clone(),
            });
            Ok(revision)
        }
    }

    struct MemorySessionProvider {
        access: Arc<FfiPubkySessionAccess>,
    }

    impl FfiSdkPubkySessionProvider for MemorySessionProvider {
        fn load_session_access(
            &self,
        ) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError> {
            Ok(Some(self.access.clone()))
        }

        fn public_storage_available(&self) -> Result<bool, PaykitFfiError> {
            Ok(true)
        }

        fn clear_session_access(&self) -> Result<(), PaykitFfiError> {
            Ok(())
        }
    }

    let secret = FfiPubkyLocalSecretKey::new(vec![8; 32]);
    let receiver_noise_secret = FfiReceiverNoiseSecretKey::random();
    let bootstrap = FfiPubkySessionBootstrap::new().unwrap();
    let config = default_config("bitkit/wallet".into()).unwrap();
    let capabilities = required_session_capabilities(config.clone()).unwrap();
    let result = bootstrap
        .sign_in(
            Arc::new(secret),
            Arc::new(receiver_noise_secret),
            capabilities,
        )
        .await
        .unwrap();
    let store = Arc::new(MemoryStore::default());
    let provider = Arc::new(MemorySessionProvider {
        access: result.session_access.clone(),
    });
    let sdk = FfiPaykitSdk::with_payment_adapter(
        store,
        provider,
        Arc::new(FfiNoopSdkPaymentAdapter),
        config,
    )
    .unwrap();

    sdk.initialize().await.unwrap();
    for _ in 0..5 {
        let status = sdk.identity_status().await.unwrap().unwrap();
        assert_eq!(status.public_key, Some(result.public_key.clone()));
        assert!(status.live_session_available);
    }
}

#[test]
fn test_parked_report_crosses_ffi_boundary_redacted() {
    use crate::private_links::{FfiOutboundPrivateParkReason, FfiOutboundPrivateSendReport};

    let report = paykit_sdk::OutboundPrivateSendReport {
        parked_unsupported: vec![paykit_sdk::OutboundPrivateParkedMessage {
            outbound_message_id: 9,
            reason: paykit_sdk::OutboundPrivateParkReason::UnsupportedKind,
        }],
        ..paykit_sdk::OutboundPrivateSendReport::default()
    };

    let ffi = FfiOutboundPrivateSendReport::from(report);

    assert_eq!(ffi.parked_unsupported.len(), 1);
    assert_eq!(ffi.parked_unsupported[0].outbound_message_id, 9);
    assert_eq!(
        ffi.parked_unsupported[0].reason,
        FfiOutboundPrivateParkReason::UnsupportedKind
    );
    // The platform-visible signal is id plus closed vocabulary only: no
    // payload text and no kind string can cross, so Debug shows none.
    let debug = format!("{ffi:?}");
    assert!(!debug.contains("paykit."), "kind text leaked: {debug}");
}

/// The parked-head-never-claimed invariant over the FFI storage adapter: the
/// same claim path that parks unknown-kind heads in `InMemoryStorage` must
/// leave the persisted blob byte-for-byte unchanged when it runs against a
/// platform blob store.
#[tokio::test]
async fn test_ffi_storage_claim_leaves_parked_unknown_head_unchanged() {
    let now = Utc.with_ymd_and_hms(2026, 6, 22, 12, 0, 0).unwrap();
    let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
    let receiver_path = paykit_sdk::PaykitReceiverPath::new("bitkit/wallet").unwrap();
    let parked = paykit_sdk::storage::OutboundPrivateMessageRecord {
        outbound_message_id: 0,
        counterparty: counterparty.clone(),
        counterparty_receiver_path: receiver_path.clone(),
        kind: "paykit.allowance".into(),
        raw_json: r#"{"version":1,"kind":"paykit.allowance","body":{}}"#.into(),
        status: OutboundPrivateMessageStatus::Pending,
        attempt_count: 0,
        created_at: now,
        updated_at: now,
        last_attempt_at: None,
        sent_at: None,
        last_error: None,
    };
    let mut later = parked.clone();
    later.outbound_message_id = 1;
    later.kind = "paykit.private_payment_list".into();
    later.raw_json =
        r#"{"version":1,"kind":"paykit.private_payment_list","payment_endpoints":{}}"#.into();
    let state = StorageState {
        outbound_private_messages: vec![parked.clone(), later.clone()],
        next_outbound_private_message_id: 2,
        ..StorageState::default()
    };

    let seeded_snapshot = FfiSdkStateBlobSnapshot {
        blob: Arc::new(FfiSdkStateBlob::new(encode_storage_state(&state).unwrap())),
        revision: "revision-0".into(),
    };
    let seeded_bytes = encode_sdk_state_blob_snapshot(seeded_snapshot).unwrap();
    let store = Arc::new(SeededSnapshotStore {
        data: Mutex::new(Some(seeded_bytes.clone())),
    });
    let storage = FfiSdkStorage {
        store: store.clone(),
        transaction_lock: Arc::new(Mutex::new(())),
    };

    let claimed = storage
        .transaction_erased(Box::new({
            let counterparty = counterparty.clone();
            let receiver_path = receiver_path.clone();
            move |tx| {
                Ok(Box::new(tx.claim_next_outbound_private_message(
                    &counterparty,
                    &receiver_path,
                    now,
                    now - chrono::Duration::seconds(60),
                    now - chrono::Duration::seconds(60),
                )) as Box<dyn Any + Send>)
            }
        }))
        .await
        .unwrap()
        .downcast::<Option<paykit_sdk::storage::OutboundPrivateMessageRecord>>()
        .unwrap();

    // Parked: no claim, and nothing was written back (the stored blob is the
    // exact seeded bytes, so the parked head survives byte-for-byte).
    assert_eq!(*claimed, None);
    assert_eq!(store.data.lock().unwrap().clone(), Some(seeded_bytes));
    let stored = store.load_state_blob().unwrap().unwrap();
    let decoded = decode_storage_state(&stored.blob.export_bytes()).unwrap();
    assert_eq!(decoded.outbound_private_messages, vec![parked, later]);
}
