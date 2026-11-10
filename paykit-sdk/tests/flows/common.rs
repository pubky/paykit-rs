//! The world the flow tests run in: two identities, alice (the payer) and bob (the payee),
//! each with its own signer and its own scripted switch at an origin of its own
//! ([`TestSigner::at`]), routed by the client's [`Router`], Paykit apps joined from
//! approvals, and a fake rail.
//!
//! Each switch answers the other identity's apps as a sender sees it: admission by the
//! lists and invite codes, the directory, peer copies of `identity.json` and the inbox. The
//! identities admit each other as real apps do: a payer invites the payee it buys from (a
//! conversation it starts, as at a checkout), and the first message an app sends a
//! counterparty writes its conversation entry, so the counterparty's replies are admitted
//! (Paykit spec 1, Switch spec 7).

use std::sync::Arc;

use paykit_lib::bodies::RequestDestination;
use paykit_lib::{
    Asset, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
};
use paykit_lib::{Label, PaymentReference};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{AppScopes, Paykit, PaykitConfig, RequestDraft, RequestRef, RequestedScopes};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_client::store::{MemoryStore, StateStore};
use pubky_switch_client::testing::{
    AppSpec, ProtocolSpec, Router, Seeds, TestHomeserver, TestIdentity, TestSigner, START_MS,
};
use pubky_switch_client::transport::Transport;
use pubky_switch_core::ops::OpKind;
use serde_json::Value;

pub const HOUR_MS: u64 = 3_600_000;

/// Where alice's switch is.
pub const ALICE_ORIGIN: &str = "https://alice-switch.example";
/// Where bob's switch is.
pub const BOB_ORIGIN: &str = "https://bob-switch.example";

/// Every scope Paykit gives keys to, for the signers' approvals.
pub fn spec() -> ProtocolSpec {
    ProtocolSpec {
        keyed_scopes: [
            "pay",
            "request",
            "allowance",
            "allowance_execute",
            "contacts",
            "manage_contacts",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
        ..ProtocolSpec::default()
    }
}

/// Alice, bob, their switches, the Homeserver, the clock and the rail.
pub struct World {
    pub clock: Arc<TestClock>,
    pub homeserver: Arc<TestHomeserver>,
    pub alice: TestSigner,
    pub bob: TestSigner,
    pub router: Arc<Router>,
    pub rail: Arc<FakeRail>,
}

/// The host's timer in the tests: its sleep moves the test clock on and resolves at once.
pub struct TestTimer {
    pub clock: Arc<TestClock>,
}

impl paykit_sdk::Timer for TestTimer {
    fn sleep(&self, ms: u64) -> pubky_switch_client::transport::BoxFuture<'static, ()> {
        self.clock.advance_ms(ms);
        Box::pin(async {})
    }
}

impl World {
    /// The host's timer, on the world's clock.
    pub fn timer(&self) -> Arc<dyn paykit_sdk::Timer> {
        Arc::new(TestTimer {
            clock: Arc::clone(&self.clock),
        })
    }

    pub fn new() -> Self {
        let seeds = Seeds::default();
        let clock = Arc::new(TestClock::new(START_MS));
        let homeserver = Arc::new(TestHomeserver::new());
        let signer = |name: &str, origin: &str| {
            TestSigner::at(
                TestIdentity::new(&seeds, name),
                origin,
                Arc::clone(&clock),
                Arc::clone(&homeserver),
                spec(),
            )
        };
        let alice = signer("alice", ALICE_ORIGIN);
        let bob = signer("bob", BOB_ORIGIN);
        let router = Arc::new(Router::new());
        router.add_switch(alice.origin(), alice.switch());
        router.add_switch(bob.origin(), bob.switch());
        let rail = Arc::new(FakeRail::with_clock(Arc::clone(&clock) as Arc<dyn Clock>));
        Self {
            clock,
            homeserver,
            alice,
            bob,
            router,
            rail,
        }
    }

    pub fn alice_id(&self) -> [u8; 32] {
        self.alice.identity().public()
    }

    pub fn bob_id(&self) -> [u8; 32] {
        self.bob.identity().public()
    }

    /// The approval of app `name` of `signer` for `scopes`.
    pub fn approve(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: &RequestedScopes,
    ) -> pubky_switch_client::app::JoinRequest {
        let keys = signer.identity().app_keys(name);
        let app = AppSpec::new(&scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>())
            .with_permissions(&scopes.permissions);
        signer.approve(&keys, &app).unwrap()
    }

    /// The environment of an app of `signer` on `store`.
    pub fn env(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
    ) -> pubky_switch_client::app::AppEnv {
        signer.env_with(store, Arc::clone(&self.router) as Arc<dyn Transport>)
    }

