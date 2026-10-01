use serde::{Deserialize, Serialize};

use super::StorageState;
use crate::{PaykitSdkError, Result};

/// Current encoded SDK state-blob version.
pub const SDK_STATE_BLOB_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct StorageStateEnvelope<T> {
    version: u32,
    state: T,
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
    let (mut envelope, remainder): (StorageStateEnvelope<StorageState>, _) =
        postcard::take_from_bytes(bytes).map_err(|err| PaykitSdkError::Storage {
            context: "decode SDK state blob".into(),
            source: Some(err.into()),
        })?;
    if !remainder.is_empty() {
        return Err(PaykitSdkError::Storage {
            context: "SDK state blob contains trailing bytes".into(),
            source: None,
        });
    }
    if envelope.version != SDK_STATE_BLOB_VERSION {
        return Err(PaykitSdkError::Storage {
            context: format!(
                "unsupported SDK state blob version {}, expected {}",
                envelope.version, SDK_STATE_BLOB_VERSION
            ),
            source: None,
        });
    }
    crate::backup::refresh_storage_state_classification(&mut envelope.state).map_err(|_| {
        PaykitSdkError::Storage {
            context: "SDK state blob failed validation".into(),
            source: None,
        }
    })?;
    Ok(envelope.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        domain::private_stream::persist_private_stream_batch, IdentityState, InMemoryStorage,
        PrivateStreamParseStatus, PubkyPublicKey,
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

    #[test]
    fn test_storage_state_blob_round_trips() {
        let state = StorageState::default();
        let encoded = encode_storage_state_blob(&state).unwrap();
        assert_eq!(decode_storage_state_blob(&encoded).unwrap(), state);
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
}
