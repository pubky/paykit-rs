//! Shared harness for testnet-backed end-to-end tests.
//!
//! Mirrors the testnet pattern from paykit-lib's test suite: one Docker
//! Postgres instance shared across the test binary, one ephemeral
//! Pubky testnet (homeserver) per test, and real signed-up sessions wrapped in
//! the SDK's own `PubkySessionAccess` via `PubkySessionBootstrap`.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use paykit_sdk::{
    InMemoryStorage, LinkedPeerState, PaykitApp, PaykitAppCapabilities, PaykitAppId, PaykitSdk,
    PaykitSdkConfig, PaymentAdapter, PaymentTarget, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointReservationCancellation, PrivatePaymentEndpointSelectionRequest,
    PrivateReceivingDetail, PubkyLocalSecretKey, PubkyPublicKey, PubkySessionAccess,
    PubkySessionBootstrap, PubkySessionProvider, PublicPaymentEndpointCandidate,
    PublicPaymentEndpointSelectionRequest, PublicReceivingDetail, Result,
    PAYKIT_AUTHORIZER_SESSION_CAPABILITIES, PAYKIT_SESSION_CAPABILITIES,
};
use pubky_testnet::{
    docker_postgres::DockerPostgres, pubky::Keypair, pubky_homeserver::ConfigToml, EphemeralTestnet,
};
use tokio::sync::{oneshot, Mutex as TokioMutex, Semaphore, SemaphorePermit};

const TEST_CLIENT_ID: &str = "paykit-sdk.test";

static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());
static TESTNET_CONCURRENCY: Semaphore = Semaphore::const_new(2);

pub struct TestnetInstance {
    inner: EphemeralTestnet,
    _permit: SemaphorePermit<'static>,
}

impl Deref for TestnetInstance {
    type Target = EphemeralTestnet;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub async fn build_testnet() -> TestnetInstance {
    build_testnet_with_config(ConfigToml::minimal_test_config()).await
}

pub async fn build_testnet_with_admin() -> TestnetInstance {
    let mut config = ConfigToml::minimal_test_config();
    config.admin.enabled = true;
    build_testnet_with_config(config).await
}

pub async fn build_testnet_with_config(config: ConfigToml) -> TestnetInstance {
    let permit = TESTNET_CONCURRENCY
        .acquire()
        .await
        .expect("testnet concurrency semaphore should remain open");
    let _guard = TESTNET_BUILD_LOCK.lock().await;

    let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
        EphemeralTestnet::builder()
    } else {
        let postgres = DockerPostgres::shared()
            .await
            .connection_string()
            .expect("Docker Postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };

    TestnetInstance {
        inner: builder
            .config(config)
            .with_http_relay()
            .build()
            .await
            .unwrap(),
        _permit: permit,
    }
}

pub fn session_bootstrap(testnet: &EphemeralTestnet, client_id: &str) -> PubkySessionBootstrap {
    let auth_relay_url = testnet
        .http_relay()
        .local_url()
        .join("inbox")
        .expect("test auth relay inbox URL should be valid");
    PubkySessionBootstrap::with_pubky(testnet.sdk().expect("testnet Pubky client"), client_id)
        .expect("test client ID should be valid")
        .with_auth_relay(auth_relay_url.as_str())
        .expect("test auth relay URL should be valid")
}

/// Session provider backed by a real testnet session.
///
/// `clear_session_access` genuinely drops the stored access so sign-out
/// behaves like an app clearing platform credential storage.
#[derive(Clone)]
pub struct TestnetSessionProvider {
    session: Arc<Mutex<Option<PubkySessionAccess>>>,
    session_secret: Option<Arc<String>>,
}

impl TestnetSessionProvider {
    pub fn new(access: PubkySessionAccess) -> Self {
        Self {
            session: Arc::new(Mutex::new(Some(access))),
            session_secret: None,
        }
    }

    pub fn with_session_secret(access: PubkySessionAccess, session_secret: String) -> Self {
        Self {
            session: Arc::new(Mutex::new(Some(access))),
            session_secret: Some(Arc::new(session_secret)),
        }
    }
}

#[async_trait]
impl PubkySessionProvider for TestnetSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(self.session.lock().expect("session lock poisoned").clone())
    }

