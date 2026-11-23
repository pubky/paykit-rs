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

/// Deliver the first collection page of each bounded read from before a newer directory.
pub fn delay_listing_pages(signer: &TestSigner, earlier: u64) {
    use pubky_switch_client::app::{Protocol, LISTING_PAGES_PER_READ};
    assert!(signer.switch().tip_seq() > earlier);
    let reads = paykit_sdk::PaykitProtocol::new()
        .collections()
        .len()
        .div_ceil(LISTING_PAGES_PER_READ as usize);
    for _ in 0..reads {
        signer
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Head {
                path: "records?collection".into(),
                at: Some(earlier),
                forked: false,
            });
    }
}

/// Complete the bounded client and wallet listings used by these small fixtures.
pub async fn finish_listing(wallet: &mut Paykit) {
    for _ in 0..8 {
        let summary = wallet.receive(None).await.unwrap();
        if summary.report.more {
            continue;
        }
        if let Some(reason) = summary.records_unlisted.as_deref() {
            assert!(reason.contains("Call receive again"), "{summary:?}");
            continue;
        }
        assert!(!wallet.app().listing_owed().await.unwrap(), "{summary:?}");
        return;
    }
    panic!("the test wallet's listing did not finish");
}

/// Enroll a wallet with its own identity, Switch and signer, using the given app store.
pub async fn enrolled_wallet(
    world: &pubky_switch_harness::World,
    name: &str,
    scopes: RequestedScopes,
    config: PaykitConfig,
    rail: &Arc<FakeRail>,
    storage: Arc<dyn StateStore>,
) -> Result<Paykit, Box<dyn std::error::Error>> {
    use pubky_switch_harness::parties::Asking;
    use pubky_switch_signer::{Protocols, Signer};

    let identity = world.identity(name)?;
    let origin = format!("https://{name}.public.test");
    world.add_switch(&origin, &[&identity]).await?;
    let mut env = world.signer_env(&identity, store())?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let mut signer = Box::pin(Signer::create(env, world.keyring(&identity)?, &origin)).await?;
    signer.resume().await?;
    let names = scopes.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let asking = Asking::for_scopes(world.app_keys(&identity, name)?, &names)?;
    let review = signer.review(asking.request.signed(), &asking.auth)?;
    let approved = Box::pin(signer.approve(review)).await?;
    signer.claim_delivered(&approved.request).await?;
    world.advance_ms(1_000);
    let timer = Arc::new(TestTimer {
        clock: world.clock().clone(),
    });
    let mut app = Box::pin(Paykit::join(
        world.app_env(storage),
        config,
        asking.join_request(identity.public(), &approved)?,
        rail.clone(),
        timer,
    ))
    .await?;
    finish_listing(&mut app).await;
    Ok(app)
}

/// Read the accepted message and check that its effect is reported exactly once in that read.
pub async fn receive_message(wallet: &mut Paykit, msg_id: &str) {
    for _ in 0..8 {
        let summary = wallet.receive(None).await.unwrap();
        let count = summary
            .applied
            .iter()
            .filter(|id| id.as_str() == msg_id)
            .count();
        if count > 0 {
            assert_eq!(count, 1);
            return;
        }
        if summary.report.more {
            continue;
        }
        if let Some(reason) = summary.records_unlisted.as_deref() {
            assert!(reason.contains("Call receive again"), "{summary:?}");
        }
    }
    panic!("the accepted message {msg_id} did not reach its wallet");
}

pub const HOUR_MS: u64 = 3_600_000;

/// Where alice's switch is.
pub const ALICE_ORIGIN: &str = "https://alice-switch.example";
/// Where bob's switch is.
pub const BOB_ORIGIN: &str = "https://bob-switch.example";
/// Where carol's switch is.
pub const CAROL_ORIGIN: &str = "https://carol-switch.example";

