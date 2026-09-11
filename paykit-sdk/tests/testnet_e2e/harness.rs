//! Shared harness for testnet-backed end-to-end tests.
//!
//! Mirrors the testnet pattern from paykit-lib's test suite: one Docker
//! Postgres instance shared across the test binary, one ephemeral
//! Pubky testnet (homeserver) per test, and real signed-up sessions wrapped in
//! the SDK's own `PubkySessionAccess` via `PubkySessionBootstrap`.

use chrono::Utc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use paykit_sdk::{
    InMemoryStorage, LinkedPeerState, PaykitReceiverCapabilities, PaykitReceiverPath, PaykitSdk,
    PaykitSdkConfig, PaymentAdapter, PaymentTarget, PrivatePaymentEndpointCandidate,
    PrivatePaymentEndpointSelectionRequest, PrivateReceivingDetail, PubkyLocalSecretKey,
    PubkyPublicKey, PubkySessionAccess, PubkySessionBootstrap, PubkySessionProvider,
    PublicPaymentEndpointCandidate, PublicPaymentEndpointSelectionRequest, PublicReceivingDetail,
    ReceiverNoiseSecretKey, Result, StorageAdapter,
};
use pubky_testnet::{docker_postgres::DockerPostgres, pubky::Keypair, EphemeralTestnet};
use tokio::sync::{Mutex as TokioMutex, OnceCell};

const TEST_CLIENT_ID: &str = "paykit-sdk.test";

static SHARED_POSTGRES: OnceCell<DockerPostgres> = OnceCell::const_new();
static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

async fn shared_postgres() -> &'static DockerPostgres {
    SHARED_POSTGRES
        .get_or_init(|| async {
            DockerPostgres::start()
                .await
                .expect("failed to start Docker Postgres")
        })
        .await
}