    async fn revoke_session_access(&self, access: &PubkySessionAccess) -> Result<()> {
        let session_secret =
            self.session_secret
                .as_ref()
                .ok_or_else(|| paykit_sdk::PaykitSdkError::Identity {
                    context: "test session provider has no grant restore material".into(),
                    source: None,
                })?;
        PubkySessionBootstrap::with_pubky(access.outbox_client.clone(), TEST_CLIENT_ID)?
            .revoke_grant(session_secret, access)
            .await
    }

    async fn load_public_storage(&self) -> Result<Option<pubky::PublicStorage>> {
        Ok(self
            .session
            .lock()
            .expect("session lock poisoned")
            .as_ref()
            .map(|access| access.outbox_client.public_storage()))
    }

    async fn clear_session_access(&self) -> Result<()> {
        *self.session.lock().expect("session lock poisoned") = None;
        Ok(())
    }
}

/// Payment adapter whose receiving details can be changed mid-test.
#[derive(Clone, Default)]
pub struct TestnetPaymentAdapter {
    public_details: Arc<Mutex<Vec<PublicReceivingDetail>>>,
    private_details: Arc<Mutex<Vec<PrivateReceivingDetail>>>,
    fail_reservation_cancellation: Arc<Mutex<bool>>,
    public_details_pause: Arc<Mutex<Option<AdapterPause>>>,
    reservation_cancellation_pause: Arc<Mutex<Option<AdapterPause>>>,
}

struct AdapterPause {
    loaded: oneshot::Sender<()>,
    resume: oneshot::Receiver<()>,
}

impl TestnetPaymentAdapter {
    pub fn set_public_details(&self, details: Vec<PublicReceivingDetail>) {
        *self.public_details.lock().expect("details lock poisoned") = details;
    }

    pub fn set_private_details(&self, details: Vec<PrivateReceivingDetail>) {
        *self.private_details.lock().expect("details lock poisoned") = details;
    }

    pub fn pause_next_public_details_load(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (loaded_tx, loaded_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        *self
            .public_details_pause
            .lock()
            .expect("public details pause lock poisoned") = Some(AdapterPause {
            loaded: loaded_tx,
            resume: resume_rx,
        });
        (loaded_rx, resume_tx)
    }

    pub fn set_fail_reservation_cancellation(&self, fail: bool) {
        *self
            .fail_reservation_cancellation
            .lock()
            .expect("failure flag lock poisoned") = fail;
    }

    pub fn pause_next_reservation_cancellation(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (loaded_tx, loaded_rx) = oneshot::channel();
        let (resume_tx, resume_rx) = oneshot::channel();
        *self
            .reservation_cancellation_pause
            .lock()
            .expect("reservation cancellation pause lock poisoned") = Some(AdapterPause {
            loaded: loaded_tx,
            resume: resume_rx,
        });
        (loaded_rx, resume_tx)
    }
}

#[async_trait]
impl PaymentAdapter for TestnetPaymentAdapter {
    async fn current_public_receiving_details(&self) -> Result<Vec<PublicReceivingDetail>> {
        let details = self
            .public_details
            .lock()
            .expect("details lock poisoned")
            .clone();
        let pause = self
            .public_details_pause
            .lock()
            .expect("public details pause lock poisoned")
            .take();
        if let Some(pause) = pause {
            let _ = pause.loaded.send(());
            let _ = pause.resume.await;
        }
        Ok(details)
    }

    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
    ) -> Result<Vec<PrivateReceivingDetail>> {
        Ok(self
            .private_details
            .lock()
            .expect("details lock poisoned")
            .clone())
    }