    /// Join app `name` of `signer` with `scopes` and `config`, on a store of its own, and
    /// finish the join.
    pub async fn join_with(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: RequestedScopes,
        config: PaykitConfig,
        store: Arc<dyn StateStore>,
    ) -> Paykit {
        let request = self.approve(signer, name, &scopes);
        let mut app = Paykit::join(
            self.env(signer, store),
            config,
            request,
            Arc::clone(&self.rail) as Arc<dyn paykit_sdk::Rail>,
            self.timer(),
        )
        .await
        .unwrap();
        app.receive(None).await.unwrap();
        app
    }

    /// Load app `app_id` of `signer` from `store`, its device clock `clock` instead of the
    /// world's, which the switches keep.
    pub async fn load_on_clock(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
        app_id: [u8; 16],
        clock: Arc<TestClock>,
    ) -> Paykit {
        let mut env = self.env(signer, store);
        env.clock = clock as Arc<dyn Clock>;
        Paykit::load(
            env,
            PaykitConfig::default(),
            app_id,
            Arc::clone(&self.rail) as Arc<dyn paykit_sdk::Rail>,
            self.timer(),
        )
        .await
        .unwrap()
    }

    pub async fn join(&self, signer: &TestSigner, name: &str, scopes: RequestedScopes) -> Paykit {
        self.join_with(signer, name, scopes, PaykitConfig::default(), store())
            .await
    }

    /// Alice's wallet, holding `pay`. Alice buys from bob: the wallet invites him, a
    /// conversation she starts, as at a checkout, so his first request is admitted (Paykit
    /// spec 1).
    pub async fn wallet(&self, name: &str) -> Paykit {
        self.wallet_on(name, store()).await
    }

    /// Alice's wallet, as [`World::wallet`], on `store`.
    pub async fn wallet_on(&self, name: &str, store: Arc<dyn StateStore>) -> Paykit {
        let mut wallet = self
            .join_with(
                &self.alice,
                name,
                AppScopes::new().pay().build().unwrap(),
                PaykitConfig::default(),
                store,
            )
            .await;
        wallet.app_mut().invite(&self.bob_id()).await.unwrap();
        wallet
    }

    /// Bob's shop, holding `request`.
    pub async fn shop(&self) -> Paykit {
        self.join(
            &self.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await
    }

    /// Bob's labelled selling app, holding `request:<label>`.
    pub async fn labelled_shop(&self, name: &str, label: &str) -> Paykit {
        let label = Label::parse(label).unwrap();
        self.join(
            &self.bob,
            name,
            AppScopes::new().request_label(&label).build().unwrap(),
        )
        .await
    }
}

pub fn store() -> Arc<dyn StateStore> {
    Arc::new(MemoryStore::new())
}

/// A store whose reads of single shared records fail once armed, while its scans still
/// answer: a collection listed, then a record of it that can't be read again.
#[derive(Default)]
pub struct FailingRecordReads {
    inner: MemoryStore,
    armed: std::sync::atomic::AtomicBool,
}

impl FailingRecordReads {
    /// Fail every read of a held record from now on.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl StateStore for FailingRecordReads {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        if self.armed.load(std::sync::atomic::Ordering::SeqCst) && key.starts_with(b"rec/") {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.get(ns, key)
    }

    fn scan(
        &self,
        ns: &str,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: u32,
    ) -> Result<Vec<pubky_switch_client::store::KeyValue>, pubky_switch_client::error::StoreError>
    {
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        self.inner.commit(batch)
    }
}

pub fn btc(value: &str) -> PaymentAmount {
    PaymentAmount {
        value: Decimal::new(value).unwrap(),
        asset: Asset::new("btc").unwrap(),
    }
}

pub fn endpoint(id: &str) -> PaymentEndpointIdentifier {
    PaymentEndpointIdentifier::new(id).unwrap()
}

/// A one-time request for `value` btc, paid to one invoice of the sending app, expiring a day
/// from now.
pub fn draft(value: &str, now: u64) -> RequestDraft {
    let mut endpoints = std::collections::BTreeMap::new();
    endpoints.insert(
        endpoint("btc-lightning-bolt11"),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    );
    RequestDraft {
        amount: btc(value),
        payment_reference: PaymentReference::new("order-1").unwrap(),
        proposal_expires_at_ms: Some(now + 24 * HOUR_MS),
        payment_deadline_ms: None,
        destination: RequestDestination::Endpoints(endpoints),
        accepted_endpoint_identifiers: None,
        metadata: None,
    }
}

/// Bob sends alice a request from `shop`; alice's `wallets` receive it. Returns the request.
pub async fn request_from(shop: &mut Paykit, alice: [u8; 32], draft: RequestDraft) -> RequestRef {
    let sent = shop.send_request(alice, draft).await.unwrap();
    sent.request.expect("the request is accepted at once")
}

