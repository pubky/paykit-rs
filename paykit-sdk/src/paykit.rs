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
//! Recurring requests' Billing Periods have theirs (`periods.rs`); Allowances and publishing
//! Private Payment Lists add their own the same way.

use std::collections::HashMap;
use std::sync::Arc;

use paykit_lib::derive::request_obligation_blinded;
use paykit_lib::scopes::{acts_as_payee, check_certificate};
use paykit_lib::Label;
use pubky_switch_client::app::{
    App, AppBackup, AppConfig, AppEnv, AppError, AppEvent, HeldCursor, HomeserverReader,
    IdentityRefresh, JoinRequest, ProtocolSet, ReceiveReport,
};
use pubky_switch_client::clock::Clock;
use pubky_switch_core::records::{AppCert, AppRecordBody, Approval};
use pubky_switch_core::scopes::owns_root;

use crate::destination::KeptLookup;
use crate::error::{Refusal, Result};
use crate::hold::OwnResendOwed;
use crate::own_record::{capabilities_for, check_join_capabilities, holds_capabilities};
use crate::protocol::PaykitProtocol;
use crate::rail::{Rail, RailDeclaration};
use crate::state::{key, Tx};
use crate::timer::Timer;

/// The longest a first dispatch may take from its attempt's answer to the payment method's
/// call: 10 minutes. Between them come the destination and identity checks, the
/// acceptance's journaling, the final payment check and the journal write of `Paying`: a few
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

/// Refuse a `config` whose `orphan_after_s` is set below the floor its rail needs.
pub(crate) fn check_orphan_window(config: &PaykitConfig, rail: &dyn Rail) -> Result<()> {
    let floor = orphan_floor_s(&rail.declaration());
    match config.orphan_after_s {
        Some(set) if set < floor => Err(paykit_lib::Error::Validation(format!(
            "Switch spec 10.2: orphan_after_s {set} is below the rail's floor of {floor} s, its \
             longest completion plus the pre-dispatch budget"
        ))
        .into()),
        _ => Ok(()),
    }
}

/// The default number of messages re-sent to one requester identity a day (Paykit spec 9).
pub const DEFAULT_RESEND_BUDGET: u64 = 500;

/// The integrator's choices for one Paykit App.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PaykitConfig {
    /// The client's choices when Paykit opens the app. An attached app has its own choices.
    pub app: AppConfig,
    /// Seconds after which an open attempt of this app is orphaned (Switch spec 10.2).
    /// `None`, the default, is [`orphan_floor_s`] for the app's rail: its declared longest
    /// time to complete or fail plus [`PRE_DISPATCH_BUDGET_MS`], so a default config suits
    /// any rail. A value set below that floor is refused when opening the app and on
    /// every attempt. An idempotent dispatch is repeated after a crash only while the rail's
    /// longest time still fits before the orphan age (Paykit spec 7.2, "Dispatch window"):
    /// at the floor that leaves only the pre-dispatch budget, less the clock tolerance, so
    /// an app that wants later repeats sets a larger value.
    pub orphan_after_s: Option<u64>,
    /// The label a payee app sends its requests under (Paykit spec 3.1, 8.1). An app holding
    /// only labelled selling scopes needs one unless it holds exactly one label.
    pub selling_label: Option<Label>,
    /// How many messages the app re-sends to one requester identity a day (Paykit spec 9).
    pub resend_budget: u64,
    /// The extension kinds the host's extension handles (Paykit spec 3.3): the record
    /// accepts each one whose prefix the certificate holds, and the receive step keeps them
    /// for the host ([`Paykit::extension_messages`]).
    pub extension_kinds: Vec<paykit_lib::kinds::ExtensionKind>,
}

impl PaykitConfig {
    /// The client's choices.
    #[must_use]
    pub fn with_app(mut self, app: AppConfig) -> Self {
        self.app = app;
        self
    }

    /// Seconds after which an open attempt of this app is orphaned (Switch spec 10.2). When
    /// not set it is [`orphan_floor_s`] for the app's rail: its declared longest time to
    /// complete or fail plus [`PRE_DISPATCH_BUDGET_MS`], so a default config suits any rail.
    /// A value set below that floor is refused by `join`, `load`, `restore` and every
    /// attempt. An idempotent dispatch is repeated after a crash only while the rail's
    /// longest time still fits before the orphan age (Paykit spec 7.2, "Dispatch window"):
    /// at the floor that leaves only the pre-dispatch budget, less the clock tolerance, so
    /// an app that wants later repeats sets a larger value.
    #[must_use]
    pub fn with_orphan_after_s(mut self, orphan_after_s: u64) -> Self {
        self.orphan_after_s = Some(orphan_after_s);
        self
    }

    /// The label a payee app sends its requests under (Paykit spec 3.1, 8.1). An app holding
    /// only labelled selling scopes needs one unless it holds exactly one label.
    #[must_use]
    pub fn with_selling_label(mut self, selling_label: Label) -> Self {
        self.selling_label = Some(selling_label);
        self
    }

    /// How many messages the app re-sends to one requester identity a day (Paykit spec 9).
    #[must_use]
    pub fn with_resend_budget(mut self, resend_budget: u64) -> Self {
        self.resend_budget = resend_budget;
        self
    }

    /// The extension kinds the host's extension handles (Paykit spec 3.3, Paykit wire 4.3),
    /// replacing any set before. A kind whose `ext:<prefix>` the certificate doesn't hold is
    /// neither accepted nor kept.
    #[must_use]
    pub fn with_extension_kinds(mut self, kinds: Vec<paykit_lib::kinds::ExtensionKind>) -> Self {
        self.extension_kinds = kinds;
        self
    }

    /// Build the Paykit protocol to register when the host opens its own Switch app
    /// for [`Paykit::attach`]. It handles this wallet's registered extension kinds.
    pub fn protocol(&self, rail: Arc<dyn Rail>) -> Arc<PaykitProtocol> {
        Arc::new(PaykitProtocol::with_rail(rail).with_extension_kinds(self.extension_kinds.clone()))
    }

    /// The `orphan_after` the app's attempts carry for a rail that declares `declaration`:
    /// the value set, or else the rail's floor ([`orphan_floor_s`]).
    pub fn orphan_after_s_for(&self, declaration: &RailDeclaration) -> u64 {
        self.orphan_after_s
            .unwrap_or_else(|| orphan_floor_s(declaration))
    }
}