    async fn select_public_payment_endpoints(
        &self,
        request: &PublicPaymentEndpointSelectionRequest,
    ) -> Result<Vec<PublicPaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_public_payment_target(
        &self,
        endpoint: &PublicPaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }

    async fn select_private_payment_endpoints(
        &self,
        request: &PrivatePaymentEndpointSelectionRequest,
    ) -> Result<Vec<PrivatePaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_private_payment_target(
        &self,
        endpoint: &PrivatePaymentEndpointCandidate,
    ) -> Result<PaymentTarget> {
        Ok(PaymentTarget {
            payload: endpoint.payload.clone(),
        })
    }

    async fn cancel_private_receiving_detail_reservation(
        &self,
        _cancellation: &PrivatePaymentEndpointReservationCancellation,
    ) -> Result<()> {
        let pause = self
            .reservation_cancellation_pause
            .lock()
            .expect("reservation cancellation pause lock poisoned")
            .take();
        if let Some(pause) = pause {
            let _ = pause.loaded.send(());
            let _ = pause.resume.await;
        }
        if *self
            .fail_reservation_cancellation
            .lock()
            .expect("failure flag lock poisoned")
        {
            return Err(paykit_sdk::PaykitSdkError::Policy {
                context: "injected reservation cancellation failure".into(),
                source: None,
            });
        }
        Ok(())
    }
}

/// One signed-up testnet user with an initialized SDK runtime.
///
/// `storage` is a clone sharing state with the SDK's storage, kept for direct
/// record assertions. `access` retains the real session for unauthenticated
/// public-storage reads in assertions.
pub struct TestUser {
    pub sdk: TestSdk,
    pub storage: InMemoryStorage,
    pub adapter: TestnetPaymentAdapter,
    pub access: PubkySessionAccess,
    /// Local grant restore material, reused when the runtime is rebuilt.
    pub session_secret: String,
    pub public_key: PubkyPublicKey,
    pub app_id: PaykitAppId,
    identity_secret: PubkyLocalSecretKey,
}

impl TestUser {
    /// Sign up a fresh keypair on the testnet homeserver through the SDK's own
    /// bootstrap, then build and initialize a runtime around the session.
    ///
    pub async fn sign_up(testnet: &EphemeralTestnet) -> TestUser {
        Self::sign_up_with_app(testnet, app_id("bitkit")).await
    }

    pub async fn sign_up_with_app(testnet: &EphemeralTestnet, app_id: PaykitAppId) -> TestUser {
        let keypair = Keypair::random();
        let secret_key = PubkyLocalSecretKey::new(keypair.secret_key());
        let homeserver_public_key =
            PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
        let bootstrap = session_bootstrap(testnet, TEST_CLIENT_ID);
        let config = PaykitSdkConfig::new(app_id.clone()).unwrap();
        let result = bootstrap
            .sign_up(
                &secret_key,
                &homeserver_public_key,
                None,
                PAYKIT_AUTHORIZER_SESSION_CAPABILITIES,
            )
            .await
            .expect("testnet sign-up should succeed");
        let session_secret = result
            .export_session_secret()
            .await
            .expect("test grant should export local restore material")
            .into_inner();
        let access = result.access;

        let storage = InMemoryStorage::default();
        let adapter = TestnetPaymentAdapter::default();
        let provider =
            TestnetSessionProvider::with_session_secret(access.clone(), session_secret.clone());
        let sdk = PaykitSdk::new(storage.clone(), provider, adapter.clone(), config);

        let report = sdk
            .initialize()
            .await
            .expect("SDK initialization should succeed");
        assert_eq!(
            report.capability,
            paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable
        );
        sdk.publish_paykit_noise_key_authorization().await.unwrap();
        sdk.publish_paykit_app(
            PaykitApp::new(
                "Paykit Test App",
                PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            )
            .unwrap(),
        )
        .await
        .expect("Paykit app publication should succeed");

        TestUser {
            sdk,
            storage,
            adapter,
            access,
            session_secret,
            public_key: result.public_key,
            app_id,
            identity_secret: secret_key,
        }
    }

