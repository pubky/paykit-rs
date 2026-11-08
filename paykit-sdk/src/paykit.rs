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

use std::sync::Arc;

use paykit_lib::scopes::{acts_as_payee, check_certificate};
use paykit_lib::{Decimal, Label};
use pubky_switch_client::app::{
    App, AppBackup, AppConfig, AppEnv, HomeserverReader, JoinRequest, ReceiveReport,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_client::store::client_namespace;
use pubky_switch_core::ident::NamingKey;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::journal::JournalEntry;
use crate::protocol::PaykitProtocol;
use crate::rail::Rail;
use crate::state::{key, Local, Tx};

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
    /// Labels of the identity's labelled selling scopes whose records an app holding plain
    /// `request` follows (Paykit spec 3.1).
    pub known_labels: Vec<Label>,
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
            known_labels: Vec::new(),
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
    /// The `msg_id`s whose effects were written.
    pub applied: Vec<String>,
    /// The `resend_request`s answered and asked.
    pub resends: u64,
}

/// One Paykit App of an identity.
pub struct Paykit {
    pub(crate) app: App,
    pub(crate) protocol: Arc<PaykitProtocol>,
    pub(crate) rail: Arc<dyn Rail>,
    pub(crate) local: Local,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) homeserver: Arc<dyn HomeserverReader>,
    pub(crate) config: PaykitConfig,
}

impl std::fmt::Debug for Paykit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Paykit({:?})", self.app)
    }
}

impl Paykit {
    fn parts(
        env: &AppEnv,
        config: &PaykitConfig,
        app_id: &[u8; 16],
    ) -> (Arc<PaykitProtocol>, Local) {
        let ns = client_namespace(app_id);
        let protocol = Arc::new(PaykitProtocol::new(
            Arc::clone(env.store.store()),
            ns.clone(),
            config.known_labels.clone(),
        ));
        (protocol, Local::new(env.store.clone(), ns))
    }

    fn assemble(
        app: App,
        env: AppEnv,
        protocol: Arc<PaykitProtocol>,
        local: Local,
        config: PaykitConfig,
        rail: Arc<dyn Rail>,
    ) -> Result<Self> {
        // Paykit's verifiers check the own certificate as they check every other (Paykit
        // wire 3.1): its scope rules, and permissions exactly as its scopes derive them.
        check_certificate(app.cert())?;
        protocol.set_own(app.identity(), app.cert().clone());
        Ok(Self {
            app,
            protocol,
            rail,
            local,
            clock: env.clock,
            homeserver: env.homeserver,
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
    ) -> Result<Self> {
        let app_id = request.keys.app_id_bytes();
        let (protocol, local) = Self::parts(&env, &config, &app_id);
        let app = Box::pin(App::join(
            env.clone(),
            protocol.clone(),
            config.app.clone(),
            request,
        ))
        .await?;
        Self::assemble(app, env, protocol, local, config, rail)
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
    ) -> Result<Self> {
        let (protocol, local) = Self::parts(&env, &config, &app_id);
        let app = App::load(env.clone(), protocol.clone(), config.app.clone(), app_id).await?;
        Self::assemble(app, env, protocol, local, config, rail)
    }

    /// Restore from a backup (Switch spec 12.4): the client's restore, then the journal, its
    /// entries marked restored, so their open attempts are resolved as a restore requires
    /// (Paykit spec 7.2). The app is in reconcile mode until [`Paykit::reconcile`] and
    /// [`Paykit::end_reconcile`].
    ///
    /// # Errors
    ///
    /// The client's errors.
    pub async fn restore(
        env: AppEnv,
        config: PaykitConfig,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
    ) -> Result<Self> {
        let app = App::restore(
            env.clone(),
            Arc::new(PaykitProtocol::new(
                Arc::clone(env.store.store()),
                String::new(),
                Vec::new(),
            )),
            config.app.clone(),
            &backup.app,
        )
        .await?;
        let app_id = app.app_id();
        drop(app);
        let (protocol, local) = Self::parts(&env, &config, &app_id);
        let app = App::load(env.clone(), protocol.clone(), config.app.clone(), app_id).await?;
        let mut tx = Tx::default();
        for entry in &backup.journal {
            let mut entry = entry.clone();
            entry.restored = true;
            tx.put(&key::journal(&entry.attempt()?), &entry)?;
        }
        local.commit(tx).await?;
        Self::assemble(app, env, protocol, local, config, rail)
    }

    /// The app's backup: the client's and the journal (Paykit spec 7.2).
    ///
    /// # Errors
    ///
    /// The client's errors; a store failure.
    pub async fn backup(&self) -> Result<PaykitBackup> {
        let journal = self
            .local
            .scan::<JournalEntry>(key::JOURNAL)
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

    pub(crate) fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    pub(crate) fn naming(&self) -> &NamingKey {
        self.app.naming_key()
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

    /// Read the feed (Switch spec 8.2) and write the effects of the Paykit messages it
    /// received, then answer and ask for re-sends and write the records of requests this
    /// app sent (Paykit spec 6, 9). `wait` is the long-poll wait in seconds.
    ///
    /// # Errors
    ///
    /// The client's errors. A message whose effect couldn't be written stays queued, and the
    /// next call writes it.
    pub async fn receive(&mut self, wait: Option<u32>) -> Result<ReceiveSummary> {
        let report = Box::pin(self.app.receive(wait)).await?;
        self.refresh_own();
        let applied = Box::pin(self.apply_effects()).await?;
        let resends = self.send_asks().await?;
        self.write_request_records().await?;
        self.send_owed_replies().await?;
        Ok(ReceiveSummary {
            report,
            applied,
            resends,
        })
    }

    /// Try every queued message once, and write the records of requests this app sent that
    /// were accepted since (Switch spec 11).
    ///
    /// # Errors
    ///
    /// The client's errors.
    pub async fn flush(&mut self) -> Result<()> {
        Box::pin(self.app.flush_outbox()).await?;
        self.write_request_records().await?;
        self.send_owed_replies().await?;
        Ok(())
    }
}
