//! [`Paykit`]: one Paykit App of an identity, on the client's [`App`], with Paykit's API.
//!
//! It is async only at the edges and returns plain owned types. The Switch's mechanics
//! (sending, receiving checks, records, claims, batches, cursors, moves) are the client's;
//! [`Paykit::app`] and [`Paykit::app_mut`] give the client's API for what Paykit doesn't wrap,
//! such as contacts and invite codes.
//!
//! The flows live in their own modules, each an `impl Paykit` block: receiving
//! (`receive.rs`), requests and their answers (`requests.rs`), paying (`pay.rs`), destination
//! checks (`destination.rs`), re-sends (`resend.rs`) and reconciliation (`reconcile.rs`).
//! Recurring requests, Allowances and publishing Private Payment Lists add their own modules
//! the same way.

use std::collections::HashMap;
use std::sync::Arc;

use paykit_lib::derive::request_obligation_blinded;
use paykit_lib::scopes::{acts_as_payee, check_certificate};
use paykit_lib::{Decimal, Label};
use pubky_switch_client::app::{
    App, AppBackup, AppConfig, AppEnv, AppEvent, HomeserverReader, JoinRequest, ReceiveReport,
};
use pubky_switch_client::clock::Clock;

use crate::destination::KeptLookup;
use crate::error::{Refusal, Result};
use crate::protocol::PaykitProtocol;
use crate::rail::{Rail, RailDeclaration};
use crate::state::{key, Tx};
use crate::timer::Timer;

/// The default `orphan_after` of an attempt: a day, the longest a payment method is expected
/// to take (Switch spec 10.2). It must be at least the floor [`orphan_floor_s`] gives for
/// the app's rail.
pub const DEFAULT_ORPHAN_AFTER_S: u64 = 86_400;

/// The longest a first dispatch may take from its attempt's answer to the payment method's
/// call: 10 minutes. Between them come the destination and identity checks, the
/// acceptance's queueing, the final payment check and the journal write of `Paying`: a few
/// requests in sequence, each bounded by the client's request timeout (40 s, with 10 s to
/// connect), so a few minutes at worst. The fence judges the attempt's age with the clock
/// tolerance (5 minutes) added, so a dispatch goes only within 5 minutes of its answer,
/// and the budget, added to the rail's longest time in the floor of `orphan_after`, keeps
/// the attempt from being orphaned while its payment may still run (Switch spec 10.2).
pub const PRE_DISPATCH_BUDGET_MS: u64 = 10 * 60_000;

/// The least `orphan_after`, in seconds, for an app paying through a rail that declares
/// `declaration`: the rail's longest time to complete or fail, plus
/// [`PRE_DISPATCH_BUDGET_MS`], rounded up (Switch spec 10.2: the attempting app sets
/// `orphan_after` to the longest its method can take). Below it, another own app could
/// resolve an attempt as orphaned while its dispatch was still to come or still running.
pub fn orphan_floor_s(declaration: &RailDeclaration) -> u64 {
    declaration
        .longest_completion_ms
        .saturating_add(PRE_DISPATCH_BUDGET_MS)
        .div_ceil(1000)
}

/// Refuse a `config` whose `orphan_after_s` is below the floor its rail needs.
pub(crate) fn check_orphan_window(config: &PaykitConfig, rail: &dyn Rail) -> Result<()> {
    let floor = orphan_floor_s(&rail.declaration());
    if config.orphan_after_s < floor {
        return Err(paykit_lib::Error::Validation(format!(
            "Switch spec 10.2: orphan_after_s {} is below the rail's floor of {floor} s, its \
             longest completion plus the pre-dispatch budget",
            config.orphan_after_s
        ))
        .into());
    }
    Ok(())
}

/// The default number of messages re-sent to one requester identity a day (Paykit spec 9).
pub const DEFAULT_RESEND_BUDGET: u64 = 500;