    /// Build another application runtime for this identity and shared state.
    pub async fn additional_app(
        &self,
        testnet: &EphemeralTestnet,
        app_id: PaykitAppId,
        display_name: &str,
    ) -> TestUser {
        let storage = self.storage.clone();
        let adapter = TestnetPaymentAdapter::default();
        let result = session_bootstrap(testnet, &format!("{app_id}.test"))
            .sign_in(&self.identity_secret, PAYKIT_SESSION_CAPABILITIES)
            .await
            .expect("shared application sign-in should succeed");
        let session_secret = result
            .export_session_secret()
            .await
            .expect("shared application grant should export local restore material")
            .into_inner();
        let access = result.access;
        let provider =
            TestnetSessionProvider::with_session_secret(access.clone(), session_secret.clone());
        let sdk = PaykitSdk::new(
            storage.clone(),
            provider,
            adapter.clone(),
            PaykitSdkConfig::new(app_id.clone()).unwrap(),
        );

        let report = sdk
            .initialize()
            .await
            .expect("shared application initialization should succeed");
        assert_eq!(
            report.capability,
            paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable
        );
        sdk.publish_paykit_app(
            PaykitApp::new(
                display_name,
                PaykitAppCapabilities {
                    private_payments: true,
                    payment_requests: true,
                    receipts: true,
                    outgoing_payments: true,
                },
            )
            .unwrap(),
        )
        .await
        .expect("shared application publication should succeed");

        TestUser {
            sdk,
            storage,
            adapter,
            access,
            session_secret,
            public_key: result.public_key,
            app_id,
            identity_secret: self.identity_secret.clone(),
        }
    }

    /// Rebuild this user's runtime around the supplied durable storage.
    ///
    /// Tests use this to distinguish an ordinary process restart (shared
    /// storage) from a backup restore into a fresh local store.
    pub async fn restart_with_storage(&self, storage: InMemoryStorage) -> TestUser {
        let config = PaykitSdkConfig::new(self.app_id.clone()).unwrap();
        let sdk = build_initialized_sdk(
            storage.clone(),
            &self.access,
            &self.session_secret,
            self.adapter.clone(),
            config,
        )
        .await;

        TestUser {
            sdk,
            storage,
            adapter: self.adapter.clone(),
            access: self.access.clone(),
            session_secret: self.session_secret.clone(),
            public_key: self.public_key.clone(),
            app_id: self.app_id.clone(),
            identity_secret: self.identity_secret.clone(),
        }
    }
}

pub type TestSdk = PaykitSdk<InMemoryStorage, TestnetSessionProvider, TestnetPaymentAdapter>;

/// Construct and initialize an SDK over `storage` with a live testnet session.
async fn build_initialized_sdk(
    storage: InMemoryStorage,
    access: &PubkySessionAccess,
    session_secret: &str,
    adapter: TestnetPaymentAdapter,
    config: PaykitSdkConfig,
) -> TestSdk {
    let provider =
        TestnetSessionProvider::with_session_secret(access.clone(), session_secret.to_string());
    let sdk = PaykitSdk::new(storage, provider, adapter, config);
    let report = sdk
        .initialize()
        .await
        .expect("SDK initialization should succeed");
    assert_eq!(
        report.capability,
        paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable
    );
    sdk
}

/// Two signed-up users sharing one testnet homeserver.
pub struct TwoParty {
    pub _testnet: TestnetInstance,
    pub alice: TestUser,
    pub bob: TestUser,
}

pub async fn two_party() -> TwoParty {
    let testnet = build_testnet().await;
    let alice = TestUser::sign_up_with_app(&testnet, app_id("bitkit")).await;
    let bob = TestUser::sign_up_with_app(&testnet, app_id("paykit-server")).await;
    TwoParty {
        _testnet: testnet,
        alice,
        bob,
    }
}

