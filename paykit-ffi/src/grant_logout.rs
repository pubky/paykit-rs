//! Original-grant logout with revocable caller admission and separate local cleanup.

use std::sync::{Arc, Mutex};

use paykit_sdk::{PubkyGrantLogout, PubkyGrantLogoutAttempt, PubkyGrantLogoutReceipt};
use tokio::sync::watch;

use crate::{
    errors::identity_error,
    session::{parse_public_key, FfiSdkPubkySessionProviderAdapter},
    FfiPaykitSdk, PaykitFfiError,
};

/// Acknowledged original-grant logout, independent of local credential erasure.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiPubkyGrantLogoutReceipt {
    /// Original identity in app public-key representation.
    pub public_key: String,
    /// Homeserver that acknowledged proof logout.
    pub homeserver: String,
    /// Original grant, including refreshed bearers belonging to that grant.
    pub grant_id: String,
    /// Original application client identifier.
    pub client_id: String,
}

impl From<PubkyGrantLogoutReceipt> for FfiPubkyGrantLogoutReceipt {
    fn from(value: PubkyGrantLogoutReceipt) -> Self {
        Self {
            public_key: value.public_key.to_app_key(),
            homeserver: value.homeserver.to_app_key(),
            grant_id: value.grant_id,
            client_id: value.client_id,
        }
    }
}

struct Admission {
    provider: FfiSdkPubkySessionProviderAdapter,
    fingerprint: [u8; 32],
    revoked: watch::Sender<bool>,
}

impl Admission {
    fn check(&self) -> Result<(), PaykitFfiError> {
        if *self.revoked.borrow() {
            return Err(ended());
        }
        let matches = self.provider.matches_session_access(&self.fingerprint);
        match matches {
            Ok(true) if !*self.revoked.borrow() => Ok(()),
            other => {
                self.revoked.send_replace(true);
                match other {
                    Err(error) => Err(error),
                    _ => Err(ended()),
                }
            }
        }
    }
}

/// Live original grant/signer retained with the caller's original removal intent.
///
/// Capture before confirmation. The app must revoke admission when its original
/// owner/lifecycle ends, including same-seed replacement, and drain its active
/// operation before installing replacement credentials. This object never
/// restores credentials or clears storage. Restart offers local recovery only.
#[derive(uniffi::Object)]
pub struct FfiPubkyGrantLogout {
    inner: Arc<PubkyGrantLogout>,
    admission: Arc<Admission>,
}

impl_redacted_debug!(FfiPubkyGrantLogout);

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkyGrantLogout {
    /// Original non-secret binding to retain with the confirmation and marker.
    pub fn binding(&self) -> FfiPubkyGrantLogoutReceipt {
        self.inner.binding().into()
    }

    /// Permanently stop this handle's admission and interrupt active waits.
    /// Cancellation after dispatch does not prove that the server did not commit.
    pub fn revoke_admission(&self) {
        self.admission.revoked.send_replace(true);
    }

    /// Prepare only after fresh confirmation of this original intent.
    ///
    /// This reads current proof-logout support without mutating remote state.
    /// Same-grant bearer refresh is permitted. Replacement bundles and ended
    /// admission are refused. A failed or lost response does not authorize an
    /// automatic retry or replay of endpoint/private-list cleanup.
    pub async fn prepare_attempt(&self) -> Result<Arc<FfiPubkyGrantLogoutAttempt>, PaykitFfiError> {
        let mut revoked = self.admission.revoked.subscribe();
        self.admission.check()?;
        let attempt = tokio::select! {
            biased;
            _ = revoked.changed() => return Err(ended()),
            result = self.inner.prepare_attempt() => result?,
        };
        self.admission.check()?;
        Ok(Arc::new(FfiPubkyGrantLogoutAttempt {
            inner: Mutex::new(Some(attempt)),
            admission: Arc::clone(&self.admission),
        }))
    }

    /// Keep known remote completion even if app storage cleanup fails or is cancelled.
    /// This never implies that local credentials, watermarks or markers were cleared.
    pub fn acknowledged(&self) -> Option<FfiPubkyGrantLogoutReceipt> {
        self.inner.acknowledged().map(Into::into)
    }
}

/// A single confirmed proof-logout attempt, consumed before any execution await.
#[derive(uniffi::Object)]
pub struct FfiPubkyGrantLogoutAttempt {
    inner: Mutex<Option<PubkyGrantLogoutAttempt>>,
    admission: Arc<Admission>,
}

impl_redacted_debug!(FfiPubkyGrantLogoutAttempt);

