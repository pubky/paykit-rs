use std::sync::{Arc, Mutex};

use crate::*;

fn next_test_revision(current: Option<&str>) -> String {
    let next = current
        .and_then(|revision| revision.strip_prefix("revision-"))
        .and_then(|revision| revision.parse::<u64>().ok())
        .unwrap_or_default()
        .saturating_add(1);
    format!("revision-{next}")
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

#[test]
fn test_paykit_identity_secret_derivation_and_generation() {
    let pubky_secret = FfiPubkyLocalSecretKey::new(vec![3; 32]);
    let paykit_secret = pubky_secret.derive_paykit_identity_secret_key(1).unwrap();
    let next_paykit_secret = pubky_secret.derive_paykit_identity_secret_key(2).unwrap();

    assert_eq!(paykit_secret.export_bytes().len(), 32);
    assert_ne!(paykit_secret.export_bytes(), pubky_secret.export_bytes());
    assert_ne!(
        paykit_secret.export_bytes(),
        next_paykit_secret.export_bytes()
    );
    assert_eq!(paykit_secret.key_generation(), 1);
    assert_eq!(next_paykit_secret.key_generation(), 2);
    assert!(pubky_secret.derive_paykit_identity_secret_key(0).is_err());
    assert!(FfiPaykitIdentitySecretKey::new(vec![7; 32], 0).is_err());
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
            let revision = next_test_revision(current_revision.as_deref());
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
    let result = bootstrap
        .sign_in(Arc::new(secret), required_session_capabilities())
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
        assert_eq!(
            status.capability,
            FfiPubkyIdentityCapability::PrivateLinkCapable
        );
    }
}