/// Two users with an established Encrypted Link between them.
pub async fn linked_two_party() -> TwoParty {
    let pair = two_party().await;
    pair.alice
        .sdk
        .initiate_link_with_peer(pair.bob.public_key.clone())
        .await
        .expect("initiating the Encrypted Link Handshake should succeed");
    pair.bob
        .sdk
        .accept_link_with_peer(pair.alice.public_key.clone())
        .await
        .expect("accepting the Encrypted Link Handshake should succeed");
    drive_link_to_linked(&pair.alice, &pair.bob).await;
    pair
}

/// Poll both sides of an in-progress Encrypted Link Handshake until both
/// report `Linked`.
///
/// One `advance_link_handshake` call performs one Noise XX step; a missing
/// counterparty message is `Linking` (not an error), so advance failures are
/// real faults and unwrap loudly.
pub async fn drive_link_to_linked(alice: &TestUser, bob: &TestUser) {
    drive_until_linked(alice, bob, false).await;
}

/// Re-establish a link after both peers have entered recovery.
pub async fn drive_recovery_to_linked(alice: &TestUser, bob: &TestUser) {
    drive_until_linked(alice, bob, true).await;
}

/// Shared poll loop for `drive_link_to_linked` and `drive_recovery_to_linked`.
///
/// `recovering` selects the per-step SDK call: `ensure_link_with_peer` restarts
/// the handshake from `RecoveryRequired`, while `advance_link_handshake` drives
/// one already in progress.
async fn drive_until_linked(alice: &TestUser, bob: &TestUser, recovering: bool) {
    async fn step(
        local: &TestUser,
        peer: &TestUser,
        recovering: bool,
        side: &str,
        phase: &str,
    ) -> LinkedPeerState {
        let peer_key = peer.public_key.clone();
        let result = if recovering {
            local.sdk.ensure_link_with_peer(peer_key, 1).await
        } else {
            local.sdk.advance_link_handshake(peer_key).await
        };
        result
            .unwrap_or_else(|error| panic!("{side} {phase} advance should succeed: {error}"))
            .state
    }

    let (phase, initial_state) = if recovering {
        ("recovery", LinkedPeerState::RecoveryRequired)
    } else {
        ("Handshake", LinkedPeerState::Linking)
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alice_state = initial_state.clone();
    let mut bob_state = initial_state;
    while alice_state != LinkedPeerState::Linked || bob_state != LinkedPeerState::Linked {
        assert!(
            Instant::now() < deadline,
            "Encrypted Link {phase} timed out"
        );
        if alice_state != LinkedPeerState::Linked {
            alice_state = step(alice, bob, recovering, "initiator", phase).await;
        }
        if bob_state != LinkedPeerState::Linked {
            bob_state = step(bob, alice, recovering, "responder", phase).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn app_id(value: &str) -> PaykitAppId {
    PaykitAppId::new(value).expect("test App ID should be valid")
}

pub fn public_receiving_detail(identifier: &str, payload: &str) -> PublicReceivingDetail {
    PublicReceivingDetail {
        identifier: identifier.into(),
        payload: payload.into(),
    }
}

pub fn private_receiving_detail(identifier: &str, payload: &str) -> PrivateReceivingDetail {
    PrivateReceivingDetail {
        identifier: identifier.into(),
        payload: payload.into(),
    }
}

pub async fn deliver(sender: &TestUser, receiver: &TestUser) {
    let sent = sender
        .sdk
        .process_outbound_private_messages(receiver.public_key.clone())
        .await
        .expect("processing the private outbound queue should succeed");
    assert!(!sent.sent.is_empty());
    assert!(sent.failed.is_empty());

    let received = receiver
        .sdk
        .receive_private_messages(sender.public_key.clone())
        .await
        .expect("receiving private messages should succeed");
    assert!(!received.stream_item_ids.is_empty());
    assert!(received.event_conflicts.is_empty());
}
