use serde::{Deserialize, Serialize};

use super::StorageState;
use crate::{PaykitSdkError, Result};

/// Current encoded SDK state-blob version.
pub const SDK_STATE_BLOB_VERSION: u32 = 3;
const SDK_STATE_BLOB_VERSION_V1: u32 = 1;
const SDK_STATE_BLOB_VERSION_V2: u32 = 2;

#[derive(Serialize, Deserialize)]
struct StorageStateEnvelope<T> {
    version: u32,
    state: T,
}

// Frozen top-level schema written by SDK state blob version 1. Postcard encodes
// structs positionally, so current fields must never be added to this migration
// type. Nested records retain their own compatibility contract.
#[derive(Deserialize)]
struct StorageStateV1 {
    paykit_noise_public_key: Option<crate::PubkyPublicKey>,
    allowance_accounting: Option<crate::AllowanceAccountingState>,
    identity_state: Option<crate::IdentityState>,
    linked_peers: std::collections::HashMap<crate::PubkyPublicKey, super::LinkedPeerRecord>,
    contact_records:
        std::collections::HashMap<crate::PubkyPublicKey, crate::domain::contacts::ContactRecord>,
    authorized_paykit_apps: std::collections::HashMap<
        crate::PubkyPublicKey,
        std::collections::HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>,
    >,
    registered_paykit_apps: std::collections::HashSet<paykit_lib::PaykitAppId>,
    registered_paykit_app_capabilities:
        std::collections::HashMap<paykit_lib::PaykitAppId, paykit_lib::PaykitAppCapabilities>,
    retired_paykit_apps: std::collections::HashSet<paykit_lib::PaykitAppId>,
    public_endpoint_records:
        std::collections::HashMap<(paykit_lib::PaykitAppId, String), super::PublicEndpointRecord>,
    payment_endpoint_reservations: std::collections::HashMap<
        (crate::PubkyPublicKey, paykit_lib::PaykitAppId, String),
        super::PaymentEndpointReservationRecord,
    >,
    encrypted_link_states:
        std::collections::HashMap<crate::PubkyPublicKey, super::EncryptedLinkStateRecord>,
    peer_link_operation_leases:
        std::collections::HashMap<crate::PubkyPublicKey, super::PeerLinkOperationLease>,
    next_peer_link_operation_lease_id: u64,
    paykit_app_operation_leases:
        std::collections::HashMap<paykit_lib::PaykitAppId, super::PaykitAppOperationLease>,
    next_paykit_app_operation_lease_id: u64,
    payment_request_execution_claims: std::collections::HashMap<
        (crate::PubkyPublicKey, String),
        super::PaymentRequestExecutionClaim,
    >,
    outbound_private_messages: Vec<super::OutboundPrivateMessageRecord>,
    next_outbound_private_message_id: u64,
    private_stream_items: Vec<super::PrivateStreamItemRecord>,
    next_receive_batch_id: u64,
    next_private_stream_item_id: u64,
    event_dedup_records:
        std::collections::HashMap<(crate::PubkyPublicKey, String), super::EventDedupRecord>,
    receipt_access_records: std::collections::HashMap<
        (crate::PubkyPublicKey, String),
        crate::domain::receipts::ReceiptAccessRecord,
    >,
    receipt_records: std::collections::HashMap<
        (crate::PubkyPublicKey, String),
        crate::domain::receipts::ReceiptRecord,
    >,
    receipt_issuance_records: std::collections::HashMap<
        (crate::PubkyPublicKey, String),
        crate::domain::receipts::ReceiptIssuanceRecord,
    >,
}

impl From<StorageStateV1> for StorageState {
    fn from(state: StorageStateV1) -> Self {
        Self {
            paykit_noise_public_key: state.paykit_noise_public_key,
            allowance_accounting: state.allowance_accounting,
            identity_state: state.identity_state,
            linked_peers: state.linked_peers,
            contact_records: state.contact_records,
            authorized_paykit_apps: state.authorized_paykit_apps,
            registered_paykit_apps: state.registered_paykit_apps,
            registered_paykit_app_capabilities: state.registered_paykit_app_capabilities,
            retired_paykit_apps: state.retired_paykit_apps,
            public_endpoint_records: state.public_endpoint_records,
            payment_endpoint_reservations: state.payment_endpoint_reservations,
            encrypted_link_states: state.encrypted_link_states,
            peer_link_operation_leases: state.peer_link_operation_leases,
            next_peer_link_operation_lease_id: state.next_peer_link_operation_lease_id,
            paykit_app_operation_leases: state.paykit_app_operation_leases,
            next_paykit_app_operation_lease_id: state.next_paykit_app_operation_lease_id,
            private_intake_supervisor_lease: None,
            next_private_intake_supervisor_lease_id: 0,
            payment_request_execution_claims: state.payment_request_execution_claims,
            outbound_private_messages: state.outbound_private_messages,
            next_outbound_private_message_id: state.next_outbound_private_message_id,
            private_stream_items: state.private_stream_items,
            next_receive_batch_id: state.next_receive_batch_id,
            next_private_stream_item_id: state.next_private_stream_item_id,
            event_dedup_records: state.event_dedup_records,
            receipt_access_records: state.receipt_access_records,
            receipt_records: state.receipt_records,
            receipt_issuance_records: state.receipt_issuance_records,
            private_intake_cursors: std::collections::HashMap::new(),
            private_intake_peers: std::collections::HashMap::new(),
        }
    }
}