/// The integrator's choices for one Paykit App.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaykitConfig {
    /// The client's choices.
    pub app: AppConfig,
    /// Seconds after which an open attempt of this app is orphaned: the longest its payment
    /// method takes (Switch spec 10.2), and never below [`orphan_floor_s`] for the app's
    /// rail, which `join`, `load`, `restore` and every attempt refuse.
    pub orphan_after_s: u64,
    /// The largest fee the payee can influence through its destination that the app pays
    /// automatically (Paykit spec 7.2 step 1); `None` accepts no such fee.
    pub fee_cap: Option<Decimal>,
    /// The label a payee app sends its requests under (Paykit spec 3.1, 8.1). An app holding
    /// only labelled selling scopes needs one unless it holds exactly one label.
    pub selling_label: Option<Label>,
    /// How many messages the app re-sends to one requester identity a day (Paykit spec 9).
    pub resend_budget: u64,
}

impl Default for PaykitConfig {
    fn default() -> Self {
        Self {
            app: AppConfig::default(),
            orphan_after_s: DEFAULT_ORPHAN_AFTER_S,
            fee_cap: None,
            selling_label: None,
            resend_budget: DEFAULT_RESEND_BUDGET,
        }
    }
}

/// A Paykit App's backup: the client's [`AppBackup`] with Paykit's layer, its local state
/// and in it the payment journal, which a restore installs with the client's state in one
/// commit (Paykit spec 2, 7.2; Switch spec 12.4). It holds the app's signing key: keep it as
/// [`AppBackup`] says. Its `Debug` shows no key.
#[derive(Clone)]
pub struct PaykitBackup {
    app: AppBackup,
}

impl std::fmt::Debug for PaykitBackup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PaykitBackup({:?})", self.app)
    }
}

impl PaykitBackup {
    /// The backup's bytes.
    ///
    /// # Errors
    ///
    /// The client's error if it can't be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.app.to_bytes()?)
    }

    /// Read a backup's bytes.
    ///
    /// # Errors
    ///
    /// The client's error for bytes that don't read as a backup.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            app: AppBackup::from_bytes(bytes)?,
        })
    }
}

/// What a [`Paykit::receive`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReceiveSummary {
    /// The client's report of the feed read.
    pub report: ReceiveReport,
    /// The `msg_id`s of the messages received, each with its effect committed with the
    /// cursor move (Switch spec 8.2).
    pub applied: Vec<String>,
    /// The `resend_request`s answered and asked.
    pub resends: u64,
}

/// One Paykit App of an identity.
pub struct Paykit {
    pub(crate) app: App,
    pub(crate) protocol: Arc<PaykitProtocol>,
    pub(crate) rail: Arc<dyn Rail>,
    pub(crate) clock: Arc<dyn Clock>,
    /// The Homeserver reader, for the payees' app records (Paykit spec 4).
    pub(crate) homeserver: Arc<dyn HomeserverReader>,
    /// The host's timer, which bounds the wait for a payee's switch (Switch spec 4.3).
    pub(crate) timer: Arc<dyn Timer>,
    /// The payees' switch lookups still running after their grace: an answer that arrives
    /// before money moves is still used (Paykit spec 4).
    pub(crate) lookups: HashMap<[u8; 32], KeptLookup>,
    pub(crate) config: PaykitConfig,
}

impl std::fmt::Debug for Paykit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Paykit({:?})", self.app)
    }
}

impl Paykit {
    fn new_protocol() -> Arc<PaykitProtocol> {
        Arc::new(PaykitProtocol::new())
    }

    fn assemble(
        app: App,
        env: AppEnv,
        protocol: Arc<PaykitProtocol>,
        config: PaykitConfig,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        // Paykit's verifiers check the own certificate as they check every other (Paykit
        // wire 3.1): its scope rules, and permissions exactly as its scopes derive them.
        check_certificate(app.cert())?;
        protocol.set_own(app.identity(), app.cert().clone());
        Ok(Self {
            app,
            protocol,
            rail,
            clock: env.clock,
            homeserver: env.homeserver,
            timer,
            lookups: HashMap::new(),
            config,
        })
    }