impl Default for PaykitConfig {
    fn default() -> Self {
        Self {
            app: AppConfig::default(),
            orphan_after_s: None,
            selling_label: None,
            resend_budget: DEFAULT_RESEND_BUDGET,
            extension_kinds: Vec::new(),
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

/// The client's completed read, before the SDK's outgoing recovery work.
pub(crate) struct ReadResult {
    report: ReceiveReport,
    paused_before: std::collections::BTreeSet<[u8; 32]>,
    keys_held_before: bool,
    blocks_after_read: bool,
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
#[non_exhaustive]
pub struct ReceiveSummary {
    /// The client's report of the feed read. Once the receive has returned, the host may
    /// acknowledge its duty events for Paykit up to the last one the report shows, and
    /// app-wide only once every other protocol each is for has handled it
    /// ([`Paykit::receive`]).
    /// A received message's entry receipt identifies any invitation redeemed for that
    /// delivery. This service observation does not authorize a payment or replace the
    /// signed request and its seller-scope checks.
    ///
    /// The read is bounded. `report.more` says it stopped on one of its per-call budgets,
    /// such as its feed pages or the kept batches it sends again, with more to do now;
    /// nothing it left is lost or reordered. The host handles the summary's events, then,
    /// while `report.more` is set and `report.stopped` is not, calls [`Paykit::receive`]
    /// again without waiting. With `report.stopped` set it inspects the reported event
    /// instead: reading again does nothing until that is dealt with.
    pub report: ReceiveReport,
    /// The `msg_id`s of the messages received, each with its effect committed with the
    /// cursor move (Switch spec 8.2).
    pub applied: Vec<String>,
    /// The `resend_request`s answered and asked.
    pub resends: u64,
    /// A recovery ask waits because its sender certificate lacks `paykit_contacts`.
    /// The host can offer to request the scope from the signer.
    pub resend_refused: Option<Refusal>,
    /// The app's certificate fails Paykit's checks, as [`Paykit::certificate_refused`]
    /// gives it: the read went on, holding Paykit's entries unprocessed, and none of the
    /// Paykit steps after it ran.
    pub certificate_refused: Option<Refusal>,
    /// The wallet's required listing under a new certificate or after a missed feed is
    /// incomplete. The reason indicates when to call `receive` again to continue a
    /// bounded listing or why it failed. Read-only activity views wait for completion.
    pub records_unlisted: Option<String>,
    /// A certificate taken is another than the one the records were last listed under, and
    /// the app waits for its scope keys, which the signer stores when it is next
    /// online (Switch spec 3.5). The client asks the signer for a full bundle once, and asks
    /// again only after a later bundle part leaves a key still missing, or after a restore;
    /// it doesn't ask at every read. Until the keys come the records the certificate adds
    /// aren't listed, and the calls that act on them are refused as not now; the host tells
    /// the user the signer must come online. Set too while an entry waits for the key of a
    /// data epoch its record's value is sealed at ([`ReceiveSummary::key_epochs_awaited`]).
    pub scope_keys_awaited: bool,
    /// The data epochs whose scope keys entries stored by the client wait for, ascending, as
    /// the read's `AppEvent::AwaitingKeys` names them: such an entry's record holds a value
    /// sealed at that epoch, which the app can't open yet, or its write needs the current
    /// epoch's key. The client stores each such entry and passes it at once, and runs its
    /// step again once the key comes (Switch spec 8.2). One it drops past its bounds first,
    /// 21 days from its acceptance on the service time verified, was never processed: the
    /// host is told once ([`crate::NoticeKind::MessageGivenUp`]), and its sender is asked to
    /// send it again once the app holds every key such messages waited for (Paykit spec 8.1).
    pub key_epochs_awaited: Vec<u64>,
    /// The account is gone at this switch: the client took it as erased (Switch spec 12.7),
    /// a provider loss (Switch spec 12.6). The host tells the user. The app reads nothing
    /// more there, and the SDK acts on nothing by itself: lookups of the identity there fail
    /// closed (Switch spec 4.3), and the payment journal stays, for the recovery at a new
    /// switch to resume from. Set from the read that takes the account as gone until the
    /// host acknowledges its `AppEvent::AccountGone`, or until the account shows again.
    pub account_gone: bool,
    /// The records this receive paused: a view showed them divergent after a provider loss
    /// (Paykit spec 6). The host shows each to the user, who confirms it
    /// ([`Paykit::confirm_divergent`]); until then nothing is paid, accepted or credited on
    /// it automatically, nor paid or accepted by hand. That confirmation lets payments and
    /// writes go on, but releases no credit (Paykit spec 8.1 step 4): these records are
    /// shown with no [`crate::DivergentRecord::may_hold_credit`]. To credit a proof, the host
    /// calls [`Paykit::confirm_payment`] again; while a record of that payer may hold a
    /// credit, it is refused [`crate::Refusal::Divergent`], each record it names shown with
    /// `may_hold_credit`. Once the user has seen that, the host confirms each of those
    /// records exactly as that refusal gives them, then calls `confirm_payment` once more;
    /// while messages kept over one of them wait to be written
    /// ([`crate::DivergentRecord::writes_waiting`]), it is refused again until a receive
    /// writes them: confirming again writes nothing, so the host runs [`Paykit::receive`]
    /// until no record the refusal names shows `writes_waiting`, and only then calls again.
    /// Every record paused, whenever, is in [`Paykit::divergent_records`].
    pub divergent: Vec<crate::DivergentRecord>,
    /// The app record couldn't be brought up to date at the latest receive that tried (Paykit
    /// spec 4), and the receive went on: why, as fixed text to show. The record stays as last
    /// signed, or its change stays owed by the client, and is tried again at the next receive,
    /// or at once by [`Paykit::update_own_record`], which returns its outcome. The receive goes
    /// on past such a failure only when every later step would take the same failure as one
    /// to try again later; any other failure ends the receive with its error.
    pub app_record_owed: Option<String>,
    /// Whether this receive tried the app record's upkeep: a summary of two reads keeps the
    /// earlier read's `app_record_owed` when the later one didn't.
    upkeep_tried: bool,
}

impl ReceiveSummary {
    /// The summary of two receives, `earlier` and then `self`, as one: the events, messages,
    /// re-sends and records paused of both, in order; the worse held cursor (ahead over
    /// unknown over clean); the data epochs either awaited, ascending and unique; an app
    /// record owed from the later read that tried its upkeep. Current state, including the
    /// cursor and certificate status, comes from the later read.
    fn after(mut self, earlier: Self) -> Self {
        let report = &mut self.report;
        report.events.splice(0..0, earlier.report.events);
        report.held_cursor = match (earlier.report.held_cursor, report.held_cursor) {
            (Some(ahead @ HeldCursor::Ahead { .. }), _)
            | (_, Some(ahead @ HeldCursor::Ahead { .. })) => Some(ahead),
            (Some(HeldCursor::Unknown), _) | (_, Some(HeldCursor::Unknown)) => {
                Some(HeldCursor::Unknown)
            }
            (earlier, later) => later.or(earlier),
        };
        self.applied.splice(0..0, earlier.applied);
        self.resends = self.resends.saturating_add(earlier.resends);
        self.divergent.splice(0..0, earlier.divergent);
        self.key_epochs_awaited.extend(earlier.key_epochs_awaited);
        self.key_epochs_awaited.sort_unstable();
        self.key_epochs_awaited.dedup();
        self.scope_keys_awaited |= !self.key_epochs_awaited.is_empty();
        if !self.upkeep_tried {
            self.app_record_owed = earlier.app_record_owed;
            self.upkeep_tried = earlier.upkeep_tried;
        }
        self
    }
}

/// The data epochs `events` name as waited for (`AppEvent::AwaitingKeys`), ascending, each
/// once.
fn key_epochs_awaited(events: &[AppEvent]) -> Vec<u64> {
    let mut epochs: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            AppEvent::AwaitingKeys { epochs, .. } => Some(epochs.iter().copied()),
            _ => None,
        })
        .flatten()
        .collect();
    epochs.sort_unstable();
    epochs.dedup();
    epochs
}

/// Whether `events` hold the client's `AccountGone` (Switch spec 12.7).
fn account_gone(events: &[AppEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, AppEvent::AccountGone { .. }))
}

/// One Paykit App of an identity.
///
/// The convenience constructors register Paykit alone; the `*_with_protocols` constructors
/// let the host register its other protocols under the same App ID (Paykit spec 3.2).
/// Each own scope root needs its registered owner (Switch wire 3.1). Other protocols'
/// scopes grant no Paykit rights, whether in the own or a counterparty's certificate.
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
    /// The last payable public records and the source observations covering their reads.
    pub(crate) last_public_read: Option<crate::app_records::PublicRead>,
    pub(crate) config: PaykitConfig,
    /// Whether this load made the restore's identity-confirmed first read.
    pub(crate) first_read_done: bool,
    /// A read made before a public write, to include in the host's next receive summary.
    pub(crate) first_read_summary: Option<Box<ReceiveSummary>>,
    /// Whether a write of the app's own record ended with the store unable to tell whether
    /// it was applied: the record this handle holds may not be the stored one, so no
    /// public endpoint edit builds on it until the app is loaded again
    /// (`crate::own_record`).
    pub(crate) own_record_unknown: bool,
    /// The capabilities the join checked, which its first update signs as they are
    /// (`crate::own_record`); `None` otherwise.
    pub(crate) capability_snapshot: Option<Vec<paykit_lib::PaymentEndpointIdentifier>>,
    /// The identity-wide notices this load has told the host of (`crate::notices`): each
    /// once per load while what it tells stands.
    pub(crate) identity_told: Vec<crate::NoticeKind>,
    /// Whether a floor raise this app owes was tried and left unwritten
    /// ([`Paykit::settle_raise_waiting`]).
    pub(crate) raise_unwritten: bool,
    /// Whether the lists were read since this handle's last receive or flush began: a raise
    /// owed asks for no second read before the next one ([`Paykit::raise_owed`]).
    pub(crate) lists_read_this_pass: bool,
    /// Whether the read of the lists in progress already left a raise unwritten: one read
    /// counts one try, however many raises it leaves unwritten ([`crate::floor::RaiseRetry`]).
    pub(crate) raise_tried_in_read: bool,
    /// The latest device time this handle's retry check saw since its last unwritten try, so
    /// a clock put back, part of the way included, ends a wait ([`crate::floor::RaiseRetry`]).
    pub(crate) raise_clock_seen: std::sync::atomic::AtomicU64,
    /// When this handle last left a raise unwritten, on its monotonic clock: none since it
    /// started means every wait is over, the time waited not being known; with it, a clock set
    /// back between checks can't hide time ([`crate::floor::RaiseRetry`]).
    pub(crate) raise_tried_at: std::sync::Mutex<Option<std::time::Instant>>,
    /// How many activity records this handle bound ([`Paykit::activity_binds`]).
    #[cfg(feature = "test-support")]
    pub(crate) activity_binds: std::sync::atomic::AtomicU64,
    /// The certificates this handle verified (`crate::records::Certificates`): empty when it
    /// is built, at a load, a join or a restore, so each certificate is verified in full the
    /// first time this handle reads it.
    pub(crate) certificates: std::sync::Mutex<crate::records::Certificates>,
}