/// Encode one logical SDK storage state.
///
/// This codec does not encrypt its output. Storage adapters are responsible
/// for protecting the encoded state at rest.
pub fn encode_storage_state_blob(state: &StorageState) -> Result<Vec<u8>> {
    postcard::to_allocvec(&StorageStateEnvelope {
        version: SDK_STATE_BLOB_VERSION,
        state,
    })
    .map_err(|err| PaykitSdkError::Storage {
        context: "encode SDK state blob".into(),
        source: Some(err.into()),
    })
}

/// Decode one logical SDK storage state, refresh derived message classifications,
/// and validate the resulting state without changing raw evidence or transport state.
pub fn decode_storage_state_blob(bytes: &[u8]) -> Result<StorageState> {
    let (version, state_bytes): (u32, _) =
        postcard::take_from_bytes(bytes).map_err(|err| PaykitSdkError::Storage {
            context: "decode SDK state blob".into(),
            source: Some(err.into()),
        })?;
    let (mut state, remainder) = match version {
        SDK_STATE_BLOB_VERSION_V1 => {
            let (state, remainder): (StorageStateV1, _) = postcard::take_from_bytes(state_bytes)
                .map_err(|err| PaykitSdkError::Storage {
                    context: "decode SDK state blob version 1".into(),
                    source: Some(err.into()),
                })?;
            (state.into(), remainder)
        }
        SDK_STATE_BLOB_VERSION_V2 => {
            // Version 3 only appends two defaultable supervisor-lease fields.
            // Postcard does not synthesize missing sequence fields, so append
            // their canonical `None`/zero encoding before decoding frozen v2.
            let mut upgraded = state_bytes.to_vec();
            upgraded.extend_from_slice(&[0, 0]);
            let (state, remainder) =
                postcard::take_from_bytes(&upgraded).map_err(|err| PaykitSdkError::Storage {
                    context: "decode SDK state blob version 2".into(),
                    source: Some(err.into()),
                })?;
            if !remainder.is_empty() {
                return Err(PaykitSdkError::Storage {
                    context: "SDK state blob version 2 contains trailing bytes".into(),
                    source: None,
                });
            }
            (state, &[][..])
        }
        SDK_STATE_BLOB_VERSION => {
            postcard::take_from_bytes(state_bytes).map_err(|err| PaykitSdkError::Storage {
                context: "decode SDK state blob version 3".into(),
                source: Some(err.into()),
            })?
        }
        version => {
            return Err(PaykitSdkError::Storage {
                context: format!(
                    "unsupported SDK state blob version {version}, expected {SDK_STATE_BLOB_VERSION}"
                ),
                source: None,
            });
        }
    };
    if !remainder.is_empty() {
        return Err(PaykitSdkError::Storage {
            context: "SDK state blob contains trailing bytes".into(),
            source: None,
        });
    }
    crate::backup::refresh_storage_state_classification(&mut state).map_err(|_| {
        PaykitSdkError::Storage {
            context: "SDK state blob failed validation".into(),
            source: None,
        }
    })?;
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::private_stream::persist_private_stream_batch, IdentityState, InMemoryStorage,
        PrivateStreamParseStatus, PubkyPublicKey, StorageAdapter,
    };

    async fn state_with_private_events() -> StorageState {
        let public_key = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let storage = InMemoryStorage::from_state(StorageState {
            identity_state: Some(IdentityState {
                public_key: Some(public_key.clone()),
                initialized_at: chrono::Utc::now(),
            }),
            ..StorageState::default()
        });
        let receipt =
            crate::test_utils::receipt_access_json("8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201");
        let allowance = crate::test_utils::allowance_event_json(
            "paykit.allowance_proposal",
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d202",
        );
        let mut conflicting_receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        conflicting_receipt["payment_reference"] = serde_json::json!("conflicting-reference");
        let conflicting_allowance = crate::test_utils::allowance_event_json(
            "paykit.allowance_proposal",
            "8a0d8b4c-913f-4e31-9f2c-2a6f5bb4d201",
        );
        let messages = [
            receipt.clone(),
            receipt,
            allowance,
            conflicting_receipt.to_string(),
            conflicting_allowance,
        ]
        .into_iter()
        .map(|raw_json| {
            let header: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
            paykit_lib::PrivateApplicationMessage {
                version: Some(1),
                kind: header["kind"].as_str().map(str::to_owned),
                app_id: header["app_id"].as_str().map(str::to_owned),
                raw_json,
            }
        })
        .collect();
        persist_private_stream_batch(&storage, public_key, messages, None, chrono::Utc::now())
            .await
            .unwrap();
        storage.snapshot().unwrap()
    }

    async fn state_with_buffered_private_packet() -> StorageState {
        let counterparty = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let subscribed_user =
            PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        let storage = InMemoryStorage::from_state(StorageState {
            identity_state: Some(IdentityState {
                public_key: Some(subscribed_user.clone()),
                initialized_at: chrono::Utc::now(),
            }),
            ..StorageState::default()
        });
        storage
            .transaction({
                let counterparty = counterparty.clone();
                move |tx| {
                    tx.save_encrypted_link_state(crate::storage::EncryptedLinkStateRecord {
                        counterparty: counterparty.clone(),
                        link_snapshot: None,
                        handshake_snapshot: None,
                        handshake_role: None,
                        generation: 3,
                        checkpointed_at: chrono::Utc::now(),
                    });
                    tx.buffer_private_intake_packet(crate::storage::BufferedPrivatePacketRecord {
                        counterparty,
                        link_generation: 3,
                        receive_counter: 8,
                        ciphertext: vec![7; crate::storage::PRIVATE_INTAKE_PACKET_BYTES],
                        source_cursor: Some(crate::storage::PrivateIntakeCursorRecord {
                            subscribed_user,
                            homeserver: "https://homeserver.example".into(),
                            event_id: 21,
                        }),
                        buffered_at: chrono::Utc::now(),
                    })?;
                    Ok(())
                }
            })
            .await
            .unwrap();
        storage.snapshot().unwrap()
    }

    #[test]
    fn test_storage_state_blob_round_trips() {
        let state = StorageState::default();
        let encoded = encode_storage_state_blob(&state).unwrap();
        assert_eq!(decode_storage_state_blob(&encoded).unwrap(), state);
    }

    #[test]
    fn test_storage_state_blob_decodes_v2_before_supervisor_lease_fields() {
        let mut encoded = encode_storage_state_blob(&StorageState::default()).unwrap();
        encoded[0] = SDK_STATE_BLOB_VERSION_V2 as u8;
        assert_eq!(encoded.pop(), Some(0));
        assert_eq!(encoded.pop(), Some(0));

        let decoded = decode_storage_state_blob(&encoded).unwrap();
        assert!(decoded.private_intake_supervisor_lease.is_none());
        assert_eq!(decoded.next_private_intake_supervisor_lease_id, 0);
    }

    #[test]
    fn test_storage_state_blob_upgrades_real_v1_fixture() {
        let encoded = include_bytes!("tests/fixtures/sdk-state-v1.postcard");
        let state = decode_storage_state_blob(encoded).unwrap();

        assert_eq!(SDK_STATE_BLOB_VERSION, 3);
        assert_eq!(
            state.identity_state.unwrap().public_key.unwrap().as_str(),
            "7jfgaa9nutjyixzikb7tgmsf9gkwq7iqz498zr1nd5ig1fng4esy"
        );
        assert_eq!(
            state.paykit_noise_public_key.unwrap().as_str(),
            "9wm1eqn4wddis385xdgsym7b5ge9549zpcjhmdszymicopxj6acy"
        );

        let current = encode_storage_state_blob(&StorageState::default()).unwrap();
        let (version, _): (u32, _) = postcard::take_from_bytes(&current).unwrap();
        assert_eq!(version, 3);
    }

    #[test]
    fn test_storage_state_blob_rejects_unsupported_version() {
        let state = StorageState::default();
        let encoded = postcard::to_allocvec(&StorageStateEnvelope {
            version: SDK_STATE_BLOB_VERSION + 1,
            state: &state,
        })
        .unwrap();
        assert!(matches!(
            decode_storage_state_blob(&encoded),
            Err(PaykitSdkError::Storage { context, .. })
                if context.contains("unsupported SDK state blob version")
        ));
    }

    #[test]
    fn test_storage_state_blob_rejects_trailing_bytes() {
        let mut encoded = encode_storage_state_blob(&StorageState::default()).unwrap();
        encoded.push(0);
        assert!(decode_storage_state_blob(&encoded).is_err());
    }

    #[tokio::test]
    async fn test_storage_state_blob_refreshes_derived_classifications() {
        let expected = state_with_private_events().await;
        let authoritative = expected
            .event_dedup_records
            .values()
            .find(|record| record.first_stream_item_id == 0)
            .unwrap();
        assert_eq!(authoritative.duplicate_stream_item_ids, vec![1]);
        assert_eq!(authoritative.conflicting_stream_item_ids, vec![3, 4]);
        let mut stale = expected.clone();
        for record in &mut stale.private_stream_items {
            record.parse_error = Some("cached parser error".into());
            record.parse_status = PrivateStreamParseStatus::MalformedRecognized;
        }
        for index in [2, 4] {
            let allowance = &mut stale.private_stream_items[index];
            allowance.known_paykit_kind = None;
            allowance.parse_status = PrivateStreamParseStatus::UnknownKind;
        }
        stale
            .event_dedup_records
            .retain(|_, record| record.first_stream_item_id != 2);
        for record in stale.event_dedup_records.values_mut() {
            record.conflicting_stream_item_ids.retain(|id| *id != 4);
        }
        stale.receipt_access_records.clear();
        let encoded = encode_storage_state_blob(&stale).unwrap();
        assert_eq!(decode_storage_state_blob(&encoded).unwrap(), expected);
    }

    #[tokio::test]
    async fn test_storage_state_blob_refresh_preserves_evidence_validation() {
        let original = state_with_private_events().await;
        for corrupt in [
            |state: &mut StorageState| state.private_stream_items[0].parsed_version = Some(2),
            |state: &mut StorageState| {
                state
                    .event_dedup_records
                    .values_mut()
                    .next()
                    .unwrap()
                    .payload_hash = "changed".into()
            },
            |state: &mut StorageState| {
                state
                    .event_dedup_records
                    .values_mut()
                    .next()
                    .unwrap()
                    .first_stream_item_id = 99
            },
            |state: &mut StorageState| {
                state
                    .receipt_access_records
                    .values_mut()
                    .next()
                    .unwrap()
                    .stream_item_id = 1
            },
            |state: &mut StorageState| state.private_stream_items.swap(0, 1),
            |state: &mut StorageState| state.next_private_stream_item_id = 0,
            |state: &mut StorageState| {
                state
                    .event_dedup_records
                    .values_mut()
                    .find(|record| record.first_stream_item_id == 0)
                    .unwrap()
                    .conflicting_stream_item_ids
                    .clear();
            },
        ] {
            let mut state = original.clone();
            corrupt(&mut state);
            let encoded = encode_storage_state_blob(&state).unwrap();
            assert!(decode_storage_state_blob(&encoded).is_err());
        }
    }

    #[tokio::test]
    async fn test_storage_state_blob_rejects_corrupt_private_intake_state() {
        let original = state_with_buffered_private_packet().await;
        let peer = original.private_intake_peers.keys().next().unwrap().clone();
        let user = original
            .private_intake_cursors
            .keys()
            .next()
            .unwrap()
            .clone();
        crate::validate_storage_state(&original).unwrap();
        decode_storage_state_blob(&encode_storage_state_blob(&original).unwrap()).unwrap();

        let mut wrong_size = original.clone();
        wrong_size
            .private_intake_peers
            .get_mut(&peer)
            .unwrap()
            .buffered_packets[0]
            .ciphertext
            .pop();
        assert!(
            decode_storage_state_blob(&encode_storage_state_blob(&wrong_size).unwrap()).is_err()
        );

        let mut counter_gap = original.clone();
        let mut second = counter_gap.private_intake_peers[&peer].buffered_packets[0].clone();
        second.receive_counter += 2;
        counter_gap
            .private_intake_peers
            .get_mut(&peer)
            .unwrap()
            .buffered_packets
            .push(second);
        assert!(
            decode_storage_state_blob(&encode_storage_state_blob(&counter_gap).unwrap()).is_err()
        );

        let mut over_capacity = original.clone();
        let first = over_capacity.private_intake_peers[&peer].buffered_packets[0].clone();
        for offset in 1..=crate::storage::PRIVATE_INTAKE_MAX_PACKETS_PER_PEER {
            let mut packet = first.clone();
            packet.receive_counter += offset as u64;
            over_capacity
                .private_intake_peers
                .get_mut(&peer)
                .unwrap()
                .buffered_packets
                .push(packet);
        }
        assert!(
            decode_storage_state_blob(&encode_storage_state_blob(&over_capacity).unwrap()).is_err()
        );

        let mut mismatched_cursor_key = original;
        let cursor = mismatched_cursor_key
            .private_intake_cursors
            .remove(&user)
            .unwrap();
        mismatched_cursor_key
            .private_intake_cursors
            .insert(peer, cursor);
        assert!(decode_storage_state_blob(
            &encode_storage_state_blob(&mismatched_cursor_key).unwrap()
        )
        .is_err());
    }
}