impl World {
    /// A message of `kind` with `body` and `refs` from `signer`'s app `app`, entered into the
    /// switch of `to` for its apps `targets`, as if its inbox had accepted it. Returns the
    /// `msg_id`.
    // A test fixture naming the sender, the recipient and the message in full.
    #[allow(clippy::too_many_arguments)]
    pub fn inject(
        &self,
        signer: &TestSigner,
        app: &str,
        to: &TestSigner,
        targets: &[[u8; 16]],
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: serde_json::Map<String, Value>,
    ) -> String {
        let keys = signer.identity().app_keys(app);
        let cert = signer.switch().app(&keys.app_id_bytes()).cert.unwrap();
        let message = pubky_switch_core::message::Message::new(
            kind,
            pubky_switch_core::message::Sender::of_app(signer.identity().public(), &keys),
            to.identity().public(),
            self.clock.now_ms(),
            refs,
            body,
        )
        .unwrap();
        let plaintext = message.sign(&keys).unwrap();
        to.switch()
            .send_plaintext(signer.identity().public(), &cert, &plaintext, targets)
            .unwrap();
        message.msg_id
    }

    /// A message of `kind` with `body` and `refs`, signed by `signer`'s app `app` for `to`,
    /// and not delivered: [`World::deliver`] enters it later, as a re-send would.
    pub fn sign_as(
        &self,
        signer: &TestSigner,
        app: &str,
        to: &TestSigner,
        kind: &str,
        refs: Vec<[u8; 32]>,
        body: serde_json::Map<String, Value>,
    ) -> pubky_switch_core::message::MessagePlaintext {
        let keys = signer.identity().app_keys(app);
        let message = pubky_switch_core::message::Message::new(
            kind,
            pubky_switch_core::message::Sender::of_app(signer.identity().public(), &keys),
            to.identity().public(),
            self.clock.now_ms(),
            refs,
            body,
        )
        .unwrap();
        message.sign(&keys).unwrap()
    }

    /// Enter `plaintext`, signed by `signer`'s app `app`, into the switch of `to` for its apps
    /// `targets`, as if its inbox had accepted it now.
    pub fn deliver(
        &self,
        signer: &TestSigner,
        app: &str,
        to: &TestSigner,
        targets: &[[u8; 16]],
        plaintext: &pubky_switch_core::message::MessagePlaintext,
    ) {
        let keys = signer.identity().app_keys(app);
        let cert = signer.switch().app(&keys.app_id_bytes()).cert.unwrap();
        to.switch()
            .send_plaintext(signer.identity().public(), &cert, plaintext, targets)
            .unwrap();
    }

    /// Bob's `shop` sends alice's apps `targets` a Private Payment List of `version`, with a
    /// single-use on-chain endpoint and a reusable Lightning offer, for 30 days. Bob shares
    /// his list with alice, a conversation he starts: his switch admits her proofs.
    pub async fn list_to_alice(
        &self,
        shop: &mut Paykit,
        targets: &[[u8; 16]],
        version: u64,
    ) -> String {
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            endpoint("btc-bitcoin-p2wpkh"),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new(&format!("bc1qsingle{version}")).unwrap(),
                single_use: true,
            },
        );
        endpoints.insert(
            endpoint("btc-lightning-bolt12"),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new("lno1reusable").unwrap(),
                single_use: false,
            },
        );
        self.list_to_alice_with(shop, targets, version, endpoints)
            .await
    }

    /// Bob's `shop` sends alice's apps `targets` a Private Payment List of `version` with
    /// `endpoints`, for 30 days, as [`World::list_to_alice`].
    pub async fn list_to_alice_with(
        &self,
        shop: &mut Paykit,
        targets: &[[u8; 16]],
        version: u64,
        endpoints: std::collections::BTreeMap<
            PaymentEndpointIdentifier,
            paykit_lib::bodies::ListEndpoint,
        >,
    ) -> String {
        shop.app_mut().invite(&self.alice_id()).await.unwrap();
        let body = paykit_lib::bodies::PrivatePaymentList {
            version,
            payment_endpoints: endpoints,
            expires_at: self.clock.now_ms() + 30 * 24 * HOUR_MS,
        }
        .to_body()
        .unwrap();
        self.inject(
            &self.bob,
            "shop",
            &self.alice,
            targets,
            "paykit.private_payment_list",
            Vec::new(),
            body,
        )
    }

    /// How many record writes `signer`'s switch applied to the record `key`.
    pub fn record_writes(&self, signer: &TestSigner, key: &[u8; 32]) -> usize {
        signer
            .switch()
            .applied()
            .iter()
            .filter(|op| matches!(&op.body().kind, OpKind::RecordPut(put) if put.key == *key))
            .count()
    }
}

