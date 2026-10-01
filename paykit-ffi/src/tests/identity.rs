use std::sync::Arc;

use paykit_sdk::{
    PubkyLocalSecretKey, PubkyPublicKey, PubkySessionBootstrap, PubkySessionProvider,
    PAYKIT_SESSION_CAPABILITIES,
};
use pubky_testnet::{docker_postgres::DockerPostgres, EphemeralTestnet};
use tokio::sync::{Mutex as TokioMutex, OnceCell};

use crate::*;

const TEST_CLIENT_ID: &str = "paykit.test";

static SHARED_POSTGRES: OnceCell<DockerPostgres> = OnceCell::const_new();
static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

async fn build_testnet() -> EphemeralTestnet {
    let _guard = TESTNET_BUILD_LOCK.lock().await;
    let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
        EphemeralTestnet::builder()
    } else {
        let postgres = SHARED_POSTGRES
            .get_or_init(|| async {
                DockerPostgres::start()
                    .await
                    .expect("failed to start Docker Postgres")
            })
            .await
            .connection_string()
            .expect("Docker Postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };
    builder.with_http_relay().build().await.unwrap()
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
async fn test_ffi_shared_storage_rotation_reencrypts_with_replacement_key() {
    use paykit_sdk::storage::{PubkySharedStateStorage, StorageAdapter};
    use std::sync::Mutex;

    struct SessionProvider(Mutex<Arc<FfiPubkySessionAccess>>);

    impl FfiSdkPubkySessionProvider for SessionProvider {
        fn load_session_access(
            &self,
        ) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError> {
            Ok(Some(self.0.lock().unwrap().clone()))
        }

        fn public_storage_available(&self) -> Result<bool, PaykitFfiError> {
            Ok(true)
        }

        fn clear_session_access(&self) -> Result<(), PaykitFfiError> {
            Ok(())
        }
    }

    let testnet = build_testnet().await;
    let pubky = testnet.sdk().unwrap();
    let secret = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), TEST_CLIENT_ID)
        .unwrap()
        .with_auth_relay(
            testnet
                .http_relay()
                .local_url()
                .join("inbox")
                .unwrap()
                .as_str(),
        )
        .unwrap();
    let result = bootstrap
        .sign_up(&secret, &homeserver, None, PAYKIT_SESSION_CAPABILITIES)
        .await
        .unwrap();
    let session_secret = result.export_session_secret().await.unwrap().into_inner();
    let current_key = secret.derive_paykit_identity_secret_key(1).unwrap();
    let replacement_key = secret.derive_paykit_identity_secret_key(2).unwrap();
    let access_for_key = |key: &paykit_sdk::PaykitIdentitySecretKey| {
        Arc::new(
            FfiPubkySessionAccess::new(
                TEST_CLIENT_ID.into(),
                session_secret.clone(),
                None,
                Some(Arc::new(
                    FfiPaykitIdentitySecretKey::new(key.as_bytes().to_vec(), key.key_generation())
                        .unwrap(),
                )),
            )
            .unwrap(),
        )
    };
    let provider = Arc::new(SessionProvider(Mutex::new(access_for_key(&current_key))));
    let storage = crate::storage::FfiSdkStorageAdapter::PubkyShared(PubkySharedStateStorage::new(
        FfiSdkPubkySessionProviderAdapter::new(
            provider.clone(),
            pubky,
            default_pubky_client_config(),
        ),
    ));
    let identity = paykit_sdk::IdentityState {
        public_key: Some(result.public_key),
        initialized_at: chrono::Utc::now(),
    };
    storage.save_identity_state(identity.clone()).await.unwrap();
    storage
        .rotate_paykit_identity_key(
            current_key.clone(),
            replacement_key.clone(),
            |tx, already_rotated| {
                assert!(!already_rotated);
                tx.allocate_receive_batch_id()
            },
        )
        .await
        .unwrap();

    *provider.0.lock().unwrap() = access_for_key(&replacement_key);
    assert_eq!(storage.load_identity_state().await.unwrap(), Some(identity));
    storage
        .rotate_paykit_identity_key(
            current_key.clone(),
            replacement_key,
            |tx, already_rotated| {
                assert!(already_rotated);
                assert_eq!(tx.export_storage_state().next_receive_batch_id, 1);
                Ok(())
            },
        )
        .await
        .unwrap();
    *provider.0.lock().unwrap() = access_for_key(&current_key);
    assert!(storage.load_identity_state().await.is_err());
}

#[tokio::test]
async fn test_ffi_session_provider_caches_restores_and_revokes_rotated_bearer() {
    struct RestoredSessionProvider {
        session_secret: String,
        local_secret_key: Arc<FfiPubkyLocalSecretKey>,
    }

    impl FfiSdkPubkySessionProvider for RestoredSessionProvider {
        fn load_session_access(
            &self,
        ) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError> {
            Ok(Some(Arc::new(FfiPubkySessionAccess::new(
                TEST_CLIENT_ID.into(),
                self.session_secret.clone(),
                Some(self.local_secret_key.clone()),
                None,
            )?)))
        }

        fn public_storage_available(&self) -> Result<bool, PaykitFfiError> {
            Ok(true)
        }

        fn clear_session_access(&self) -> Result<(), PaykitFfiError> {
            Ok(())
        }
    }

    let testnet = build_testnet().await;
    let pubky = testnet.sdk().expect("testnet Pubky client");
    let local_secret = PubkyLocalSecretKey::new([8; 32]);
    let homeserver = PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
    let auth_relay_url = testnet
        .http_relay()
        .local_url()
        .join("inbox")
        .expect("test auth relay inbox URL should be valid");
    let bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), TEST_CLIENT_ID)
        .unwrap()
        .with_auth_relay(auth_relay_url.as_str())
        .unwrap();
    let result = bootstrap
        .sign_up(
            &local_secret,
            &homeserver,
            None,
            PAYKIT_SESSION_CAPABILITIES,
        )
        .await
        .unwrap();
    let public_key = result.public_key.clone();
    let session_secret = result.export_session_secret().await.unwrap().into_inner();
    let provider = Arc::new(RestoredSessionProvider {
        session_secret: session_secret.clone(),
        local_secret_key: Arc::new(FfiPubkyLocalSecretKey::new(
            local_secret.as_bytes().to_vec(),
        )),
    });
    let adapter = FfiSdkPubkySessionProviderAdapter::new(
        provider,
        pubky.clone(),
        default_pubky_client_config(),
    );

    let (first, second, third) = tokio::join!(
        adapter.load_session_access(),
        adapter.load_session_access(),
        adapter.load_session_access(),
    );
    let first = first.unwrap().expect("restored access should be available");
    for access in [
        first.clone(),
        second.unwrap().unwrap(),
        third.unwrap().unwrap(),
    ] {
        assert_eq!(
            access.session.info().public_key().z32(),
            public_key.as_str()
        );
        assert!(access.session.revalidate().await.unwrap().is_some());
    }

    pubky
        .restore_session(&session_secret)
        .await
        .expect("a second runtime should rotate the cached bearer");
    adapter
        .revoke_session_access(&first)
        .await
        .expect("revocation should recover from a rotated bearer");
    assert!(pubky.restore_session(&session_secret).await.is_err());
}
