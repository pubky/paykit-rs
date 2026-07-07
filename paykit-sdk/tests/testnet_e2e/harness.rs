//! Shared harness for testnet-backed end-to-end tests.
//!
//! Mirrors the embedded-testnet pattern from paykit-lib's test suite: one
//! embedded Postgres instance shared across the test binary, one ephemeral
//! Pubky testnet (homeserver) per test, and real signed-up sessions wrapped in
//! the SDK's own `PubkySessionAccess` via `PubkySessionBootstrap`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use paykit_sdk::{
    InMemoryStorage, LinkedPeerState, PaykitReceiverId, PaykitSdk, PaykitSdkConfig, PaymentAdapter,
    PaymentEndpointCandidate, PaymentEndpointSelectionRequest, PaymentTarget, PubkyLocalSecretKey,
    PubkyPublicKey, PubkySessionAccess, PubkySessionBootstrap, PubkySessionProvider,
    ReceivingDetail, ReceivingDetailScope, Result,
};
use pubky_testnet::{embedded_postgres::EmbeddedPostgres, pubky::Keypair, EphemeralTestnet};
use tokio::sync::{Mutex as TokioMutex, OnceCell};

static SHARED_POSTGRES: OnceCell<EmbeddedPostgres> = OnceCell::const_new();
static TESTNET_BUILD_LOCK: TokioMutex<()> = TokioMutex::const_new(());

async fn shared_postgres() -> &'static EmbeddedPostgres {
    SHARED_POSTGRES
        .get_or_init(|| async {
            EmbeddedPostgres::start()
                .await
                .expect("failed to start embedded postgres")
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
            .expect("embedded postgres connection string should be valid");
        EphemeralTestnet::builder().postgres(postgres)
    };

    builder.build().await.unwrap()
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
    public_details: Arc<Mutex<Vec<ReceivingDetail>>>,
    private_details: Arc<Mutex<Vec<ReceivingDetail>>>,
}

impl TestnetPaymentAdapter {
    pub fn set_public_details(&self, details: Vec<ReceivingDetail>) {
        *self.public_details.lock().expect("details lock poisoned") = details;
    }

    pub fn set_private_details(&self, details: Vec<ReceivingDetail>) {
        *self.private_details.lock().expect("details lock poisoned") = details;
    }
}

#[async_trait]
impl PaymentAdapter for TestnetPaymentAdapter {
    async fn current_receiving_details(
        &self,
        scope: ReceivingDetailScope,
    ) -> Result<Vec<ReceivingDetail>> {
        let details = if matches!(scope, ReceivingDetailScope::Public) {
            &self.public_details
        } else {
            &self.private_details
        };
        Ok(details.lock().expect("details lock poisoned").clone())
    }

    async fn select_payment_endpoints(
        &self,
        request: &PaymentEndpointSelectionRequest,
    ) -> Result<Vec<PaymentEndpointCandidate>> {
        Ok(request.candidates.clone())
    }

    async fn build_payment_target(
        &self,
        endpoint: &PaymentEndpointCandidate,
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
    pub sdk: PaykitSdk<InMemoryStorage, TestnetSessionProvider, TestnetPaymentAdapter>,
    pub storage: InMemoryStorage,
    pub adapter: TestnetPaymentAdapter,
    pub access: PubkySessionAccess,
    pub public_key: PubkyPublicKey,
    pub receiver_id: PaykitReceiverId,
}

impl TestUser {
    /// Sign up a fresh keypair on the testnet homeserver through the SDK's own
    /// bootstrap, then build and initialize a runtime around the session.
    ///
    /// One keypair is used for both signup and the local secret key so that
    /// `PubkySessionAccess::validate` holds and the session is
    /// private-link-capable.
    pub async fn sign_up(testnet: &EphemeralTestnet) -> TestUser {
        Self::sign_up_with_receiver(testnet, receiver_id("test-receiver")).await
    }

    pub async fn sign_up_with_receiver(
        testnet: &EphemeralTestnet,
        receiver_id: PaykitReceiverId,
    ) -> TestUser {
        let keypair = Keypair::random();
        let secret_key = PubkyLocalSecretKey::new(keypair.secret_key());
        let homeserver_public_key =
            PubkyPublicKey::from_public_key(&testnet.homeserver_app().public_key());
        let bootstrap =
            PubkySessionBootstrap::with_pubky(testnet.sdk().expect("testnet Pubky client"));
        let config = PaykitSdkConfig::new(receiver_id.clone());
        let result = bootstrap
            .sign_up(
                &secret_key,
                &homeserver_public_key,
                None,
                &config.required_session_capabilities(),
            )
            .await
            .expect("testnet sign-up should succeed");

        let storage = InMemoryStorage::default();
        let adapter = TestnetPaymentAdapter::default();
        let provider = TestnetSessionProvider::new(result.access.clone());
        let sdk = PaykitSdk::new(storage.clone(), provider, adapter.clone(), config)
            .expect("SDK construction should succeed");

        let report = sdk
            .initialize()
            .await
            .expect("SDK initialization should succeed");
        assert!(report.live_session_available);
        assert!(report.identity.private_link_capable);

        TestUser {
            sdk,
            storage,
            adapter,
            access: result.access,
            public_key: result.public_key,
            receiver_id,
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
    let alice = TestUser::sign_up_with_receiver(&testnet, receiver_id("alice-receiver")).await;
    let bob = TestUser::sign_up_with_receiver(&testnet, receiver_id("bob-receiver")).await;
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
        .initiate_link_with_peer(pair.bob.public_key.clone(), pair.bob.receiver_id.clone())
        .await
        .expect("initiating the Encrypted Link Handshake should succeed");
    pair.bob
        .sdk
        .accept_link_with_peer(
            pair.alice.public_key.clone(),
            pair.alice.receiver_id.clone(),
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
                .advance_link_handshake(bob.public_key.clone(), bob.receiver_id.clone())
                .await
                .expect("initiator handshake advance should succeed")
                .state;
        }
        if bob_state != LinkedPeerState::Linked {
            bob_state = bob
                .sdk
                .advance_link_handshake(alice.public_key.clone(), alice.receiver_id.clone())
                .await
                .expect("responder handshake advance should succeed")
                .state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn receiver_id(value: &str) -> PaykitReceiverId {
    PaykitReceiverId::new(value).expect("test receiver id should be valid")
}

pub fn receiving_detail(identifier: &str, payload: &str) -> ReceivingDetail {
    ReceivingDetail {
        identifier: identifier.into(),
        payload: payload.into(),
    }
}