/// Every scope Paykit gives keys to, for the signers' approvals.
pub fn spec() -> ProtocolSpec {
    ProtocolSpec {
        keyed_scopes: [
            "paykit_pay",
            "paykit_request",
            "paykit_settlements",
            "paykit_recovery",
            "paykit_allowance",
            "paykit_allowance_execute",
            "paykit_contacts",
            "paykit_manage_contacts",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect(),
        ..ProtocolSpec::default()
    }
}

/// Alice, bob, carol, their switches, the Homeserver, the clock and the rail. Carol is a
/// third party to alice's and bob's activity, with no app of her own until a test joins one.
pub struct World {
    pub clock: Arc<TestClock>,
    pub homeserver: Arc<TestHomeserver>,
    pub alice: TestSigner,
    pub bob: TestSigner,
    pub carol: TestSigner,
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
        Self::at(START_MS)
    }

    pub fn at(now_ms: u64) -> Self {
        let clock = Arc::new(TestClock::new(now_ms));
        Self::on_clocks(Arc::clone(&clock), clock)
    }

    pub fn on_clocks(clock: Arc<TestClock>, bob_clock: Arc<TestClock>) -> Self {
        let seeds = Seeds::default();
        let homeserver = Arc::new(TestHomeserver::new());
        let signer = |name: &str, origin: &str, time: &Arc<TestClock>| {
            TestSigner::at(
                TestIdentity::new(&seeds, name),
                origin,
                Arc::clone(time),
                Arc::clone(&homeserver),
                spec(),
            )
        };
        let alice = signer("alice", ALICE_ORIGIN, &clock);
        let bob = signer("bob", BOB_ORIGIN, &bob_clock);
        let carol = signer("carol", CAROL_ORIGIN, &clock);
        let router = Arc::new(Router::new());
        router.add_switch(alice.origin(), alice.switch());
        router.add_switch(bob.origin(), bob.switch());
        router.add_switch(carol.origin(), carol.switch());
        let rail = Arc::new(FakeRail::with_clock(Arc::clone(&clock) as Arc<dyn Clock>));
        Self {
            clock,
            homeserver,
            alice,
            bob,
            carol,
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

    /// The wallet learns a higher signed version through Bob's recipient directory.
    /// The Homeserver still serves the previous document.
    pub async fn bob_directory_version(
        &self,
        wallet: &mut Paykit,
    ) -> pubky_switch_core::records::IdentityDoc {
        use paykit_lib::bodies::PrivatePaymentListRequest;
        use pubky_switch_core::message::{Message, Sender};
        use pubky_switch_core::records::IdentityDoc;
        let mut body = self.bob.body();
        body.version += 1;
        let higher = IdentityDoc::sign(self.bob.identity().key(), body).unwrap();
        self.bob.switch().deliver_identity(&higher).unwrap();
        let message = Message::new(
            "paykit.private_payment_list_request",
            Sender {
                identity: self.alice_id(),
                app_id: wallet.app_id(),
            },
            self.bob_id(),
            self.clock.now_ms(),
            vec![],
            PrivatePaymentListRequest::default().to_body().unwrap(),
        )
        .unwrap();
        wallet
            .app_mut()
            .forget_directory(&self.bob_id())
            .await
            .unwrap();
        assert!(!wallet
            .app_mut()
            .recipients_for(&self.bob_id(), &message)
            .await
            .unwrap()
            .is_empty());
        higher
    }

    pub fn carol_id(&self) -> [u8; 32] {
        self.carol.identity().public()
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
        let router = Arc::clone(&self.router) as Arc<dyn Transport>;
        self.join_through(signer, name, scopes, config, store, router)
            .await
    }

    /// [`World::join_with`], the app's requests going through `transport`.
    pub async fn join_through(
        &self,
        signer: &TestSigner,
        name: &str,
        scopes: RequestedScopes,
        config: PaykitConfig,
        store: Arc<dyn StateStore>,
        transport: Arc<dyn Transport>,
    ) -> Paykit {
        let request = self.approve(signer, name, &scopes);
        let mut env = self.env(signer, store);
        env.transport = transport;
        let mut app = Paykit::join(
            env,
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
        self.load_with(signer, store, app_id, clock, PaykitConfig::default())
            .await
    }

    /// [`World::load_on_clock`], with `config`.
    pub async fn load_with(
        &self,
        signer: &TestSigner,
        store: Arc<dyn StateStore>,
        app_id: [u8; 16],
        clock: Arc<TestClock>,
        config: PaykitConfig,
    ) -> Paykit {
        let mut env = self.env(signer, store);
        env.clock = clock as Arc<dyn Clock>;
        Paykit::load(
            env,
            config,
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

    /// Alice's wallet, holding `paykit_pay`. Alice buys from bob: the wallet invites him, a
    /// conversation she starts, as at a checkout, so his first request is admitted (Paykit
    /// spec 1).
    pub async fn wallet(&self, name: &str) -> Paykit {
        self.wallet_on(name, store()).await
    }

    /// Alice's wallet, as [`World::wallet`], on `store`.
    pub async fn wallet_on(&self, name: &str, store: Arc<dyn StateStore>) -> Paykit {
        self.wallet_with(name, store, PaykitConfig::default()).await
    }

    /// Alice's wallet, as [`World::wallet_on`], with `config`.
    pub async fn wallet_with(
        &self,
        name: &str,
        store: Arc<dyn StateStore>,
        config: PaykitConfig,
    ) -> Paykit {
        let mut wallet = self
            .join_with(
                &self.alice,
                name,
                AppScopes::new().pay().build().unwrap(),
                config,
                store,
            )
            .await;
        wallet.app_mut().invite(&self.bob_id()).await.unwrap();
        wallet
    }

    /// Bob's shop, holding `paykit_request`.
    pub async fn shop(&self) -> Paykit {
        self.join(
            &self.bob,
            "shop",
            AppScopes::new().request().build().unwrap(),
        )
        .await
    }

    /// Bob's labelled selling app, holding `paykit_request:<label>`.
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

/// A config whose attempts carry a day as `orphan_after`, for the scenarios that repeat a
/// dispatch hours after it: the default for the fake rail, its hour plus the pre-dispatch
/// budget, would have them orphaned by then.
pub fn day_window() -> PaykitConfig {
    PaykitConfig::default().with_orphan_after_s(86_400)
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
    /// Once set, the read of a held record that brings it to 1 fails, alone.
    countdown: std::sync::atomic::AtomicU64,
    /// One stored value whose next read exceeds the store's limit.
    limited_key: std::sync::Mutex<Option<String>>,
    /// One held record that stays too large until the store's limit is lifted.
    capped_record: std::sync::Mutex<Option<String>>,
    /// While set, every journal entry reads back as bytes that don't read.
    corrupt_journal: std::sync::atomic::AtomicBool,
    /// While set, every listing of the held records fails.
    scans_armed: std::sync::atomic::AtomicBool,
}

impl FailingRecordReads {
    pub fn limit_next_record(&self, key: &[u8; 32]) {
        *self.limited_key.lock().unwrap() =
            Some(format!("rec/{}", pubky_switch_core::enc::b64(key)));
    }

    /// Hold or release repeated reads of the same local record.
    pub fn cap_record(&self, key: Option<&[u8; 32]>) {
        *self.capped_record.lock().unwrap() =
            key.map(|key| format!("rec/{}", pubky_switch_core::enc::b64(key)));
    }

    pub fn limit_next_cursor_time(&self) {
        *self.limited_key.lock().unwrap() = Some("p/paykit/ct".into());
    }

    /// Fail every read of a held record from now on.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fail every listing of the held records from now on.
    pub fn arm_scans(&self) {
        self.scans_armed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The store heals: reads pass again.
    pub fn heal(&self) {
        self.armed.store(false, std::sync::atomic::Ordering::SeqCst);
        self.scans_armed
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.countdown.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    /// Serve every journal entry as bytes that don't read, or stop.
    pub fn corrupt_journal(&self, corrupt: bool) {
        self.corrupt_journal
            .store(corrupt, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fail only the `n`th read of a held record from now on, once.
    pub fn fail_nth(&self, n: u64) {
        self.countdown.store(n, std::sync::atomic::Ordering::SeqCst);
    }
}

impl StateStore for FailingRecordReads {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        let mut limited = self.limited_key.lock().unwrap();
        let once = limited.as_deref().map(str::as_bytes) == Some(key);
        let capped = self
            .capped_record
            .lock()
            .unwrap()
            .as_deref()
            .map(str::as_bytes)
            == Some(key);
        if once || capped {
            if let Some(stored) = self.inner.get(ns, key)? {
                if once {
                    *limited = None;
                }
                let size = u64::try_from(stored.len()).unwrap();
                return Err(pubky_switch_client::error::StoreError::TooLarge {
                    size,
                    cap: size.saturating_sub(1),
                });
            }
        }
        drop(limited);
        if self
            .corrupt_journal
            .load(std::sync::atomic::Ordering::SeqCst)
            && key.starts_with(b"p/paykit/j/")
        {
            return Ok(Some(b"not a journal entry".to_vec()));
        }
        if key.starts_with(b"rec/") {
            if self.armed.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(pubky_switch_client::error::StoreError::Unavailable);
            }
            let left = self.countdown.load(std::sync::atomic::Ordering::SeqCst);
            if left > 0 {
                self.countdown
                    .store(left - 1, std::sync::atomic::Ordering::SeqCst);
                if left == 1 {
                    return Err(pubky_switch_client::error::StoreError::Unavailable);
                }
            }
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
        if prefix.starts_with(b"rec/") && self.scans_armed.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        self.inner.scan(ns, prefix, after, limit)
    }

    fn commit(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<(), pubky_switch_client::error::StoreError> {
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl FailingRecordReads {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        apply(batch)
    }
}

/// A store that shows the app's copy of the lists (Switch wire 7.4.6) as a test sets it:
/// senders flagged, which the scripted switch never flags (its `FAKES.md`), a copy not
/// known to be complete, as after a restore, and reads of the deny list that fail. It
/// answers the client's own list keys (`lc/flag/<pubky>`, `lc/lists_at`, `lc/deny/<pubky>`)
/// in place of the copy it holds, and a flagged sender in the copy of the flagged senders
/// Paykit keeps from the same `GET /allow` (`fg`), once one is kept; everything else is the
/// inner store's.
#[derive(Default)]
pub struct ListsStore {
    inner: MemoryStore,
    flagged: std::sync::Mutex<Vec<Vec<u8>>>,
    flagged_senders: std::sync::Mutex<Vec<[u8; 32]>>,
    flagged_unknown: std::sync::atomic::AtomicBool,
    unknown: std::sync::atomic::AtomicBool,
    deny_fails: std::sync::atomic::AtomicBool,
    pub missing_list_reads: std::sync::atomic::AtomicUsize,
    pub failed_deny_reads: std::sync::atomic::AtomicUsize,
    pending_unblocks_unreadable: std::sync::atomic::AtomicBool,
}

impl ListsStore {
    /// Show `pubky` among the flagged senders of the last `GET /allow`.
    pub fn flag(&self, pubky: &[u8; 32]) {
        let key = format!("lc/flag/{}", pubky_switch_core::enc::pubky_id(pubky));
        self.flagged.lock().unwrap().push(key.into_bytes());
        self.flagged_senders.lock().unwrap().push(*pubky);
    }

    /// Show Paykit's copy of the flagged senders (`fg`) as not kept, as past its bound, or
    /// stop.
    pub fn flagged_unknown(&self, unknown: bool) {
        self.flagged_unknown
            .store(unknown, std::sync::atomic::Ordering::SeqCst);
    }

    /// Show `pubky` no longer flagged.
    pub fn unflag(&self, pubky: &[u8; 32]) {
        let key = format!("lc/flag/{}", pubky_switch_core::enc::pubky_id(pubky));
        self.flagged
            .lock()
            .unwrap()
            .retain(|k| *k != key.as_bytes());
        self.flagged_senders.lock().unwrap().retain(|p| p != pubky);
    }

    /// Show the copy of the lists as never read (`lc/lists_at` absent), or stop.
    pub fn lists_unknown(&self, unknown: bool) {
        self.unknown
            .store(unknown, std::sync::atomic::Ordering::SeqCst);
    }

    /// Fail every read of a deny-list entry, or stop.
    pub fn deny_reads_fail(&self, fail: bool) {
        self.deny_fails
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn pending_unblocks_unreadable(&self, unreadable: bool) {
        self.pending_unblocks_unreadable
            .store(unreadable, std::sync::atomic::Ordering::SeqCst);
    }
}

impl StateStore for ListsStore {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
        if self.unknown.load(std::sync::atomic::Ordering::SeqCst) && key == b"lc/lists_at" {
            self.missing_list_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Ok(None);
        }
        if key == b"p/paykit/bf"
            && self
                .pending_unblocks_unreadable
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        if self.deny_fails.load(std::sync::atomic::Ordering::SeqCst) && key.starts_with(b"lc/deny/")
        {
            self.failed_deny_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        if self.flagged.lock().unwrap().iter().any(|k| k == key) {
            return Ok(Some(Vec::new()));
        }
        // Paykit's own keys are under the client's protocol namespace, `p/paykit/`.
        if key == b"p/paykit/fg" {
            if self
                .flagged_unknown
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Ok(None);
            }
            let Some(kept) = self.inner.get(ns, key)? else {
                return Ok(None);
            };
            let mut copy: Value = serde_json::from_slice(&kept).unwrap();
            let senders = copy["d"].as_array_mut().unwrap();
            for pubky in self.flagged_senders.lock().unwrap().iter() {
                let pubky = serde_json::to_value(pubky).unwrap();
                if !senders.contains(&pubky) {
                    senders.push(pubky);
                }
            }
            return Ok(Some(serde_json::to_vec(&copy).unwrap()));
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
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl ListsStore {
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        apply(batch)
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
    RequestDraft::new(
        btc(value),
        PaymentReference::new("order-1").unwrap(),
        RequestDestination::Endpoints(endpoints),
    )
    .with_proposal_expires_at_ms(now + 24 * HOUR_MS)
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
        version: paykit_lib::ListVersion,
    ) -> String {
        let mut endpoints = std::collections::BTreeMap::new();
        endpoints.insert(
            endpoint("btc-bitcoin-p2wpkh"),
            paykit_lib::bodies::ListEndpoint {
                payload: PaymentEndpointPayload::new(&single_use_payload(version)).unwrap(),
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
        version: paykit_lib::ListVersion,
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

/// A hook [`ReadHook`] runs once.
type Hook = Box<dyn FnOnce() + Send>;

/// A transport that runs a hook once, when the next `GET` whose path holds a part it was
/// given has its answer, before the app gets the answer: what the hook writes lands after
/// that read. Every request goes through the world's router.
pub struct ReadHook {
    pub inner: Arc<Router>,
    hook: std::sync::Mutex<Option<(String, Hook)>>,
    fired: std::sync::atomic::AtomicBool,
}

impl ReadHook {
    pub fn new(inner: Arc<Router>) -> Self {
        Self {
            inner,
            hook: std::sync::Mutex::new(None),
            fired: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Run `hook` once, right after the next `GET` whose path holds `part` is answered.
    pub fn after_read(&self, part: &str, hook: impl FnOnce() + Send + 'static) {
        *self.hook.lock().unwrap() = Some((part.to_owned(), Box::new(hook)));
    }

    /// Whether the hook ran.
    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Transport for ReadHook {
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
        let hook = {
            let mut armed = self.hook.lock().unwrap();
            let due = request.method == "GET"
                && armed
                    .as_ref()
                    .is_some_and(|(part, _)| request.path_and_query.contains(part.as_str()));
            if due {
                armed.take().map(|(_, hook)| hook)
            } else {
                None
            }
        };
        Box::pin(async move {
            let answer = self.inner.send(request).await;
            if let Some(hook) = hook {
                hook();
                self.fired.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            answer
        })
    }
}

/// A transport whose public reads of one switch's `identity.json` hang while held: a slow
/// switch. Every other request goes through the world's router.
pub struct SlowSwitch {
    pub inner: Arc<Router>,
    pub origin: String,
    held: std::sync::atomic::AtomicBool,
    wakers: std::sync::Mutex<Vec<std::task::Waker>>,
    /// Once released, the next held read takes this long on this clock to answer.
    answer_delay: std::sync::Mutex<Option<(Arc<TestClock>, u64)>>,
}

impl SlowSwitch {
    pub fn new(inner: Arc<Router>, origin: &str) -> Self {
        Self {
            inner,
            origin: origin.to_owned(),
            held: std::sync::atomic::AtomicBool::new(true),
            wakers: std::sync::Mutex::new(Vec::new()),
            answer_delay: std::sync::Mutex::new(None),
        }
    }

    /// Whether a public identity read is waiting on this switch.
    pub fn has_waiter(&self) -> bool {
        !self.wakers.lock().unwrap().is_empty()
    }

    /// The next held read, once released, takes `ms` on `clock` to answer.
    pub fn delay_answer(&self, clock: Arc<TestClock>, ms: u64) {
        *self.answer_delay.lock().unwrap() = Some((clock, ms));
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
                if let Some((clock, ms)) = self.answer_delay.lock().unwrap().take() {
                    clock.advance_ms(ms);
                }
            }
            self.inner.send(request).await
        })
    }
}

/// The world's switches, where the next answer to an operation batch (`/ops`), once armed,
/// takes `ms` on the clock: the call returns after the batch was applied, the clock moved on.
pub struct SlowOps {
    pub inner: Arc<Router>,
    delay: std::sync::Mutex<Option<(Arc<TestClock>, u64)>>,
}

impl SlowOps {
    pub fn new(inner: Arc<Router>) -> Self {
        Self {
            inner,
            delay: std::sync::Mutex::new(None),
        }
    }

    /// The next batch's answer takes `ms` on `clock`.
    pub fn delay_next(&self, clock: Arc<TestClock>, ms: u64) {
        *self.delay.lock().unwrap() = Some((clock, ms));
    }
}

impl Transport for SlowOps {
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
        let ops = request.method == "POST" && request.path_and_query.ends_with("/ops");
        Box::pin(async move {
            let answer = self.inner.send(request).await;
            if ops {
                if let Some((clock, ms)) = self.delay.lock().unwrap().take() {
                    clock.advance_ms(ms);
                }
            }
            answer
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

/// An original message reported for reconciliation without a new local delivery.
pub async fn possibly_delivered(app: &Paykit, msg_id: &str) -> bool {
    app.message_delivery(msg_id).await.unwrap().is_none()
        && app.notices().await.unwrap().iter().any(|notice| {
            notice.kind == paykit_sdk::NoticeKind::PossiblyDelivered
                && notice.msg_id.as_deref() == Some(msg_id)
                && notice.message_hash.is_some()
        })
}

/// The re-send requests `app` delivered to `to`, as their `accepted_from` and
/// `accepted_until`, in the order delivered.
pub async fn asked(app: &Paykit, to: &[u8; 32]) -> Vec<(Option<u64>, Option<u64>)> {
    let mut sent = app.app().sent_to(to, 0, u64::MAX).await.unwrap();
    sent.sort_by_key(|m| m.accepted_at_ms);
    sent.iter()
        .filter(|m| m.kind == "paykit.resend_request")
        .map(|m| {
            let message = pubky_switch_core::message::Message::parse(
                m.plaintext.json(),
                paykit_lib::kinds::OPAQUE_MEMBERS,
            )
            .unwrap();
            let body = paykit_lib::bodies::ResendRequest::from_body(&message.body).unwrap();
            (body.accepted_from, body.accepted_until)
        })
        .collect()
}

/// Marks of messages dropped before they were taken, under `prefix` (`pd/` or `ud/`), for
/// `n` made-up senders `[i + 100; 32]`, the `i`-th with both times `latest + i`, not yet due.
/// Returns the senders, in that order.
pub async fn seed_marks(app: &mut Paykit, prefix: &str, n: usize, latest: u64) -> Vec<[u8; 32]> {
    let mut senders = Vec::new();
    let mut writes = Vec::new();
    for i in 0..n {
        let sender = [u8::try_from(i + 100).unwrap(); 32];
        let at = latest + u64::try_from(i).unwrap();
        let value = serde_json::json!({
            "v": 1,
            "d": {
                "sender": pubky_switch_core::enc::b64(&sender),
                "earliest_ms": at,
                "latest_ms": at,
                "due": false,
                "asked_at_ms": null,
                "queued": null,
            }
        });
        writes.push(pubky_switch_client::app::LocalWrite::Put {
            key: format!("{prefix}{}", pubky_switch_core::enc::b64(&sender)).into_bytes(),
            value: serde_json::to_vec(&value).unwrap(),
        });
        senders.push(sender);
    }
    app.app_mut()
        .protocol_commit("paykit", &writes)
        .await
        .unwrap();
    senders
}

/// The senders holding a mark under `prefix`.
pub async fn mark_senders(app: &Paykit, prefix: &str) -> Vec<[u8; 32]> {
    app.app()
        .protocol_scan("paykit", prefix.as_bytes())
        .await
        .unwrap()
        .into_iter()
        .map(|(key, _)| {
            let rest = std::str::from_utf8(&key[prefix.len()..]).unwrap();
            pubky_switch_core::enc::unb64_array::<32>(rest).unwrap()
        })
        .collect()
}

/// The next `n` batches at `signer`'s switch are refused for good, `403 permission_missing`,
/// as an effect the switch refuses whatever is sent again.
pub fn refuse_writes(signer: &TestSigner, n: usize) {
    for _ in 0..n {
        signer
            .switch()
            .fail_next(pubky_switch_client::testing::Fault::Error {
                path: "/ops".into(),
                code: pubky_switch_core::http::ErrorCode::PermissionMissing,
                current: serde_json::Value::Null,
            });
    }
}

/// Whether `error` is Paykit's corrupt-store error: state read back that doesn't verify.
pub fn is_corrupt(error: &paykit_sdk::PaykitError) -> bool {
    matches!(
        error,
        paykit_sdk::PaykitError::Client(pubky_switch_client::app::AppError::Store(
            pubky_switch_client::error::StoreError::Corrupt
        ))
    )
}

/// Bob's shop sends `request` again, as a re-send does.
pub async fn send_request_again(w: &World, shop: &mut Paykit, request: &RequestRef) {
    let sent = shop
        .app()
        .sent_to(&w.alice_id(), 0, u64::MAX)
        .await
        .unwrap();
    let msg_id = sent
        .iter()
        .find(|s| s.plaintext.hash() == request.request_hash)
        .unwrap()
        .msg_id
        .clone();
    shop.app_mut().resend(&msg_id).await.unwrap();
}

/// Moving the cursor before the first restored page gives the client an unknown verdict.
pub async fn restore_with_unknown_cursor(app: &mut Paykit) {
    app.app_mut().flush_cursor().await.unwrap();
}

/// The current hold of a readable request, as shown to the user.
pub async fn held(app: &Paykit, request: &paykit_sdk::RequestRef) -> paykit_sdk::HeldRequest {
    app.request(request)
        .await
        .unwrap()
        .held
        .expect("the request is held")
}

/// Options confirming the current hold when this request can be shown as held.
pub async fn over_hold(app: &Paykit, request: &paykit_sdk::RequestRef) -> paykit_sdk::PayOptions {
    match app.request(request).await.ok().and_then(|view| view.held) {
        Some(held) => paykit_sdk::PayOptions::default().with_possibly_cancelled_confirmed(held),
        None => paykit_sdk::PayOptions::default(),
    }
}

/// The Private Payment List version `(generation, counter)` (Paykit wire 4.13).
pub fn list_version(generation: u64, counter: u64) -> paykit_lib::ListVersion {
    paykit_lib::ListVersion::new(generation, counter).unwrap()
}

/// The single-use on-chain payload of [`World::list_to_alice`]'s list of `version`.
pub fn single_use_payload(version: paykit_lib::ListVersion) -> String {
    format!("bc1qsingle{}x{}", version.generation(), version.counter())
}

/// A store that, once armed, fails the client's next commit that keeps a batch
/// (`opq/<batch_id>`), once, applying nothing.
#[derive(Default)]
pub struct FailNextKeep {
    inner: pubky_switch_client::store::MemoryStore,
    before: std::sync::atomic::AtomicBool,
    after: std::sync::atomic::AtomicBool,
}

impl pubky_switch_client::store::StateStore for FailNextKeep {
    fn get(
        &self,
        ns: &str,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>, pubky_switch_client::error::StoreError> {
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
        self.commit_with(batch, |batch| {
            self.inner
                .commit(batch)
                .map(|()| pubky_switch_client::store::CommitOutcome::Applied)
        })
        .map(|_| ())
    }

    fn acquire(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
    ) -> Result<pubky_switch_client::store::AcquireOutcome, pubky_switch_client::error::StoreError>
    {
        self.inner.acquire(ns, owner)
    }

    fn commit_if(
        &self,
        ns: &str,
        owner: pubky_switch_client::store::StoreOwner,
        batch: pubky_switch_client::store::WriteBatch,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        self.commit_with(batch, |batch| self.inner.commit_if(ns, owner, batch))
    }
}
impl FailNextKeep {
    pub fn fail_before_keep(&self) {
        self.before.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn lose_keep_answer(&self) {
        self.after.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn commit_with(
        &self,
        batch: pubky_switch_client::store::WriteBatch,
        apply: impl FnOnce(
            pubky_switch_client::store::WriteBatch,
        ) -> Result<
            pubky_switch_client::store::CommitOutcome,
            pubky_switch_client::error::StoreError,
        >,
    ) -> Result<pubky_switch_client::store::CommitOutcome, pubky_switch_client::error::StoreError>
    {
        use pubky_switch_client::store::Write;
        use std::sync::atomic::Ordering;
        let keeps = batch
            .writes()
            .iter()
            .any(|write| matches!(write, Write::Put { key, .. } if key.starts_with(b"opq/")));
        if keeps && self.before.swap(false, Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unavailable);
        }
        let outcome = apply(batch)?;
        if keeps && self.after.swap(false, Ordering::SeqCst) {
            return Err(pubky_switch_client::error::StoreError::Unknown);
        }
        Ok(outcome)
    }
}
