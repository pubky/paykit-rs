//! The world the browser flow tests run in: a payer and a payee, each with a signer and a
//! scripted switch at an origin of its own, one `Router` between the switches, a shared
//! Homeserver, a test clock started at the browser's wall clock, and the SDK's fake rail.
//!
//! The `request_flow` and `pay_flow` targets both include this module. The items only one of
//! them uses allow `dead_code`, as the other target builds them unused.

use std::collections::BTreeMap;
use std::sync::Arc;

use paykit_browser::clock::JsClock;
use paykit_browser::timer::JsTimer;
use paykit_lib::bodies::{PaymentRequest, RequestDestination};
use paykit_lib::records::{ActivityRecord, ActivitySide};
use paykit_lib::scopes::{keyed_scopes, PAYKIT_SCOPE_NAMES};
use paykit_lib::{
    Asset, Body, Decimal, PaymentAmount, PaymentEndpointIdentifier, PaymentEndpointPayload,
    PaymentReference,
};
use paykit_sdk::rail::FakeRail;
use paykit_sdk::{
    AppScopes, Paykit, PaykitConfig, RequestDraft, RequestRef, RequestView, RequestedScopes,
};
use pubky_switch_client::app::{AppEnv, RecordContent};
use pubky_switch_client::clock::{Clock, TestClock};
use pubky_switch_client::store::{MemoryStore, StateStore};
use pubky_switch_client::testing::{
    AppSpec, ProtocolSpec, Router, Seeds, TestHomeserver, TestIdentity, TestSigner,
};

pub const HOUR_MS: u64 = 3_600_000;
#[allow(dead_code)] // `pay_flow` only.
pub const DAY_MS: u64 = 24 * HOUR_MS;

/// Where the payer's switch is.
const PAYER_ORIGIN: &str = "https://payer-switch.example";
/// Where the payee's switch is.
const PAYEE_ORIGIN: &str = "https://payee-switch.example";

/// The payer, the payee, their switches, the clock and the rail.
pub struct World {
    pub clock: Arc<TestClock>,
    pub payer: TestSigner,
    payee: TestSigner,
    router: Arc<Router>,
    pub rail: Arc<FakeRail>,
}

impl World {
    /// A new world whose clock starts now, by the browser's wall clock.
    pub fn new() -> Self {
        let clock = Arc::new(TestClock::new(JsClock.now_ms()));
        let seeds = Seeds::default();
        let homeserver = Arc::new(TestHomeserver::new());
        // Every scope Paykit gives keys to (Paykit spec 3.5), as paykit-lib picks them out of
        // its scope names. An approval carries the keys of each scope the app holds, the scopes its
        // scopes require included (Paykit spec 3.1).
        let spec = ProtocolSpec {
            keyed_scopes: keyed_scopes(&PAYKIT_SCOPE_NAMES.map(str::to_owned)),
            ..ProtocolSpec::default()
        };
        let signer = |name: &str, origin: &str| {
            TestSigner::at(
                TestIdentity::new(&seeds, name),
                origin,
                Arc::clone(&clock),
                Arc::clone(&homeserver),
                spec.clone(),
            )
        };
        let payer = signer("payer", PAYER_ORIGIN);
        let payee = signer("payee", PAYEE_ORIGIN);
        let router = Arc::new(Router::new());
        router.add_switch(payer.origin(), payer.switch());
        router.add_switch(payee.origin(), payee.switch());
        let rail = Arc::new(FakeRail::with_clock(Arc::clone(&clock) as Arc<dyn Clock>));
        Self {
            clock,
            payer,
            payee,
            router,
            rail,
        }
    }

    pub fn payer_id(&self) -> [u8; 32] {
        self.payer.identity().public()
    }

    pub fn payee_id(&self) -> [u8; 32] {
        self.payee.identity().public()
    }

    /// The payer's wallet, holding `paykit_pay`, which has not invited the payee.
    #[allow(dead_code)] // `request_flow` only.
    pub async fn uninvited_wallet(&self) -> Paykit {
        self.uninvited_wallet_with(Arc::new(MemoryStore::new()), PaykitConfig::default())
            .await
    }

    /// The payer's wallet, as [`World::uninvited_wallet`], on `store` and with `config`.
    async fn uninvited_wallet_with(
        &self,
        store: Arc<dyn StateStore>,
        config: PaykitConfig,
    ) -> Paykit {
        self.join_with(
            &self.payer,
            "wallet",
            AppScopes::new().pay().build().unwrap(),
            config,
            store,
        )
        .await
    }

