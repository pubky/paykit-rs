//! Shared harness for testnet-backed end-to-end tests.
//!
//! Mirrors the testnet pattern from paykit-lib's test suite: one Docker
//! Postgres instance shared across the test binary, one ephemeral
//! Pubky testnet (homeserver) per test, and real signed-up sessions wrapped in
//! the SDK's own `PubkySessionAccess` via `PubkySessionBootstrap`.

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
    PAYKIT_SESSION_CAPABILITIES,
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
}

impl TestnetSessionProvider {
    pub fn new(access: PubkySessionAccess) -> Self {
        Self {
            session: Arc::new(Mutex::new(Some(access))),
        }
    }
}

#[async_trait]
impl PubkySessionProvider for TestnetSessionProvider {
    async fn load_session_access(&self) -> Result<Option<PubkySessionAccess>> {
        Ok(self.session.lock().expect("session lock poisoned").clone())
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
}

impl TestnetPaymentAdapter {
    pub fn set_public_details(&self, details: Vec<PublicReceivingDetail>) {
        *self.public_details.lock().expect("details lock poisoned") = details;
    }

    pub fn set_private_details(&self, details: Vec<PrivateReceivingDetail>) {
        *self.private_details.lock().expect("details lock poisoned") = details;
    }

    pub fn set_fail_reservation_cancellation(&self, fail: bool) {
        *self
            .fail_reservation_cancellation
            .lock()
            .expect("failure flag lock poisoned") = fail;
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
    pub sdk: PaykitSdk<InMemoryStorage, TestnetSessionProvider, TestnetPaymentAdapter>,
    pub storage: InMemoryStorage,
    pub adapter: TestnetPaymentAdapter,
    pub access: PubkySessionAccess,
    pub public_key: PubkyPublicKey,
    pub app_id: PaykitAppId,
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
                PAYKIT_SESSION_CAPABILITIES,
            )
            .await
            .expect("testnet sign-up should succeed");
        let access = result.access;

        let storage = InMemoryStorage::default();
        let adapter = TestnetPaymentAdapter::default();
        let provider = TestnetSessionProvider::new(access.clone());
        let sdk = PaykitSdk::new(storage.clone(), provider, adapter.clone(), config);

        let report = sdk
            .initialize()
            .await
            .expect("SDK initialization should succeed");
        assert_eq!(
            report.capability,
            paykit_sdk::PubkyIdentityCapability::PrivateLinkCapable
        );
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
            public_key: result.public_key,
            app_id,
        }
    }

    /// Build another application runtime for this identity and shared state.
    pub async fn additional_app(&self, app_id: PaykitAppId, display_name: &str) -> TestUser {
        let storage = self.storage.clone();
        let adapter = TestnetPaymentAdapter::default();
        let provider = TestnetSessionProvider::new(self.access.clone());
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
            access: self.access.clone(),
            public_key: self.public_key.clone(),
            app_id,
        }
    }
}

/// Two signed-up users sharing one testnet homeserver.
pub struct TwoParty {
    pub _testnet: EphemeralTestnet,
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
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut alice_state = LinkedPeerState::Linking;
    let mut bob_state = LinkedPeerState::Linking;
    while alice_state != LinkedPeerState::Linked || bob_state != LinkedPeerState::Linked {
        assert!(
            Instant::now() < deadline,
            "Encrypted Link Handshake timed out"
        );
        if alice_state != LinkedPeerState::Linked {
            alice_state = alice
                .sdk
                .advance_link_handshake(bob.public_key.clone())
                .await
                .expect("initiator handshake advance should succeed")
                .state;
        }
        if bob_state != LinkedPeerState::Linked {
            bob_state = bob
                .sdk
                .advance_link_handshake(alice.public_key.clone())
                .await
                .expect("responder handshake advance should succeed")
                .state;
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