    /// Join from an approval (Switch spec 12.1), as a Paykit App: the client's join, with
    /// Paykit as its protocol. The approval's certificate must pass Paykit's checks (Paykit
    /// wire 3.1). Session creation, key storage and the approval's channel stay the host's.
    ///
    /// # Errors
    ///
    /// The client's errors; [`crate::PaykitError::Lib`] for a certificate Paykit refuses,
    /// and `Validation` for an `orphan_after_s` below the rail's floor ([`orphan_floor_s`]),
    /// before anything is written.
    pub async fn join(
        env: AppEnv,
        config: PaykitConfig,
        request: JoinRequest,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        check_orphan_window(&config, rail.as_ref())?;
        let protocol = Self::new_protocol();
        let app = App::join(env.clone(), protocol.clone(), config.app.clone(), request).await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// Load a Paykit App from the store alone: nothing is sent. Payments a crash left in the
    /// journal resume at [`Paykit::resume_payments`].
    ///
    /// # Errors
    ///
    /// The client's errors; [`crate::PaykitError::Lib`] `Validation` for an
    /// `orphan_after_s` below the rail's floor ([`orphan_floor_s`]).
    pub async fn load(
        env: AppEnv,
        config: PaykitConfig,
        app_id: [u8; 16],
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        check_orphan_window(&config, rail.as_ref())?;
        let protocol = Self::new_protocol();
        let app = App::load(env.clone(), protocol.clone(), config.app.clone(), app_id).await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// Restore from a backup (Switch spec 12.4): the client's restore, which installs
    /// Paykit's layer, its local state with the journal, in the commit of its own state.
    /// The journal is checked whole before anything is written, and its entries are marked
    /// restored, so their open attempts are resolved as a restore requires (Paykit spec
    /// 7.2), each request payment's claim key bound to its request under the backup's naming
    /// key. On an error, neither layer is in the store. The app is in reconcile mode until
    /// [`Paykit::reconcile`] and [`Paykit::end_reconcile`].
    ///
    /// # Errors
    ///
    /// [`crate::PaykitError::Lib`] `Validation` for an `orphan_after_s` below the rail's floor
    /// ([`orphan_floor_s`]), before anything is written. The client's errors, all before
    /// anything is written but a store failure: `InvalidData` for a backup whose certificate
    /// Paykit refuses (Paykit wire 3.1), one without Paykit's layer or one another protocol
    /// made, or a journal with a malformed entry or two open entries for one obligation; a
    /// store failure.
    pub async fn restore(
        env: AppEnv,
        config: PaykitConfig,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        // The client checks its state, then Paykit's layer checks the certificate and the
        // whole journal (`PaykitProtocol::restore_layer`), before anything is written; then
        // both go in one commit: on an error, neither is in the store.
        check_orphan_window(&config, rail.as_ref())?;
        let protocol = Self::new_protocol();
        let app = App::restore(
            env.clone(),
            protocol.clone(),
            config.app.clone(),
            &backup.app,
        )
        .await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// The app's backup: the client's state and Paykit's, the payment journal in it, read in
    /// one call (Paykit spec 7.2; Switch spec 12.4). A backup that couldn't be restored is
    /// refused now, as the client checks it.
    ///
    /// # Errors
    ///
    /// The client's errors, such as a backup over the store's sizes
    /// (`AppEvent::BackupRefused`) or a journal Paykit's layer refuses; a store failure.
    pub async fn backup(&mut self) -> Result<PaykitBackup> {
        Ok(PaykitBackup {
            app: self.app.backup_layered().await?,
        })
    }

    // ---------------------------------------------------------------------------------
    // Accessors

    /// The client's app, for what Paykit doesn't wrap.
    pub fn app(&self) -> &App {
        &self.app
    }

    /// The client's app, mutable, for what Paykit doesn't wrap, such as contacts and invite
    /// codes.
    pub fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// The protocol the app runs with.
    pub fn protocol(&self) -> &Arc<PaykitProtocol> {
        &self.protocol
    }

    /// The identity.
    pub fn identity(&self) -> [u8; 32] {
        self.app.identity()
    }

    /// The App ID.
    pub fn app_id(&self) -> [u8; 16] {
        self.app.app_id()
    }

    /// The device clock, for what the spec reads on the reader's clock.
    pub(crate) fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The highest service time the app verified (Switch spec 10.1), for the deadlines the
    /// spec reads on the service's clock; `None` before any verified answer.
    pub(crate) fn service_time(&self) -> Option<u64> {
        self.app.highest_service_time()
    }

    /// The obligation ID of a request, or of its Billing Period starting at
    /// `period_start_secs` (0 for a one-time request), blinded through the client with the
    /// identity's naming key (Paykit spec 7.1, Paykit wire 1.5).
    pub(crate) fn obligation(
        &self,
        payee: &[u8; 32],
        request_hash: &[u8; 32],
        period_start_secs: u64,
    ) -> [u8; 32] {
        request_obligation_blinded(
            &self.app.blind(payee),
            &self.app.blind(request_hash),
            period_start_secs,
        )
    }

    pub(crate) fn scopes(&self) -> Vec<String> {
        self.app.cert().scopes().to_vec()
    }

    pub(crate) fn holds(&self, scope: &str) -> bool {
        self.app.cert().scopes().iter().any(|s| s == scope)
    }

    pub(crate) fn require_pay(&self) -> Result<()> {
        if self.holds("pay") {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("pay".into()).into())
        }
    }

    pub(crate) fn require_payee(&self) -> Result<()> {
        if acts_as_payee(self.app.cert().scopes()) {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("request or a labelled selling scope".into()).into())
        }
    }

    /// The certificate may have been renewed: the protocol follows it, once it passes
    /// Paykit's verifiers' checks as at load (Paykit wire 3.1). A renewed certificate they
    /// refuse, which the Switch took, is an error, and the protocol keeps the one before.
    pub(crate) fn refresh_own(&self) -> Result<()> {
        check_certificate(self.app.cert())?;
        self.protocol
            .set_own(self.app.identity(), self.app.cert().clone());
        Ok(())
    }

    /// Note a counterparty with Paykit activity, for re-sends after a `missed` item or a
    /// provider loss (Paykit spec 9, Switch spec 12.6).
    pub(crate) fn note_peer(&self, tx: &mut Tx, pubky: &[u8; 32]) -> Result<()> {
        tx.put(&key::peer(pubky), &self.now())?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------
    // Receiving and flushing (Paykit spec 9)

    /// Read the feed (Switch spec 8.2): each Paykit message's effect is committed with the
    /// cursor move, in the batch that carries it ([`crate::PaykitProtocol`]). Then the
    /// `resend_request`s received are answered and the re-sends the app needs are asked for
    /// (Paykit spec 9). `wait` is the long-poll wait in seconds.
    ///
    /// # Errors
    ///
    /// The client's errors. A message whose effect couldn't be written yet stays before the
    /// cursor, and the next call hands it over again.
    pub async fn receive(&mut self, wait: Option<u32>) -> Result<ReceiveSummary> {
        // An app holding `contacts` reads the flagged senders again first, at most every 10
        // minutes, for the spam rule (Paykit wire 4.1).
        self.refresh_lists_when_due().await?;
        let report = self.app.receive(wait).await?;
        self.refresh_own()?;
        // The payees' cancellations the read kept go into their records, as later writes of
        // this app's own (Paykit spec 8.1).
        self.place_kept_cancellations().await?;
        // Lists held over a `payment_lists` record that didn't read, once it reads.
        self.place_held_lists().await?;
        let applied = report
            .events
            .iter()
            .filter_map(|event| match event {
                AppEvent::MessageReceived { msg_id, .. } => Some(msg_id.clone()),
                _ => None,
            })
            .collect();
        let answered = self.answer_kept_resends().await?;
        let asked = self.send_asks().await?;
        Ok(ReceiveSummary {
            report,
            applied,
            resends: answered.saturating_add(asked),
        })
    }

    /// Try every queued message once (Switch spec 11).
    ///
    /// # Errors
    ///
    /// The client's errors.
    pub async fn flush(&mut self) -> Result<()> {
        self.app.flush_outbox().await?;
        Ok(())
    }
}
