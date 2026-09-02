use serde::{Deserialize, Serialize};

use super::StorageState;
use crate::{validate_storage_state, PaykitSdkError, Result};

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

/// Decode and validate one logical SDK storage state.
pub fn decode_storage_state_blob(bytes: &[u8]) -> Result<StorageState> {
    let envelope: StorageStateEnvelope<StorageState> =
        postcard::from_bytes(bytes).map_err(|err| PaykitSdkError::Storage {
            context: "decode SDK state blob".into(),
            source: Some(err.into()),
        })?;
    if envelope.version != SDK_STATE_BLOB_VERSION {
        return Err(PaykitSdkError::Storage {
            context: format!(
                "unsupported SDK state blob version {}, expected {}",
                envelope.version, SDK_STATE_BLOB_VERSION
            ),
            source: None,
        });
    }
    validate_storage_state(&envelope.state).map_err(|_| PaykitSdkError::Storage {
        context: "SDK state blob failed validation".into(),
        source: None,
    })?;
    Ok(envelope.state)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