impl std::fmt::Debug for Paykit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Paykit({:?})", self.app)
    }
}

/// The app's own certificate by Paykit's checks (Paykit wire 3.1): its scope rules, and
/// permissions exactly as its scopes derive them.
pub(crate) fn check_own_certificate(cert: &AppCert) -> Result<()> {
    check_certificate(cert).map_err(|error| Refusal::CertificateRefused(error.to_string()).into())
}

impl Paykit {
    fn protocols<F>(
        rail: &Arc<dyn Rail>,
        config: &PaykitConfig,
        register: F,
    ) -> Result<(Arc<PaykitProtocol>, ProtocolSet)>
    where
        F: FnOnce(Arc<PaykitProtocol>) -> std::result::Result<ProtocolSet, AppError>,
    {
        let protocol = config.protocol(Arc::clone(rail));
        protocol.set_publishes_record(config.app.publishes_record);
        let protocols = register(Arc::clone(&protocol))?;
        Self::check_protocol(&protocol, &protocols)?;
        Ok((protocol, protocols))
    }

    fn check_protocol(protocol: &Arc<PaykitProtocol>, protocols: &ProtocolSet) -> Result<()> {
        let expected = Arc::clone(protocol) as Arc<dyn pubky_switch_client::app::Protocol>;
        if !protocols
            .get("paykit")
            .is_some_and(|p| Arc::ptr_eq(p, &expected))
        {
            return Err(AppError::Refused(pubky_switch_client::app::Refusal::Usage(
                "the protocol set must retain the supplied Paykit protocol".into(),
            ))
            .into());
        }
        Ok(())
    }

