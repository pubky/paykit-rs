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
use serde::{Deserialize, Serialize};

use crate::destination::KeptLookup;
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::JournalEntry;
use crate::protocol::PaykitProtocol;
use crate::rail::Rail;
use crate::state::{key, Tx};
use crate::timer::Timer;

/// The default `orphan_after` of an attempt: a day, the longest a payment method is expected
/// to take (Switch spec 10.2).
pub const DEFAULT_ORPHAN_AFTER_S: u64 = 86_400;

/// The default number of messages re-sent to one requester identity a day (Paykit spec 9).
pub const DEFAULT_RESEND_BUDGET: u64 = 500;

/// The integrator's choices for one Paykit App.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaykitConfig {
    /// The client's choices.
    pub app: AppConfig,
    /// Seconds after which an open attempt of this app is orphaned: the longest its payment
    /// method takes (Switch spec 10.2).
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

/// A Paykit App's backup: the client's [`AppBackup`] and the payment journal (Paykit spec 2,
/// 7.2). It holds the app's signing key: keep it as [`AppBackup`] says. Its `Debug` shows no
/// key.
#[derive(Clone, Serialize, Deserialize)]
pub struct PaykitBackup {
    app: AppBackup,
    journal: Vec<JournalEntry>,
}

impl std::fmt::Debug for PaykitBackup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PaykitBackup({:?}, {} payments)",
            self.app,
            self.journal.len()
        )
    }
}

impl PaykitBackup {
    /// The backup's bytes.
    ///
    /// # Errors
    ///
    /// A store failure if it can't be written.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|_| PaykitError::corrupt())
    }

    /// The backup's journal, checked as one (Paykit spec 7.2): every entry as
    /// [`JournalEntry::check`] checks it, each attempt once, and at most one open entry for
    /// one obligation, since a payment in progress blocks any other.
    fn check_journal(&self) -> Result<()> {
        let mut attempts = std::collections::BTreeSet::new();
        let mut open = std::collections::BTreeSet::new();
        for entry in &self.journal {
            entry.check()?;
            if !attempts.insert(entry.attempt_id.clone())
                || (entry.open() && !open.insert(entry.obligation))
            {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }

    /// Read a backup's bytes.
    ///
    /// # Errors
    ///
    /// A store failure for bytes that don't read as a backup.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|_| PaykitError::corrupt())
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
    /// The client's errors; [`PaykitError::Lib`] for a certificate Paykit refuses.
    pub async fn join(
        env: AppEnv,
        config: PaykitConfig,
        request: JoinRequest,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        let protocol = Self::new_protocol();
        let app = App::join(env.clone(), protocol.clone(), config.app.clone(), request).await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// Load a Paykit App from the store alone: nothing is sent. Payments a crash left in the
    /// journal resume at [`Paykit::resume_payments`].
    ///
    /// # Errors
    ///
    /// The client's errors.
    pub async fn load(
        env: AppEnv,
        config: PaykitConfig,
        app_id: [u8; 16],
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        let protocol = Self::new_protocol();
        let app = App::load(env.clone(), protocol.clone(), config.app.clone(), app_id).await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// Restore from a backup (Switch spec 12.4): the journal checked whole first, then the
    /// client's restore, then the journal, its entries marked restored, so their open attempts are resolved as a restore requires
    /// (Paykit spec 7.2). The app is in reconcile mode until [`Paykit::reconcile`] and
    /// [`Paykit::end_reconcile`].
    ///
    /// # Errors
    ///
    /// A store failure (`Corrupt`) for a journal with a malformed or repeated entry, before
    /// anything is restored; the client's errors.
    pub async fn restore(
        env: AppEnv,
        config: PaykitConfig,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        // The journal is checked whole before anything is restored: a backup with a damaged
        // or repeated entry is refused, never taken in part.
        backup.check_journal()?;
        let app = App::restore(
            env.clone(),
            Self::new_protocol(),
            config.app.clone(),
            &backup.app,
        )
        .await?;
        let app_id = app.app_id();
        drop(app);
        let protocol = Self::new_protocol();
        let mut app = App::load(env.clone(), protocol.clone(), config.app.clone(), app_id).await?;
        let mut tx = Tx::default();
        for entry in &backup.journal {
            let mut entry = entry.clone();
            entry.restored = true;
            tx.put(&key::journal(&entry.attempt()?), &entry)?;
        }
        app.protocol_commit(&tx.into_writes()).await?;
        Self::assemble(app, env, protocol, config, rail, timer)
    }

    /// The app's backup: the client's and the journal (Paykit spec 7.2).
    ///
    /// # Errors
    ///
    /// The client's errors; a store failure.
    pub async fn backup(&self) -> Result<PaykitBackup> {
        let journal = self
            .local_scan::<JournalEntry>(key::JOURNAL)
            .await?
            .into_iter()
            .map(|(_, entry)| entry)
            .collect();
        Ok(PaykitBackup {
            app: self.app.backup()?,
            journal,
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

    /// The certificate may have been renewed: the protocol follows it.
    pub(crate) fn refresh_own(&self) {
        self.protocol
            .set_own(self.app.identity(), self.app.cert().clone());
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
        let report = self.app.receive(wait).await?;
        self.refresh_own();
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