/// A Homeserver whose reads of `identity.json` follow a script, once armed: each step names
/// an identity, how long its next read of that identity takes on the clock, and whether it
/// then fails. Every other read goes to the world's Homeserver.
pub struct ScriptedIdentityReads {
    pub inner: Arc<TestHomeserver>,
    pub clock: Arc<TestClock>,
    steps: std::sync::Mutex<std::collections::VecDeque<([u8; 32], u64, bool)>>,
}

impl ScriptedIdentityReads {
    pub fn new(inner: Arc<TestHomeserver>, clock: Arc<TestClock>) -> Self {
        Self {
            inner,
            clock,
            steps: std::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// The next read of `identity`'s `identity.json` takes `delay_ms`, then fails or not.
    pub fn then(&self, identity: [u8; 32], delay_ms: u64, fails: bool) {
        self.steps
            .lock()
            .unwrap()
            .push_back((identity, delay_ms, fails));
    }
}

impl pubky_switch_client::app::HomeserverReader for ScriptedIdentityReads {
    fn get(
        &self,
        identity: [u8; 32],
        path: String,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<Option<Vec<u8>>, pubky_switch_client::error::TransportError>,
    > {
        let step = {
            let mut steps = self.steps.lock().unwrap();
            let due = path == pubky_switch_core::records::IDENTITY_PATH
                && steps.front().is_some_and(|(who, _, _)| *who == identity);
            if due {
                steps.pop_front()
            } else {
                None
            }
        };
        if let Some((_, delay_ms, fails)) = step {
            self.clock.advance_ms(delay_ms);
            if fails {
                return Box::pin(async {
                    Err(pubky_switch_client::error::TransportError::Connect(
                        "slow, then failed".into(),
                    ))
                });
            }
        }
        self.inner.get(identity, path)
    }
}

impl World {
    /// Load app `app_id` of `signer` from `store`, reading Homeservers through `homeserver`.
    pub async fn load_with_homeserver(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
        app_id: [u8; 16],
        homeserver: Arc<dyn pubky_switch_client::app::HomeserverReader>,
    ) -> Paykit {
        let mut env = self.env(signer, store);
        env.homeserver = homeserver;
        Paykit::load(
            env,
            PaykitConfig::default(),
            app_id,
            Arc::clone(&self.rail) as Arc<dyn paykit_sdk::Rail>,
            self.timer(),
        )
        .await
        .unwrap()
    }
}

/// A transport whose public reads of one switch's `identity.json` hang while held: a slow
/// switch. Every other request goes through the world's router.
pub struct SlowSwitch {
    pub inner: Arc<Router>,
    pub origin: String,
    held: std::sync::atomic::AtomicBool,
    wakers: std::sync::Mutex<Vec<std::task::Waker>>,
}

impl SlowSwitch {
    pub fn new(inner: Arc<Router>, origin: &str) -> Self {
        Self {
            inner,
            origin: origin.to_owned(),
            held: std::sync::atomic::AtomicBool::new(true),
            wakers: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The switch answers from now on, the reads held included.
    pub fn release(&self) {
        self.held.store(false, std::sync::atomic::Ordering::SeqCst);
        for waker in self.wakers.lock().unwrap().drain(..) {
            waker.wake();
        }
    }
}

impl Transport for SlowSwitch {
    fn send(
        &self,
        request: pubky_switch_client::transport::Request,
    ) -> pubky_switch_client::transport::BoxFuture<
        '_,
        Result<
            pubky_switch_client::transport::Response,
            pubky_switch_client::error::TransportError,
        >,
    > {
        let slow = request.origin == self.origin
            && request.method == "GET"
            && request.path_and_query.ends_with("/identity");
        Box::pin(async move {
            if slow {
                std::future::poll_fn(|cx| {
                    if self.held.load(std::sync::atomic::Ordering::SeqCst) {
                        self.wakers.lock().unwrap().push(cx.waker().clone());
                        std::task::Poll::Pending
                    } else {
                        std::task::Poll::Ready(())
                    }
                })
                .await;
            }
            self.inner.send(request).await
        })
    }
}

impl World {
    /// Load app `app_id` of `signer` from `store`, its requests going through `transport`.
    pub async fn load_with_transport(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
        app_id: [u8; 16],
        transport: Arc<dyn Transport>,
    ) -> Paykit {
        let mut env = self.env(signer, store);
        env.transport = transport;
        Paykit::load(
            env,
            PaykitConfig::default(),
            app_id,
            Arc::clone(&self.rail) as Arc<dyn paykit_sdk::Rail>,
            self.timer(),
        )
        .await
        .unwrap()
    }
}