    fn assemble(
        app: App,
        protocol: Arc<PaykitProtocol>,
        config: PaykitConfig,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Self {
        Self {
            clock: app.clock(),
            homeserver: app.homeserver_reader(),
            app,
            protocol,
            rail,
            timer,
            lookups: HashMap::new(),
            last_public_read: None,
            config,
            first_read_done: false,
            first_read_summary: None,
            own_record_unknown: false,
            capability_snapshot: None,
            identity_told: Vec::new(),
            raise_unwritten: false,
            lists_read_this_pass: false,
            raise_tried_in_read: false,
            raise_clock_seen: std::sync::atomic::AtomicU64::new(0),
            raise_tried_at: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            activity_binds: std::sync::atomic::AtomicU64::new(0),
            certificates: std::sync::Mutex::default(),
        }
    }

    /// How many activity records this handle has bound since it was built: each bind checks
    /// every stored message the record holds (Paykit wire 5.3). For tests that guard how
    /// often a read binds a record.
    #[cfg(feature = "test-support")]
    pub fn activity_binds(&self) -> u64 {
        self.activity_binds
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Move this handle's last unwritten floor raise try `ms` back on its monotonic clock, as
    /// if that much had passed since it without a receive. For tests of the retry wait, whose
    /// monotonic count no test clock drives.
    #[cfg(feature = "test-support")]
    pub fn age_raise_try(&self, ms: u64) {
        if let Ok(mut at) = self.raise_tried_at.lock() {
            *at = at.and_then(|at| at.checked_sub(std::time::Duration::from_millis(ms)));
        }
    }

    /// Join from an approval (Switch spec 12.1), as a Paykit App: the client's join, with
    /// Paykit as its protocol. The approval's certificate must pass Paykit's checks (Paykit
    /// wire 3.1). Session creation, key storage and the approval's channel stay the host's.
    /// This convenience constructor registers only Paykit. Use [`Paykit::join_with_protocols`]
    /// when the own certificate also holds another protocol's scopes (Paykit spec 3.2).
    ///
    /// The join then brings the app's record to its capabilities ([`crate::own_record`]),
    /// as each receive does.
    ///
    /// # Errors
    ///
    /// Before anything is written: [`Refusal::CertificateRefused`] for a certificate Paykit
    /// refuses; `Validation` for an `orphan_after_s` below the rail's floor
    /// ([`orphan_floor_s`]); the client's `Usage` refusal for a capabilities list
    /// ([`crate::Rail::endpoint_identifiers`]) the app's record can't carry. Otherwise the
    /// client's join errors, as [`pubky_switch_client::app::App::join`] gives them. The
    /// client may have committed state before an error finishing its join; when state was
    /// kept, [`Paykit::load`] followed by a receive resumes the pending work.
    ///
    /// Once the client's join has returned the app, nothing else fails the join: the app is
    /// joined and kept, and its handle is returned. The record update that follows remains
    /// owed if it fails. After a delivery error the client keeps the new record and sends
    /// it again; after a definite failure before the client kept it, the held record still
    /// lacks the capabilities, so the next receive or [`Paykit::update_own_record`] retries.
    /// If the store cannot tell whether its write applied, own-record edits return
    /// [`Refusal::NotNow`] and upkeep waits until [`Paykit::load`] reloads the stored state.
    /// A receive or update can then retry the pending work.
    pub async fn join(
        env: AppEnv,
        config: PaykitConfig,
        request: JoinRequest,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        Self::join_with_protocols(env, config, request, rail, timer, |p| Ok(p.into())).await
    }

    /// Join with the host's complete protocol set (Paykit spec 3.2; Switch spec 3.3).
    /// `register` receives the Paykit implementation configured for this rail and must
    /// include that same instance in its returned set, alongside the host's protocols.
    /// The set routes approval keys and callbacks to their owners. Registration, the own
    /// certificate and the capabilities list are checked before any state is written.
    ///
    /// # Errors
    ///
    /// As [`Paykit::join`], plus the host's registration error or the client's `Usage`
    /// refusal if the set omits or replaces the supplied Paykit implementation.
    pub async fn join_with_protocols<F>(
        env: AppEnv,
        config: PaykitConfig,
        request: JoinRequest,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
        register: F,
    ) -> Result<Self>
    where
        F: FnOnce(Arc<PaykitProtocol>) -> std::result::Result<ProtocolSet, AppError>,
    {
        // Keep the nested client future on the heap, as at `receive`.
        Box::pin(async move {
            check_orphan_window(&config, rail.as_ref())?;
            let (protocol, protocols) = Self::protocols(&rail, &config, register)?;
            // The certificate passes Paykit's checks before the client writes anything: the
            // client's join reads the approval with this same function, on these same inputs,
            // before its first commit, so this is the certificate it would keep. An approval
            // that doesn't read is left to the client's join, which refuses it, writing nothing.
            let mut snapshot = None;
            if let Ok(approved) = Approval::read(
                &request.approval,
                &request.request,
                &request.identity,
                &request.keys.bundle_secret(),
                &|scope| {
                    let root = scope.split_once(':').map_or(scope, |(root, _)| root);
                    protocols
                        .names()
                        .iter()
                        .find(|name| owns_root(name, root))
                        .and_then(|name| protocols.get(name))
                        .is_some_and(|owner| owner.has_keys(scope))
                },
            ) {
                check_own_certificate(&approved.cert)?;
                // The record the client's join signs, with the capabilities the update below
                // adds: a list it can't carry refuses the join here, before anything is
                // written. Its `accepts` is the client's: each registered protocol's kinds for
                // the certificate, in registration order, each kept only when that protocol
                // owns the kind's root, its prefix before the first `.` (Switch wire 3.1,
                // 5.1), once. The client's set computes it privately, so its rule is applied
                // here with the same public ownership check.
                let mut accepts: Vec<String> = Vec::new();
                for name in protocols.names() {
                    let Some(owner) = protocols.get(&name) else {
                        continue;
                    };
                    for kind in owner.accepts(&approved.cert) {
                        let root = kind.split_once('.').map_or(kind.as_str(), |(root, _)| root);
                        if owns_root(&name, root) && !accepts.contains(&kind) {
                            accepts.push(kind);
                        }
                    }
                }
                // Wire timestamps have fixed width; the certificate supplies a checked one.
                let base = AppRecordBody::new(&approved.cert, accepts, approved.cert.issued_at());
                let pays = holds_capabilities(&approved.cert);
                // One reading of what the payment method pays, checked here and signed by the
                // update after the join: the method promises no two readings agree.
                let ids = capabilities_for(rail.as_ref(), pays);
                if pays {
                    check_join_capabilities(base, &ids, approved.identity.generation())?;
                }
                snapshot = Some(ids);
            }
            let app = App::join(env, protocols, config.app.clone(), request).await?;
            // No certificate is kept as listed under: the copy is empty until a listing has run
            // with the keys held, the client's at its first read or one of Paykit's, so until then
            // the records count as unlisted (Paykit spec 8.1, "Records not listed yet";
            // `Paykit::unlisted`) and a call that reads them lists first or is refused.
            protocol.set_own(app.identity(), app.cert().clone());
            let mut paykit = Self::assemble(app, protocol, config, rail, timer);
            // The app is joined and kept: an error here would hide it from the host, which
            // could join again or lose it. A failed update stays owed instead, for the next
            // receive or `update_own_record`, after a reload if the store result was unknown
            // (the Errors section above), and nothing is marked done for it. The update
            // signs the capabilities checked above; later upkeep reads the payment method again.
            paykit.capability_snapshot = snapshot;
            let _owed = Box::pin(paykit.tend_own_record()).await;
            paykit.capability_snapshot = None;
            // The host hears at once of what holds the identity for good, as at a load; one that
            // can't be told now is told at the first receive's end.
            let _told = paykit.tell_identity_notices().await;
            Ok(paykit)
        })
        .await
    }

    /// Load a Paykit App from the store alone: nothing is sent. Payments a crash left in the
    /// journal resume at [`Paykit::resume_payments`]. A stored certificate Paykit refuses,
    /// a renewal the Switch took, still loads: the app reads its feed, to take a renewal
    /// that passes, and takes no Paykit action meanwhile ([`Paykit::certificate_refused`]).
    ///
    /// This convenience constructor registers only Paykit. Use [`Paykit::load_with_protocols`]
    /// for an app joined with other protocols (Paykit spec 3.2).
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
        Self::load_with_protocols(env, config, app_id, rail, timer, |p| Ok(p.into())).await
    }

    /// Load with every protocol the app joined with (Paykit spec 3.2; Switch spec 3.3).
    /// As at [`Paykit::join_with_protocols`], `register` must retain the supplied Paykit
    /// instance. Missing or extra registered owners are refused before state is written.
    ///
    /// # Errors
    ///
    /// As [`Paykit::load`], plus the host's registration error or the client's `Usage`
    /// refusal if the set omits or replaces the supplied Paykit implementation.
    pub async fn load_with_protocols<F>(
        env: AppEnv,
        config: PaykitConfig,
        app_id: [u8; 16],
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
        register: F,
    ) -> Result<Self>
    where
        F: FnOnce(Arc<PaykitProtocol>) -> std::result::Result<ProtocolSet, AppError>,
    {
        // Keep the nested client future on the heap, as at `receive`.
        Box::pin(async move {
            check_orphan_window(&config, rail.as_ref())?;
            let (protocol, protocols) = Self::protocols(&rail, &config, register)?;
            let app = App::load(env, protocols, config.app.clone(), app_id).await?;
            protocol.set_own(app.identity(), app.cert().clone());
            Self::assemble(app, protocol, config, rail, timer)
                .finish_load()
                .await
        })
        .await
    }

    /// Take ownership of a host-built Switch app immediately after [`App::join`] or
    /// [`App::load`]. Register a fresh [`PaykitConfig::protocol`] for that app and pass
    /// the same instance here. The app supplies its clock, Homeserver reader and record
    /// publishing setting. The host supplies Paykit's other settings and its timer.
    ///
    /// Use [`Paykit::restore`] or [`Paykit::restore_with_protocols`] to open a backup so
    /// Paykit checks its layer before the first write. To reopen a store after an SDK
    /// restore that is still in reconciliation, use [`Paykit::load_with_protocols`].
    ///
    /// # Errors
    ///
    /// The client's `Usage` refusal if the protocol differs from the registered one,
    /// has different extension kinds, has no rail, already has an owner, or the app is in
    /// reconciliation or has an unfinished restore. As with [`Paykit::load`], returns an
    /// error for an invalid orphan window or a store failure.
    pub async fn attach(
        app: App,
        config: PaykitConfig,
        protocol: Arc<PaykitProtocol>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        Box::pin(async move {
            Self::check_protocol(&protocol, app.protocols())?;
            if !protocol.has_extension_kinds(&config.extension_kinds) {
                return Err(AppError::Refused(pubky_switch_client::app::Refusal::Usage(
                    "the registered Paykit protocol and wallet must handle the same extension kinds"
                        .into(),
                ))
                .into());
            }
            let rail = Arc::clone(protocol.rail().ok_or_else(|| {
                AppError::Refused(pubky_switch_client::app::Refusal::Usage(
                    "the registered Paykit protocol must have a rail".into(),
                ))
            })?);
            check_orphan_window(&config, rail.as_ref())?;
            let wallet = Self::assemble(app, protocol, config, rail, timer);
            if wallet.app.reconcile_mode()
                || wallet
                    .local_get::<OwnResendOwed>(key::OWN_RESEND_OWED)
                    .await?
                    .is_some_and(|owed| owed.restored_at_ms.is_none())
            {
                return Err(AppError::Refused(pubky_switch_client::app::Refusal::Usage(
                    "use Paykit's restore and load constructors for backup recovery".into(),
                ))
                .into());
            }
            if !wallet
                .protocol
                .bind_for_attachment(wallet.app.identity(), wallet.app.cert().clone())
            {
                return Err(AppError::Refused(pubky_switch_client::app::Refusal::Usage(
                    "the registered Paykit protocol already has an owner".into(),
                ))
                .into());
            }
            wallet
                .protocol
                .set_publishes_record(wallet.app.publishes_record());
            wallet.finish_load().await
        })
        .await
    }

    async fn finish_load(mut self) -> Result<Self> {
        // A divergent record pauses even when a crash came before the receive's events
        // were taken: look at each held record once (`divergent.rs`).
        Box::pin(self.sweep_divergent()).await?;
        // The host hears again, at each load, of what holds the identity for good.
        self.tell_identity_notices().await?;
        // A crash between the restore's commit and clock clamp leaves the clamp for the
        // next load. Nothing is written under a certificate Paykit refuses.
        if self.certificate_refused().is_none() {
            self.bound_restored_cursor_time().await?;
            Box::pin(self.recover_original_approvals()).await?;
            Box::pin(self.take_restore_verdict()).await?;
        }
        // Records are unlisted here until the next list read (`hold::side_unlisted`).
        Ok(self)
    }

    /// Restore from a backup (Switch spec 12.4): the client's restore, which installs
    /// Paykit's layer, its local state with the journal, in the commit of its own state.
    /// The journal is checked whole before anything is written, and its entries are marked
    /// restored, so their open attempts are resolved as a restore requires (Paykit spec
    /// 7.2), each request payment's claim key bound to its request under the backup's naming
    /// key. On an error, neither layer is in the store. The app is in reconcile mode until
    /// [`Paykit::reconcile`] and [`Paykit::end_reconcile`].
    ///
    /// This convenience constructor registers only Paykit. Use [`Paykit::restore_with_protocols`]
    /// to restore an app with other protocols (Paykit spec 3.2).
    ///
    /// # Errors
    ///
    /// [`crate::PaykitError::Lib`] `Validation` for an `orphan_after_s` below the rail's floor
    /// ([`orphan_floor_s`]), before anything is written. The client's errors, all before
    /// anything is written but a store failure: `InvalidData` for a backup whose certificate
    /// Paykit refuses (Paykit wire 3.1), one without Paykit's layer or one another protocol
    /// made, or a journal with a malformed entry or two open entries for one obligation that
    /// no earlier restore left; for a restore in place, state of the installation's it keeps
    /// that doesn't verify, where a restore into an empty store, which reads none of it, is
    /// the way to recover; a store failure.
    pub async fn restore(
        env: AppEnv,
        config: PaykitConfig,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
    ) -> Result<Self> {
        Self::restore_with_protocols(env, config, backup, rail, timer, |p| Ok(p.into())).await
    }

    /// Restore the client's state and every registered protocol layer together (Paykit
    /// spec 3.2, 7.2; Switch spec 12.4). As at [`Paykit::join_with_protocols`], `register`
    /// must retain the supplied Paykit instance. The client validates the named layers
    /// and each protocol validates its own state before the atomic restore commit.
    ///
    /// # Errors
    ///
    /// As [`Paykit::restore`], plus the host's registration error or the client's `Usage`
    /// refusal if the set omits or replaces the supplied Paykit implementation.
    pub async fn restore_with_protocols<F>(
        env: AppEnv,
        config: PaykitConfig,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
        register: F,
    ) -> Result<Self>
    where
        F: FnOnce(Arc<PaykitProtocol>) -> std::result::Result<ProtocolSet, AppError>,
    {
        // Keep the nested client future on the heap, as at `receive`.
        Box::pin(async move {
            // The client checks its state, then Paykit's layer checks the certificate and the
            // whole journal (`PaykitProtocol::restore_layer`), before anything is written; then
            // both go in one commit: on an error, neither is in the store.
            check_orphan_window(&config, rail.as_ref())?;
            crate::protocol::check_restored_loss(&backup.app)?;
            let (protocol, protocols) = Self::protocols(&rail, &config, register)?;
            let app = App::restore(env, protocols, config.app.clone(), &backup.app).await?;
            protocol.set_own(app.identity(), app.cert().clone());
            let mut paykit = Self::assemble(app, protocol, config, rail, timer);
            paykit.bound_restored_cursor_time().await?;
            paykit.stamp_restore_time().await?;
            // An app restored opens as one loaded does.
            paykit.tell_identity_notices().await?;
            Ok(paykit)
        })
        .await
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
        // A divergence of the block floor the client holds or reported goes into the backup
        // as the hold (`crate::floor`), whatever the receive that met it got to, under a
        // refused certificate too: it is this app's own state.
        Box::pin(self.keep_held_floor()).await?;
        // So does the loss marker of a list record the client holds, read before the receive
        // that would keep it (`Paykit::place_held_lists`).
        Box::pin(self.keep_list_loss_markers()).await?;
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
    /// codes. Calls made through it bypass Paykit's gates, such as the refusal of every
    /// Paykit action under a certificate Paykit refuses ([`Paykit::certificate_refused`]):
    /// a host that flushes the client's outbox itself sends what is queued in it, and a
    /// `payment_lists` value naming a delete that the host reads itself is sure to be kept
    /// as a hold only once the next [`Paykit::receive`] or [`Paykit::backup`] has run
    /// (Paykit spec 6). After a
    /// restore, the host must run [`Paykit::receive`] before such calls; Paykit's outgoing
    /// methods make that first read themselves (Switch spec 12.4).
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

    /// The obligation of a payment without a request: its attempt ID blinded with the
    /// payer's naming key (Paykit wire 1.5).
    pub(crate) fn payment_obligation(&self, attempt_id: &paykit_lib::AttemptId) -> [u8; 32] {
        paykit_lib::derive::payment_obligation_blinded(&self.app.blind(&attempt_id.to_bytes()))
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
        if self.holds("paykit_pay") {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("paykit_pay".into()).into())
        }
    }

    pub(crate) fn require_payee(&self) -> Result<()> {
        if acts_as_payee(self.app.cert().scopes()) {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("paykit_request or paykit_request:<label>".into()).into())
        }
    }

