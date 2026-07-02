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
    OutboundPrivateMessageStatus, PaykitProfile, PrivateStreamParseStatus, PubkyIdentityCapability,
    PubkyPublicKey, PublicationStatus,
};

use crate::errors::storage_error;
use crate::storage::{decode_storage_state, encode_storage_state, FfiSdkStorage};
use crate::*;

#[test]
fn test_default_config_round_trips_to_sdk_config() {
    let ffi = default_config("bitkit".into()).unwrap();
    let sdk = PaykitSdkConfig::try_from(ffi.clone()).unwrap();
    let round_trip = FfiPaykitSdkConfig::from(sdk);

    assert_eq!(ffi, round_trip);
}

#[test]
fn test_required_capabilities_include_custom_namespace_scope() {
    let mut config = default_config("bitkit".into()).unwrap();
    config.public_contact_sharing = FfiPublicContactSharingPolicy::ConfiguredPublicNamespace;
    config.profile_namespace = "bitkit.to".into();

    let capabilities = required_session_capabilities(config).unwrap();

    assert!(capabilities.contains("/pub/paykit/v0/receivers/bitkit/:rw"));
    assert!(capabilities.contains("/pub/paykit/v0/private/bitkit/:rw"));
    assert!(capabilities.contains("/pub/bitkit.to/:rw"));
}

#[test]
fn test_required_capabilities_validate_config() {
    let mut config = default_config("bitkit".into()).unwrap();
    config.profile_namespace = "pubky.app".into();

    let err = required_session_capabilities(config).unwrap_err();

    assert!(
        err.to_string().contains("profile namespace"),
        "expected profile namespace validation error, got: {err}"
    );
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
            public_key: Some(local_key),
            capability: PubkyIdentityCapability::PrivateLinkCapable,
            local_secret_available: true,
            initialized_at: now,
            sign_out_generation: 0,
        }),
        ..StorageState::default()
    };
    assert_round_trip("identity", &state);

    state.contact_records = HashMap::from([(
        counterparty.clone(),
        ContactRecord {
            public_key: counterparty.clone(),
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
            handshake_snapshot: Some(vec![1, 2, 3, 4, 5]),
            handshake_role: Some(EncryptedLinkHandshakeRole::Initiator),
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
            expires_at: now,
        },
    )]);
    state.next_peer_link_operation_lease_id = 1;
    assert_round_trip("lease", &state);

    state.outbound_private_messages = vec![outbound.clone()];
    state.next_outbound_private_message_id = 1;
    assert_round_trip("outbound", &state);

    state.payment_endpoint_reservations = HashMap::from([(
        (counterparty.clone(), "reservation-1".into()),
        PaymentEndpointReservationRecord {
            reservation_id: "reservation-1".into(),
            counterparty: counterparty.clone(),
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

#[test]
fn test_blob_debug_redacts_bytes() {
    let state = FfiSdkStateBlob::new(vec![1, 2, 3]);
    let backup = FfiSdkBackupBlob::new(vec![4, 5, 6, 7]);
    let secret = FfiPubkyLocalSecretKey::new(vec![8; 32]);
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
        format!("{payment_payload:?}"),
        "FfiPaymentPayload(<redacted:11 bytes>)"
    );
    assert_eq!(
        format!("{attribution:?}"),
        "FfiReservationAttribution(<redacted:1 fields>)"
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
    let bootstrap = FfiPubkySessionBootstrap::new().unwrap();
    let config = default_config("bitkit".into()).unwrap();
    let capabilities = required_session_capabilities(config.clone()).unwrap();
    let result = bootstrap
        .sign_in(Arc::new(secret), capabilities)
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
        assert!(status.private_link_capable);
    }
}