#[uniffi::export(async_runtime = "tokio")]
impl FfiPubkyGrantLogoutAttempt {
    /// Recheck original provider/admission, then execute this attempt once.
    /// No platform erase callback is invoked, even after remote acknowledgement.
    pub async fn execute(&self) -> Result<FfiPubkyGrantLogoutReceipt, PaykitFfiError> {
        let attempt = self
            .inner
            .lock()
            .map_err(|_| ended())?
            .take()
            .ok_or_else(|| {
                identity_error(
                    "pubky_grant_logout_consumed",
                    "logout attempt was already consumed",
                )
            })?;
        let mut revoked = self.admission.revoked.subscribe();
        self.admission.check()?;
        tokio::select! {
            biased;
            _ = revoked.changed() => Err(ended()),
            result = attempt.execute() => result.map(Into::into).map_err(Into::into),
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl FfiPaykitSdk {
    /// Capture the original provider's existing live grant without authentication.
    /// The caller must already admit the original owner and recheck its lifecycle
    /// after this await. No cache restore, session import or remote write occurs.
    pub async fn capture_grant_logout(
        &self,
        expected_public_key: String,
    ) -> Result<Arc<FfiPubkyGrantLogout>, PaykitFfiError> {
        let owner = parse_public_key(expected_public_key)?;
        let (inner, fingerprint) = self.session_provider.capture_grant_logout(&owner).await?;
        Ok(Arc::new(FfiPubkyGrantLogout {
            inner: Arc::new(inner),
            admission: Arc::new(Admission {
                provider: self.session_provider.clone(),
                fingerprint,
                revoked: watch::channel(false).0,
            }),
        }))
    }
}

fn ended() -> PaykitFfiError {
    identity_error(
        "pubky_grant_logout_admission_ended",
        "original logout admission ended",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        default_pubky_client_config, session::session_access_fingerprint,
        FfiPaykitIdentitySecretKey, FfiPubkySessionAccess, FfiSdkPubkySessionProvider,
    };

    struct Provider {
        current: Mutex<Option<Arc<FfiPubkySessionAccess>>>,
        revoke_during_callback: Mutex<Option<watch::Sender<bool>>>,
    }

    impl FfiSdkPubkySessionProvider for Provider {
        fn load_session_access(
            &self,
        ) -> Result<Option<Arc<FfiPubkySessionAccess>>, PaykitFfiError> {
            if let Some(revoked) = self.revoke_during_callback.lock().unwrap().as_ref() {
                revoked.send_replace(true);
            }
            Ok(self.current.lock().unwrap().clone())
        }
        fn public_storage_available(&self) -> Result<bool, PaykitFfiError> {
            Ok(false)
        }
        fn clear_session_access(&self) -> Result<(), PaykitFfiError> {
            panic!("logout must not erase credentials");
        }
    }

    fn access(secret: &str, generation: u64) -> Arc<FfiPubkySessionAccess> {
        Arc::new(
            FfiPubkySessionAccess::new(
                "bitkit.test".into(),
                secret.into(),
                None,
                Some(Arc::new(
                    FfiPaykitIdentitySecretKey::new(vec![7; 32], generation).unwrap(),
                )),
            )
            .unwrap(),
        )
    }

    fn fixture() -> (Arc<Provider>, Admission) {
        let original = access("original-grant-test-fixture", 1);
        let fingerprint = session_access_fingerprint(&original);
        let provider = Arc::new(Provider {
            current: Mutex::new(Some(original)),
            revoke_during_callback: Mutex::new(None),
        });
        let client = pubky::PubkyHttpClient::builder()
            .pkarr(|b| {
                b.no_default_network()
                    .relays(&["http://127.0.0.1:1"])
                    .unwrap()
            })
            .build()
            .unwrap();
        let adapter = FfiSdkPubkySessionProviderAdapter::new(
            provider.clone(),
            pubky::Pubky::with_client(client),
            default_pubky_client_config(),
        );
        (
            provider,
            Admission {
                provider: adapter,
                fingerprint,
                revoked: watch::channel(false).0,
            },
        )
    }

    #[tokio::test]
    async fn test_original_grant_native_attempt_preserves_replacement_and_local_credentials() {
        use paykit_sdk::{
            PubkyLocalSecretKey, PubkyPublicKey, PubkySessionBootstrap, PubkySessionProvider,
            PAYKIT_SESSION_CAPABILITIES,
        };
        use pubky_testnet::{docker_postgres::DockerPostgres, EphemeralTestnet};

        let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
            EphemeralTestnet::builder()
        } else {
            EphemeralTestnet::builder()
                .postgres(DockerPostgres::shared().await.connection_string().unwrap())
        };
        let testnet = builder.with_http_relay().build().await.unwrap();
        let pubky = testnet.sdk().unwrap();
        let identity = PubkyLocalSecretKey::new(pubky::Keypair::random().secret_key());
        let bootstrap = PubkySessionBootstrap::with_pubky(pubky.clone(), "bitkit.test")
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
            .sign_up(
                &identity,
                &PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key()),
                None,
                PAYKIT_SESSION_CAPABILITIES,
            )
            .await
            .unwrap();
        let original_access = Arc::new(
            FfiPubkySessionAccess::new(
                "bitkit.test".into(),
                result.export_session_secret().await.unwrap().into_inner(),
                None,
                None,
            )
            .unwrap(),
        );
        let provider = Arc::new(Provider {
            current: Mutex::new(Some(original_access.clone())),
            revoke_during_callback: Mutex::new(None),
        });
        let adapter = FfiSdkPubkySessionProviderAdapter::new(
            provider.clone(),
            pubky,
            default_pubky_client_config(),
        );
        // Restart/cache miss must not activate retained credentials for logout.
        assert!(adapter
            .capture_grant_logout(&result.public_key)
            .await
            .is_err());
        assert!(result.access.session.revalidate().await.unwrap().is_some());
        let live = adapter.load_session_access().await.unwrap().unwrap();
        let wrong_owner = PubkyPublicKey::from_public_key(&pubky::Keypair::random().public_key());
        assert!(adapter.capture_grant_logout(&wrong_owner).await.is_err());

        async fn capture(
            adapter: &FfiSdkPubkySessionProviderAdapter,
            owner: &PubkyPublicKey,
        ) -> FfiPubkyGrantLogout {
            let (inner, fingerprint) = adapter.capture_grant_logout(owner).await.unwrap();
            FfiPubkyGrantLogout {
                inner: Arc::new(inner),
                admission: Arc::new(Admission {
                    provider: adapter.clone(),
                    fingerprint,
                    revoked: watch::channel(false).0,
                }),
            }
        }

        let original = capture(&adapter, &result.public_key).await;
        let attempt = original.prepare_attempt().await.unwrap();
        let other = bootstrap
            .sign_in(&identity, PAYKIT_SESSION_CAPABILITIES)
            .await
            .unwrap();
        let replacement_access = Arc::new(
            FfiPubkySessionAccess::new(
                "bitkit.test".into(),
                other.export_session_secret().await.unwrap().into_inner(),
                None,
                None,
            )
            .unwrap(),
        );
        *provider.current.lock().unwrap() = Some(replacement_access.clone());
        assert!(attempt.execute().await.is_err());
        assert!(original.acknowledged().is_none());
        assert!(live.session.revalidate().await.unwrap().is_some());
        assert!(other.access.session.revalidate().await.unwrap().is_some());
        *provider.current.lock().unwrap() = Some(original_access.clone());
        assert!(original.prepare_attempt().await.is_err());

        let cancelled = capture(&adapter, &result.public_key).await;
        let attempt = cancelled.prepare_attempt().await.unwrap();
        cancelled.revoke_admission();
        assert!(attempt.execute().await.is_err());
        assert!(cancelled.acknowledged().is_none());
        assert!(live.session.revalidate().await.unwrap().is_some());

        let confirmed = capture(&adapter, &result.public_key).await;
        let attempt = confirmed.prepare_attempt().await.unwrap();
        let receipt = attempt.execute().await.unwrap();
        assert!(attempt.execute().await.is_err());
        assert!(confirmed.prepare_attempt().await.is_err());
        assert_eq!(confirmed.acknowledged(), Some(receipt.clone()));
        // Remote acknowledgement remains independently available after owner
        // revocation. No provider erase callback or replacement deletion occurs.
        *provider.current.lock().unwrap() = Some(replacement_access.clone());
        confirmed.revoke_admission();
        assert_eq!(confirmed.acknowledged(), Some(receipt));
        assert!(Arc::ptr_eq(
            provider.current.lock().unwrap().as_ref().unwrap(),
            &replacement_access
        ));
        assert!(other.access.session.revalidate().await.unwrap().is_some());
    }

    #[tokio::test]
    async fn test_original_grant_provider_admission_rejects_replacement_and_aba() {
        let (provider, admission) = fixture();
        admission.check().unwrap();
        *provider.current.lock().unwrap() = Some(access("replacement-grant", 1));
        assert!(admission.check().is_err());
        *provider.current.lock().unwrap() = Some(access("original-grant-test-fixture", 1));
        assert!(admission.check().is_err());
    }

    #[tokio::test]
    async fn test_original_grant_provider_admission_allows_same_bundle_not_new_key_generation() {
        let (provider, admission) = fixture();
        *provider.current.lock().unwrap() = Some(access("original-grant-test-fixture", 1));
        admission.check().unwrap();
        *provider.current.lock().unwrap() = Some(access("original-grant-test-fixture", 2));
        assert!(admission.check().is_err());
    }

    #[tokio::test]
    async fn test_original_grant_provider_admission_rechecks_cancellation_after_callback() {
        let (provider, admission) = fixture();
        *provider.revoke_during_callback.lock().unwrap() = Some(admission.revoked.clone());
        assert!(admission.check().is_err());
    }
}