    /// The payer's wallet, holding `paykit_pay`. It invites the payee first, a conversation
    /// the payer starts, as at a checkout, so the payee's Payment Requests are admitted
    /// (Paykit spec 1).
    pub async fn wallet(&self) -> Paykit {
        self.wallet_with(Arc::new(MemoryStore::new()), PaykitConfig::default())
            .await
    }

    /// The payer's wallet, as [`World::wallet`], on `store` and with `config`.
    pub async fn wallet_with(&self, store: Arc<dyn StateStore>, config: PaykitConfig) -> Paykit {
        let mut wallet = self.uninvited_wallet_with(store, config).await;
        wallet.app_mut().invite(&self.payee_id()).await.unwrap();
        wallet
    }

    /// The payer's app `app_id`, loaded again from `store` with `config`, as a host does after
    /// a restart.
    #[allow(dead_code)] // `pay_flow` only.
    pub async fn reload_wallet(
        &self,
        store: Arc<dyn StateStore>,
        app_id: [u8; 16],
        config: PaykitConfig,
    ) -> Paykit {
        Paykit::load(
            self.env(&self.payer, store),
            config,
            app_id,
            Arc::clone(&self.rail) as Arc<_>,
            Arc::new(JsTimer),
        )
        .await
        .unwrap()
    }

    /// The payee's shop, holding `paykit_request`.
    pub async fn shop(&self) -> Paykit {
        self.join(
            &self.payee,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await
    }

    /// The environment of an app of `signer` on `store`, reaching the switches through the
    /// router.
    fn env(&self, signer: &TestSigner, store: Arc<dyn StateStore>) -> AppEnv {
        signer.env_with(store, Arc::clone(&self.router) as Arc<_>)
    }

    /// Join app `name` of `signer` with `scopes`, approved by the signer, on a store of its
    /// own, and finish the join with a first receive.
    async fn join(&self, signer: &TestSigner, name: &str, scopes: RequestedScopes) -> Paykit {
        self.join_with(
            signer,
            name,
            scopes,
            PaykitConfig::default(),
            Arc::new(MemoryStore::new()),
        )
        .await
    }

    /// [`World::join`], with `config`, on `store`.
    async fn join_with(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: RequestedScopes,
        config: PaykitConfig,
        store: Arc<dyn StateStore>,
    ) -> Paykit {
        let keys = signer.identity().app_keys(name);
        let names: Vec<&str> = scopes.scopes.iter().map(String::as_str).collect();
        let app = AppSpec::new(&names).with_permissions(&scopes.permissions);
        let request = signer.approve(&keys, &app).unwrap();
        let mut app = Paykit::join(
            self.env(signer, store),
            config,
            request,
            Arc::clone(&self.rail) as Arc<_>,
            Arc::new(JsTimer),
        )
        .await
        .unwrap();
        app.receive(None).await.unwrap();
        app
    }
}

/// A one-time Payment Request for `value` btc, paid to one Payment Endpoint of the sending
/// app, open until `expires_at_ms`.
pub fn draft(value: &str, expires_at_ms: u64) -> RequestDraft {
    let endpoints = BTreeMap::from([(
        PaymentEndpointIdentifier::new("btc-lightning-bolt11").unwrap(),
        PaymentEndpointPayload::new("lnbc1invoice").unwrap(),
    )]);
    RequestDraft::new(
        PaymentAmount {
            value: Decimal::new(value).unwrap(),
            asset: Asset::new("btc").unwrap(),
        },
        PaymentReference::new("order-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(expires_at_ms)
}

/// The payee's `shop` sends `draft` to `payer`, and the payer's switch accepts it at once.
/// Returns the request.
pub async fn send(shop: &mut Paykit, payer: [u8; 32], draft: RequestDraft) -> RequestRef {
    let sent = shop.send_request(payer, draft).await.unwrap();
    sent.request.expect("the request is accepted at once")
}

/// The Payment Request as the payer's activity record for `view` stores it, from the copy
/// `wallet` holds: the message the payee signed (Paykit wire 5.3).
#[allow(dead_code)] // `request_flow` only.
pub async fn stored_request(wallet: &Paykit, view: &RequestView) -> PaymentRequest {
    let record = wallet
        .app()
        .shared_record(&view.record)
        .await
        .unwrap()
        .expect("the wallet holds the request record");
    let RecordContent::Value(json) = record.content else {
        panic!("the request record does not read: {:?}", record.content);
    };
    let activity = ActivityRecord::read(&json, &wallet.identity(), ActivitySide::Payer).unwrap();
    match activity
        .request
        .expect("the record stores the request")
        .body()
    {
        Body::PaymentRequest(request) => request.clone(),
        other => panic!("the record stores {other:?}"),
    }
}