pub async fn build_testnet() -> EphemeralTestnet {
    let _guard = TESTNET_BUILD_LOCK.lock().await;

    let builder = if std::env::var_os("TEST_PUBKY_CONNECTION_STRING").is_some() {
        EphemeralTestnet::builder()
    } else {
        let postgres = shared_postgres()
            .await
            .connection_string()
            .expect("Docker Postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };

    builder.with_http_relay().build().await.unwrap()
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
    session_secret: Arc<String>,
}

impl TestnetSessionProvider {
    pub fn new(access: PubkySessionAccess, session_secret: String) -> Self {
        Self {
            session: Arc::new(Mutex::new(Some(access))),
            session_secret: Arc::new(session_secret),
        }
    }
}

#[async_trait]
impl PubkySessionProvider for TestnetSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(self.session.lock().expect("session lock poisoned").clone())
    }

    async fn revoke_session_access(&self, access: &PubkySessionAccess) -> Result<()> {
        PubkySessionBootstrap::with_pubky(access.outbox_client.clone(), TEST_CLIENT_ID)?
            .revoke_grant(&self.session_secret, access)
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
}

impl TestnetPaymentAdapter {
    pub fn set_public_details(&self, details: Vec<PublicReceivingDetail>) {
        *self.public_details.lock().expect("details lock poisoned") = details;
    }

    pub fn set_private_details(&self, details: Vec<PrivateReceivingDetail>) {
        *self.private_details.lock().expect("details lock poisoned") = details;
    }
}

#[async_trait]
impl PaymentAdapter for TestnetPaymentAdapter {
    async fn current_public_receiving_details(&self) -> Result<Vec<PublicReceivingDetail>> {
        Ok(self
            .public_details
            .lock()
            .expect("details lock poisoned")
            .clone())
    }

    async fn current_private_receiving_details(
        &self,
        _counterparty: &PubkyPublicKey,
        _counterparty_receiver_path: &PaykitReceiverPath,
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
    pub receiver_path: PaykitReceiverPath,
}

impl TestUser {
    /// Sign up a fresh keypair on the testnet homeserver through the SDK's own
    /// bootstrap, then build and initialize a runtime around the session.
    ///
    /// The identity keypair authenticates Pubky while bootstrap creates the
    /// independent receiver Noise key used for private links.
    pub async fn sign_up(testnet: &EphemeralTestnet) -> TestUser {
        Self::sign_up_with_receiver(testnet, receiver_path("bitkit/wallet")).await
    }

    pub async fn sign_up_with_receiver(
        testnet: &EphemeralTestnet,
        receiver_path: PaykitReceiverPath,
    ) -> TestUser {
        Self::sign_up_with_receiver_access(testnet, receiver_path, true).await
    }

    pub async fn sign_up_with_server_owned_identity(
        testnet: &EphemeralTestnet,
        receiver_path: PaykitReceiverPath,
    ) -> TestUser {
        Self::sign_up_with_receiver_access(testnet, receiver_path, false).await
    }

    async fn sign_up_with_receiver_access(
        testnet: &EphemeralTestnet,
        receiver_path: PaykitReceiverPath,
        retain_identity_secret: bool,
    ) -> TestUser {
        let keypair = Keypair::random();
        let secret_key = PubkyLocalSecretKey::new(keypair.secret_key());
        let homeserver_public_key =
            PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
        let bootstrap = session_bootstrap(testnet, TEST_CLIENT_ID);
        let config = PaykitSdkConfig::new(receiver_path.clone());
        let receiver_noise_secret_key = ReceiverNoiseSecretKey::random();
        let receiver_noise_public_key = receiver_noise_secret_key.public_key();
        let result = bootstrap
            .sign_up(
                &secret_key,
                receiver_noise_secret_key,
                &homeserver_public_key,
                None,
                &config.required_session_capabilities(),
            )
            .await
            .expect("testnet sign-up should succeed");
        let session_secret = result
            .export_session_secret()
            .await
            .expect("test grant should export local restore material")
            .into_inner();
        assert_eq!(
            result.access.receiver_noise_secret_key.public_key(),
            receiver_noise_public_key
        );
        let mut access = result.access;
        if !retain_identity_secret {
            access.local_secret_key = None;
        }

        let storage = InMemoryStorage::default();
        let adapter = TestnetPaymentAdapter::default();
        let sdk = build_initialized_sdk(
            storage.clone(),
            &access,
            &session_secret,
            adapter.clone(),
            config,
        )
        .await;
        sdk.publish_paykit_receiver_marker(PaykitReceiverCapabilities {
            private_payments: true,
            payment_requests: true,
            receipts: true,
            outgoing_payments: true,
        })
        .await
        .expect("receiver marker publication should succeed");

        TestUser {
            sdk,
            storage,
            adapter,
            access,
            session_secret,
            public_key: result.public_key,
            receiver_path,
        }
    }

    /// Rebuild this user's runtime around the supplied durable storage.
    ///
    /// Tests use this to distinguish an ordinary process restart (shared
    /// storage) from a backup restore into a fresh local store.
    pub async fn restart_with_storage(&self, storage: InMemoryStorage) -> TestUser {
        let config = PaykitSdkConfig::new(self.receiver_path.clone());
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
            receiver_path: self.receiver_path.clone(),
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
    let provider = TestnetSessionProvider::new(access.clone(), session_secret.to_string());
    let sdk = PaykitSdk::new(storage, provider, adapter, config)
        .expect("SDK construction should succeed");
    let report = sdk
        .initialize()
        .await
        .expect("SDK initialization should succeed");
    assert!(report.identity.live_session_available);
    sdk
}

/// Two signed-up users sharing one testnet homeserver.
pub struct TwoParty {
    pub _testnet: EphemeralTestnet,
    pub alice: TestUser,
    pub bob: TestUser,
}

pub async fn two_party() -> TwoParty {
    let testnet = build_testnet().await;
    let alice = TestUser::sign_up_with_receiver(&testnet, receiver_path("bitkit/wallet")).await;
    let bob = TestUser::sign_up_with_receiver(&testnet, receiver_path("bitkit/server")).await;
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
        .initiate_link_with_peer(pair.bob.public_key.clone(), pair.bob.receiver_path.clone())
        .await
        .expect("initiating the Encrypted Link Handshake should succeed");
    pair.bob
        .sdk
        .accept_link_with_peer(
            pair.alice.public_key.clone(),
            pair.alice.receiver_path.clone(),
        )
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
        let peer_path = peer.receiver_path.clone();
        let result = if recovering {
            local
                .sdk
                .ensure_link_with_peer(peer_key, peer_path, 1)
                .await
        } else {
            local.sdk.advance_link_handshake(peer_key, peer_path).await
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

/// Wait until a newly published recovery marker will be newer than the
/// observer's persisted second-resolution checkpoint.
pub async fn wait_until_marker_is_newer_than_observer_checkpoint(
    observer: &TestUser,
    counterparty: &PubkyPublicKey,
    counterparty_receiver_path: &PaykitReceiverPath,
) {
    let cutoff = observer
        .storage
        .transaction({
            let counterparty = counterparty.clone();
            let counterparty_receiver_path = counterparty_receiver_path.clone();
            move |tx| {
                let link_checkpoint = tx
                    .encrypted_link_state(&counterparty, &counterparty_receiver_path)
                    .and_then(|state| {
                        (state.link_snapshot.is_some() || state.handshake_snapshot.is_some())
                            .then_some(state.checkpointed_at)
                    });
                let receive_checkpoint = tx
                    .linked_peer(&counterparty, &counterparty_receiver_path)
                    .and_then(|peer| peer.last_private_receive_at);
                Ok(link_checkpoint.max(receive_checkpoint))
            }
        })
        .await
        .expect("observer checkpoint lookup should succeed");

    let Some(cutoff) = cutoff else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Utc::now().timestamp() <= cutoff.timestamp() {
        assert!(
            Instant::now() < deadline,
            "test clock did not advance past observer checkpoint"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub fn receiver_path(value: &str) -> PaykitReceiverPath {
    PaykitReceiverPath::new(value).expect("test receiver path should be valid")
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