    /// The refusal the app's current certificate meets by Paykit's checks (Paykit wire 3.1),
    /// such as a renewal's the Switch took: `Some` while it fails them, read from the stored
    /// certificate, so it holds across reloads until a certificate that passes replaces it.
    /// The host shows it to the user: meanwhile the app sends nothing, pays nothing and
    /// holds the entries it receives, unprocessed.
    pub fn certificate_refused(&self) -> Option<Refusal> {
        check_own_certificate(self.app.cert())
            .err()
            .and_then(|error| error.refusal().cloned())
    }

    /// The certificate may have been renewed: the protocol follows it, once it passes
    /// Paykit's verifiers' checks as at load (Paykit wire 3.1). A renewed certificate they
    /// refuse, which the Switch took, is [`Refusal::CertificateRefused`], and the protocol
    /// keeps the one before. Every call that writes or sends runs this first, so no Paykit
    /// action is taken under a certificate that fails; the refusal holds as long as the
    /// stored certificate fails, so it needs no mark of its own.
    pub(crate) fn refresh_own(&self) -> Result<()> {
        check_own_certificate(self.app.cert())?;
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
    /// **Acknowledging duty events.** Once this call has returned, the host may acknowledge
    /// for Paykit the duty events up to the last one its summary's report shows, with
    /// [`App::ack_events_for`] under `"paykit"`, the name [`PaykitProtocol`] registers,
    /// whether the call returned early, under a certificate Paykit refuses or an account
    /// gone, or not: the SDK has kept what it needs of the events about stored entries
    /// before it returns. The app-wide [`App::ack_events`] acknowledges each event for every
    /// protocol it is for, so in an app with other protocols the host calls it only once
    /// each of those has handled the event too. The SDK itself acknowledges for Paykit each
    /// event it took, one by one ([`App::ack_event_with`]); the rest wait for the host, such
    /// as `AppEvent::ListsUnverified`, which the host shows the user, and none of them holds
    /// back an event the SDK took. An entry dropped unverified
    /// (`AppEvent::StoredEntryDropped`) counts in its sender's mark, which asks that sender
    /// to send it again (`marks.rs`).
    ///
    /// An outgoing method may make the restore's first read before preparing its effects.
    /// This receive includes that read's events and effects in its summary, then its own.
    /// After a reload, the client still holds unacknowledged duty events for the host.
    /// When a deletion stops a record job, the host can read [`AppEvent::RecordJobStopped`]
    /// from the report. The message protocol and the record collection's protocol can each
    /// retrieve and acknowledge the notice through their own event queue. The record version
    /// and `item_hash` identify the signed delete so the collection owner can reconsider the write.
    ///
    /// A receive that returns an error shows no report. When the client's read returned
    /// before the error, the client counts the duty events the read raised as handed to the
    /// host, so no later report shows them: after an error, the host takes them from
    /// [`App::unacknowledged_events`] (through [`Paykit::app_mut`]), and acknowledges them,
    /// as above, only once a later receive has returned, up to the last event its report
    /// shows, so the SDK has taken those about stored entries first.
    ///
    /// # Errors
    ///
    /// The client's errors; [`Refusal::SwitchUnavailable`] while the account may have been
    /// erased and the client's wait before it takes it as gone goes on, as for a switch that
    /// doesn't answer (Switch spec 12.7). A message whose effect couldn't be written yet
    /// stays before the cursor, and the next call hands it over again. One waiting for a
    /// scope key the app doesn't hold yet is stored by the client and passed at once, not
    /// held behind the cursor, and is read again once the key comes.
    ///
    /// A `missed` item's listing is made inside the client's read, before any later entry is
    /// handed over (Switch spec 8.2): a listing that fails there fails the read, and this
    /// call returns its error; the next receive lists first.
    pub async fn receive(&mut self, wait: Option<u32>) -> Result<ReceiveSummary> {
        let _delivery = self.delivery_operation();
        // The read's future goes on the heap: the client's read nests deep, and a host
        // polling it on a small stack, such as a test thread's 2 MiB, keeps room for it.
        let summary = Box::pin(self.receive_inner(wait)).await?;
        Ok(match self.first_read_summary.take() {
            Some(earlier) => summary.after(*earlier),
            None => summary,
        })
    }

    async fn receive_inner(&mut self, wait: Option<u32>) -> Result<ReceiveSummary> {
        self.lists_read_this_pass = false;
        let read = Box::pin(self.read_before_effects(wait)).await?;
        Box::pin(self.after_read(read)).await
    }

    /// Complete the client's read before a caller's already-recorded block is sent.
    /// That block must precede the outgoing recovery work in `after_read`.
    pub(crate) async fn read_before_effects(&mut self, wait: Option<u32>) -> Result<ReadResult> {
        // Each step's future goes on the heap: built in this future's poll, every one of them
        // would otherwise take its size of the stack there, beside the client's read, and a
        // host thread of 2 MiB would not hold a deep read in a debug build.
        // Confirm the newest own identity before the restore's first page. A read that
        // cannot be made leaves the first read owed (Switch spec 12.4, "The held cursor").
        let first_read = self.first_read_pending();
        if first_read && self.certificate_refused().is_none() {
            let refreshed = self.app.refresh_identity().await;
            // The refresh can take a renewal into the own record.
            self.note_own_record_write(&refreshed);
            match refreshed {
                Ok(IdentityRefresh::Current { .. } | IdentityRefresh::Newer { .. }) => {}
                Ok(_) | Err(AppError::Transport(_)) => {
                    return Err(Refusal::IdentityUnavailable(
                        "Switch spec 12.4: the restore's first read awaits the own identity".into(),
                    )
                    .into());
                }
                Err(AppError::Refused(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        // This app's unblocks whose batch the client no longer keeps count no more: they go
        // first, so the lists are read again below, before the copy this app judges by is
        // used. The steps never count an unblock: the client's copy decides there.
        if self.certificate_refused().is_none() {
            Box::pin(self.drop_unblocks_not_kept()).await?;
        }
        // The record accepts the kinds this app handles now, extension kinds included
        // (`crate::extensions`), before the feed is read.
        if self.certificate_refused().is_none() {
            Box::pin(self.align_accepts()).await?;
        }
        // An app holding `paykit_contacts` reads the flagged senders again first, at most every 10
        // minutes, for the spam rule (Paykit wire 4.1).
        Box::pin(self.refresh_lists_when_due()).await?;
        // This app's blocks that may not reach the switch otherwise are written again: one
        // whose batch the client neither keeps nor applied, a restored one among them.
        if self.certificate_refused().is_none() && !first_read {
            Box::pin(self.send_own_blocks_again()).await?;
        }
        // The records paused before this read, for the summary's new ones.
        let paused_before = Box::pin(self.paused_records()).await?;
        // Whether the client may list at this read's start ([`Paykit::records_listed`]).
        let keys_held_before = self.holds_scope_keys();
        let received = Box::pin(self.app.receive(wait)).await;
        // A read that stopped before any page got no response to observe (Paykit spec 8.1).
        if received
            .as_ref()
            .is_ok_and(|r| !r.stopped || r.held_cursor.is_some())
        {
            self.observe_service_time();
        }
        // A read can take another device's record or a renewal into the own record.
        self.note_own_record_write(&received);
        let report = match received {
            Ok(report) => report,
            // The erasure's wait: the switch answers as for an account it doesn't host, and
            // nothing authenticates that, so it counts as not answering until the wait ends.
            Err(error @ AppError::NotAnsweringAsAccount { .. }) => {
                return Err(Refusal::SwitchUnavailable(error.to_string()).into());
            }
            Err(error) => return Err(error.into()),
        };
        // A deny change or gap the read told takes effect before the sends that follow it.
        Box::pin(self.take_learned_lists()).await?;
        // A successful call can stop while keys or a listing wait, before any page.
        // Only a page's verdict under the confirmed current identity opens outgoing work.
        if report.held_cursor.is_some() && self.app.identity_confirmed_at().is_some() {
            self.first_read_done = true;
        }
        Ok(ReadResult {
            report,
            paused_before,
            keys_held_before,
            blocks_after_read: first_read,
        })
    }

    /// Finish a first read made for an explicit block, keeping its report for the host.
    pub(crate) async fn finish_first_read(&mut self, read: ReadResult) -> Result<()> {
        let summary = Box::pin(self.after_read(read)).await?;
        self.remember_read(summary);
        Ok(())
    }

    /// Count proposals against this completed client read before combining its events with
    /// any earlier report the host has yet to collect.
    pub(crate) async fn own_pending_count(
        &mut self,
        peer: &[u8; 32],
    ) -> Result<std::result::Result<usize, crate::Refusal>> {
        let read = Box::pin(self.read_before_effects(None)).await?;
        let summary = Box::pin(self.after_read(read)).await?;
        let report = summary
            .records_unlisted
            .is_none()
            .then_some(&summary.report);
        let count = Box::pin(self.pending_count(report, peer, true)).await;
        self.remember_read(summary);
        count
    }

    pub(crate) fn remember_read(&mut self, summary: ReceiveSummary) {
        self.first_read_summary = Some(Box::new(match self.first_read_summary.take() {
            Some(earlier) => summary.after(*earlier),
            None => summary,
        }));
    }

    /// Run the SDK's recovery work after the client's read and any explicit block.
    async fn after_read(&mut self, read: ReadResult) -> Result<ReceiveSummary> {
        let ReadResult {
            report,
            paused_before,
            keys_held_before,
            blocks_after_read,
        } = read;
        // The data epochs the entries the client stores for a key wait for, as its report
        // names them at every read while any waits.
        let key_epochs_awaited = key_epochs_awaited(&report.events);
        // The duty events kept, listed once, right after the read: a listing hands every
        // kept duty event to the host, so one made later in this receive would hand over,
        // in no report, those the steps below raise, such as a listing's.
        let duties = Box::pin(self.app.unacknowledged_events()).await?;
        let account_gone =
            account_gone(&report.events) || (report.stopped && account_gone(&duties));
        // The duty events about stored entries and divergent records are taken before
        // anything returns, so that once the host acknowledges what this report showed none
        // is lost: only the SDK's own marks and notes are kept here, and nothing is written at
        // the switch (`stored.rs`, `divergent.rs`).
        Box::pin(self.take_stored_events(&duties)).await?;
        if account_gone {
            // Nothing more is read or written there: the summary tells the host.
            let divergent = Box::pin(self.newly_paused(&paused_before)).await?;
            return Ok(ReceiveSummary {
                report,
                account_gone,
                divergent,
                ..ReceiveSummary::default()
            });
        }
        // Under a certificate Paykit refuses, the host still gets the client's report, with
        // its events and cursor; the Paykit steps below act under the certificate, so none
        // runs.
        if let Some(refused) = self.certificate_refused() {
            let divergent = Box::pin(self.newly_paused(&paused_before)).await?;
            return Ok(ReceiveSummary {
                report,
                certificate_refused: Some(refused),
                divergent,
                ..ReceiveSummary::default()
            });
        }
        self.refresh_own()?;
        Box::pin(self.note_service_time()).await?;
        // A recovery notice holds own writes. Return the client's report before upkeep
        // can try a held write. A restore or a kept floor batch also defers that work.
        if self.app.recovery_required().is_some()
            || self.first_read_pending()
            || self.floor_write_is_kept().await?
        {
            let applied = report
                .events
                .iter()
                .filter_map(|event| match event {
                    AppEvent::MessageReceived { msg_id, .. } => Some(msg_id.clone()),
                    _ => None,
                })
                .collect();
            return Ok(ReceiveSummary {
                report,
                applied,
                records_unlisted: self
                    .unlisted()
                    .await?
                    .map(|_| crate::hold::unlisted().to_string()),
                scope_keys_awaited: self.scope_keys_awaited().await?.is_some()
                    || !key_epochs_awaited.is_empty(),
                key_epochs_awaited,
                divergent: self.newly_paused(&paused_before).await?,
                ..ReceiveSummary::default()
            });
        }
        if Box::pin(self.take_restore_verdict()).await? {
            Box::pin(self.refresh_lists_when_due()).await?;
        }
        // Restore-held blocks go before the other outgoing work, after the first page.
        if blocks_after_read {
            Box::pin(self.send_own_blocks_again()).await?;
        }
        // A restore's listing may need several reads. Clear its mark only when the client
        // finishes the listing with the required scope keys.
        let mut app_record_owed = None;
        let upkeep_tried = !report.stopped;
        if upkeep_tried {
            Box::pin(self.records_listed(keys_held_before)).await?;
            // A `missed` item whose end isn't known holds every open request until a read
            // completes: this one did, so the hold is bounded by the service time verified
            // (`hold.rs`).
            Box::pin(self.bound_missed_hold()).await?;
            // The app record's upkeep: none of the steps below depends on it, so a failure
            // that is the publisher's own leaves the record owed and is told in the summary
            // once they have run; a wallet-wide stop still ends the receive here (Paykit
            // spec 4, `Paykit::tend_own_record_at_receive`).
            app_record_owed = Box::pin(self.tend_own_record_at_receive()).await?;
        }
        // A certificate the read took, any other than the one the records were listed under,
        // a narrower one included: they are listed now, or stay unlisted for the next
        // receive or write (`Paykit::list_under_new_cert`).
        let records_unlisted = Box::pin(self.list_under_new_cert()).await?;
        // A `missed` item this read took waits for its read of the lists, with or without the
        // divergence hold and whatever the app's scopes: it is made now, its records listed,
        // before any ask (Paykit wire 5.6, Raising).
        if Box::pin(self.missed_widening_unread()).await? {
            Box::pin(self.refresh_lists_when_due()).await?;
        }
        let pending_read = records_unlisted.is_none().then_some(&report);
        let keys = Box::pin(self.scope_keys_awaited()).await?;
        // The entries the client stored for good: their messages, kept above, written into
        // their records as room frees (`stored.rs`), unless the payer's side waits for its
        // scope keys or its listing, as for the payees' cancellations below.
        if !Box::pin(self.payer_waits(keys)).await? {
            Box::pin(self.place_stored(pending_read)).await?;
        }
        // The Allowance answers the read kept go into their records next, in the order their
        // inbox accepted them, after the stored entries noted above, so an earlier answer
        // whose write the switch refused is seen first (`allowances.rs`).
        Box::pin(self.recover_pending_replies()).await?;
        Box::pin(self.place_kept_allowance_responses(pending_read)).await?;
        // The payees' cancellations the read kept go into their records, as later writes of
        // this app's own (Paykit spec 8.1), unless the payer's side waits for its scope keys:
        // they build on request records the copy may lack, and stay kept meanwhile.
        if !Box::pin(self.payer_waits(keys)).await? {
            Box::pin(self.place_kept_cancellations()).await?;
            Box::pin(self.record_ledger_outcomes()).await?;
        }
        // Ask for replies dropped for a bound and entries dropped unverified as soon as
        // the sender's holds and mark cadence allow. Forget the pairs a block's marks
        // counted first, so their asks may go
        // (`marks.rs`). Before them, the notices of lost messages owed on block marks the
        // receive steps dropped for the bound are settled from the outbox's answer, which
        // only this side of the read can make (`marks::LostOwed`).
        Box::pin(self.settle_lost_owed()).await?;
        Box::pin(self.forget_blocked_pairs()).await?;
        let mut resend_refused = None;
        let marked = Box::pin(self.ask_on_marks(&mut resend_refused)).await?;
        // A higher floor or a divergence the reads showed is kept (`crate::floor`). The
        // payer's records opened before this app held the block floor they fall under carry
        // `manual_only` from now on, unless the payer's side waits.
        Box::pin(self.keep_held_floor()).await?;
        // A loss followed in this read is told as at a load (`crate::loss`).
        self.tell_identity_notices().await?;
        if !Box::pin(self.payer_waits(keys)).await? {
            Box::pin(self.sweep_floor()).await?;
        }
        // The replies and period details kept for requests whose record wasn't held yet,
        // unless the payer's side waits (`Paykit::payer_waits`), as above; each record they
        // go into that is absent while its side isn't listed waits on its own. The messages
        // kept over divergent records whose user has confirmed them, a list to its held list,
        // are placed next (`divergent.rs`).
        if !Box::pin(self.payer_waits(keys)).await? {
            Box::pin(self.place_kept_replies()).await?;
            Box::pin(self.place_divergent_writes()).await?;
        }
        // Lists held over a `payment_lists` record that didn't read, once it reads.
        Box::pin(self.place_held_lists()).await?;
        // The counterparties' Allowance ends kept as stops go into their records, with their
        // executor notices, and the consent list drops every Allowance seen ended, rejected or
        // conflicted (Paykit spec 8.3). After a `missed` item, every Allowance on it is to be confirmed
        // again, as after a restore. An executor notice's target goes once its notice has.
        Box::pin(self.place_allowance_stops()).await?;
        Box::pin(self.send_owed_ends()).await?;
        Box::pin(self.confirm_consents_again()).await?;
        Box::pin(self.sweep_consents()).await?;
        Box::pin(self.sweep_notice_targets()).await?;
        let applied = report
            .events
            .iter()
            .filter_map(|event| match event {
                AppEvent::MessageReceived { msg_id, .. } => Some(msg_id.clone()),
                _ => None,
            })
            .collect();
        Box::pin(self.repair_settlement_buckets()).await?;
        let answered = Box::pin(self.answer_kept_resends()).await?;
        // Open records resolve re-send requests and locally held answers. After a restore,
        // signed records also identify outgoing messages whose outcome the host must check.
        if Box::pin(self.unlisted()).await?.is_none()
            && Box::pin(self.scope_keys_awaited()).await?.is_none()
        {
            Box::pin(self.queue_restore_asks()).await?;
            // A stop held in the outbox past its 30 days is never sent (Paykit spec 8.1).
            Box::pin(self.give_up_expired_stops()).await?;
            Box::pin(self.queue_own_again()).await?;
            Box::pin(self.note_imported_receipt_batches()).await?;
        }
        let asked = Box::pin(self.send_asks(&mut resend_refused)).await?;
        // After every read of this receive, the last of which may have written the raise.
        Box::pin(self.settle_raise_waiting()).await?;
        Box::pin(self.retire_end_origins()).await?;
        // A confirmed mark goes once its record's view isn't divergent any more, after this
        // read's events were taken.
        Box::pin(self.end_confirmed_marks()).await?;
        let divergent = Box::pin(self.newly_paused(&paused_before)).await?;
        Box::pin(self.tend_private_lists(Some(&report))).await?;
        Ok(ReceiveSummary {
            report,
            applied,
            resends: answered.saturating_add(asked).saturating_add(marked),
            resend_refused,
            certificate_refused: None,
            records_unlisted,
            scope_keys_awaited: keys.is_some() || !key_epochs_awaited.is_empty(),
            key_epochs_awaited,
            account_gone: false,
            divergent,
            app_record_owed,
            upkeep_tried,
        })
    }

    /// Try every queued message once (Switch spec 11).
    ///
    /// # Errors
    ///
    /// The client's errors.
    pub async fn flush(&mut self) -> Result<()> {
        let _delivery = self.delivery_operation();
        self.lists_read_this_pass = false;
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        Box::pin(self.refresh_lists_when_due()).await?;
        Box::pin(self.note_service_time()).await?;
        self.send_outbox().await?;
        // Flushing can settle a kept deletion. Its signed reply can leave only
        // once Paykit has verified the applied operation and its service result.
        Box::pin(self.recover_pending_replies()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use paykit_lib::AttemptId;
    use pubky_switch_client::app::{AppEnv, AppEvent, HeldCursor, JoinRequest, UserConfirmed};

    use super::{Paykit, PaykitBackup, PaykitConfig, ReceiveSummary};
    use crate::pay::{PayOptions, UnrequestedPayment};
    use crate::rail::Rail;
    use crate::receipts::{ReceiptDetails, ReceiptsOf};
    use crate::reconcile::Resolution;
    use crate::requests::{RequestDraft, RequestRef};
    use crate::resend::ResendSelection;
    use crate::timer::Timer;

    /// A read whose summary says `owed`, having tried the app record's upkeep or not.
    fn upkeep_read(owed: Option<&str>, tried: bool) -> ReceiveSummary {
        ReceiveSummary {
            app_record_owed: owed.map(String::from),
            upkeep_tried: tried,
            ..ReceiveSummary::default()
        }
    }

    #[test]
    fn test_receive_summary_keeps_the_app_record_owed_past_a_read_that_skipped_the_upkeep() {
        // A first read made for an outgoing call reported the record owed; the next read
        // stopped or returned early, skipping the upkeep: the host is still told.
        let both = upkeep_read(None, false).after(upkeep_read(Some("earlier"), true));
        assert_eq!(both.app_record_owed.as_deref(), Some("earlier"));
        // And a later read that skipped it too keeps it again.
        let all = upkeep_read(None, false).after(both);
        assert_eq!(all.app_record_owed.as_deref(), Some("earlier"));
    }

    #[test]
    fn test_receive_summary_clears_the_app_record_owed_after_an_upkeep_that_went_through() {
        // The later read tried the upkeep: its outcome is the latest, so one that went
        // through clears the earlier reason, and one that failed gives its own.
        let both = upkeep_read(None, true).after(upkeep_read(Some("earlier"), true));
        assert_eq!(both.app_record_owed, None);
        let both = upkeep_read(Some("later"), true).after(upkeep_read(Some("earlier"), true));
        assert_eq!(both.app_record_owed.as_deref(), Some("later"));
    }

    fn assert_send<T: Send>(_: &T) {}

    fn assert_send_sync<T: Send + Sync>() {}

    /// Every public async call's future is `Send`, so a host on a multi-threaded runtime
    /// can spawn it. Only compiled: the futures are made and dropped, never polled.
    #[allow(clippy::too_many_arguments)]
    fn public_futures_are_send(
        wallet: &mut Paykit,
        env: AppEnv,
        config: PaykitConfig,
        join: JoinRequest,
        backup: &PaykitBackup,
        rail: Arc<dyn Rail>,
        timer: Arc<dyn Timer>,
        confirmed: UserConfirmed,
        attempt: AttemptId,
        resolution: Resolution,
        draft: RequestDraft,
        payment: UnrequestedPayment,
        shown: crate::DivergentRecord,
        held: crate::HeldRequest,
        role: crate::AllowanceRole,
        terms: paykit_lib::allowances::AllowanceTerms,
        details: paykit_lib::bodies::PeriodDetails,
    ) {
        let key = [0; 32];
        let request = RequestRef {
            payee: key,
            request_hash: key,
        };
        assert_send(&Paykit::join(
            env.clone(),
            config.clone(),
            join,
            Arc::clone(&rail),
            Arc::clone(&timer),
        ));
        assert_send(&Paykit::load(
            env.clone(),
            config.clone(),
            [0; 16],
            Arc::clone(&rail),
            Arc::clone(&timer),
        ));
        assert_send(&Paykit::restore(env, config, backup, rail, timer));
        assert_send(&wallet.backup());
        assert_send(&wallet.receive(None));
        assert_send(&wallet.flush());
        assert_send(&wallet.public_endpoints(&key, Some(confirmed)));
        assert_send(&wallet.set_public_endpoints(std::collections::BTreeMap::new(), None));
        let endpoint = paykit_lib::PaymentEndpointIdentifier::new("btc-lightning-bolt12").unwrap();
        let payload = paykit_lib::PaymentEndpointPayload::new("lno1tips").unwrap();
        assert_send(&wallet.set_public_endpoint(endpoint.clone(), payload));
        assert_send(&wallet.remove_public_endpoint(&endpoint));
        assert_send(&wallet.update_own_record());
        assert_send(&wallet.reconcile());
        assert_send(&wallet.resolve_attempt(&key, attempt, resolution, confirmed));
        assert_send(&wallet.end_reconcile(confirmed));
        assert_send(&wallet.request_resend(key, ResendSelection::default()));
        assert_send(&wallet.held_lists());
        assert_send(&wallet.set_list_endpoints(std::collections::BTreeMap::new()));
        assert_send(&wallet.publish_list_to(key));
        assert_send(&wallet.withdraw_list_from(key));
        let unknown = crate::UnknownListDelivery {
            recipient: key,
            version: paykit_lib::ListVersion::new(1, 1).unwrap(),
            msg_id: String::new(),
            withdrawal: true,
            payable_until_ms: None,
        };
        assert_send(&wallet.send_list_again(&unknown, confirmed));
        assert_send(&wallet.lists_waiting_to_send_again());
        assert_send(&wallet.spam_count(&key));
        assert_send(&wallet.send_request(key, draft));
        assert_send(&wallet.message_delivery(""));
        assert_send(&wallet.payee_activity());
        assert_send(&wallet.verify_payment(&key));
        assert_send(&wallet.confirm_payment(&key));
        assert_send(&wallet.confirm_payment_with(&key, ReceiptDetails::default()));
        assert_send(&wallet.cancel_sent_request(&key, None));
        assert_send(&wallet.requests());
        assert_send(&wallet.request(&request));
        assert_send(&wallet.reject_request(&request, None));
        assert_send(&wallet.cancel_request(&request, None));
        assert_send(&wallet.pay_request(&request, PayOptions::default()));
        assert_send(&wallet.pay_without_request(payment));
        assert_send(&wallet.payments());
        assert_send(&wallet.repair_activity_record(&attempt));
        assert_send(&wallet.resume_payments());
        assert_send(&wallet.block(&key));
        assert_send(&wallet.unblock(&key));
        assert_send(&wallet.confirm_possibly_cancelled(&request, &held, confirmed));
        assert_send(&wallet.notices());
        assert_send(&wallet.acknowledge_notice(""));
        assert_send(&wallet.receipts(ReceiptsOf::All));
        assert_send(&wallet.divergent_records());
        assert_send(&wallet.confirm_divergent(&shown, confirmed));
        assert_send(&wallet.propose_allowance(key, role, terms));
        assert_send(&wallet.accept_allowance(&key));
        assert_send(&wallet.reject_allowance(&key));
        assert_send(&wallet.end_allowance(&key));
        assert_send(&wallet.allowances());
        assert_send(&wallet.allowance(&key));
        assert_send(&wallet.accept_subscription(&request));
        assert_send(
            &wallet.accept_subscription_with(&request, crate::requests::AcceptOptions::default()),
        );
        assert_send(&wallet.send_period_details(&key, details));
    }

    #[test]
    fn test_paykit_futures_are_send() {
        assert_send_sync::<Paykit>();
        let _ = public_futures_are_send;
    }

    #[test]
    fn test_receive_summary_keeps_the_implicit_read_before_the_later_one() {
        let mut earlier = ReceiveSummary::default();
        earlier.report.events.push(AppEvent::RenewalSeen);
        earlier.report.held_cursor = Some(HeldCursor::Clean);
        earlier.applied.push("first".into());
        earlier.resends = 2;
        earlier.key_epochs_awaited = vec![3, 5];
        let mut later = ReceiveSummary::default();
        later.report.events.push(AppEvent::FullBundleRequested);
        later.applied.push("second".into());
        later.resends = 3;
        later.key_epochs_awaited = vec![4, 5];
        let merged = later.after(earlier);
        assert_eq!(
            merged.report.events,
            [AppEvent::RenewalSeen, AppEvent::FullBundleRequested]
        );
        assert_eq!(merged.report.held_cursor, Some(HeldCursor::Clean));
        assert_eq!(merged.applied, ["first", "second"]);
        assert_eq!(merged.resends, 5);
        assert_eq!(merged.key_epochs_awaited, [3, 4, 5]);
        assert!(merged.scope_keys_awaited);
    }

    #[test]
    fn test_receive_summary_keeps_the_worse_held_cursor() {
        let ahead = HeldCursor::Ahead {
            held: 9,
            position: 4,
        };
        for (first, second, expected) in [
            (Some(HeldCursor::Clean), Some(ahead), Some(ahead)),
            (Some(ahead), Some(HeldCursor::Unknown), Some(ahead)),
            (
                Some(HeldCursor::Clean),
                Some(HeldCursor::Unknown),
                Some(HeldCursor::Unknown),
            ),
            (
                Some(HeldCursor::Unknown),
                Some(HeldCursor::Clean),
                Some(HeldCursor::Unknown),
            ),
            (None, Some(HeldCursor::Clean), Some(HeldCursor::Clean)),
            (None, None, None),
        ] {
            let mut earlier = ReceiveSummary::default();
            earlier.report.held_cursor = first;
            let mut later = ReceiveSummary::default();
            later.report.held_cursor = second;
            assert_eq!(later.after(earlier).report.held_cursor, expected);
        }
    }
}
