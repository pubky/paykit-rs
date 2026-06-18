use std::{any::Any, collections::HashMap, sync::Arc};

use paykit_sdk::storage::{StorageAdapter, StorageState};
use paykit_sdk::PaykitSdkConfig;

use crate::errors::storage_error;
use crate::storage::{decode_storage_state, encode_storage_state, FfiSdkStorage};
use crate::*;

#[test]
fn test_default_config_round_trips_to_sdk_config() {
    let ffi = default_config();
    let sdk = PaykitSdkConfig::try_from(ffi.clone()).unwrap();
    let round_trip = FfiPaykitSdkConfig::from(sdk);

    assert_eq!(ffi, round_trip);
}

#[test]
fn test_required_capabilities_include_custom_namespace_scope() {
    let mut config = default_config();
    config.public_contact_sharing = FfiPublicContactSharingPolicy::ConfiguredPublicNamespace;
    config.profile_namespace = "bitkit.to".into();

    let capabilities = required_session_capabilities(config).unwrap();

    assert!(capabilities.contains("/pub/paykit/:rw"));
    assert!(capabilities.contains("/pub/bitkit.to:rw"));
}

#[test]
fn test_storage_state_blob_round_trips() {
    let state = StorageState::default();
    let encoded = encode_storage_state(&state).unwrap();
    let decoded = decode_storage_state(&encoded).unwrap();

    assert_eq!(decoded, state);
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

        fn clear_state_blob(
            &self,
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
fn test_pubky_secret_key_derivation_uses_sdk_derivation() {
    let seed = vec![3; 64];
    let secret = derive_pubky_secret_key(seed, "bitkit.to".into()).unwrap();

    assert_eq!(
        hex::encode(secret.export_bytes()),
        "7cd9a283688abc70e2cb0a13bb7aa4826ee4d7972f3070d4dade0706a83c5dee"
    );
}
