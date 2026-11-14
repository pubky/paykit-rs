//! The executor (Paykit spec 8.3): the grant, the ledger it builds on, and paying a request
//! automatically under an Allowance.
//!
//! - **The grant:** an app holding `paykit_allowance_execute` becomes an Allowance's executor once
//!   its user confirms it: it signs its executor grant with its own signing key over the
//!   identity, the counterparty, the Allowance, the terms hash, the current generation and
//!   the Allowance record version its write builds on (Paykit wire 3.3), appends it to the
//!   record's grant history and puts the Allowance on its consent list, in one batch. A
//!   handoff is the same write by the new executor, which tells the previous one with an
//!   `executor_notice` (Paykit wire 4.17). After the first grant it writes the empty
//!   ledger, conditioned on the record version its grant left.
//! - **The ledger it builds on:** read from the switch before each automatic payment and
//!   accepted only when it passes every check: bound to its Allowance, written by the
//!   executor the grant history names at the Allowance record version the write was
//!   conditioned on ("Who wrote it"), the kept version or one built on it or later, and
//!   counting no less than the version last accepted ("From one version to the next"). The
//!   last version accepted is kept with its content (`lv/`). A missing ledger is taken as
//!   empty only by the executor of the first grant that kept none. An executor that holds no
//!   ledger version builds on none the switch shows alone: after a handoff, only on the write
//!   the previous executor names, and otherwise only on a ledger its user confirms by the
//!   repair.
//! - **The handoff's ledger version** (Paykit spec 8.3, "Handoff"): at its next resume after
//!   a handoff, the previous executor writes `ledger_handoff` into the Allowance record,
//!   signed with its certificate, naming its last ledger version, unless it holds none or
//!   only one a restore installed. The new executor waits for it in its own journal
//!   (`lb/`): holding no version, it pays nothing automatically until the write named, or
//!   one built on it, is shown; holding one, it checks the named write against what its
//!   first ledger write since the handoff built on, and an older base is a rollback.
//! - **A rollback raises the alarm** (Paykit spec 8.3, "Last ledger version"): a ledger
//!   write shown that is neither the version kept nor built on it, or a missing ledger
//!   against the version kept, stops every new attempt of the app at that generation
//!   (`la`, [`Paykit::ledger_alarm`]); a write built on it that fails a check by policy only
//!   pauses the Allowance.
//! - **Outcomes in the ledger:** the executor writes each attempt's outcome into its entry,
//!   in the batch recording the outcome when it records it itself, and from the claims in
//!   every executor batch and at every resume; the user releases an amount another app
//!   failed ([`Paykit::release_allowance_attempt`]).
//! - **Compaction:** once the ledger nears the record's size limit, the executor batch
//!   compacts it before adding its entry, and a ledger that can't take the entry even then,
//!   with room for every outcome, pays nothing automatically.
//! - **After a confirmation:** a ledger the executor doesn't accept (refused by a check of
//!   policy, missing or deleted, at its key before the first grant, or past a version kept at
//!   an earlier generation) is written again once its user confirms the figures shown
//!   ([`Paykit::ledger_repair`], [`Paykit::confirm_ledger_repair`]): a merge that never
//!   counts less of the version kept, what the ledger shown adds, the journal's attempts and
//!   the claims'. A rollback isn't repaired.
//! - **Paying under an Allowance:** the executor's checks (the Allowance record read now,
//!   its grant, the consent list, the stops, the holds, the deny list read now, the
//!   occurrence's record, the block floor, the terms and the limits at the service's time),
//!   then one batch:
//!   the `attempt`, the ledger with the new entry under `record_version(Allowance record)`
//!   and `server_time_in(at, end)`, and the request record naming the Allowance. The rest is
//!   the paying order of Paykit spec 7.2, with the Allowance checked again before money
//!   moves and the proof naming it.
//!
//! - **Nothing under a record seen divergent** (Paykit spec 6): while the view of the
//!   Allowance's `allowances` or `allowance_ledgers` record shows it divergent, and after
//!   that until the user confirms it ([`Paykit::confirm_divergent`]) and the messages kept
//!   over it are written, the executor pays nothing under the Allowance. Both collections
//!   are declared to the client, so both records are held, marked and listed
//!   ([`Paykit::divergent_records`]).
//! - **Nothing while the response isn't settled:** while an answer to the Allowance's
//!   proposal is kept to write (`pr/`, `se/`), or its counterparty owes a re-send of replies
//!   the bounds dropped (`pd/`), an earlier different answer may still take the response or
//!   make the Allowance conflicted, so the executor pays nothing under it.
//! - **A repeat finishes a payment already dispatched** (Paykit spec 8.3, "Conflicted",
//!   "Handoff"): at a resume it is made after a handoff or a conflict too, through the
//!   method's repeat with the same idempotency key, while what holds while unknown still
//!   holds it ([`Paykit::repeat_under_allowance_held`]). The Allowance record is read from
//!   the switch again for it, before the final payment check and after it, and an end or a
//!   rejection it shows, or a read that fails, holds the repeat: a crash can leave `Paying`
//!   journaled for a dispatch the final payment check stopped before any call, so a repeat
//!   may be the first call ([`Paykit::repeat_allowance_read_now`]).
//! - **Immediately before the dispatch** (Paykit spec 8.1, 8.3): the deny list, the request
//!   record, the block floor and the Allowance record are read from the switch again, twice,
//!   before the attempt's `Paying` commit and after it, and the dispatch holds on a block, a
//!   stop, a raise of the floor past the request's date, an Allowance no longer in force
//!   (ended, rejected, conflicted, handed off, its grant voided, paused), or a read that
//!   fails (`Paykit::automatic_dispatch_held`); the batch doesn't condition on the floor's
//!   version, and the Allowance record can change after the batch is applied.
//!
//! Security: every executor batch is conditioned on the Allowance record version read, so
//! an end, a rejection or a handoff written since makes it fail; usage is counted from the
//! ledger and the claims, and a ledger the checks refuse stops automatic payment rather
//! than being trusted (fail closed). Limits are enforced here, not by the service.

use std::collections::HashMap;

use paykit_lib::allowances::{
    check_limits, check_successor, check_successor_since, compact, merge_for_repair,
    record_outcomes, usage, AllowanceTerms, ClaimState, EntryOutcome, Ledger, LedgerEntry,
    PeriodUsage,
};
use paykit_lib::bodies::{
    Body, ExecutorNotice, NoticeReason, PaymentDeadline, PaymentRequest, ProposerRole,
};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::grant::{grant_fields, ExecutorGrant, LedgerHandoff, EXECUTOR_GRANT_LABEL};
use paykit_lib::kinds::Kind;
use paykit_lib::periods::Recurrence;
use paykit_lib::records::{ActivityRecord, AllowanceRecord, GrantEntry, Origin, RecordBinding};
use paykit_lib::{AllowanceId, AttemptId, PaymentAmount, PaymentEndpointIdentifier};
use pubky_switch_client::app::{
    Initiator, OpBatch, RecordContent, RecordRead, RecordTarget, SharedRecord, UserConfirmed,
};
use pubky_switch_core::claims::{ClaimView, Outcome, RECONCILIATION_MS};
use pubky_switch_core::enc::{serde_b64, serde_b64_vec};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::ops::{Condition, OpKind, RecordVersion, SignedOp};
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;
use serde::{Deserialize, Serialize};

use crate::activity::ActivityTarget;
use crate::allowances::{allowance_target, Consent, ConsentEntry, NoticeTarget};
use crate::error::{PaykitError, Refusal, Result};
use crate::journal::Destination;
use crate::pay::PayOptions;
use crate::paykit::Paykit;
use crate::requests::RequestRef;
use crate::state::{self, key, Tx};
use crate::PayOutcome;

/// The longest ledger value a shared record holds: the plaintext sealed is the value's
/// length in 4 bytes, the value and its padding, at most 256 KiB (Switch wire 6.2,
/// "Padding").
const MAX_LEDGER_VALUE: usize = 256 * 1024 - 4;

/// The length from which a ledger is compacted before it takes a new entry: it nears the
/// size limit (Paykit spec 8.3, "Compaction"). Three quarters of the limit, so an
/// Allowance's entries are compacted well before a payment could be refused for size.
const COMPACT_FROM: usize = MAX_LEDGER_VALUE / 4 * 3;

/// How many times a grant's write is built again after its record changed meanwhile.
const GRANT_TRIES: usize = 3;

fn not_automatic(why: &str) -> PaykitError {
    Refusal::NotAutomatic(why.into()).into()
}

/// The last ledger version the executor wrote or read with an authenticated version, and
/// accepted (`lv/<Allowance record key>`, Paykit spec 8.3 "Last ledger version"): its
/// version, the item hash of its write, the service time it was written at, and its
/// content, the base the next version is checked against. The Allowance record it names
/// repeats the key's, so a value read back is bound to its key.
///
/// With it go the attempts of this app's journal entries under the Allowance that its
/// content doesn't list (`dropped`): compaction drops a `done` entry into the totals, and a
/// compacted `failed` attempt leaves `totals.failed` into the totals once its `done` is
/// seen, so the ledger alone no longer shows the Allowance such an attempt was made under.
/// They stay what bears out the Allowance its journal entry names
/// ([`Paykit::entry_allowance`]), for a payment whose proof is still to be written. Bounded
/// by the journal: every attempt there is one of a journal entry this app holds naming the
/// Allowance, only those are written, a pruned entry's attempt leaves it in the prune's
/// commit, and a restore refuses a backup that carries another.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptLedger {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) version: u64,
    /// The service time of the write, from its proof or verified stub, else the latest
    /// service time verified when it was kept, which is no earlier: a newer version's
    /// compacted entries are judged from it (Paykit spec 8.3, "From one version to the
    /// next"), and a later time only judges them more strictly.
    pub(crate) written_at: u64,
    #[serde(with = "serde_b64")]
    pub(crate) item_hash: [u8; 32],
    #[serde(with = "serde_b64_vec")]
    pub(crate) value: Vec<u8>,
    /// The attempts, as text, of this app's journal entries under the Allowance that this
    /// version doesn't list among its entries or `totals.failed`.
    pub(crate) dropped: Vec<String>,
    /// A restore from a backup installed it: it may predate later writes this app made, so
    /// it is never named in a `ledger_handoff` (Paykit spec 8.3, "Handoff"). A version kept
    /// since is the app's own again.
    pub(crate) restored: bool,
    /// The version of the first grant history entry of this app's run of grants under which
    /// it wrote this version, or took it as its kept version by a `ledger_handoff`; `None`
    /// for one it only read, or a restore installed. Only such a version, of the run that
    /// handed the Allowance on, is named in a `ledger_handoff` (Paykit spec 8.3, "Handoff").
    pub(crate) term: Option<(u64, u64)>,
}

/// The ledger write an executor checks what it builds on against
/// (`lb/<Allowance record key>`, Paykit spec 8.3, "Handoff", "Last ledger version", "After
/// a confirmation"): after a handoff to this app, until the `ledger_handoff` for it settles;
/// after a repair its user confirmed, until a version past the repair is kept. Written in the
/// batch of the grant or of the repair, so only once it is applied. Never restored: a restored
/// executor that holds no ledger version builds only through the repair, never on a
/// `ledger_handoff` still in the record. One per Allowance record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerBase {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    /// After a handoff to this app.
    pub(crate) handoff: Option<HandoffWait>,
    /// After a repair.
    pub(crate) repair: Option<RepairWrite>,
}

/// A handoff to this app whose `ledger_handoff` is still to settle (Paykit spec 8.3,
/// "Handoff").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HandoffWait {
    /// The version of the grant history entry that handed the Allowance on to this app.
    pub(crate) generation: u64,
    pub(crate) version: u64,
    /// What this app's first ledger write since the handoff built on, `None` before it has
    /// written one.
    pub(crate) first: Option<FirstWrite>,
}

/// What an executor's first ledger write since a handoff built on: the version its
/// `expected_version` names, `{0, 0}` for the ledger's absence, with the item hash of the
/// write at that version when it was the version kept then.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FirstWrite {
    pub(crate) generation: u64,
    pub(crate) version: u64,
    pub(crate) item_hash: Option<[u8; 32]>,
}

/// A repair's write (Paykit spec 8.3, "After a confirmation"): built on the version
/// `built_on` names, `{0, 0}` for the ledger's absence, with a value hashing to `value_hash`,
/// at the own `identity.json`'s generation `generation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RepairWrite {
    pub(crate) built_on_generation: u64,
    pub(crate) built_on_version: u64,
    #[serde(with = "serde_b64")]
    pub(crate) value_hash: [u8; 32],
    pub(crate) generation: u64,
}

impl LedgerBase {
    /// Whether it is a form a run writes: something to wait for, versions a write gives or
    /// the absence, an item hash only for a version a write gives.
    pub(crate) fn well_formed(&self) -> bool {
        let pair = |generation: u64, version: u64| (generation == 0) == (version == 0);
        let handoff = self.handoff.is_none_or(|h| {
            h.generation > 0
                && h.first.is_none_or(|f| {
                    pair(f.generation, f.version) && (f.version > 0 || f.item_hash.is_none())
                })
        });
        let repair = self
            .repair
            .is_none_or(|r| pair(r.built_on_generation, r.built_on_version) && r.generation > 0);
        (self.handoff.is_some() || self.repair.is_some()) && handoff && repair
    }
}

impl HandoffWait {
    fn grant(&self) -> RecordVersion {
        RecordVersion {
            generation: self.generation,
            version: self.version,
        }
    }
}

impl FirstWrite {
    fn version(&self) -> RecordVersion {
        RecordVersion {
            generation: self.generation,
            version: self.version,
        }
    }
}

impl RepairWrite {
    fn built_on(&self) -> RecordVersion {
        RecordVersion {
            generation: self.built_on_generation,
            version: self.built_on_version,
        }
    }
}

/// A ledger write the executor builds only on, or on one signed as built on it or a later
/// one, beside its kept version (Paykit spec 8.3, "Last ledger version").
#[derive(Clone, Copy, Debug)]
enum BuildOn {
    /// The write a counting `ledger_handoff` names, by version and item hash.
    Named {
        version: RecordVersion,
        item_hash: [u8; 32],
    },
    /// The repair's own write ([`RepairWrite`]).
    Repaired(RepairWrite),
}

impl BuildOn {
    /// The generation of the own `identity.json` it was named or written at: against one of
    /// an earlier generation an older write only pauses the Allowance.
    fn generation(&self) -> u64 {
        match self {
            Self::Named { version, .. } => version.generation,
            Self::Repaired(repair) => repair.generation,
        }
    }

    /// Whether the write `op`, with `value`, is older than it: neither it nor signed as built
    /// on it or a later version.
    fn older(&self, op: &SignedOp, value: Option<&[u8]>) -> Result<bool> {
        let built_on = match &op.body().kind {
            OpKind::RecordPut(put) => put.expected_version,
            _ => return Err(PaykitError::corrupt()),
        };
        Ok(match self {
            Self::Named { version, item_hash } => {
                op.item_hash().map_err(|_| PaykitError::corrupt())? != *item_hash
                    && built_on < *version
            }
            // A compare-and-swap applies one write at `built_on`: a write built on a later
            // version is built on the repair's or after it.
            Self::Repaired(repair) => {
                !(built_on > repair.built_on()
                    || built_on == repair.built_on()
                        && value
                            .is_some_and(|v| pubky_switch_core::enc::hash(v) == repair.value_hash))
            }
        })
    }

    /// Whether a write the client refused, at `refused`, is older than it.
    fn older_refused(&self, refused: RecordVersion) -> bool {
        match self {
            Self::Named { version, .. } => refused < *version,
            Self::Repaired(repair) => refused <= repair.built_on(),
        }
    }

    /// Whether a delete held at `deleted` came at or after it.
    fn deleted_since(&self, deleted: RecordVersion) -> bool {
        match self {
            Self::Named { version, .. } => deleted >= *version,
            Self::Repaired(repair) => deleted > repair.built_on(),
        }
    }
}

/// How a counting `ledger_handoff` settles against what a new executor holds (Paykit spec
/// 8.3, "Handoff").
enum Settled {
    /// None counts yet.
    Waiting,
    /// The record holds one that doesn't verify, or doesn't read: ignored, and none counts.
    Unverified,
    /// The named write is the one to build on, or on one built on it or later.
    Take(BuildOn),
    /// Settled: the named write is the one the executor holds or built on.
    Kept,
    /// Settled, the named write ignored: one before the version the executor holds or built
    /// on, or another write at that version. The host is told, with its writer
    /// ([`crate::NoticeKind::LedgerHandoffIgnored`]).
    Ignored([u8; 16]),
    /// The executor built on a version older than the previous executor's last, named at
    /// the generation given: a rollback.
    Rollback(u64),
}

/// The first grant's empty ledger in the executor's own journal (`el/<Allowance record key>`,
/// Paykit spec 8.3 "A grant implies a ledger"): written with the grant, `sent` before a batch
/// carrying the ledger's first write is submitted, and gone once a ledger version is kept.
/// While it isn't sent the empty ledger is owed, written before the first payment.
/// Never restored: only the executor that wrote the grant, while its own journal shows the
/// empty ledger not yet sent, takes a missing ledger as empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EmptyLedgerMark {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) sent: bool,
    /// The kept batch carrying the empty ledger, when its send got no answer: a batch the
    /// client drops as refused or unsent was never applied, so the empty ledger is owed
    /// again.
    pub(crate) batch: Option<[u8; 16]>,
    /// A ledger was already at its key at the first grant: the executor writes no empty
    /// ledger over it and takes it as one it didn't accept, paying nothing on it until its
    /// user confirms a repair (Paykit spec 8.3, "A grant implies a ledger").
    pub(crate) unaccepted: bool,
}

/// The version of an Allowance record's current write the app ignores (Switch spec 8.2)
/// that its user confirmed the executor may pay under (`fc/<Allowance record key>`, Paykit
/// spec 8.3 "Executor"). A later ignored write pauses the Allowance again. Never restored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FlagConfirmed {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) version: u64,
}

/// A rollback of an Allowance ledger the executor found (`la`, Paykit spec 8.3, "Last
/// ledger version"; Switch spec 10.1): the ledger the service showed is neither the version
/// kept nor built on it, or is missing against it. While the own `identity.json` is at the
/// `generation` it was found at, the app makes no new attempt through that service, by hand
/// or automatic; the user resolves it by moving to another provider or declaring a provider
/// loss, which starts a later generation. The first one found is kept, as the client keeps
/// its first alarm. Restored with a backup: the evidence of a service's misbehaviour stays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerAlarm {
    /// The Allowance record whose ledger was rolled back.
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    /// The generation of the own `identity.json` it was found at.
    pub(crate) generation: u64,
    /// The rule the ledger shown broke, without content.
    pub(crate) reason: String,
}

impl LedgerAlarm {
    /// The alarm as the client reports a rollback (Switch spec 10.1).
    pub(crate) fn alarm(&self) -> pubky_switch_client::app::Alarm {
        pubky_switch_client::app::Alarm::Rollback {
            reason: format!("Paykit spec 8.3: an Allowance ledger {}", self.reason),
        }
    }
}

/// An attempt under an Allowance held reserved because an app other than its owner wrote
/// it `failed` (Paykit spec 8.3, "`failed` isn't final"): only its user's release frees it
/// ([`Paykit::release_allowance_attempt`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReservedByOtherApp {
    /// The attempt.
    pub attempt_id: AttemptId,
    /// The amount it holds reserved, in the Allowance's asset.
    pub value: paykit_lib::Decimal,
}

/// What an outcome batch adds for the ledger (Paykit spec 8.3, "Outcomes in the ledger"):
/// the ledger write, and what keeps its version once the batch is applied.
pub(crate) struct LedgerWrite {
    /// The ledger write, under `record_version(Allowance record)`.
    pub(crate) write: (Vec<Condition>, OpKind),
    /// The Allowance record's key.
    pub(crate) record_key: [u8; 32],
    /// The ledger's target.
    pub(crate) ledger: RecordTarget,
}

/// The ledger an executor batch writes (Paykit spec 8.3, "Outcomes in the ledger" and
/// "Compaction"; Paykit wire 5.5): `base` with the outcomes its attempts' claims `claims`
/// show and `entry`, fitted to the record ([`fitted`]): compacted at `at`, the latest service
/// time verified, once it nears the size limit ([`COMPACT_FROM`]). It counts what `base`
/// counts with `claims`, and the entry: [`paykit_lib::allowances::record_outcomes`] and
/// [`paykit_lib::allowances::compact`] each keep usage at `at` and after.
///
/// A ledger that doesn't fit the record even after compaction, with room for an outcome
/// of every attempt it holds, takes no entry: no automatic payment is made, and the refusal
/// tells the user. The room kept for the outcomes lets every later outcome write fit, as
/// none of them grows the ledger past [`Ledger::len_with_outcomes`].
///
/// # Errors
///
/// The rule that refuses the payment.
pub(crate) fn next_ledger(
    terms: &AllowanceTerms,
    base: &Ledger,
    claims: &HashMap<AttemptId, ClaimState>,
    at: u64,
    entry: LedgerEntry,
) -> std::result::Result<Ledger, &'static str> {
    let mut ledger = record_outcomes(terms, base, claims)
        .map_err(|_| UNCOUNTED)?
        .unwrap_or_else(|| base.clone());
    ledger.entries.push(entry);
    fitted(terms, ledger, at)
        .map_err(|why| why.unwrap_or("the ledger can't take another entry, even after compaction"))
}

/// Why a ledger can't be counted, as [`next_ledger`] and [`repair_ledger`] refuse it.
const UNCOUNTED: &str = "usage can't be counted";

/// `ledger` as an executor writes it (Paykit spec 8.3, "Compaction"): compacted at `at`, the
/// latest service time verified, once it nears the size limit ([`COMPACT_FROM`]), and
/// refused, `Err(None)`, when it doesn't fit the record even then with room for an outcome
/// of every attempt it holds ([`Ledger::len_with_outcomes`]); `Err(Some(_))` for a ledger
/// whose usage can't be counted. [`paykit_lib::allowances::compact`] keeps usage at `at`
/// and after.
fn fitted(
    terms: &AllowanceTerms,
    mut ledger: Ledger,
    at: u64,
) -> std::result::Result<Ledger, Option<&'static str>> {
    if ledger.len_with_outcomes().map_err(|_| Some(UNCOUNTED))? > COMPACT_FROM {
        if let Some(compacted) = compact(terms, &ledger, at).map_err(|_| Some(UNCOUNTED))? {
            ledger = compacted;
        }
    }
    if ledger.len_with_outcomes().map_err(|_| Some(UNCOUNTED))? > MAX_LEDGER_VALUE {
        return Err(None);
    }
    Ok(ledger)
}

/// The ledger a repair writes (Paykit spec 8.3, "After a confirmation", "Outcomes in the
/// ledger", "Compaction"): `merged` with the outcomes its attempts' claims `outcomes` show,
/// then fitted to the record as an executor batch's ledger is ([`fitted`]), so a ledger
/// near the size limit is compacted and keeps room for every outcome. It counts what
/// `merged` counts with the claims at `at` and after.
///
/// # Errors
///
/// The rule that refuses the repair.
pub(crate) fn repair_ledger(
    terms: &AllowanceTerms,
    merged: &Ledger,
    outcomes: &HashMap<AttemptId, ClaimState>,
    at: u64,
) -> std::result::Result<Ledger, &'static str> {
    let ledger = record_outcomes(terms, merged, outcomes)
        .map_err(|_| UNCOUNTED)?
        .unwrap_or_else(|| merged.clone());
    fitted(terms, ledger, at).map_err(|why| {
        why.unwrap_or("the repair's ledger doesn't fit its record, even after compaction")
    })
}

/// What a ledger's attempts show, as [`Paykit::ledger_claims`] reads them: for usage, and
/// for the outcomes written into the ledger, which leave out what counts as `done` only
/// because whether it was paid is unknown.
#[derive(Default)]
pub(crate) struct LedgerClaims {
    /// What each attempt counts by in usage.
    pub(crate) usage: HashMap<AttemptId, ClaimState>,
    /// What each attempt's outcome is written from.
    pub(crate) outcomes: HashMap<AttemptId, ClaimState>,
}

impl LedgerClaims {
    fn both(&mut self, attempt: AttemptId, state: ClaimState) {
        self.usage.insert(attempt, state);
        self.outcomes.insert(attempt, state);
    }
}

/// Why a ledger is refused while its version kept is of an earlier generation than the
/// service now serving (Paykit spec 8.3, "Last ledger version").
const EARLIER_GENERATION: &str = "the ledger kept is of an earlier generation than the service's";

/// Why an executor restored from a backup pays nothing on the version the backup held
/// (Paykit spec 8.3, "Last ledger version"): only through the repair.
const RESTORED_VERSION: &str =
    "the ledger version kept is a backup's, which may predate later writes: only a repair the user confirms is built on";

/// Why a new executor that holds no ledger version pays nothing yet after a handoff (Paykit
/// spec 8.3, "Handoff"): until the record holds a `ledger_handoff` that counts, or its user
/// confirms the repair.
const AWAITING_HANDOFF: &str =
    "after the handoff, the previous executor hasn't named its last ledger version yet: confirm a repair to pay before it does";

/// The conditions of a repair's ledger write (Paykit spec 8.3, "After a confirmation";
/// Paykit wire 5.5): `record_version` on the Allowance record `record` at `version`, and
/// `server_time_in(at, end)` with the `end` of an executor batch
/// ([`AllowanceTerms::entry_window_end`]): `at + 60 s`, cut at the next boundary of any
/// anchored limit, so the write lands in the periods its figures were counted in.
///
/// # Errors
///
/// [`Refusal::NotAutomatic`] for a window that can't be written.
fn repair_conditions(
    terms: &AllowanceTerms,
    record: &[u8; 32],
    version: RecordVersion,
    at: u64,
) -> Result<Vec<Condition>> {
    let end = terms
        .entry_window_end(at)
        .map_err(|_| not_automatic("the repair's window can't be written"))?;
    Ok(vec![
        Condition::RecordVersion {
            key: *record,
            version,
        },
        Condition::ServerTimeIn {
            start: Some(at),
            end: Some(end),
        },
    ])
}

/// What the executor makes of the ledger the switch shows (`Paykit::ledger_shown`).
enum LedgerShown {
    /// One it builds on, kept as the last version; `None` for the absence the first grant's
    /// executor takes as an empty ledger.
    Accepted(Option<Ledger>),
    /// One it doesn't accept: the Allowance pays nothing automatically until its user
    /// confirms a repair (Paykit spec 8.3, "After a confirmation").
    Unaccepted {
        /// The rule, without content.
        why: String,
        /// What the switch shows of it.
        content: ShownContent,
    },
}

/// The content of a ledger the executor doesn't accept, as a repair takes it.
enum ShownContent {
    /// A value that reads as a ledger and binds to its Allowance.
    Ledger(Box<Ledger>),
    /// None: a ledger missing or deleted, or a value that doesn't read as the Allowance's
    /// ledger or never opens. For a write the client refused, which it hands without
    /// content, what is shown is the copy the app trusts.
    Nothing,
    /// A value not open for now, its scope key not held yet: what it adds can't be told.
    Unknown,
}

fn unaccepted(why: &str, content: ShownContent) -> LedgerShown {
    LedgerShown::Unaccepted {
        why: why.into(),
        content,
    }
}

/// The claims one computation reads, each once ([`Paykit::claim_view`]): what each shows,
/// `None` for one the switch doesn't hold.
#[derive(Default)]
struct ClaimReads {
    views: std::collections::BTreeMap<[u8; 32], Option<ClaimView>>,
}

impl ClaimReads {
    /// Each claim read, by its key, with the step it showed.
    fn steps(&self) -> Vec<([u8; 32], Option<u64>)> {
        self.views
            .iter()
            .map(|(key, view)| (*key, view.as_ref().map(|v| v.step)))
            .collect()
    }
}

/// What a ledger repair was built from (Paykit spec 8.3, "After a confirmation"): its
/// confirmation writes the ledger only while each is as it was shown.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RepairBasis {
    /// The Allowance record version read, which the write is conditioned on.
    record_version: RecordVersion,
    /// The ledger version kept and its write's item hash, if any.
    kept: Option<(RecordVersion, [u8; 32])>,
    /// The ledger version the write builds on (its `expected_version`): the write shown, a
    /// refused write's version, or none for a ledger absent.
    built_on: RecordVersion,
    /// The item hash of the ledger write the app holds, if any.
    shown_item: Option<[u8; 32]>,
    /// The journal's attempts under the Allowance.
    journal: Vec<[u8; 16]>,
    /// The counterparty's payer records read, by key, with their versions.
    records: RecordsRead,
    /// Every claim read, by key, with the step it showed.
    claims: Vec<([u8; 32], Option<u64>)>,
}

/// Records read, by key, with the version of each.
type RecordsRead = Vec<([u8; 32], RecordVersion)>;

/// A repair of an Allowance ledger the executor didn't accept (Paykit spec 8.3, "After a
/// confirmation"), as [`Paykit::ledger_repair`] computes it: the figures the host shows its
/// user, counted over the ledger the repair writes at the service time `at_ms`, and, held
/// inside, the ledger itself and what it was built from. Once the user has seen the
/// figures, [`Paykit::confirm_ledger_repair`] writes exactly that ledger, or refuses with
/// the figures computed then if anything it was built from changed.
///
/// The ledger is a merge that never counts less
/// ([`paykit_lib::allowances::merge_for_repair`]): the last version the executor accepted,
/// as it was; what the ledger shown adds to it; every attempt the journal holds under the
/// Allowance; every attempt of an app the grant history names that the claims of the
/// counterparty's obligations show, one-time requests and every Billing Period whose claim
/// may still be held, unless the attempt's owner failed it or the payer identity's own
/// evidence shows it made otherwise; and every payment a payer or period record's proof
/// shows made under the Allowance. Each attempt counts the most any of them shows, and the
/// outcomes the claims show are written in, compacted near the size limit as an executor
/// batch's ledger is.
#[derive(Clone, PartialEq, Eq)]
pub struct LedgerRepair {
    /// The Allowance record's key.
    pub allowance: [u8; 32],
    /// Why the executor didn't accept the ledger shown: the rule, without content.
    pub reason: String,
    /// The service time the figures are counted at.
    pub at_ms: u64,
    /// Each period limit's usage, its amount and its count of payments, in the order of the
    /// terms' `period_limits`.
    pub periods: Vec<PeriodUsage>,
    /// The lifetime amount: compacted totals, entries reserved and spent, and compacted
    /// failed attempts whose claim shows `done`.
    pub lifetime: paykit_lib::Decimal,
    /// The entries reserved: an attempt open or `unknown`, or `failed` by an app other than
    /// its owner.
    pub reserved: u64,
    /// The entries spent: paid, or whose claim is gone.
    pub spent: u64,
    /// The entries released: `failed` by their owner, or released by the user.
    pub released: u64,
    /// The amounts held reserved because an app other than the attempt's owner wrote it
    /// `failed`: only the user's release frees one ([`Paykit::release_allowance_attempt`]).
    pub reserved_by_other_apps: Vec<ReservedByOtherApp>,
    basis: RepairBasis,
    ledger: Vec<u8>,
}

impl std::fmt::Debug for LedgerRepair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerRepair")
            .field("reason", &self.reason)
            .field("at_ms", &self.at_ms)
            .field("periods", &self.periods)
            .field("lifetime", &self.lifetime)
            .field("reserved", &self.reserved)
            .field("spent", &self.spent)
            .field("released", &self.released)
            .field("reserved_by_other_apps", &self.reserved_by_other_apps)
            .finish_non_exhaustive()
    }
}

/// Whether `holder`'s proof is the payer identity's own word that `attempt` wasn't made
/// under `allowance` (Paykit spec 8.3, "After a confirmation"): a proof of the attempt that
/// names no Allowance or another one, sent by `own`, the payer identity, and signed by
/// `owner`, the attempt's owner, an app not listed as removed (`removed`). A record reads
/// only with a proof its payer sent (Paykit wire 5.3), so the counterparty can't supply one;
/// still a proof another app signed, or one of a removed app, shows nothing of how the
/// owner made the attempt, and the attempt counts.
fn proof_shows_made_otherwise(
    holder: Option<&ActivityRecord>,
    own: &[u8; 32],
    attempt: &AttemptId,
    owner: [u8; 16],
    removed: &[[u8; 16]],
    allowance: &AllowanceId,
) -> bool {
    let Some(proof) = holder.and_then(|record| record.proof.as_ref()) else {
        return false;
    };
    let Body::PaymentProof(body) = proof.body() else {
        return false;
    };
    proof.sender() == *own
        && proof.cert().app_id() == owner
        && !removed.contains(&owner)
        && body.attempt_id == *attempt
        && body.allowance_id.as_ref() != Some(allowance)
}

/// The payment `holder`'s proof shows made under `allowance` on `obligation`, as a ledger
/// entry (Paykit spec 8.3, "After a confirmation"): `done`, with the request's `amount`,
/// dated by the proof's `created_at`. A record holds only a proof its payer sent (Paykit
/// wire 5.3), whichever own app made the payment, so it is the payer identity's own
/// evidence, also once the claim is gone.
fn paid_under(
    holder: Option<&ActivityRecord>,
    allowance: &AllowanceId,
    obligation: [u8; 32],
    amount: &PaymentAmount,
) -> Option<LedgerEntry> {
    let proof = holder?.proof.as_ref()?;
    let Body::PaymentProof(body) = proof.body() else {
        return None;
    };
    (body.allowance_id.as_ref() == Some(allowance)).then(|| LedgerEntry {
        obligation,
        attempt_id: body.attempt_id,
        value: amount.value.clone(),
        asset: amount.asset.clone(),
        at: proof.message().created_at_ms,
        outcome: Some(EntryOutcome::Done),
    })
}

/// The most Billing Periods of one recurring request a repair walks ([`periods_to_read`]).
const MAX_REPAIR_PERIODS: usize = paykit_lib::allowances::MAX_PERIODS_JUDGED;

/// The starts, in Unix seconds, of the Billing Periods of a recurring request, from its
/// first to the one containing `at`, whose claim the service may still hold (Pubky Switch
/// spec, Section 6.3; Paykit spec 7.1, "Deadlines"): with a payment deadline, those whose
/// deadline plus the reconciliation window and the clock tolerance isn't before `at`; without
/// one, every period, as a claim without a deadline is kept 400 days after its last entry,
/// which may come at any time. A period paid early is one the request record lists in
/// `attempted_periods`, or whose period record the app holds, read beside these.
///
/// # Errors
///
/// The rule, for more such periods than [`MAX_REPAIR_PERIODS`] or a period that can't be
/// computed: the repair refuses rather than leave one unread.
fn periods_to_read(
    recurrence: &Recurrence,
    deadline: Option<&PaymentDeadline>,
    at: u64,
) -> std::result::Result<Vec<u64>, &'static str> {
    const UNWALKED: &str = "the recurring request's Billing Periods can't be walked";
    let rule = recurrence.rule();
    let first = recurrence.first().map_err(|_| UNWALKED)?.index;
    let mut last = rule.containing(at).map_err(|_| UNWALKED)?.index;
    if let Some(end) = recurrence.last().map_err(|_| UNWALKED)? {
        last = last.min(end.index);
    }
    let window = match deadline {
        Some(PaymentDeadline::PeriodStart { seconds }) => Some(
            seconds
                .checked_mul(1000)
                .and_then(|d| d.checked_add(1 + RECONCILIATION_MS + CLOCK_TOLERANCE_MS))
                .ok_or(UNWALKED)?,
        ),
        _ => None,
    };
    let mut starts = Vec::new();
    let mut k = last;
    while k >= first {
        let period = rule.period(k).map_err(|_| UNWALKED)?;
        // Periods only start later as k grows: once one's claim is gone, every earlier one's
        // is.
        if window.is_some_and(|w| period.starts_at.saturating_add(w) < at) {
            break;
        }
        if starts.len() == MAX_REPAIR_PERIODS {
            return Err("the recurring request has too many Billing Periods to read for a repair");
        }
        starts.push(period.start_secs());
        let Some(earlier) = k.checked_sub(1) else {
            break;
        };
        k = earlier;
    }
    starts.reverse();
    Ok(starts)
}

/// What a payer record shows of an attempt under an Allowance (Paykit spec 8.3, "`failed`
/// isn't final").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Evidence {
    /// The record holds a proof of the attempt naming the Allowance: it was paid.
    Paid,
    /// The record reads and holds no proof.
    NotPaid,
    /// No copy of the record that reads, or one whose proof is of another attempt.
    Unknown,
}

/// What `record`, this app's copy of an attempt's payer record, shows of `attempt` under
/// `allowance`: a proof is sent only for a payment made, whichever own app made it. The
/// record holds one proof, so a proof of another attempt or Allowance, such as a retry's,
/// shows nothing of this one: unknown, not unpaid.
fn payer_evidence(
    record: Option<&ActivityRecord>,
    attempt: &AttemptId,
    allowance: &AllowanceId,
) -> Evidence {
    let Some(record) = record else {
        return Evidence::Unknown;
    };
    let Some(proof) = &record.proof else {
        return Evidence::NotPaid;
    };
    match proof.body() {
        Body::PaymentProof(proof)
            if proof.attempt_id == *attempt && proof.allowance_id.as_ref() == Some(allowance) =>
        {
            Evidence::Paid
        }
        _ => Evidence::Unknown,
    }
}

/// What an attempt counts by, for usage and for the outcome written, from what its payer
/// record shows (`evidence`), for a `failed` entry (`failed`) or one without an outcome,
/// with its claim gone or not (`claim_gone`) and with or without a journal entry of this
/// app's (`journaled`) (Paykit spec 8.3, "`failed` isn't final"): paid is `done` for both;
/// a `failed` entry whose claim is gone, that this app didn't journal, counts as `done`
/// for usage alone while its record can't be read; anything else changes nothing.
fn evidence_counts(
    evidence: Evidence,
    failed: bool,
    claim_gone: bool,
    journaled: bool,
) -> (Option<ClaimState>, Option<ClaimState>) {
    match evidence {
        Evidence::Paid => (Some(ClaimState::Done), Some(ClaimState::Done)),
        Evidence::Unknown if failed && claim_gone && !journaled => (Some(ClaimState::Done), None),
        Evidence::Unknown | Evidence::NotPaid => (None, None),
    }
}

/// Whether `error` is a failure of this app's own store: every other failure of a ledger
/// write that goes with an outcome leaves the outcome to go on its claim alone.
fn own_store_failure(error: &PaykitError) -> bool {
    matches!(
        error,
        PaykitError::Client(client)
            if matches!(client.unkept(), pubky_switch_client::app::AppError::Store(_))
    )
}

/// The version of the first entry of the run of grants of one app that holds the entry at
/// `at` in `allowance`'s history.
fn run_start(allowance: &AllowanceRecord, at: usize) -> Option<RecordVersion> {
    let app = allowance.grants.get(at)?.grant.app_id;
    let mut start = at;
    while start > 0
        && allowance
            .grants
            .get(start - 1)
            .is_some_and(|entry| entry.grant.app_id == app)
    {
        start -= 1;
    }
    allowance.grants.get(start).map(|entry| entry.built_on)
}

impl KeptLedger {
    fn version(&self) -> RecordVersion {
        RecordVersion {
            generation: self.generation,
            version: self.version,
        }
    }

    /// The attempts kept as dropped, read as written: one that doesn't read is a corrupt
    /// store.
    pub(crate) fn dropped(&self) -> Result<Vec<AttemptId>> {
        self.dropped
            .iter()
            .map(|text| AttemptId::parse(text).map_err(|_| PaykitError::corrupt()))
            .collect()
    }
}

/// Whether `ledger`, the content of `kept`, lists `attempt` (Paykit wire 5.5) among its
/// entries or in `totals.failed`, or `kept` carries it as dropped: an attempt of a journal
/// entry under the Allowance that the content no longer lists, or never did.
fn kept_lists(kept: &KeptLedger, ledger: &Ledger, attempt: &AttemptId) -> Result<bool> {
    Ok(ledger_lists(ledger, attempt) || kept.dropped()?.contains(attempt))
}

/// Whether `ledger` lists `attempt` among its entries or in `totals.failed` (Paykit wire
/// 5.5): a compacted failed attempt keeps its `attempt_id` there.
fn ledger_lists(ledger: &Ledger, attempt: &AttemptId) -> bool {
    ledger.entries.iter().any(|e| e.attempt_id == *attempt)
        || ledger.failed.iter().any(|f| f.attempt_id == *attempt)
}

/// Check a kept ledger version read back from the store or a backup, the same check for
/// both: it names the Allowance record its key names, its version is one a write gives
/// (`seq` at least 1), its content is no longer than a shared record holds
/// (`MAX_LEDGER_VALUE`), which no write or read the executor keeps exceeds, and it reads as
/// a ledger; each attempt kept as dropped reads, once, and isn't one the ledger lists.
/// Returns the ledger. That each attempt kept as dropped is one of a journal entry naming
/// the Allowance needs the journal: a write takes them from it
/// ([`Paykit::dropped_attempts`]), and a restore checks them against the journal restored
/// (`protocol::restore_paykit_layer`).
///
/// # Errors
///
/// A corrupt store, never an absence.
pub(crate) fn check_kept_ledger(record: &[u8; 32], kept: &KeptLedger) -> Result<Ledger> {
    if &kept.record != record
        || kept.version == 0
        || kept.generation == 0
        || kept.value.len() > MAX_LEDGER_VALUE
    {
        return Err(PaykitError::corrupt());
    }
    let ledger = Ledger::from_json(&kept.value).map_err(|_| PaykitError::corrupt())?;
    let dropped = kept.dropped()?;
    for (i, attempt) in dropped.iter().enumerate() {
        if ledger_lists(&ledger, attempt) || dropped[..i].contains(attempt) {
            return Err(PaykitError::corrupt());
        }
    }
    Ok(ledger)
}

/// Whether `shared`'s current write is by an app the own `identity.json` lists as removed,
/// written at or after its `distrust_from`, or with no service time that dates it (Switch
/// spec 12.2 step 3): a write the executor never builds or pays on, whether the client
/// flagged it, marked it doubtful, or took it before it learned of the removal.
fn written_by_removed_app(
    identity: &pubky_switch_core::records::IdentityDoc,
    shared: &SharedRecord,
) -> bool {
    identity
        .body()
        .removed
        .iter()
        .find(|removed| removed.app_id == shared.writer)
        // Both times are on the service's clock, the write's dated by a proof or a verified
        // stub: no tolerance between them. A write exactly at `distrust_from` is refused too:
        // one millisecond stricter than the spec's "after its distrust_from" and the client's
        // own check, so a write the service can't order against the removal pays nothing.
        .is_some_and(|removed| {
            shared
                .written_at_ms
                .is_none_or(|at| at >= removed.distrust_from)
        })
}

/// The client's target of the ledger of `counterparty`'s Allowance `allowance_id` (Paykit
/// wire 1.5, 5.5).
pub(crate) fn ledger_target(counterparty: &[u8; 32], allowance_id: &AllowanceId) -> RecordTarget {
    RecordTarget::new(
        crate::activity::switch_collection(Collection::AllowanceLedgers, None),
        &natural_key::allowance(counterparty, allowance_id),
    )
}

/// The Allowance an attempt rebuilt after a restore was made under, as its ledger shows it
/// ([`Paykit::attempt_under`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AttemptUnder {
    /// The Allowance: its ledger lists the attempt.
    Allowance(AllowanceId),
    /// None: made by hand.
    ByHand,
    /// Not known: its proof can name neither, so the attempt stays doubtful.
    Unknown,
}

/// The Allowance an executor pays under, as its checks read it now.
pub(crate) struct Under {
    /// The Allowance record's key.
    pub(crate) record_key: [u8; 32],
    /// The record, bound to its key.
    pub(crate) allowance: AllowanceRecord,
    /// The record version read, every executor batch's `record_version` condition.
    pub(crate) version: RecordVersion,
    /// The accepted terms.
    pub(crate) terms: AllowanceTerms,
}

/// What an executor's batch adds to the `attempt` (Paykit spec 8.3 step 3).
pub(crate) struct ExecutorOps {
    /// The ledger write and the request record's.
    pub(crate) entries: Vec<(Vec<Condition>, OpKind)>,
    /// The Allowance record's key, whose ledger version is kept once the batch is applied.
    pub(crate) record_key: [u8; 32],
    /// The ledger's target.
    pub(crate) ledger: RecordTarget,
    /// Whether this batch carries the ledger's first write: the first grant's empty ledger
    /// is then marked sent just before the batch goes ([`Paykit::empty_ledger_sent`]).
    pub(crate) first_ledger_write: bool,
}

/// The identifiers a payment may use: those the request accepts and, under an Allowance
/// whose terms list some, only those too (Paykit wire 4.5, 4.15). `None`: any.
pub(crate) fn accepted_ids(
    request: Option<&[PaymentEndpointIdentifier]>,
    under: Option<&Under>,
) -> Option<Vec<PaymentEndpointIdentifier>> {
    let terms = under.and_then(|u| u.terms.allowed_payment_endpoint_identifiers.as_deref());
    both_allow(request, terms)
}

/// The identifiers both `request` and `terms` allow, `None` standing for any.
fn both_allow(
    request: Option<&[PaymentEndpointIdentifier]>,
    terms: Option<&[PaymentEndpointIdentifier]>,
) -> Option<Vec<PaymentEndpointIdentifier>> {
    match (request, terms) {
        (None, None) => None,
        (Some(ids), None) | (None, Some(ids)) => Some(ids.to_vec()),
        (Some(asked), Some(allowed)) => Some(
            asked
                .iter()
                .filter(|id| allowed.contains(id))
                .cloned()
                .collect(),
        ),
    }
}

/// What the claim shows of `attempt`, for usage (Paykit wire 5.5 "Counting"): `None` for a
/// claim the switch no longer holds, or that doesn't hold the attempt.
fn claim_state(
    view: &pubky_switch_core::claims::ClaimView,
    attempt: &AttemptId,
) -> Option<ClaimState> {
    let shown = view.attempt(&attempt.to_bytes())?;
    Some(match shown.outcome {
        Outcome::Open => ClaimState::Open,
        Outcome::Unknown => ClaimState::Unknown,
        Outcome::Done => ClaimState::Done,
        Outcome::Failed => ClaimState::Failed {
            by_owner: shown.resolved_by == Some(shown.owner),
        },
    })
}

/// Whether the claim of `obligation` holds an attempt, one of `attempts`, that `ledger` names
/// neither in an entry nor in a `totals.failed` item for that obligation: one made by hand or
/// otherwise not under the Allowance the ledger is of, whatever its outcome, so the occurrence
/// is paid again only by hand (Paykit spec 8.3, "Doubtful obligations").
fn attempted_elsewhere(obligation: &[u8; 32], attempts: &[[u8; 16]], ledger: &Ledger) -> bool {
    attempts.iter().any(|attempt| {
        let attempt = AttemptId::from_bytes(*attempt);
        let in_entry = ledger
            .entries
            .iter()
            .any(|e| e.attempt_id == attempt && &e.obligation == obligation);
        let in_failed = ledger
            .failed
            .iter()
            .any(|f| f.attempt_id == attempt && &f.obligation == obligation);
        !in_entry && !in_failed
    })
}

/// "Who wrote it" (Paykit spec 8.3, Paykit wire 5.4): the ledger write `op`, by `writer`,
/// counts only if it carries `record_version` on the Allowance record `record_key` at a
/// version no newer than `held`, the version read, and `writer` is the executor the grant
/// history names at that version. A history that isn't usable fails closed.
fn written_by_executor(
    allowance: &AllowanceRecord,
    own: &[u8; 32],
    record_key: &[u8; 32],
    held: RecordVersion,
    op: &SignedOp,
    writer: [u8; 16],
) -> std::result::Result<(), &'static str> {
    if op.body().app_id != Some(writer) {
        return Err("the ledger's write names another writer");
    }
    let Some(at) = op.body().conditions.iter().find_map(|c| match c {
        Condition::RecordVersion { key, version } if key == record_key => Some(*version),
        _ => None,
    }) else {
        return Err("the ledger's write isn't conditioned on the Allowance record");
    };
    if at > held {
        return Err("the ledger's write is conditioned on an Allowance record version not held");
    }
    match allowance.executor_at(own, at) {
        Ok(Some(app)) if app == writer => Ok(()),
        Ok(_) => Err("the ledger's writer isn't the executor at the version it was conditioned on"),
        Err(_) => Err("the grant history isn't usable"),
    }
}

impl Paykit {
    fn require_executor(&self) -> Result<()> {
        if self.holds("paykit_allowance_execute") {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into())
        }
    }

    /// The Allowance `entry`, a journal entry, was paid under, with the proof that the
    /// stored pointer names the right one (Paykit spec 8.3): the executor's batch named the
    /// Allowance in the request record in the same batch as the attempt, so the payer record
    /// held must name it; without a record that reads, such as one another own app deleted,
    /// the kept ledger version must list the attempt, among its entries, in `totals.failed`
    /// once compacted, or as dropped ([`KeptLedger`]), so neither compaction, nor a compacted
    /// attempt's move into the totals, nor a version that was never kept loses it. An entry
    /// naming another Allowance than its proof is a corrupt store, checked before money
    /// moves and before the outcome's proof or record is built.
    ///
    /// `Ok(Err(..))` for an entry whose Allowance its proof doesn't bear out, so the caller
    /// decides for that one entry: the gate holds it, the outcome returns it as a corrupt
    /// store. A failure of the store is the error.
    pub(crate) async fn entry_allowance(
        &self,
        entry: &crate::journal::JournalEntry,
    ) -> Result<std::result::Result<Option<AllowanceId>, Refusal>> {
        let Some(allowance_id) = entry.allowance()? else {
            return Ok(Ok(None));
        };
        let unproven = || {
            Ok(Err(Refusal::NotAutomatic(
                "the journal entry's Allowance isn't the one its request record or ledger names"
                    .into(),
            )))
        };
        let target = crate::activity::ActivityTarget::payer(entry.natural_key.clone());
        if let Some(record) = self.held_activity(&target).await? {
            if record.allowance_id.as_ref() != Some(&allowance_id) {
                return unproven();
            }
            return Ok(Ok(Some(allowance_id)));
        }
        let allowance = self
            .app
            .record_key(&allowance_target(&entry.payee, &allowance_id))?;
        let attempt = entry.attempt()?;
        let in_ledger = match self.kept_ledger(&allowance).await? {
            Some((kept, ledger)) => kept_lists(&kept, &ledger, &attempt)?,
            None => false,
        };
        if !in_ledger {
            return unproven();
        }
        Ok(Ok(Some(allowance_id)))
    }

    /// The Allowance `named`, of the request record of `payee`, that `attempt` was made
    /// under, for a journal entry rebuilt after a restore: its ledger record, as held or else
    /// as read from the switch once, lists the attempt among its entries or, compacted, in
    /// `totals.failed`. [`AttemptUnder::ByHand`] for a record naming none, or a ledger read
    /// from the switch now that reads and binds, was never compacted and doesn't list it: the
    /// attempt was made by hand, and its proof names no Allowance. [`AttemptUnder::Unknown`]
    /// for a ledger that isn't read from the switch now (the read fails, the switch's write
    /// fails the checks, or the switch holds none), doesn't read or bind, or was compacted
    /// without listing it, since compaction drops a `done` entry into the totals: its proof
    /// can name neither, and the attempt stays doubtful.
    pub(crate) async fn attempt_under(
        &mut self,
        payee: &[u8; 32],
        named: Option<&AllowanceId>,
        attempt: AttemptId,
    ) -> Result<AttemptUnder> {
        let Some(allowance_id) = named else {
            return Ok(AttemptUnder::ByHand);
        };
        let target = ledger_target(payee, allowance_id);
        let key = self.app.record_key(&target)?;
        if let Some(ledger) = self.held_ledger(&key, payee, allowance_id).await? {
            if ledger_lists(&ledger, &attempt) {
                return Ok(AttemptUnder::Allowance(allowance_id.clone()));
            }
        }
        // The copy a restore holds may predate the attempt's entry: the ledger is read from
        // the switch once, and only the switch's current write tells the attempt made by
        // hand. A read that fails, or one that leaves the copy held (a write that fails the
        // checks, a ledger the switch doesn't hold or forgot), leaves it unknown; only a
        // failure of the app's own store is an error.
        match self.app.read_record_fresh(&target).await {
            Ok(RecordRead::Fresh(_)) => {}
            Ok(_) => return Ok(AttemptUnder::Unknown),
            Err(error) => {
                let error: PaykitError = error.into();
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
                return Ok(AttemptUnder::Unknown);
            }
        }
        Ok(match self.held_ledger(&key, payee, allowance_id).await? {
            Some(ledger) if ledger_lists(&ledger, &attempt) => {
                AttemptUnder::Allowance(allowance_id.clone())
            }
            Some(ledger)
                if ledger.spent.is_none() && ledger.count == 0 && ledger.failed.is_empty() =>
            {
                AttemptUnder::ByHand
            }
            _ => AttemptUnder::Unknown,
        })
    }

    /// The ledger the app holds at `key`, when it reads and is bound to `payee`'s Allowance
    /// `allowance_id`.
    async fn held_ledger(
        &self,
        key: &[u8; 32],
        payee: &[u8; 32],
        allowance_id: &AllowanceId,
    ) -> Result<Option<Ledger>> {
        let Some(held) = self.app.shared_record(key).await? else {
            return Ok(None);
        };
        Ok(held
            .content
            .value()
            .and_then(|value| Ledger::from_json(value).ok())
            .filter(|l| l.counterparty == *payee && &l.allowance_id == allowance_id))
    }

    /// The kept ledger version of the Allowance at `record`, read bound to its key as a
    /// restore reads it (`state::Stored`).
    async fn kept_ledger(&self, record: &[u8; 32]) -> Result<Option<(KeptLedger, Ledger)>> {
        let Some(kept) = self
            .local_get::<KeptLedger>(&key::kept_ledger(record))
            .await?
        else {
            return Ok(None);
        };
        let ledger = Ledger::from_json(&kept.value).map_err(|_| PaykitError::corrupt())?;
        Ok(Some((kept, ledger)))
    }

    /// The signed write the app holds of record `key`, from the evidence it keeps.
    async fn held_op(&self, key: &[u8; 32]) -> Result<Option<SignedOp>> {
        for kept in self.app.shared_evidence().await?.ops {
            let op = SignedOp::peek(&kept.op).map_err(|_| PaykitError::corrupt())?;
            if matches!(&op.body().kind, OpKind::RecordPut(put) if &put.key == key) {
                return Ok(Some(op));
            }
        }
        Ok(None)
    }

    /// Keep the ledger write the app holds at `ledger_key`, once its version is
    /// authenticated, as the last version of the Allowance at `record` (Paykit spec 8.3,
    /// "Last ledger version"). Run after an own ledger write is applied, and after one is
    /// accepted.
    async fn keep_ledger(&mut self, record: &[u8; 32], ledger_key: &[u8; 32]) -> Result<()> {
        self.keep_ledger_settling(record, ledger_key, false).await
    }

    /// [`Paykit::keep_ledger`], and, when `settles`, the handoff it answers settled in the
    /// same commit: the write named, or one built on it, is the one kept.
    async fn keep_ledger_settling(
        &mut self,
        record: &[u8; 32],
        ledger_key: &[u8; 32],
        settles: bool,
    ) -> Result<()> {
        let Some(held) = self.app.shared_record(ledger_key).await? else {
            return Ok(());
        };
        let (Some(value), true) = (held.content.value(), held.authenticated) else {
            return Ok(());
        };
        let Some(op) = self.held_op(ledger_key).await? else {
            return Ok(());
        };
        // Without any service time verified nothing is kept, as for a write not
        // authenticated: an older version kept stays the base, which a write built on it or
        // later still passes.
        let Some(written_at) = held.written_at_ms.or(self.service_time()) else {
            return Ok(());
        };
        let ledger = Ledger::from_json(value).map_err(|_| PaykitError::corrupt())?;
        let dropped = self.dropped_attempts(record, &ledger).await?;
        let kept = KeptLedger {
            record: *record,
            generation: held.version.generation,
            version: held.version.version,
            written_at,
            item_hash: op.item_hash().map_err(|_| PaykitError::corrupt())?,
            value: value.to_vec(),
            dropped: dropped.iter().map(|a| a.to_text()).collect(),
            restored: false,
            term: if op.body().app_id == Some(self.app_id()) || settles {
                self.own_run_start(record).await?
            } else {
                None
            },
        };
        check_kept_ledger(record, &kept)?;
        let mut tx = Tx::default();
        tx.put(&key::kept_ledger(record), &kept)?;
        // A version kept answers the first grant's empty ledger.
        tx.delete(&key::empty_ledger(record));
        if let Some(mut mark) = self.ledger_base_mark(record).await? {
            // A version kept past the repair answers it: the write shown passed the check
            // against the repair's own write. While a handoff is unsettled no ledger write
            // but the repair's is made, and what it built on goes in the repair's own batch.
            mark.repair = None;
            if settles {
                mark.handoff = None;
            }
            if mark.handoff.is_some() {
                tx.put(&key::ledger_base(record), &mark)?;
            } else {
                tx.delete(&key::ledger_base(record));
            }
        }
        self.local_commit(tx).await
    }

    /// The ledger write the executor of the Allowance at `record` checks what it builds on
    /// against, read bound to its key ([`LedgerBase`]).
    async fn ledger_base_mark(&self, record: &[u8; 32]) -> Result<Option<LedgerBase>> {
        self.local_get::<LedgerBase>(&key::ledger_base(record))
            .await
    }

    /// The version of the first entry of this app's latest run of grants in the history of
    /// the Allowance at `record`, a grant the same app signs again being no handoff.
    async fn own_run_start(&self, record: &[u8; 32]) -> Result<Option<(u64, u64)>> {
        let Some(allowance) = self.held_allowance(record).await? else {
            return Ok(None);
        };
        let Some(last) = allowance
            .grants
            .iter()
            .rposition(|entry| entry.grant.app_id == self.app_id())
        else {
            return Ok(None);
        };
        Ok(run_start(&allowance, last).map(|v| (v.generation, v.version)))
    }

    /// Whether a ledger version a restore installed may be built on (Paykit spec 8.3, "Last
    /// ledger version"): only after a clean restore. This SDK holds every open request after
    /// every restore (`RESTORE_HOLD`) and doesn't tell a clean restore apart yet, so every
    /// restore counts as one that isn't, and the executor builds only on a ledger its user
    /// confirms by the repair.
    fn restore_keeps_ledger_version(&self) -> bool {
        false
    }

    /// Tell the host of a `ledger_handoff` of `under` the executor ignores, written by
    /// `writer`, unless it is told already ([`crate::NoticeKind::LedgerHandoffIgnored`]).
    async fn raise_handoff_notice(&mut self, under: &Under, writer: [u8; 16]) -> Result<()> {
        let (key, notice) = self.handoff_notice(under, writer);
        if self.kept_notice(&key).await?.is_some() {
            return Ok(());
        }
        self.local_put(&key, &notice).await
    }

    /// The notice of an ignored `ledger_handoff` of `under` by `writer`, with its key.
    fn handoff_notice(
        &self,
        under: &Under,
        writer: [u8; 16],
    ) -> (String, crate::notices::KeptNotice) {
        let at = self.service_time().unwrap_or(0);
        (
            key::handoff_notice(&under.record_key, &writer),
            crate::notices::KeptNotice::handoff_ignored(
                under.record_key,
                under.allowance.counterparty,
                writer,
                at,
            ),
        )
    }

    /// Drop the handoff of `mark`, the Allowance at `record`'s, once it has settled: the
    /// mark goes unless a repair is still to land. Returns what is left.
    async fn handoff_settled(
        &mut self,
        record: &[u8; 32],
        mark: Option<LedgerBase>,
        notice: Option<(String, crate::notices::KeptNotice)>,
    ) -> Result<Option<LedgerBase>> {
        let Some(mut mark) = mark else {
            return Ok(None);
        };
        mark.handoff = None;
        let mut tx = Tx::default();
        // The notice of an ignored `ledger_handoff` goes in the commit that settles it, so the
        // host is told exactly once of each.
        if let Some((key, notice)) = notice {
            if self.kept_notice(&key).await?.is_none() {
                tx.put(&key, &notice)?;
            }
        }
        let left = if mark.repair.is_some() {
            tx.put(&key::ledger_base(record), &mark)?;
            Some(mark)
        } else {
            tx.delete(&key::ledger_base(record));
            None
        };
        self.local_commit(tx).await?;
        Ok(left)
    }

    /// How the `ledger_handoff` of `under` settles against `wait`, this app's handoff, and
    /// `kept`, its kept version (Paykit spec 8.3, "Handoff"; Paykit wire 5.4, "Building on
    /// it"): one that counts, for the grant that handed the Allowance on to this app, by an
    /// app not removed since. Before this app wrote the ledger since the handoff, a named
    /// write later than its kept version, or with none kept, is the one to build on, and one
    /// before it, or another write at its version, is ignored; once it has, the named write
    /// must be the one its first write built on, and a later one, another write at that
    /// version, or any where it built on the ledger's absence is a rollback, while one before
    /// it is ignored. One that doesn't verify the client flags as it reads the record.
    fn settle_handoff(
        &self,
        under: &Under,
        wait: &HandoffWait,
        kept: Option<&KeptLedger>,
    ) -> Settled {
        let own = self.identity();
        let handoff = match under.allowance.ledger_handoff_counts(&own) {
            Ok(Some(handoff)) => handoff,
            Ok(None) => return Settled::Waiting,
            Err(_) => return Settled::Unverified,
        };
        if handoff.grant != wait.grant() || self.app.own_identity().lists_removed(&handoff.app_id) {
            return Settled::Waiting;
        }
        let named = handoff.ledger;
        match (wait.first, kept) {
            (Some(first), _) => {
                let other_at_same = named == first.version()
                    && first
                        .item_hash
                        .is_some_and(|item| item != handoff.item_hash);
                if first.version == 0 || named > first.version() || other_at_same {
                    Settled::Rollback(named.generation)
                } else if named < first.version() {
                    Settled::Ignored(handoff.app_id)
                } else {
                    Settled::Kept
                }
            }
            (None, Some(kept))
                if named == kept.version() && handoff.item_hash == kept.item_hash =>
            {
                Settled::Kept
            }
            (None, Some(kept)) if named <= kept.version() => Settled::Ignored(handoff.app_id),
            (None, _) => Settled::Take(BuildOn::Named {
                version: named,
                item_hash: handoff.item_hash,
            }),
        }
    }

    /// The attempts of this app's journal entries under the Allowance at `record` that
    /// `next`, the version about to be kept, doesn't list among its entries or
    /// `totals.failed`: what still bears out the Allowance those entries name once the ledger
    /// has compacted them or moved them into the totals ([`KeptLedger`]). A journal entry
    /// names its Allowance only when this app's executor batch made the attempt, so the
    /// journal, not an earlier kept version, is the source: a version written and compacted
    /// while no version was kept (the read after the write failed) loses no attempt. An
    /// attempt whose journal entry is gone, pruned past its reconciliation window, isn't
    /// carried: the prune takes it out of the kept version in its own commit
    /// ([`Paykit::unlist_pruned`]).
    async fn dropped_attempts(&self, record: &[u8; 32], next: &Ledger) -> Result<Vec<AttemptId>> {
        let mut dropped = Vec::new();
        for entry in self.journal().await? {
            let Some(allowance_id) = entry.allowance()? else {
                continue;
            };
            if self
                .app
                .record_key(&allowance_target(&entry.payee, &allowance_id))?
                != *record
            {
                continue;
            }
            let attempt = entry.attempt()?;
            if !ledger_lists(next, &attempt) && !dropped.contains(&attempt) {
                dropped.push(attempt);
            }
        }
        dropped.sort_by_key(|a| a.to_bytes());
        Ok(dropped)
    }

    /// Take `pruned`, attempts whose journal entries `tx` deletes, out of every kept ledger
    /// version that carries them as dropped, in the same commit: a kept version carries as
    /// dropped only attempts of journal entries this app holds under its Allowance, which a
    /// restore checks ([`KeptLedger`]).
    pub(crate) async fn unlist_pruned(&self, tx: &mut Tx, pruned: &[AttemptId]) -> Result<()> {
        if pruned.is_empty() {
            return Ok(());
        }
        for (_, mut kept) in Box::pin(self.local_scan::<KeptLedger>(key::KEPT_LEDGERS)).await? {
            let before = kept.dropped.len();
            let mut left = Vec::with_capacity(before);
            for attempt in kept.dropped()? {
                if !pruned.contains(&attempt) {
                    left.push(attempt.to_text());
                }
            }
            if left.len() != before {
                kept.dropped = left;
                tx.put(&key::kept_ledger(&kept.record), &kept)?;
            }
        }
        Ok(())
    }

    /// Whether a ledger was at its key at the first grant (`el/`), so the executor takes it
    /// as one it didn't accept (Paykit spec 8.3, "A grant implies a ledger").
    async fn ledger_unaccepted(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<EmptyLedgerMark>(&key::empty_ledger(record))
            .await?
            .is_some_and(|mark| mark.unaccepted))
    }

    /// Whether this executor's own journal shows the first grant's empty ledger not yet
    /// sent (`el/`): only then is a missing ledger the empty one (Paykit spec 8.3).
    async fn empty_ledger_unsent(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<EmptyLedgerMark>(&key::empty_ledger(record))
            .await?
            .is_some_and(|mark| !mark.sent))
    }

    /// Mark the first grant's empty ledger sent, before a batch carrying the ledger's first
    /// write is submitted, with the kept batch when its send got no answer: from then on a
    /// missing ledger is usage history lost.
    pub(crate) async fn empty_ledger_sent(
        &mut self,
        record: &[u8; 32],
        batch: Option<[u8; 16]>,
    ) -> Result<()> {
        if self
            .local_get::<EmptyLedgerMark>(&key::empty_ledger(record))
            .await?
            .is_some()
        {
            self.local_put(
                &key::empty_ledger(record),
                &EmptyLedgerMark {
                    record: *record,
                    sent: true,
                    batch,
                    unaccepted: false,
                },
            )
            .await?;
        }
        Ok(())
    }

    /// Set the first grant's empty ledger back to owed, after a batch carrying the ledger's
    /// first write applied nothing. A mark of a ledger the executor didn't accept stays as it
    /// is: a ledger is at the key, so no empty ledger is ever owed over it (Paykit spec 8.3, "A
    /// grant implies a ledger").
    pub(crate) async fn empty_ledger_owed_again(&mut self, record: &[u8; 32]) -> Result<()> {
        let Some(mark) = self
            .local_get::<EmptyLedgerMark>(&key::empty_ledger(record))
            .await?
        else {
            return Ok(());
        };
        if mark.unaccepted {
            return Ok(());
        }
        self.local_put(
            &key::empty_ledger(record),
            &EmptyLedgerMark {
                record: *record,
                sent: false,
                batch: None,
                unaccepted: false,
            },
        )
        .await
    }

    /// The first grant's empty ledger owed again: not yet sent, or sent in a kept batch the
    /// client has since dropped as refused or unsent, never applied (the mark is then set
    /// back to not sent). A batch still kept, applied, dropped as uncertain, or one the
    /// client keeps nothing of, leaves the mark sent.
    async fn empty_ledger_owed(&mut self, record: &[u8; 32]) -> Result<bool> {
        let Some(mark) = self
            .local_get::<EmptyLedgerMark>(&key::empty_ledger(record))
            .await?
        else {
            return Ok(false);
        };
        if !mark.sent {
            return Ok(true);
        }
        let Some(batch) = mark.batch else {
            return Ok(false);
        };
        let never_applied = matches!(
            self.app.kept_batch(batch).await?,
            Some(pubky_switch_client::app::KeptBatchStatus::Dropped {
                reason: pubky_switch_client::app::KeptBatchDrop::Refused { .. }
                    | pubky_switch_client::app::KeptBatchDrop::Unsent,
                ..
            })
        );
        if never_applied {
            self.local_put(
                &key::empty_ledger(record),
                &EmptyLedgerMark {
                    record: *record,
                    sent: false,
                    batch: None,
                    unaccepted: false,
                },
            )
            .await?;
        }
        Ok(never_applied)
    }

    /// Write the first grant's empty ledger while it is owed and no ledger is at its key,
    /// before the first payment (Paykit spec 8.3, "A grant implies a ledger"): a grant batch
    /// kept, or a crash between the grant and its empty ledger, leaves it owed.
    async fn write_owed_empty_ledger(&mut self, under: &Under) -> Result<()> {
        if !self.empty_ledger_owed(&under.record_key).await? {
            return Ok(());
        }
        self.write_empty_ledger(&under.record_key, &under.allowance)
            .await
    }

    /// The latest write of the record at `record` the app ignores (Switch spec 8.2), from
    /// the client's list of refused writes.
    async fn latest_ignored(&self, record: &[u8; 32]) -> Result<Option<RecordVersion>> {
        Ok(self
            .app
            .flagged_writes()
            .await?
            .into_iter()
            .filter(|flagged| flagged.key == *record)
            .map(|flagged| flagged.version)
            .max())
    }

    /// Whether the latest ignored write of the record at `record` is one a user's
    /// confirmation may cover: written by an app the own `identity.json` lists now and
    /// doesn't list as removed. A write the client refuses for its writer's standing, such
    /// as a removed app's after its `distrust_from`, is never confirmed: no confirmation
    /// lifts that (Paykit spec 8.3, "Building on a version accepts it"). The client names
    /// no typed reason for a refused write, so the writer's standing now is the test; a
    /// current app's write under an earlier certificate after its renewal closed passes it.
    async fn ignored_write_confirmable(&self, record: &[u8; 32]) -> Result<Option<RecordVersion>> {
        let latest = self
            .app
            .flagged_writes()
            .await?
            .into_iter()
            .filter(|flagged| flagged.key == *record)
            .max_by_key(|flagged| flagged.version);
        let Some(latest) = latest else {
            return Ok(None);
        };
        let identity = self.app.own_identity();
        let current = identity.listed_cert(&latest.writer).is_some()
            && !identity.lists_removed(&latest.writer);
        Ok(current.then_some(latest.version))
    }

    /// Whether the user confirmed paying under the Allowance at `record` since its latest
    /// ignored write (`fc/`), and that write is one a confirmation may cover
    /// ([`Paykit::ignored_write_confirmable`]), judged again now.
    pub(crate) async fn ignored_write_confirmed(&self, record: &[u8; 32]) -> Result<bool> {
        let Some(latest) = self.ignored_write_confirmable(record).await? else {
            return Ok(false);
        };
        Ok(self
            .local_get::<FlagConfirmed>(&key::flag_confirmed(record))
            .await?
            .is_some_and(|mark| {
                RecordVersion {
                    generation: mark.generation,
                    version: mark.version,
                } == latest
            }))
    }

    /// The Allowance at `record` as an executor may pay under it now (Paykit spec 8.3,
    /// "Executor"), read from the switch when `fresh`, else as held: the record binds, isn't
    /// divergent, flagged or written by a removed app, and wasn't seen divergent without
    /// the user's confirmation since (Paykit spec 6); it holds the acceptance and no end or
    /// rejection, and this app keeps no stop for it; this identity is the allower; the
    /// current grant is this app's; the consent list holds it, confirmed since any restore
    /// or `missed` item; its ledger record, as held, isn't divergent or paused; no answer
    /// to its proposal is kept to write, and its counterparty owes no re-send of replies
    /// dropped for the bounds; the terms are valid; and no provider loss since the grant's
    /// confirmation pauses it.
    pub(crate) async fn allowance_gate(
        &mut self,
        record: &[u8; 32],
        fresh: bool,
    ) -> Result<std::result::Result<Under, Refusal>> {
        let own = self.identity();
        // A record not held, held as deleted, or not binding is a refusal, never an error: at
        // the gate after the batch it writes `failed`, and a resume goes on with the rest.
        let Some(held) = self.held_allowance(record).await? else {
            return Ok(Err(Refusal::NotFound));
        };
        let shared: SharedRecord = if fresh {
            let target = allowance_target(&held.counterparty, &held.allowance_id);
            match self.app.read_record_fresh(&target).await? {
                RecordRead::Fresh(shared) => shared,
                RecordRead::Kept {
                    reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                    ..
                } => return Err(Refusal::SwitchUnavailable(why).into()),
                // The switch's current write is one the app ignores: the copy it trusts is
                // checked below, flagged, and pays nothing until the user confirms.
                RecordRead::Kept {
                    record: Some(shared),
                    reason: pubky_switch_client::app::KeptReason::Refused,
                } if shared.flagged => shared,
                RecordRead::Kept { .. } => {
                    return Ok(Err(Refusal::NotAutomatic(
                        "the Allowance record isn't read from the switch now".into(),
                    )))
                }
                _ => return Err(PaykitError::corrupt()),
            }
        } else {
            let Some(shared) = self.app.shared_record(record).await? else {
                return Ok(Err(Refusal::NotFound));
            };
            shared
        };
        let refuse = |why: &str| Ok(Err(Refusal::NotAutomatic(why.into())));
        if shared.divergent || shared.doubtful {
            return refuse("the Allowance record is divergent or untrusted");
        }
        // Seen divergent and not confirmed since, also once a newer write has cleared the
        // view, or messages kept over it still wait to be written (Paykit spec 6): the
        // upload may hold an end, a rejection or a `conflicted` message the current write
        // lacks. The view just read is taken, so a record divergent now is marked.
        if self.pause_of(&shared).await?.blocks_automatic() {
            return refuse(
                "the Allowance record was seen divergent and the user hasn't confirmed it",
            );
        }
        // No write of the Allowance record by a removed app after its `distrust_from`
        // (Paykit spec 8.3, "Building on a version accepts it"): no confirmation lifts it.
        if written_by_removed_app(self.app.own_identity(), &shared) {
            return refuse(
                "the Allowance record's write is a removed app's after its distrust_from",
            );
        }
        // A current write the app ignores pauses the Allowance until its user confirms it
        // (Paykit spec 8.3, "Executor").
        if shared.flagged && !self.ignored_write_confirmed(record).await? {
            return refuse("the Allowance record's current write is one this app ignores");
        }
        // Every executor batch is conditioned on the switch's current write: an ignored one,
        // which the app's next write builds on (Switch spec 8.2), when the record is flagged.
        let version = if shared.flagged {
            self.latest_ignored(record)
                .await?
                .ok_or_else(PaykitError::corrupt)?
        } else {
            shared.version
        };
        let Some(allowance) = self.held_allowance(record).await? else {
            return Ok(Err(Refusal::NotFound));
        };
        // Conflicted for good (Paykit spec 8.3, "Conflicted"): in the record, or kept as the
        // stop before the record shows it.
        if allowance.conflicted.is_some() || self.conflict_kept(record).await? {
            return Ok(Err(Refusal::AllowanceConflicted));
        }
        if allowance.end.is_some() || self.allowance_stopped(record).await? {
            return Ok(Err(Refusal::AllowanceEnded));
        }
        let Some(response) = &allowance.response else {
            return refuse("the Allowance isn't accepted");
        };
        if !matches!(response.body(), Body::AllowanceAcceptance(_)) {
            return Ok(Err(Refusal::Rejected));
        }
        let Body::AllowanceProposal(proposal) = allowance.proposal.body() else {
            return Err(PaykitError::corrupt());
        };
        let proposed_here = allowance.proposal.sender() == own;
        let allower = matches!(
            (proposal.proposer_role, proposed_here),
            (ProposerRole::Allower, true) | (ProposerRole::Allowee, false)
        );
        if !allower {
            return refuse("this identity is the Allowance's allowee");
        }
        let Some((grant, _)) = allowance.current_grant(&own) else {
            return refuse("the Allowance record holds no current grant");
        };
        if grant.app_id != self.app_id() {
            return refuse("another app is the Allowance's executor");
        }
        // After provider loss: paused while the grant's confirmation predates the loss.
        if self
            .app
            .own_identity()
            .body()
            .recovery_from
            .is_some_and(|lost| grant.confirmed_generation < lost.saturating_add(1))
        {
            return refuse("paused after a provider loss until the user confirms again");
        }
        if !self.consents(record).await? {
            return refuse("the consent list doesn't hold it, confirmed since any restore");
        }
        // The ledger record, as held: divergent, or seen so and not confirmed since
        // (Paykit spec 6): the upload may hold entries the current write lacks. The read
        // from the switch before the batch judges its view then
        // ([`Paykit::ledger_base`]).
        let ledger = self.app.record_key(&ledger_target(
            &allowance.counterparty,
            &allowance.allowance_id,
        ))?;
        if self.pause_at(&ledger).await?.blocks_automatic() {
            return refuse("the ledger is divergent, or was seen so and isn't confirmed");
        }
        // The record's response isn't settled while an answer to its proposal is still kept
        // to write, or while the counterparty owes a re-send of replies the bounds dropped,
        // an answer possibly among them: an earlier different answer takes the response, or
        // makes the Allowance conflicted, once it is written (Paykit spec 8.1, "Replies
        // before their record"; 8.3, "Conflicted"). A kept message or a mark that doesn't
        // read is an error, and nothing is paid.
        if self.answer_kept_to_write(record, &allowance).await? {
            return refuse("an answer to the Allowance's proposal is kept, not yet written");
        }
        if self.stored_answer_from(&allowance.counterparty).await? {
            return refuse(
                "the client stores an entry from the counterparty that may be an Allowance message",
            );
        }
        if self.resend_owed_by(&allowance.counterparty).await? {
            return refuse("the counterparty owes a re-send of replies dropped for the bounds");
        }
        let terms = proposal.terms.clone();
        if terms.to_json().is_err() {
            return refuse("the terms aren't valid");
        }
        Ok(Ok(Under {
            record_key: *record,
            allowance,
            version,
            terms,
        }))
    }

    /// What holds the repeat of a payment the executor already dispatched under the
    /// Allowance at `record` (Paykit spec 8.3): the record read from the switch now
    /// ([`Paykit::repeat_allowance_read_now`]: ended, rejected, or not read), then the rest as
    /// that read leaves it held. A repeat finishes that payment, so the Allowance handed off or
    /// conflicted since doesn't stop it ("Conflicted", "Handoff"), and neither does its consent
    /// removed for those. What can't be known holds it: the record not held or not binding,
    /// its view or its ledger's divergent or seen so and not confirmed (Paykit spec 6), its
    /// current write one the app ignores and the user hasn't confirmed or a removed app's
    /// after its `distrust_from`, its consent to be confirmed again after a restore or a
    /// `missed` item, or no consent entry at all.
    pub(crate) async fn repeat_under_allowance_held(
        &mut self,
        record: &[u8; 32],
    ) -> Result<std::result::Result<(), Refusal>> {
        if let Err(refusal) = self.repeat_allowance_read_now(record).await? {
            return Ok(Err(refusal));
        }
        let refuse = |why: &str| Ok(Err(Refusal::NotAutomatic(why.into())));
        let Some(allowance) = self.held_allowance(record).await? else {
            return refuse("the Allowance record isn't held as one that binds");
        };
        let Some(shared) = self.app.shared_record(record).await? else {
            return refuse("the Allowance record isn't held as one that binds");
        };
        if shared.divergent || shared.doubtful || self.pause_of(&shared).await?.blocks_automatic() {
            return refuse("the Allowance record was seen divergent and isn't confirmed");
        }
        if shared.flagged && !self.ignored_write_confirmed(record).await? {
            return refuse("the Allowance record's current write is one this app ignores");
        }
        if written_by_removed_app(self.app.own_identity(), &shared) {
            return refuse(
                "the Allowance record's write is a removed app's after its distrust_from",
            );
        }
        let ledger = self.app.record_key(&ledger_target(
            &allowance.counterparty,
            &allowance.allowance_id,
        ))?;
        if self.pause_at(&ledger).await?.blocks_automatic() {
            return refuse("the ledger is divergent, or was seen so and isn't confirmed");
        }
        if self.consent_to_confirm_again(record).await? {
            return refuse("the consent list holds it to be confirmed again");
        }
        Ok(Ok(()))
    }

    /// The Allowance at `record` as the switch holds it now, judged for the repeat of a
    /// payment the executor dispatched under it (Paykit spec 7.2, 8.3). A repeat may be the
    /// first call: a first dispatch the final payment check stopped after its `Paying` commit
    /// goes back to `Attempted`, and a crash before that write leaves `Paying`, which a resume
    /// repeats. So an end or a rejection the record shows, or the counterparty's end this app
    /// keeps as its stop, holds the repeat, as a read that fails, a record that isn't read
    /// from the switch now, or one that doesn't bind does: nothing is dispatched, and the
    /// attempt stays open, reserved in the ledger, its outcome not known. A handoff or a
    /// conflict doesn't hold it: they cancel no payment already started ("Handoff",
    /// "Conflicted"). A current write the app ignores leaves the copy it trusts to be judged,
    /// and [`Paykit::repeat_under_allowance_held`] holds the repeat until the user confirms
    /// it. Read before the final payment check, and again after its last await
    /// (`Paykit::automatic_dispatch_held`).
    ///
    /// # Errors
    ///
    /// A failure of the app's own store.
    pub(crate) async fn repeat_allowance_read_now(
        &mut self,
        record: &[u8; 32],
    ) -> Result<std::result::Result<(), Refusal>> {
        let refuse = |why: &str| Ok(Err(Refusal::NotAutomatic(why.into())));
        let Some(held) = self.held_allowance(record).await? else {
            return refuse("the Allowance record isn't held as one that binds");
        };
        let target = allowance_target(&held.counterparty, &held.allowance_id);
        match self.app.read_record_fresh(&target).await {
            Ok(RecordRead::Fresh(_)) => {}
            Ok(RecordRead::Kept {
                record: Some(shared),
                reason: pubky_switch_client::app::KeptReason::Refused,
            }) if shared.flagged => {}
            Ok(RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                ..
            }) => {
                return Ok(Err(Refusal::NotNow(format!(
                    "the Allowance record can't be read now: {why}"
                ))))
            }
            Ok(_) => return refuse("the Allowance record isn't read from the switch now"),
            Err(error) => {
                let error: PaykitError = error.into();
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
                return Ok(Err(Refusal::NotNow(format!(
                    "the Allowance record can't be read now: {error}"
                ))));
            }
        }
        let Some(allowance) = self.held_allowance(record).await? else {
            return refuse("the Allowance record isn't held as one that binds");
        };
        if allowance.end.is_some() || self.allowance_stopped(record).await? {
            return Ok(Err(Refusal::AllowanceEnded));
        }
        match &allowance.response {
            Some(response) if matches!(response.body(), Body::AllowanceAcceptance(_)) => Ok(Ok(())),
            Some(_) => Ok(Err(Refusal::Rejected)),
            None => refuse("the Allowance isn't accepted"),
        }
    }

    /// The executor's checks on the request and its record (Paykit spec 8.3, "Executor"):
    /// from the Allowance's counterparty, priced in its own asset and not by rates, its
    /// record not marked manual-only, naming this Allowance or none, and not divergent.
    pub(crate) async fn request_under(
        &self,
        under: &Under,
        request: &RequestRef,
        target: &ActivityTarget,
        record: &ActivityRecord,
        body: &PaymentRequest,
    ) -> Result<()> {
        if request.payee != under.allowance.counterparty {
            return Err(not_automatic("the request comes from another counterparty"));
        }
        if body.amount.asset != under.terms.asset {
            return Err(not_automatic("the request is priced in another asset"));
        }
        if body.conversion.is_some() {
            return Err(not_automatic(
                "a payment priced by rates is made only by hand",
            ));
        }
        if record.manual_only {
            return Err(not_automatic("the request is marked manual-only"));
        }
        // A one-time request dated before the block floor, shared or owed, as this app holds
        // them, or while the floor can't be read, is paid only by hand (Paykit spec 8.1,
        // "Blocked counterparties"): refused here, before anything is written. The floor is
        // read from the switch again immediately before the dispatch
        // (`Paykit::automatic_dispatch_held`), which is what a raise made since meets.
        if let Some((stored, _)) = crate::requests::request_of(record) {
            if self.floor_holds(stored).await? {
                return Err(not_automatic(
                    "the request is dated before the block floor, or the floor can't be read",
                ));
            }
        }
        if record
            .allowance_id
            .as_ref()
            .is_some_and(|named| named != &under.allowance.allowance_id)
        {
            return Err(not_automatic("the request record names another Allowance"));
        }
        let key = self.activity_key(target)?;
        if self
            .app
            .shared_record(&key)
            .await?
            .is_some_and(|held| held.divergent || held.flagged || held.doubtful)
        {
            return Err(not_automatic(
                "the request record is divergent or untrusted",
            ));
        }
        Ok(())
    }

    /// The ledger the executor builds on (Paykit spec 8.3, "Building on a version accepts
    /// it"), read from the switch now and checked; `None` for the absence the executor of
    /// the first grant takes as an empty ledger, until it has written one. A ledger it
    /// doesn't accept is refused, naming the rule, and pays nothing until its user confirms
    /// a repair ([`Paykit::ledger_repair`]).
    async fn ledger_base(&mut self, under: &Under) -> Result<Option<Ledger>> {
        match Box::pin(self.ledger_shown(under)).await? {
            LedgerShown::Accepted(ledger) => Ok(ledger),
            LedgerShown::Unaccepted { why, .. } => Err(not_automatic(&why)),
        }
    }

    /// What the executor makes of the ledger the switch shows now (Paykit spec 8.3, "Last
    /// ledger version", "Building on a version accepts it", "A grant implies a ledger"): one
    /// it builds on, which it then keeps as the last version, or one it doesn't accept, with
    /// what the switch shows of it. A rollback raises the alarm and is the error, as are a
    /// read that fails or isn't of the switch now, a ledger divergent or seen so and not
    /// confirmed, and a write the app doesn't hold: none of them is repaired.
    async fn ledger_shown(&mut self, under: &Under) -> Result<LedgerShown> {
        let own = self.identity();
        let target = ledger_target(&under.allowance.counterparty, &under.allowance.allowance_id);
        let ledger_key = self.app.record_key(&target)?;
        let kept = self.kept_ledger(&under.record_key).await?;
        let generation = self.app.own_identity().generation();
        // After a handoff or a repair, the ledger write the executor checks what it builds
        // on against (Paykit spec 8.3, "Handoff", "After a confirmation"), beside its kept
        // version: the repair's own write, and the write a counting `ledger_handoff` names.
        let mut mark = self.ledger_base_mark(&under.record_key).await?;
        let mut extras: Vec<BuildOn> = Vec::new();
        let mut take_handoff = false;
        if let Some(wait) = mark.as_ref().and_then(|m| m.handoff) {
            match self.settle_handoff(under, &wait, kept.as_ref().map(|(k, _)| k)) {
                Settled::Waiting => {}
                Settled::Take(named) => {
                    extras.push(named);
                    take_handoff = true;
                }
                Settled::Kept => {
                    mark = self.handoff_settled(&under.record_key, mark, None).await?;
                }
                Settled::Ignored(writer) => {
                    // Never silent: the host shows the user the ignored write and its writer.
                    let notice = self.handoff_notice(under, writer);
                    mark = self
                        .handoff_settled(&under.record_key, mark, Some(notice))
                        .await?;
                }
                Settled::Unverified => {
                    // One that doesn't verify, or doesn't read, is ignored and the host told,
                    // with the writer of the Allowance record's write that carries it; the
                    // executor goes on waiting for one that counts.
                    if let Some(writer) = self
                        .app
                        .shared_record(&under.record_key)
                        .await?
                        .map(|shared| shared.writer)
                    {
                        self.raise_handoff_notice(under, writer).await?;
                    }
                }
                Settled::Rollback(named_at) => {
                    if named_at != generation {
                        return Ok(unaccepted(EARLIER_GENERATION, ShownContent::Nothing));
                    }
                    return Err(self
                        .raise_ledger_alarm(
                            &under.record_key,
                            "its first write since the handoff built on is older than the previous executor's last",
                        )
                        .await);
                }
            }
        }
        if let Some(repair) = mark.as_ref().and_then(|m| m.repair) {
            extras.push(BuildOn::Repaired(repair));
        }
        let shared = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(shared) => Some(shared),
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::NotFound,
                record,
            } => {
                // A missing ledger record against the version kept, or against the write
                // named or repaired, is a rollback, but after a delete the executor holds at
                // or after it (Paykit spec 8.3, "Last ledger version"; Paykit wire 5.5).
                let deleted = record.as_ref().and_then(|held| {
                    matches!(held.content, RecordContent::Deleted).then_some(held.version)
                });
                if let Some((kept, _)) = &kept {
                    if !deleted.is_some_and(|at| at >= kept.version()) {
                        if kept.generation != generation {
                            return Ok(unaccepted(EARLIER_GENERATION, ShownContent::Nothing));
                        }
                        return Err(self
                            .raise_ledger_alarm(
                                &under.record_key,
                                "is missing against the version kept",
                            )
                            .await);
                    }
                }
                for extra in &extras {
                    if !deleted.is_some_and(|at| extra.deleted_since(at)) {
                        if extra.generation() != generation {
                            return Ok(unaccepted(EARLIER_GENERATION, ShownContent::Nothing));
                        }
                        return Err(self
                            .raise_ledger_alarm(
                                &under.record_key,
                                "is missing against the write named or repaired",
                            )
                            .await);
                    }
                }
                None
            }
            // The switch's current write is one the client refused (Switch spec 8.2): the copy
            // the app trusts is held, the refused write's content isn't, and the app's next
            // write builds on the refused write's version.
            RecordRead::Kept {
                record: Some(shared),
                reason: pubky_switch_client::app::KeptReason::Refused,
            } if shared.flagged => Some(shared),
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                ..
            } => return Err(Refusal::SwitchUnavailable(why).into()),
            RecordRead::Kept { .. } => {
                return Err(not_automatic("the ledger isn't read from the switch now"))
            }
            _ => return Err(PaykitError::corrupt()),
        };
        let first_grant = under.allowance.grants.len() == 1;
        let Some(shared) = shared else {
            // A grant implies a ledger: only the first grant's executor, while its own
            // journal, not one restored from a backup, shows the empty ledger not yet sent,
            // takes the absence as empty (Paykit spec 8.3).
            if kept.is_none()
                && mark.is_none()
                && first_grant
                && self.empty_ledger_unsent(&under.record_key).await?
            {
                return Ok(LedgerShown::Accepted(None));
            }
            return Ok(unaccepted(
                "usage history lost: the ledger is missing",
                ShownContent::Nothing,
            ));
        };
        let content = self.shown_content(under, &shared, &ledger_key)?;
        // The kept version first (Paykit spec 8.3, "Last ledger version"), whatever else the
        // write shown is, a write divergent, refused or doubtful included: it must be that
        // version, by item hash, or one signed as built on that version or a later one. Any
        // other write, a delete included, is a rollback, raises the alarm and is never
        // repaired; a write that is one of those but fails a check by policy, below, only
        // pauses. A write the client refused (Switch spec 8.2) isn't held, neither its item
        // nor its signed base: the version the client follows, the refused write's as shown,
        // is judged instead, and one before the version kept is a rollback. The write named
        // or repaired is judged the same way.
        let op = self.held_op(&ledger_key).await?;
        let refused = if shared.flagged {
            Some(
                self.app
                    .flagged_writes()
                    .await?
                    .into_iter()
                    .filter(|flagged| flagged.key == ledger_key)
                    .map(|flagged| flagged.version)
                    .max()
                    .ok_or_else(|| not_automatic("the refused ledger write isn't held"))?,
            )
        } else {
            None
        };
        let held_op = || {
            op.as_ref()
                .ok_or_else(|| not_automatic("the ledger's write isn't held"))
        };
        if let Some((kept, _)) = &kept {
            let base = BuildOn::Named {
                version: kept.version(),
                item_hash: kept.item_hash,
            };
            let rollback = match refused {
                Some(refused) => base.older_refused(refused),
                None => base.older(held_op()?, None)?,
            };
            if rollback {
                if kept.generation != generation {
                    return Ok(unaccepted(EARLIER_GENERATION, content));
                }
                return Err(self
                    .raise_ledger_alarm(
                        &under.record_key,
                        "shown is neither the version kept nor built on it",
                    )
                    .await);
            }
        }
        for extra in &extras {
            let rollback = match refused {
                Some(refused) => extra.older_refused(refused),
                None => extra.older(held_op()?, shared.content.value())?,
            };
            if rollback {
                if extra.generation() != generation {
                    return Ok(unaccepted(EARLIER_GENERATION, content));
                }
                return Err(self
                    .raise_ledger_alarm(
                        &under.record_key,
                        "shown is neither the write named or repaired nor built on it",
                    )
                    .await);
            }
        }
        if shared.divergent || shared.flagged || shared.doubtful {
            // The view is taken first, so a ledger divergent now is marked, and stays paused
            // until the user confirms it, also once a newer write clears the view. A write
            // refused or doubtful but not divergent is one the executor doesn't accept.
            let paused = self.pause_of(&shared).await?.blocks_automatic();
            if shared.divergent || paused {
                return Err(not_automatic("the ledger is divergent or untrusted"));
            }
            return Ok(unaccepted("the ledger is divergent or untrusted", content));
        }
        if self.pause_of(&shared).await?.blocks_automatic() {
            return Err(not_automatic(
                "the ledger was seen divergent and the user hasn't confirmed it",
            ));
        }
        let op = op.ok_or_else(|| not_automatic("the ledger's write isn't held"))?;
        if written_by_removed_app(self.app.own_identity(), &shared) {
            return Ok(unaccepted(
                "the ledger's write is a removed app's after its distrust_from",
                content,
            ));
        }
        let ledger = match content {
            ShownContent::Ledger(ledger) => *ledger,
            other => {
                return Ok(unaccepted(
                    match &shared.content {
                        RecordContent::Value(value) if !value.is_empty() => {
                            "the ledger doesn't bind to its Allowance"
                        }
                        _ => "usage history lost: the ledger is deleted or unread",
                    },
                    other,
                ))
            }
        };
        // A ledger that was at its key at the first grant is one the executor didn't accept
        // (Paykit spec 8.3, "A grant implies a ledger"), whoever wrote it.
        if kept.is_none() && self.ledger_unaccepted(&under.record_key).await? {
            return Ok(unaccepted(
                "the ledger at its key before the first grant isn't one this executor accepted",
                ShownContent::Ledger(Box::new(ledger)),
            ));
        }
        if let Err(why) = written_by_executor(
            &under.allowance,
            &own,
            &under.record_key,
            under.version,
            &op,
            shared.writer,
        ) {
            return Ok(unaccepted(why, ShownContent::Ledger(Box::new(ledger))));
        }
        // What it builds on (Paykit spec 8.3, "Last ledger version"): the write named or
        // repaired, or one built on it or later (checked above); else its kept version, or one
        // built on it or later that counts no less. One that holds no ledger version builds
        // on none the service shows it alone: after a handoff it waits for the write named,
        // and otherwise, restored from a backup that holds none or the first grant's executor
        // whose own journal no longer shows the empty ledger not yet sent, it builds only on
        // a ledger its user confirms by the repair.
        // After a handoff, nothing is built on until the handoff settles or the user confirms
        // the repair, whatever the executor holds (Paykit spec 8.3, "Handoff"): a version it
        // kept before the grant, or from an earlier term of its own, may predate the previous
        // executor's last.
        let awaiting = !take_handoff
            && mark
                .as_ref()
                .and_then(|m| m.handoff)
                .is_some_and(|wait| wait.first.is_none());
        if awaiting {
            return Ok(unaccepted(
                AWAITING_HANDOFF,
                ShownContent::Ledger(Box::new(ledger)),
            ));
        }
        let repaired = extras.iter().any(|e| matches!(e, BuildOn::Repaired(_)));
        match &kept {
            // After a restore that isn't clean, the version the backup held may predate
            // writes this app made since: it builds only on a ledger its user confirms by the
            // repair (Paykit spec 8.3, "Last ledger version").
            Some((kept, _))
                if kept.restored && !self.restore_keeps_ledger_version() && !repaired =>
            {
                return Ok(unaccepted(
                    RESTORED_VERSION,
                    ShownContent::Ledger(Box::new(ledger)),
                ))
            }
            Some((kept, held)) if !kept.restored || self.restore_keeps_ledger_version() => {
                let item = op.item_hash().map_err(|_| PaykitError::corrupt())?;
                if item != kept.item_hash {
                    // Built on the kept version or a later one (checked above): judged by the
                    // ledger rules against the kept content, whatever else it is checked
                    // against, a version that counts less ignored and only pausing the
                    // Allowance (Paykit spec 8.3, "From one version to the next").
                    let at = shared
                        .written_at_ms
                        .or(self.service_time())
                        .ok_or_else(|| not_automatic("no service time verified"))?;
                    if let Err(error) =
                        check_successor_since(&under.terms, held, kept.written_at, &ledger, at)
                    {
                        let why = match error {
                            paykit_lib::Error::Invalid(why) if why.contains("too many periods") => {
                                "too many periods since the version kept to judge it"
                            }
                            _ => "the ledger counts less than the version kept",
                        };
                        return Ok(unaccepted(why, ShownContent::Ledger(Box::new(ledger))));
                    }
                }
            }
            Some(_) => {}
            None if extras.is_empty() => {
                return Ok(unaccepted(
                    "the executor holds no ledger version to build on",
                    ShownContent::Ledger(Box::new(ledger)),
                ))
            }
            None => {}
        }
        if ledger.check_valid(&under.terms).is_err() {
            return Ok(unaccepted(
                "the ledger isn't valid",
                ShownContent::Ledger(Box::new(ledger)),
            ));
        }
        // The write named, or one built on it, is read and kept: the handoff is settled in
        // the same commit.
        self.keep_ledger_settling(&under.record_key, &ledger_key, take_handoff)
            .await?;
        Ok(LedgerShown::Accepted(Some(ledger)))
    }

    /// What `shared`, the ledger record of `under` at `ledger_key`, shows for a repair: a
    /// value that reads as a ledger and binds to its Allowance (Paykit wire 5.5, "Binding"),
    /// else nothing, or unknown while the value isn't open for now. A ledger that reads but
    /// isn't valid is shown as it reads: the repair leaves out what isn't of the terms.
    fn shown_content(
        &self,
        under: &Under,
        shared: &SharedRecord,
        ledger_key: &[u8; 32],
    ) -> Result<ShownContent> {
        let value = match &shared.content {
            RecordContent::Value(value) => value,
            RecordContent::Unreadable(pubky_switch_client::app::Unopened::KeyNotHeld {
                ..
            }) => return Ok(ShownContent::Unknown),
            _ => return Ok(ShownContent::Nothing),
        };
        let Some(ledger) = Ledger::from_json(value).ok().filter(|l| {
            l.counterparty == under.allowance.counterparty
                && l.allowance_id == under.allowance.allowance_id
        }) else {
            return Ok(ShownContent::Nothing);
        };
        if self
            .app
            .record_key(&ledger_target(&ledger.counterparty, &ledger.allowance_id))?
            != *ledger_key
        {
            return Ok(ShownContent::Nothing);
        }
        Ok(ShownContent::Ledger(Box::new(ledger)))
    }

    /// The executor's batch beyond the `attempt` (Paykit spec 8.3 steps 2 and 3): usage
    /// recomputed from the ledger and the claims at the service's latest time `at`, the
    /// terms and the limits checked for the request's `amount` paid to `destination`, then
    /// the ledger with the new entry, under `record_version(Allowance record)` and
    /// `server_time_in(at, end)`, and the request record naming the Allowance and showing the
    /// payment as being handled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn executor_ops(
        &mut self,
        under: &Under,
        target: &ActivityTarget,
        record: &ActivityRecord,
        obligation: [u8; 32],
        attempt: &AttemptId,
        amount: &PaymentAmount,
        destination: &Destination,
    ) -> Result<ExecutorOps> {
        // Doubtful obligations (Paykit spec 8.3): the claim is read first, before the owed
        // empty ledger is written or the ledger read is kept, so a read that fails writes
        // nothing and is the error, never taken as a claim without attempts. The batch's
        // `attempt` builds on the claim's step read before, so an attempt added since makes
        // it fail.
        let attempts: Option<Vec<[u8; 16]>> = self
            .app
            .read_claim(&obligation)
            .await?
            .map(|read| read.view.attempts.iter().map(|a| a.attempt_id).collect());
        // The first grant's empty ledger, while owed, goes before the first payment.
        self.write_owed_empty_ledger(under).await?;
        let base = self.ledger_base(under).await?;
        // Whether this batch carries the ledger's first write: marked sent just before it
        // goes, after every check, so a refusal here leaves the empty ledger owed.
        let first_ledger_write = base.is_none();
        let ledger = base.unwrap_or_else(|| {
            Ledger::empty(
                under.allowance.counterparty,
                under.allowance.allowance_id.clone(),
            )
        });
        // An occurrence whose claim holds an attempt the ledger names neither in an entry nor
        // in `totals.failed`, one made by hand or under another Allowance, whatever its
        // outcome, even one that never reached the payment method, is paid again only by
        // hand. Judged against the ledger built on, so after the owed empty ledger's write
        // and the ledger read's keep, which are the Allowance's, not the occurrence's:
        // nothing of the occurrence is written.
        if attempts
            .as_deref()
            .is_some_and(|attempts| attempted_elsewhere(&obligation, attempts, &ledger))
        {
            return Err(not_automatic(
                "the occurrence was attempted by hand or under another Allowance: it is paid only by hand",
            ));
        }
        let claims = self.ledger_claims(&ledger).await?;
        let at = self
            .service_time()
            .ok_or_else(|| not_automatic("no service time verified"))?;
        if self.now().abs_diff(at) > CLOCK_TOLERANCE_MS {
            return Err(not_automatic(
                "the device clock and the service's differ by more than the tolerance",
            ));
        }
        under
            .terms
            .check_payment(amount, &destination.endpoint_id()?, at)
            .map_err(|refused| not_automatic(&format!("the terms refuse it: {refused:?}")))?;
        let used = usage(&under.terms, &ledger, &claims.usage, at)
            .map_err(|_| not_automatic("usage can't be counted"))?;
        check_limits(&under.terms, &used, &amount.value)
            .map_err(|limit| not_automatic(&format!("the {} limit has no room", limit.as_str())))?;
        let end = under
            .terms
            .entry_window_end(at)
            .map_err(|_| not_automatic("the entry's window can't be written"))?;
        // The outcomes the claims show go into the entries in this batch, and the ledger is
        // compacted once it nears the size limit (Paykit spec 8.3, "Outcomes in the ledger",
        // "Compaction"): usage counted above is the same over the ledger written.
        let ledger = next_ledger(
            &under.terms,
            &ledger,
            &claims.outcomes,
            at,
            LedgerEntry {
                obligation,
                attempt_id: *attempt,
                value: amount.value.clone(),
                asset: amount.asset.clone(),
                at,
                outcome: None,
            },
        )
        .map_err(not_automatic)?;
        let value = ledger.to_json()?;
        let lt = ledger_target(&under.allowance.counterparty, &under.allowance.allowance_id);
        let conditions = vec![
            Condition::RecordVersion {
                key: under.record_key,
                version: under.version,
            },
            Condition::ServerTimeIn {
                start: Some(at),
                end: Some(end),
            },
        ];
        let mut entries = vec![self.app.record_op(&lt, Some(&value), conditions).await?];
        let mut next = record.clone();
        next.allowance_id = Some(under.allowance.allowance_id.clone());
        next.status = Some("paying".into());
        let value = next.to_json()?;
        entries.push(
            self.app
                .record_op(&target.record_target(), Some(&value), Vec::new())
                .await?,
        );
        Ok(ExecutorOps {
            entries,
            record_key: under.record_key,
            ledger: lt,
            first_ledger_write,
        })
    }

    /// After an executor batch is applied: its ledger write is kept as the last version
    /// ([`Paykit::keep_own_ledger`]). A failure of the app's own store is the call's error:
    /// the attempt is then resolved as any other whose dispatch didn't start.
    pub(crate) async fn executor_applied(
        &mut self,
        record: [u8; 32],
        ledger: RecordTarget,
    ) -> Result<()> {
        self.keep_own_ledger(&record, &ledger).await
    }

    /// Keep the ledger write this app just had applied as the last version of the Allowance
    /// at `record`: from the copy the app holds when the answer left it there, else from a
    /// read of the record. A read that fails keeps nothing, which is safe: the first
    /// grant's empty ledger is marked sent before any ledger write goes, so a missing ledger
    /// is then usage history lost, and a ledger shown has no kept version to be built on.
    /// Only a failure of the app's own store is an error.
    async fn keep_own_ledger(&mut self, record: &[u8; 32], ledger: &RecordTarget) -> Result<()> {
        let ledger_key = self.app.record_key(ledger)?;
        if self.app.shared_record(&ledger_key).await?.is_none() {
            if let Err(error) = self.app.read_record(ledger).await {
                let error: PaykitError = error.into();
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
                return Ok(());
            }
        }
        self.keep_ledger(record, &ledger_key).await
    }

    /// What the claims, the own journal and the payer records show of `ledger`'s attempts
    /// (Paykit spec 8.3, "Outcomes in the ledger", "`failed` isn't final"; Paykit wire 5.5,
    /// "Counting" and "Outcomes written"), read again for every entry but a `done` one and
    /// for every compacted failed attempt, so a later `done` over a `failed` is seen:
    ///
    /// - an attempt this app's own journal shows paid, by its payment method or by the
    ///   payee's receipt, is `done` whatever its claim shows;
    /// - otherwise its claim's state, as the claim shows it;
    /// - a `failed` one, or one without an outcome whose claim shows `failed` or is gone, is
    ///   `done` when this
    ///   app's copy of the request's payer record holds a proof of that attempt naming the
    ///   Allowance, whichever executor made it, also once the claim is gone;
    /// - and a `failed` one whose claim is gone, with no journal entry of this app's, while
    ///   that payer record can't be read, counts as `done` for usage alone: whether it was
    ///   paid is unknown, so it isn't released, and no `done` is written for it.
    ///
    /// A read that fails is the error: never taken as a claim gone, which counts as spent,
    /// or as one that releases.
    async fn ledger_claims(&mut self, ledger: &Ledger) -> Result<LedgerClaims> {
        self.ledger_claims_from(ledger, &mut ClaimReads::default())
            .await
    }

    /// [`Paykit::ledger_claims`], each claim read once into `reads`.
    async fn ledger_claims_from(
        &mut self,
        ledger: &Ledger,
        reads: &mut ClaimReads,
    ) -> Result<LedgerClaims> {
        let mut out = LedgerClaims::default();
        let open: Vec<([u8; 32], AttemptId, Option<EntryOutcome>)> = ledger
            .entries
            .iter()
            .filter(|e| e.outcome != Some(EntryOutcome::Done))
            .map(|e| (e.obligation, e.attempt_id, e.outcome))
            .chain(
                ledger
                    .failed
                    .iter()
                    .map(|f| (f.obligation, f.attempt_id, Some(EntryOutcome::Failed))),
            )
            .collect();
        let mut records: Option<HashMap<[u8; 32], ActivityRecord>> = None;
        for (obligation, attempt_id, outcome) in open {
            let journaled = self.journal_entry_on(&attempt_id, &obligation).await?;
            if journaled
                .as_ref()
                .is_some_and(|entry| entry.rail_proof.is_some() || entry.receipted)
            {
                out.both(attempt_id, ClaimState::Done);
                continue;
            }
            let shown = match self.claim_view(reads, &obligation).await? {
                Some(view) => claim_state(&view, &attempt_id),
                None => None,
            };
            if let Some(state) = shown {
                out.both(attempt_id, state);
            }
            // A `failed` entry, or one without an outcome whose claim shows `failed` or is
            // gone: its payer record may show it paid all the same.
            let failed = match outcome {
                Some(_) => shown != Some(ClaimState::Done),
                None => matches!(shown, Some(ClaimState::Failed { .. })),
            };
            if !failed && !(outcome.is_none() && shown.is_none()) {
                continue;
            }
            if records.is_none() {
                records = Some(self.payer_records_of(&ledger.counterparty).await?);
            }
            let record = records.as_ref().and_then(|r| r.get(&obligation));
            let evidence = payer_evidence(record, &attempt_id, &ledger.allowance_id);
            let (counted, written) =
                evidence_counts(evidence, failed, shown.is_none(), journaled.is_some());
            if let Some(state) = counted {
                out.usage.insert(attempt_id, state);
            }
            if let Some(state) = written {
                out.outcomes.insert(attempt_id, state);
            }
        }
        Ok(out)
    }

    /// The claim of `obligation` as the switch shows it, read once into `reads`: `None` for
    /// one the switch doesn't hold. A read that fails is the error.
    async fn claim_view(
        &mut self,
        reads: &mut ClaimReads,
        obligation: &[u8; 32],
    ) -> Result<Option<ClaimView>> {
        if let Some(view) = reads.views.get(obligation) {
            return Ok(view.clone());
        }
        let view = self.app.read_claim(obligation).await?.map(|read| read.view);
        reads.views.insert(*obligation, view.clone());
        Ok(view)
    }

    /// This app's copies of its payer records of `counterparty`'s one-time requests that
    /// read, by the obligation of their request: a record that doesn't read, or waits for
    /// its scope key, isn't among them, so what it holds is unknown.
    async fn payer_records_of(
        &self,
        counterparty: &[u8; 32],
    ) -> Result<HashMap<[u8; 32], ActivityRecord>> {
        let own = self.identity();
        let mut records = HashMap::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let Some(record) = crate::activity::read(
                held.content.value(),
                &own,
                paykit_lib::records::ActivitySide::Payer,
            ) else {
                continue;
            };
            if record.counterparty != *counterparty {
                continue;
            }
            let Some(request) = &record.request else {
                continue;
            };
            records.insert(self.obligation(counterparty, &request.hash(), 0), record);
        }
        Ok(records)
    }

    /// Whether the journal entry `entry` still decides its ledger entry, so it isn't pruned:
    /// a payment under an Allowance this app executes now, which the entry shows made (its
    /// method's proof, or the payee's receipt), while the ledger version kept doesn't show
    /// its entry `done` or moved into the totals (Paykit spec 8.3, "`failed` isn't final").
    /// Pruned, a `failed` there would release an amount that was paid.
    pub(crate) async fn journal_decides_ledger(
        &mut self,
        entry: &crate::journal::JournalEntry,
    ) -> Result<bool> {
        if entry.rail_proof.is_none() && !entry.receipted {
            return Ok(false);
        }
        let Some(allowance_id) = entry.allowance()? else {
            return Ok(false);
        };
        let record = self
            .app
            .record_key(&allowance_target(&entry.payee, &allowance_id))?;
        if self.outcome_under(&record).await?.is_none() {
            return Ok(false);
        }
        let Some((_, ledger)) = self.kept_ledger(&record).await? else {
            return Ok(false);
        };
        let attempt = entry.attempt()?;
        Ok(ledger
            .entries
            .iter()
            .any(|e| e.attempt_id == attempt && e.outcome != Some(EntryOutcome::Done))
            || ledger.failed.iter().any(|f| f.attempt_id == attempt))
    }

    /// The Allowance at `record` as the executor writes outcomes into its ledger (Paykit
    /// spec 8.3, "Outcomes in the ledger", "Handoff", "Conflicted"): only while this app holds
    /// its current grant, since only the current executor writes the ledger, whether or not
    /// the Allowance has ended or is conflicted since, as its attempts still finish. `None`
    /// for a record not held as one that binds, divergent, untrusted or paused (Paykit spec
    /// 6), its current write a removed app's after its `distrust_from` or one the app
    /// ignores and its user hasn't confirmed, for a grant of another app, and for terms not
    /// valid: the outcome then goes on its claim alone, which a later write copies.
    async fn outcome_under(&mut self, record: &[u8; 32]) -> Result<Option<Under>> {
        if !self.holds("paykit_allowance_execute") {
            return Ok(None);
        }
        let Some(allowance) = self.held_allowance(record).await? else {
            return Ok(None);
        };
        let Some(shared) = self.app.shared_record(record).await? else {
            return Ok(None);
        };
        if shared.divergent
            || shared.doubtful
            || self.pause_of(&shared).await?.blocks_automatic()
            || written_by_removed_app(self.app.own_identity(), &shared)
            || shared.flagged && !self.ignored_write_confirmed(record).await?
        {
            return Ok(None);
        }
        let version = if shared.flagged {
            self.latest_ignored(record)
                .await?
                .ok_or_else(PaykitError::corrupt)?
        } else {
            shared.version
        };
        let own = self.identity();
        if allowance
            .current_grant(&own)
            .is_none_or(|(grant, _)| grant.app_id != self.app_id())
        {
            return Ok(None);
        }
        let Body::AllowanceProposal(proposal) = allowance.proposal.body() else {
            return Err(PaykitError::corrupt());
        };
        let terms = proposal.terms.clone();
        if terms.to_json().is_err() {
            return Ok(None);
        }
        Ok(Some(Under {
            record_key: *record,
            allowance,
            version,
            terms,
        }))
    }

    /// The write of `next`, the ledger built on the one `ledger_base` accepted for `under`,
    /// under `record_version(Allowance record)` (Paykit spec 8.3, "Building on a version
    /// accepts it"): its `record.put` carries that write's version as `expected_version`.
    async fn ledger_write(&mut self, under: &Under, next: &Ledger) -> Result<LedgerWrite> {
        let value = next.to_json()?;
        if value.len() > MAX_LEDGER_VALUE {
            return Err(not_automatic("the ledger doesn't fit its record"));
        }
        let lt = ledger_target(&under.allowance.counterparty, &under.allowance.allowance_id);
        let write = self
            .app
            .record_op(
                &lt,
                Some(&value),
                vec![Condition::RecordVersion {
                    key: under.record_key,
                    version: under.version,
                }],
            )
            .await?;
        Ok(LedgerWrite {
            write,
            record_key: under.record_key,
            ledger: lt,
        })
    }

    /// The ledger write that goes with the batch writing `entry`'s outcome on its claim
    /// (Paykit spec 8.3, "Outcomes in the ledger"): the attempt's entry `done` for a payment
    /// made, and `failed` when `fails`, the batch writing it `failed` as the attempt's owner.
    /// `None` when the entry names no Allowance, this app isn't its current executor, the
    /// ledger isn't one it builds on, holds no entry for the attempt or holds that outcome
    /// already, and for any failure but of the app's own store: the outcome then goes on its
    /// claim alone (Paykit spec 8.3, "Building on a version accepts it"), and a later write
    /// copies it from there.
    pub(crate) async fn ledger_outcome_write(
        &mut self,
        entry: &crate::journal::JournalEntry,
        paid: bool,
        fails: bool,
    ) -> Result<Option<LedgerWrite>> {
        let outcome = match (paid, fails) {
            (true, _) => EntryOutcome::Done,
            (false, true) => EntryOutcome::Failed,
            (false, false) => return Ok(None),
        };
        let Some(allowance_id) = entry.allowance()? else {
            return Ok(None);
        };
        let attempt = entry.attempt()?;
        let built = async {
            let record = self
                .app
                .record_key(&allowance_target(&entry.payee, &allowance_id))?;
            let Some(under) = self.outcome_under(&record).await? else {
                return Ok(None);
            };
            let Some(mut ledger) = self.ledger_base(&under).await? else {
                return Ok(None);
            };
            let Some(held) = ledger.entries.iter_mut().find(|e| e.attempt_id == attempt) else {
                return Ok(None);
            };
            match (held.outcome, outcome) {
                (Some(EntryOutcome::Done), _) => return Ok(None),
                (Some(EntryOutcome::Failed), EntryOutcome::Failed) => return Ok(None),
                _ => held.outcome = Some(outcome),
            }
            self.ledger_write(&under, &ledger).await.map(Some)
        };
        match built.await {
            Err(error) if own_store_failure(&error) => Err(error),
            Err(_) => Ok(None),
            Ok(write) => Ok(write),
        }
    }

    /// Write into the ledger of every Allowance this app executes the outcomes its attempts'
    /// claims show now (Paykit spec 8.3, "Outcomes in the ledger"; Paykit wire 5.5, "Outcomes
    /// written"), so usage doesn't depend on claims the service deletes later: a previous
    /// executor's outcomes, a `done` a receipt wrote over a `failed`, and a compacted failed
    /// attempt whose claim shows `done`, moved into the totals once. Run at every resume.
    /// An Allowance whose ledger the executor doesn't build on now, or whose write fails, is
    /// left as it is for the next resume, the others go on; a rollback found raises the
    /// alarm and is passed by. Only a failure of the app's own store is an error.
    pub(crate) async fn record_ledger_outcomes(&mut self) -> Result<()> {
        if !self.holds("paykit_allowance_execute") {
            return Ok(());
        }
        // Every Allowance on the consent list, whether or not a version of its ledger is kept
        // yet, such as after a handoff to an executor that never read it: only one whose
        // current grant is this app's is written ([`Paykit::outcome_under`]).
        let consents = self
            .local_scan::<ConsentEntry>(crate::state::key::CONSENTS)
            .await?;
        for (rest, _) in consents {
            let record = crate::allowances::consent_record(&rest)?;
            match self.copy_outcomes(&record).await {
                Err(error) if own_store_failure(&error) => return Err(error),
                _ => {}
            }
            // As the previous executor, the `ledger_handoff` it owes: a write that fails is
            // tried again at the next resume.
            match Box::pin(self.hand_ledger_on(&record)).await {
                Err(error) if own_store_failure(&error) => return Err(error),
                _ => {}
            }
        }
        Ok(())
    }

    /// The grant history entry this app owes a `ledger_handoff` for, as the previous executor
    /// of `allowance` (Paykit spec 8.3, "Handoff"; Paykit wire 5.4): the entry just after this
    /// app's last one, of another app, with every later entry of that same app, so the
    /// handoff is the one to the current executor; and the record holds no `ledger_handoff`
    /// that counts for it. `None` once another app's grant follows, and for a history that
    /// isn't usable.
    fn handoff_owed(&self, allowance: &AllowanceRecord) -> Option<RecordVersion> {
        let own = self.identity();
        if !allowance.history_usable() {
            return None;
        }
        let (current, _) = allowance.current_grant(&own)?;
        if current.app_id == self.app_id() {
            return None;
        }
        // The first entry of the current executor's last run of entries: a grant it signs
        // again is no handoff.
        let mut at = allowance.grants.len().checked_sub(1)?;
        while at > 0
            && allowance
                .grants
                .get(at - 1)
                .is_some_and(|entry| entry.grant.app_id == current.app_id)
        {
            at -= 1;
        }
        let answered = allowance.grants.get(at)?;
        let before = allowance.grants.get(at.checked_sub(1)?)?;
        if before.grant.app_id != self.app_id() {
            return None;
        }
        // The executor this app handed on to is the one named just after it.
        if allowance.executor_at(&own, answered.built_on).ok()?? != self.app_id() {
            return None;
        }
        let counts = allowance
            .ledger_handoff_counts(&own)
            .ok()
            .flatten()
            .is_some_and(|handoff| handoff.grant == answered.built_on);
        (!counts).then_some(answered.built_on)
    }

    /// Write, as the previous executor of the Allowance at `record`, the `ledger_handoff`
    /// naming its last ledger version for the grant that handed the Allowance on (Paykit spec
    /// 8.3, "Handoff"; Paykit wire 5.4), at the record version it read, signed with its own
    /// certificate; it makes no attempt under the Allowance after it, as the current grant
    /// is another app's. Every ledger write it sent has an answer first: each was conditioned
    /// on an Allowance record version before the grant, so the ledger read now shows the
    /// last of them that applied, which it keeps when it is its own write built on its kept
    /// version or later. None is written while it holds no ledger version, or only one a
    /// restore installed, which may predate its own later writes; a grant this app signs
    /// again is no handoff. A record changed meanwhile is read again, a bounded number of
    /// times; any other failure leaves it for the next resume.
    async fn hand_ledger_on(&mut self, record: &[u8; 32]) -> Result<()> {
        let mut tries = 0;
        loop {
            match self.write_ledger_handoff(record).await {
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= GRANT_TRIES {
                        return Err(crate::records::conflict(error));
                    }
                }
                other => return other,
            }
        }
    }

    /// The version of the first entry of this app's run of grants that the current grant
    /// handed on from ([`Paykit::handoff_owed`]).
    fn handed_on_run(&self, allowance: &AllowanceRecord) -> Option<(u64, u64)> {
        let answered = self.handoff_owed(allowance)?;
        let at = allowance
            .grants
            .iter()
            .position(|entry| entry.built_on == answered)?;
        run_start(allowance, at.checked_sub(1)?).map(|v| (v.generation, v.version))
    }

    async fn write_ledger_handoff(&mut self, record: &[u8; 32]) -> Result<()> {
        let own = self.identity();
        let Some(held) = self.held_allowance(record).await? else {
            return Ok(());
        };
        if self.handoff_owed(&held).is_none() {
            return Ok(());
        }
        let Some((kept, _)) = self.kept_ledger(record).await? else {
            return Ok(());
        };
        // Only a version it wrote, or took by a `ledger_handoff`, in the run of grants that
        // handed the Allowance on: never one it only read, or a restore installed, which may
        // predate writes of its own or of an executor between (a second handoff before the
        // mark leaves the repair).
        if kept.restored || kept.term.is_none() || kept.term != self.handed_on_run(&held) {
            return Ok(());
        }
        // Every ledger write it sent has a verified answer first: none of its batches under
        // the Allowance is still kept to send. Each was conditioned on an Allowance record
        // version before the grant, so once answered, none applies later.
        let not_now = |why: &str| -> PaykitError { Refusal::NotNow(why.into()).into() };
        for entry in self.journal().await? {
            let Some(allowance_id) = entry.allowance()? else {
                continue;
            };
            if self
                .app
                .record_key(&allowance_target(&entry.payee, &allowance_id))?
                == *record
                && self.attempt_still_kept(&entry).await?
            {
                return Err(not_now(
                    "a ledger write of the Allowance is still kept to send",
                ));
            }
        }
        // Its last ledger write applied, read now, with an authenticated version: the later
        // of its kept version and the write the switch shows, when that is its own write
        // built on the kept version or later, which a keep may not have stored yet.
        let lt = ledger_target(&held.counterparty, &held.allowance_id);
        let ledger_key = self.app.record_key(&lt)?;
        let shown = match self.app.read_record_fresh(&lt).await? {
            RecordRead::Fresh(shared) if shared.authenticated && !shared.flagged => shared,
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                ..
            } => return Err(Refusal::SwitchUnavailable(why).into()),
            _ => {
                return Err(not_now(
                    "the ledger isn't read from the switch now with an authenticated version",
                ))
            }
        };
        let op = self
            .held_op(&ledger_key)
            .await?
            .ok_or_else(|| not_now("the ledger's write isn't held"))?;
        let kept_base = BuildOn::Named {
            version: kept.version(),
            item_hash: kept.item_hash,
        };
        // Its own write is one it wrote in that run: every ledger write is conditioned on the
        // Allowance record at the writer's grant.
        let (ledger, item_hash) =
            if op.body().app_id == Some(self.app_id()) && !kept_base.older(&op, None)? {
                (
                    shown.version,
                    op.item_hash().map_err(|_| PaykitError::corrupt())?,
                )
            } else {
                (kept.version(), kept.item_hash)
            };
        let target = allowance_target(&held.counterparty, &held.allowance_id);
        let shared = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(shared) => shared,
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                ..
            } => return Err(Refusal::SwitchUnavailable(why).into()),
            _ => return Err(Refusal::NotNow("the Allowance record isn't read now".into()).into()),
        };
        if shared.divergent || shared.flagged || shared.doubtful {
            return Err(
                Refusal::NotNow("the Allowance record is divergent or flagged".into()).into(),
            );
        }
        let Some(held) = self.held_allowance(record).await? else {
            return Ok(());
        };
        let Some(grant) = self.handoff_owed(&held) else {
            return Ok(());
        };
        let fields = paykit_lib::grant::handoff_fields(
            &own,
            &held.counterparty,
            &held.allowance_id,
            grant,
            ledger,
            &item_hash,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let mut next = held.clone();
        next.ledger_handoff = Some(LedgerHandoff {
            app_id: self.app_id(),
            grant,
            ledger,
            item_hash,
            cert: self.app.cert().signed().clone(),
            sig: self
                .app
                .sign_protocol(paykit_lib::grant::LEDGER_HANDOFF_LABEL, &refs)?,
            origin: Origin::default(),
        });
        next.ledger_handoff_unread = None;
        // Conditioned on the ledger record as read: a kept ledger write that lands first, a
        // batch the client sends again ahead of this one, makes it fail, and it is read again.
        let write = self
            .app
            .record_op(
                &target,
                Some(&next.to_json()?),
                vec![Condition::RecordVersion {
                    key: ledger_key,
                    version: shown.version,
                }],
            )
            .await?;
        match self.app.submit_batch(OpBatch::new().entry(write)).await {
            Ok(_) => Ok(()),
            // Kept: it goes with the app's next write or read.
            Err(error) if error.kept_batch().is_some() => Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(&target).await?;
                }
                Err(error.into())
            }
        }
    }

    /// One Allowance's pass of [`Paykit::record_ledger_outcomes`].
    async fn copy_outcomes(&mut self, record: &[u8; 32]) -> Result<()> {
        let Some(under) = self.outcome_under(record).await? else {
            return Ok(());
        };
        let Some(ledger) = self.ledger_base(&under).await? else {
            return Ok(());
        };
        let claims = self.ledger_claims(&ledger).await?;
        let Some(next) = record_outcomes(&under.terms, &ledger, &claims.outcomes)
            .map_err(|_| not_automatic("usage can't be counted"))?
        else {
            return Ok(());
        };
        let write = self.ledger_write(&under, &next).await?;
        self.submit_ledger_write(write).await
    }

    /// Send a ledger write alone, and keep its version once applied. A batch kept unanswered
    /// goes with the app's next send; one refused, a `409` included, leaves the ledger as
    /// it was, for the next try.
    async fn submit_ledger_write(&mut self, write: LedgerWrite) -> Result<()> {
        match self
            .app
            .submit_batch(OpBatch::new().entry(write.write))
            .await
        {
            Ok(_) => self.keep_own_ledger(&write.record_key, &write.ledger).await,
            Err(error) if error.kept_batch().is_some() => Ok(()),
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                self.app.read_record(&write.ledger).await?;
                Err(PaykitError::Conflict(
                    "the ledger or the Allowance record changed meanwhile".into(),
                ))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The attempts under the Allowance at `record` held reserved because an app other than
    /// their owner wrote them `failed` (Paykit spec 8.3, "`failed` isn't final"), with their
    /// amounts, for the host to show its user: only the user's release frees one
    /// ([`Paykit::release_allowance_attempt`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `paykit_allowance_execute`; [`Refusal::NotAutomatic`] for
    /// an Allowance this app isn't the current executor of, or whose ledger it doesn't build
    /// on now; the client's errors.
    pub async fn reserved_by_other_apps(
        &mut self,
        record: &[u8; 32],
    ) -> Result<Vec<ReservedByOtherApp>> {
        Box::pin(async move {
            self.require_executor()?;
            let (_, ledger) = self.released_base(record).await?;
            let claims = self.ledger_claims(&ledger).await?;
            Ok(ledger
                .entries
                .iter()
                .filter(|e| {
                    e.outcome.is_none()
                        && claims.usage.get(&e.attempt_id)
                            == Some(&ClaimState::Failed { by_owner: false })
                })
                .map(|e| ReservedByOtherApp {
                    attempt_id: e.attempt_id,
                    value: e.value.clone(),
                })
                .collect())
        })
        .await
    }

    /// Release, as its user confirmed, the amount the attempt `attempt_id` under the
    /// Allowance at `record` holds reserved because an app other than its owner wrote it
    /// `failed` (Paykit spec 8.3, "`failed` isn't final"; Paykit wire 5.5, "Outcomes
    /// written"): the executor records `failed` in the attempt's ledger entry, which frees
    /// it unless its claim shows `done` later. The host shows the user the amount first
    /// ([`Paykit::reserved_by_other_apps`]).
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `paykit_allowance_execute`; [`Refusal::NotAutomatic`] for
    /// an Allowance this app isn't the current executor of, or whose ledger it doesn't build
    /// on now; [`Refusal::Usage`] for an attempt the ledger holds no entry for, one with an
    /// outcome, or one whose claim doesn't show it `failed` by another app;
    /// [`PaykitError::Conflict`] when the ledger or the Allowance record changed meanwhile;
    /// the client's errors.
    pub async fn release_allowance_attempt(
        &mut self,
        record: &[u8; 32],
        attempt_id: AttemptId,
        confirmed: UserConfirmed,
    ) -> Result<()> {
        let _ = confirmed;
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_executor()?;
            let (under, mut ledger) = self.released_base(record).await?;
            let Some(entry) = ledger
                .entries
                .iter()
                .find(|e| e.attempt_id == attempt_id && e.outcome.is_none())
            else {
                return Err(Refusal::Usage(
                    "Paykit spec 8.3: the ledger holds no entry without an outcome for it".into(),
                )
                .into());
            };
            // As usage counts it: an attempt this app's journal shows paid is `done`, never
            // released, whatever another app wrote on its claim.
            let alone = Ledger {
                entries: vec![entry.clone()],
                failed: Vec::new(),
                ..ledger.clone()
            };
            let shown = self
                .ledger_claims(&alone)
                .await?
                .usage
                .get(&attempt_id)
                .copied();
            if shown != Some(ClaimState::Failed { by_owner: false }) {
                return Err(Refusal::Usage(
                    "Paykit spec 8.3: only an attempt another app wrote failed is released".into(),
                )
                .into());
            }
            if let Some(held) = ledger
                .entries
                .iter_mut()
                .find(|e| e.attempt_id == attempt_id)
            {
                held.outcome = Some(EntryOutcome::Failed);
            }
            let write = self.ledger_write(&under, &ledger).await?;
            self.submit_ledger_write(write).await
        })
        .await
    }

    /// The Allowance at `record` and the ledger it builds on, for a user's release.
    async fn released_base(&mut self, record: &[u8; 32]) -> Result<(Under, Ledger)> {
        let under = self
            .outcome_under(record)
            .await?
            .ok_or_else(|| not_automatic("this app isn't the Allowance's current executor"))?;
        let ledger = self
            .ledger_base(&under)
            .await?
            .ok_or_else(|| not_automatic("the ledger holds no entry yet"))?;
        Ok((under, ledger))
    }

    /// The repair of the ledger of the Allowance at `allowance`, which the executor doesn't
    /// accept (Paykit spec 8.3, "After a confirmation"): the figures the host shows its user
    /// before asking to confirm it, computed over the ledger the repair would write. Nothing
    /// is written. The user's confirmation goes to [`Paykit::confirm_ledger_repair`] with
    /// the repair returned here.
    ///
    /// A ledger refused by a check of policy (its writer, the ledger rules, a write the
    /// client refused or a removed app's), a missing or deleted ledger ("usage history
    /// lost"), one at its key before the first grant, and one whose version kept is of an
    /// earlier generation are repaired. A rollback isn't: the alarm holds every attempt.
    /// Only the current executor repairs, and only an Allowance it may otherwise pay under
    /// ([`Paykit::pay_under_allowance`]'s checks of the Allowance): after a restore, its
    /// user confirms the Allowance again first ([`Paykit::confirm_allowance_again`]), and
    /// the repair's confirmation stands for that of the ledger alone.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `paykit_allowance_execute`; [`Refusal::NotNow`] while
    /// a rollback alarm stands, the records aren't listed, the ledger shown or a payer record
    /// of the counterparty can't be opened; [`Refusal::Usage`] for a ledger the executor
    /// accepts, which needs no repair; [`Refusal::NotAutomatic`] for an Allowance this app
    /// isn't the current executor of or doesn't pay under now, naming the check, and for a
    /// ledger divergent or seen so and not confirmed; [`Refusal::SwitchUnavailable`] when
    /// the switch can't be read now; the Allowance's own refusals as
    /// [`Paykit::pay_under_allowance`] gives them; the client's errors, a claim that can't be
    /// read among them: no figures come from a partial read.
    pub async fn ledger_repair(&mut self, allowance: &[u8; 32]) -> Result<LedgerRepair> {
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_executor()?;
            self.repair_of(allowance).await
        })
        .await
    }

    /// Write, as its user confirmed, the ledger repair `repair` of the Allowance at
    /// `allowance`, which [`Paykit::ledger_repair`] returned and the host showed (Paykit spec
    /// 8.3, "After a confirmation"). The repair is computed again first: if anything it was
    /// built from changed since, nothing is written and the refusal carries the repair
    /// computed now, whose figures the user confirms in turn. Otherwise exactly that ledger
    /// is written, in one batch conditioned on the Allowance record version read, on the
    /// ledger record at the version shown or its absence, and on the service time, as an
    /// executor batch's ledger write is. Once applied it is kept as the last ledger version,
    /// the first grant's mark of a ledger the executor didn't accept goes with it, and the
    /// executor pays under the Allowance again. A write kept unanswered returns `Ok` and goes
    /// with the app's next write or read; the Allowance pays nothing until it lands, and then
    /// the executor takes it, its own write, as the ledger shown.
    ///
    /// # Errors
    ///
    /// As [`Paykit::ledger_repair`]; [`Refusal::Usage`] for a repair of another Allowance;
    /// [`Refusal::LedgerRepairChanged`] when what it was built from changed, the switch's
    /// `409` included; [`Refusal::NotAutomatic`] when the device clock and the service's
    /// differ by more than the tolerance; the client's errors.
    pub async fn confirm_ledger_repair(
        &mut self,
        allowance: &[u8; 32],
        repair: &LedgerRepair,
        confirmed: UserConfirmed,
    ) -> Result<()> {
        let _ = confirmed;
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_executor()?;
            if repair.allowance != *allowance {
                return Err(Refusal::Usage(
                    "Paykit spec 8.3: the ledger repair is another Allowance's".into(),
                )
                .into());
            }
            let (now, terms) = Box::pin(self.repair_with_terms(allowance)).await?;
            if now.basis != repair.basis || now.ledger != repair.ledger {
                return Err(Refusal::LedgerRepairChanged(Box::new(now)).into());
            }
            let at = self
                .service_time()
                .ok_or_else(|| not_automatic("no service time verified"))?;
            if self.now().abs_diff(at) > CLOCK_TOLERANCE_MS {
                return Err(not_automatic(
                    "the device clock and the service's differ by more than the tolerance",
                ));
            }
            let held = self
                .held_allowance(allowance)
                .await?
                .ok_or(Refusal::NotFound)?;
            let lt = ledger_target(&held.counterparty, &held.allowance_id);
            let conditions = repair_conditions(&terms, allowance, now.basis.record_version, at)?;
            let write = self
                .app
                .record_op(&lt, Some(&now.ledger), conditions)
                .await?;
            // The write builds on the ledger version shown, or on its absence: the
            // compare-and-swap of its `expected_version` (Switch spec 10.3).
            match &write.1 {
                OpKind::RecordPut(put) if put.expected_version == now.basis.built_on => {}
                OpKind::RecordPut(_) => {
                    return Err(PaykitError::Conflict("the ledger changed meanwhile".into()))
                }
                _ => return Err(PaykitError::corrupt()),
            }
            // The pause the repair lifts goes with the write, committed once the switch
            // applies it, a kept batch that lands later included, and never if it is
            // refused: the first grant's mark of a ledger the executor didn't accept (Paykit
            // spec 8.3, "A grant implies a ledger"). Every other refusal of the ledger is
            // judged again on the write shown, which the repair's write then is.
            let lifted = pubky_switch_client::app::LocalWrite::Delete {
                key: key::empty_ledger(allowance).into_bytes(),
            };
            // With it, the repair's own write, which the executor then builds on, or on one
            // built on it or later, also when its batch lands later: and, after a handoff, it
            // is this app's first ledger write since, which the `ledger_handoff` is checked
            // against (Paykit spec 8.3, "Handoff").
            let mut mark = self
                .ledger_base_mark(allowance)
                .await?
                .unwrap_or(LedgerBase {
                    record: *allowance,
                    handoff: None,
                    repair: None,
                });
            mark.repair = Some(RepairWrite {
                built_on_generation: now.basis.built_on.generation,
                built_on_version: now.basis.built_on.version,
                value_hash: pubky_switch_core::enc::hash(&now.ledger),
                generation: self.app.own_identity().generation(),
            });
            if let Some(wait) = &mut mark.handoff {
                if wait.first.is_none() {
                    // The item hash of the write it builds on, the write shown, when the app
                    // holds that write: a refused write's isn't held.
                    let held_at = self
                        .app
                        .shared_record(&self.app.record_key(&lt)?)
                        .await?
                        .map(|held| held.version);
                    let built_on = now.basis.built_on;
                    wait.first = Some(FirstWrite {
                        generation: built_on.generation,
                        version: built_on.version,
                        item_hash: now
                            .basis
                            .shown_item
                            .filter(|_| built_on.version > 0 && held_at == Some(built_on)),
                    });
                }
            }
            let base = state::local_put(&key::ledger_base(allowance), &mark)?;
            match Box::pin(
                self.app
                    .submit_batch(OpBatch::new().entry(write).locals(vec![lifted, base])),
            )
            .await
            {
                Ok(_) => Box::pin(self.keep_own_ledger(allowance, &lt)).await,
                Err(error) if error.kept_batch().is_some() => Ok(()),
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    Box::pin(self.app.read_record(&lt)).await?;
                    let again = Box::pin(self.repair_of(allowance)).await?;
                    Err(Refusal::LedgerRepairChanged(Box::new(again)).into())
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }

    /// The repair of the ledger of the Allowance at `record`, read and computed now
    /// ([`Paykit::ledger_repair`]).
    async fn repair_of(&mut self, record: &[u8; 32]) -> Result<LedgerRepair> {
        Ok(Box::pin(self.repair_with_terms(record)).await?.0)
    }

    /// [`Paykit::repair_of`], with the accepted terms it was counted under.
    async fn repair_with_terms(
        &mut self,
        record: &[u8; 32],
    ) -> Result<(LedgerRepair, AllowanceTerms)> {
        if self.ledger_alarm_active().is_some() || self.app.alarm().is_some() {
            return Err(Refusal::NotNow(
                "Paykit spec 8.3: a rollback alarm stands; no ledger is repaired while it holds"
                    .into(),
            )
            .into());
        }
        let under = Box::pin(self.allowance_gate(record, true)).await??;
        let (reason, content) =
            match Box::pin(self.ledger_shown(&under)).await? {
                LedgerShown::Accepted(_) => return Err(Refusal::Usage(
                    "Paykit spec 8.3: the executor accepts the ledger shown; it needs no repair"
                        .into(),
                )
                .into()),
                LedgerShown::Unaccepted { why, content } => (why, content),
            };
        let shown = match content {
            ShownContent::Ledger(ledger) => Some(*ledger),
            ShownContent::Nothing => None,
            ShownContent::Unknown => {
                return Err(Refusal::NotNow(
                    "Paykit spec 8.3: the ledger shown can't be opened yet, so what it adds can't be told"
                        .into(),
                )
                .into())
            }
        };
        let lt = ledger_target(&under.allowance.counterparty, &under.allowance.allowance_id);
        let ledger_key = self.app.record_key(&lt)?;
        let kept = self.kept_ledger(record).await?;
        let shown_item = match self.held_op(&ledger_key).await? {
            Some(op) => Some(op.item_hash().map_err(|_| PaykitError::corrupt())?),
            None => None,
        };
        let at = self
            .service_time()
            .ok_or_else(|| not_automatic("no service time verified"))?;
        let mut reads = ClaimReads::default();
        let journal = Box::pin(self.journal_attempts_under(record, &mut reads)).await?;
        let (records, claimed) =
            Box::pin(self.claimed_attempts_under(&under, at, &mut reads)).await?;
        let base = kept.as_ref().map_or_else(
            || {
                Ledger::empty(
                    under.allowance.counterparty,
                    under.allowance.allowance_id.clone(),
                )
            },
            |(_, ledger)| ledger.clone(),
        );
        let merged = merge_for_repair(&under.terms, &base, shown.as_ref(), &journal, &claimed, at)
            .map_err(|error| match error {
                paykit_lib::Error::Invalid(why) if why.contains("larger value") => not_automatic(
                    "an attempt's own evidence shows a larger value than the version kept",
                ),
                _ => not_automatic("the repair's ledger can't be counted"),
            })?;
        // The outcomes the claims show, and compaction near the size limit, as an executor
        // batch writes them: the figures are counted over the ledger written.
        let claims = Box::pin(self.ledger_claims_from(&merged, &mut reads)).await?;
        let merged =
            repair_ledger(&under.terms, &merged, &claims.outcomes, at).map_err(not_automatic)?;
        // Never a ledger that counts less than the version kept: the merge keeps every one
        // of its entries and items, and the outcomes and compaction are moves a successor
        // makes.
        if let Some((_, held)) = &kept {
            check_successor(&under.terms, held, &merged, at)
                .map_err(|_| not_automatic("the repair would count less than the version kept"))?;
        }
        let ledger = merged.to_json()?;
        // The version the write would build on, as the client builds it.
        let built_on = match self.app.record_op(&lt, Some(&ledger), Vec::new()).await?.1 {
            OpKind::RecordPut(put) => put.expected_version,
            _ => return Err(PaykitError::corrupt()),
        };
        let used = usage(&under.terms, &merged, &claims.usage, at)
            .map_err(|_| not_automatic("usage can't be counted"))?;
        let reserved_by_other_apps = merged
            .entries
            .iter()
            .filter(|e| {
                e.outcome.is_none()
                    && claims.usage.get(&e.attempt_id)
                        == Some(&ClaimState::Failed { by_owner: false })
            })
            .map(|e| ReservedByOtherApp {
                attempt_id: e.attempt_id,
                value: e.value.clone(),
            })
            .collect();
        let mut journal_ids: Vec<[u8; 16]> =
            journal.iter().map(|e| e.attempt_id.to_bytes()).collect();
        journal_ids.sort_unstable();
        let repair = LedgerRepair {
            allowance: *record,
            reason,
            at_ms: at,
            periods: used.periods().to_vec(),
            lifetime: used.lifetime().clone(),
            reserved: used.reserved(),
            spent: used.spent(),
            released: used.released(),
            reserved_by_other_apps,
            basis: RepairBasis {
                record_version: under.version,
                kept: kept.map(|(kept, _)| (kept.version(), kept.item_hash)),
                built_on,
                shown_item,
                journal: journal_ids,
                records,
                claims: reads.steps(),
            },
            ledger,
        };
        Ok((repair, under.terms))
    }

    /// The attempts this app's journal holds under the Allowance at `record`, as ledger
    /// entries (Paykit spec 8.3, "After a confirmation"): each with its amount, dated by its
    /// claim's `opened_at` where the claim, read into `reads`, shows it, else by when it was
    /// journaled, before its batch went.
    async fn journal_attempts_under(
        &mut self,
        record: &[u8; 32],
        reads: &mut ClaimReads,
    ) -> Result<Vec<LedgerEntry>> {
        let mut out = Vec::new();
        for entry in self.journal().await? {
            let Some(allowance_id) = entry.allowance()? else {
                continue;
            };
            if self
                .app
                .record_key(&allowance_target(&entry.payee, &allowance_id))?
                != *record
            {
                continue;
            }
            let attempt = entry.attempt()?;
            let amount = entry.amount.amount()?;
            let view = self.claim_view(reads, &entry.obligation).await?;
            let at = view
                .as_ref()
                .and_then(|view| view.attempt(&attempt.to_bytes()))
                .map_or(entry.journaled_at_ms, |shown| shown.opened_at);
            out.push(LedgerEntry {
                obligation: entry.obligation,
                attempt_id: attempt,
                value: amount.value,
                asset: amount.asset,
                at,
                outcome: None,
            });
        }
        Ok(out)
    }

    /// What the payer identity's own evidence shows of the counterparty's obligations under
    /// `under`, read into `reads`, as ledger entries (Paykit spec 8.3, "After a
    /// confirmation"; Paykit wire 5.5), for every request of this app's payer records from
    /// the counterparty in the Allowance's asset, one-time or recurring:
    ///
    /// - every attempt of an app the grant history names that the claim of each obligation
    ///   shows, with the request's amount and its `opened_at`, but one its owner failed and
    ///   one the payer identity's own evidence shows made otherwise
    ///   ([`Paykit::made_otherwise`]);
    /// - every payment a payer or period record's proof shows made under the Allowance, with
    ///   the request's amount, dated by the proof, `done`: also once its claim is gone.
    ///
    /// A recurring request's obligations are its Billing Periods listed in
    /// `attempted_periods`, those whose period record the app holds, and those from the
    /// first to the one containing `at`, the latest service time verified, whose claim the
    /// service may still hold ([`periods_to_read`]): a done period is dropped from
    /// `attempted_periods`, and its claim may still show it. With them, the payer records
    /// read, by key and version.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] for a payer record that can't be read, whose counterparty can't be
    /// told, and for a period record whose request record isn't held, whose amount can't be
    /// told; [`Refusal::NotAutomatic`] for a recurring request whose periods can't be walked;
    /// the client's errors, a claim that can't be read among them.
    async fn claimed_attempts_under(
        &mut self,
        under: &Under,
        at: u64,
        reads: &mut ClaimReads,
    ) -> Result<(RecordsRead, Vec<LedgerEntry>)> {
        let own = self.identity();
        let counterparty = under.allowance.counterparty;
        let allowance_id = &under.allowance.allowance_id;
        let owners: Vec<[u8; 16]> = under
            .allowance
            .grants
            .iter()
            .map(|entry| entry.grant.app_id)
            .collect();
        let unread = || -> PaykitError {
            Refusal::NotNow(
                "Paykit spec 8.3: a payer record can't be read, so the counterparty's claims can't be told"
                    .into(),
            )
            .into()
        };
        let mut records = Vec::new();
        let mut requests: Vec<ActivityRecord> = Vec::new();
        let mut periods: HashMap<[u8; 32], Vec<(u64, ActivityRecord)>> = HashMap::new();
        for held in self
            .app
            .records_of(Collection::PayerActivity.name())
            .await?
        {
            let value = match &held.content {
                RecordContent::Deleted => continue,
                RecordContent::Value(value) => value,
                RecordContent::Unreadable(_) => return Err(unread()),
            };
            let record =
                crate::activity::read(Some(value), &own, paykit_lib::records::ActivitySide::Payer)
                    .ok_or_else(unread)?;
            if record.counterparty != counterparty {
                continue;
            }
            records.push((held.key, held.version));
            if record.request.is_some() {
                requests.push(record);
            } else if let Ok(RecordBinding::Period {
                request_hash,
                billing_period,
            }) = record.binding()
            {
                periods
                    .entry(request_hash)
                    .or_default()
                    .push((billing_period.start_secs(), record));
            }
        }
        let removed: Vec<[u8; 16]> = self
            .app
            .own_identity()
            .body()
            .removed
            .iter()
            .map(|r| r.app_id)
            .collect();
        let mut out = Vec::new();
        let mut held_requests = Vec::with_capacity(requests.len());
        for record in &requests {
            let Some((stored, body)) = crate::requests::request_of(record) else {
                continue;
            };
            let hash = stored.hash();
            held_requests.push(hash);
            if body.amount.asset != under.terms.asset {
                continue;
            }
            // Each obligation, by its period's start (0 for a one-time request), with the
            // record that holds its proof.
            let mut obligations: std::collections::BTreeMap<u64, Option<&ActivityRecord>> =
                std::collections::BTreeMap::new();
            match &body.recurrence {
                None => {
                    obligations.insert(0, Some(record));
                }
                Some(recurrence) => {
                    for start in periods_to_read(recurrence, body.payment_deadline.as_ref(), at)
                        .map_err(not_automatic)?
                        .into_iter()
                        .chain(record.attempted_periods.iter().copied())
                    {
                        obligations.entry(start).or_insert(None);
                    }
                    for (start, period) in periods.get(&hash).into_iter().flatten() {
                        obligations.insert(*start, Some(period));
                    }
                }
            }
            for (start, holder) in obligations {
                let obligation = self.obligation(&counterparty, &hash, start);
                if let Some(paid) = paid_under(holder, allowance_id, obligation, &body.amount) {
                    out.push(paid);
                }
                let Some(view) = self.claim_view(reads, &obligation).await? else {
                    continue;
                };
                for shown in &view.attempts {
                    let attempt = AttemptId::from_bytes(shown.attempt_id);
                    let failed_by_owner =
                        shown.outcome == Outcome::Failed && shown.resolved_by == Some(shown.owner);
                    if !owners.contains(&shown.owner) || failed_by_owner {
                        continue;
                    }
                    if Box::pin(self.made_otherwise(
                        holder,
                        &attempt,
                        &obligation,
                        shown.owner,
                        &removed,
                        allowance_id,
                    ))
                    .await?
                    {
                        continue;
                    }
                    out.push(LedgerEntry {
                        obligation,
                        attempt_id: attempt,
                        value: body.amount.value.clone(),
                        asset: body.amount.asset.clone(),
                        at: shown.opened_at,
                        outcome: None,
                    });
                }
            }
        }
        if periods.keys().any(|hash| !held_requests.contains(hash)) {
            return Err(Refusal::NotNow(
                "Paykit spec 8.3: a period record's request record isn't held, so its payments' amounts can't be told"
                    .into(),
            )
            .into());
        }
        records.sort_unstable();
        Ok((records, out))
    }

    /// Whether the payer identity's own evidence shows `attempt`, on the claim of
    /// `obligation` and owned by `owner`, made by hand or under another Allowance than
    /// `allowance` (Paykit spec 8.3, "After a confirmation"): this app's own journal entry of
    /// it naming no Allowance or another one, or, in `holder`, the payer or period record of
    /// the obligation, a proof of it naming no Allowance or another one that the payer
    /// identity sent and the attempt's owner signed, while that app isn't listed as removed
    /// (`removed`). A proof another app signed, or a message the counterparty signed, shows
    /// nothing of how the owner made it, so the attempt counts.
    async fn made_otherwise(
        &self,
        holder: Option<&ActivityRecord>,
        attempt: &AttemptId,
        obligation: &[u8; 32],
        owner: [u8; 16],
        removed: &[[u8; 16]],
        allowance: &AllowanceId,
    ) -> Result<bool> {
        if let Some(entry) = self.journal_entry_on(attempt, obligation).await? {
            return Ok(entry.allowance()?.as_ref() != Some(allowance));
        }
        Ok(proof_shows_made_otherwise(
            holder,
            &self.identity(),
            attempt,
            owner,
            removed,
            allowance,
        ))
    }

    /// Raise the alarm for a rollback of the ledger of the Allowance at `record` (Paykit spec
    /// 8.3, "Last ledger version"; Switch spec 10.1): kept, and returned as the client
    /// returns its own alarm. From then on, while the own `identity.json` is at this
    /// generation, the app makes no new attempt ([`Paykit::may_attempt`]). An alarm kept
    /// already at this generation stays the one shown.
    ///
    /// Only against a version kept at the own `identity.json`'s current generation
    /// ([`Paykit::ledger_shown`]): one kept at an earlier generation, before a move or a
    /// provider loss, may name a write the service now serving never held, so that ledger
    /// only pauses the Allowance, as one the executor doesn't accept, and the alarm, which
    /// only a later generation lifts, isn't raised on it.
    async fn raise_ledger_alarm(&mut self, record: &[u8; 32], reason: &str) -> PaykitError {
        let alarm = match self.ledger_alarm_active() {
            Some(held) => held.clone(),
            None => {
                let alarm = LedgerAlarm {
                    record: *record,
                    generation: self.app.own_identity().generation(),
                    reason: reason.into(),
                };
                // Held in memory first, as the client holds its own: a commit that fails
                // still stops the app now, and the evidence raises it again after a load.
                self.ledger_alarm = Some(alarm.clone());
                if let Err(error) = self.local_put(key::LEDGER_ALARM, &alarm).await {
                    return error;
                }
                alarm
            }
        };
        PaykitError::Client(pubky_switch_client::app::AppError::Alarm(alarm.alarm()))
    }

    /// The ledger rollback this app found at the own `identity.json`'s current generation.
    fn ledger_alarm_active(&self) -> Option<&LedgerAlarm> {
        let generation = self.app.own_identity().generation();
        self.ledger_alarm
            .as_ref()
            .filter(|alarm| alarm.generation >= generation)
    }

    /// The rollback of an Allowance ledger this app found (Paykit spec 8.3, "Last ledger
    /// version"; Switch spec 10.1), reported as the client reports its own alarm
    /// ([`pubky_switch_client::app::App::alarm`]): while it stands, at the generation of the
    /// own `identity.json` it was found at, the app makes no new attempt, by hand or
    /// automatic. The host shows it to the user, who resolves it by moving to another
    /// provider or declaring a provider loss.
    pub fn ledger_alarm(&self) -> Option<pubky_switch_client::app::Alarm> {
        self.ledger_alarm_active().map(LedgerAlarm::alarm)
    }

    /// Whether the app may make an attempt now: the client's rules (Switch spec 10.1, 12.4),
    /// and no Allowance ledger rollback found at the current generation
    /// ([`Paykit::ledger_alarm`]), which stops the app as the client's own alarm does.
    ///
    /// # Errors
    ///
    /// The client's refusal that holds the attempt back.
    pub(crate) fn may_attempt(
        &self,
        automatic: bool,
    ) -> std::result::Result<(), pubky_switch_client::app::Refusal> {
        if self.ledger_alarm_active().is_some() {
            return Err(pubky_switch_client::app::Refusal::Stopped);
        }
        self.app.may_attempt(automatic)
    }

    /// One try to deliver `msg_id`, queued, now. A try the network or the switch fails leaves
    /// it queued for the next flush, and is no error; a store failure or an alarm is the
    /// call's error, so nothing goes on past it (as `Paykit::send_own_blocks_again` does).
    pub(crate) async fn deliver_now(&mut self, msg_id: &str) -> Result<()> {
        match self.app.flush_message(msg_id).await {
            Ok(_) => Ok(()),
            Err(error)
                if matches!(
                    error.unkept(),
                    pubky_switch_client::app::AppError::Store(_)
                        | pubky_switch_client::app::AppError::Alarm(_)
                ) =>
            {
                Err(error.into())
            }
            Err(_) => Ok(()),
        }
    }

    /// Pay the request `request` automatically under the Allowance at `allowance` (Paykit
    /// spec 8.3, "Paying under an Allowance"), as its executor: no user confirmation is asked
    /// for, and none overrides a check. The host calls it for a request it wants paid
    /// automatically, such as one just received; a refusal leaves the request to be paid by
    /// hand.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotAutomatic`] naming the executor's check that refuses it, with nothing
    /// written; [`Refusal::AllowanceEnded`] or [`Refusal::Rejected`] for an Allowance ended
    /// or rejected; [`Refusal::AllowanceConflicted`] for a conflicted one; [`Refusal::NotFound`]
    /// for an Allowance record not held, held as deleted, or not binding;
    /// [`Refusal::SwitchUnavailable`] when the Allowance record can't be read from the switch
    /// now; [`Refusal::NotNow`] while the app may not attempt automatically or the records
    /// aren't listed; [`Refusal::ScopeMissing`] without `paykit_allowance_execute`; otherwise as
    /// [`Paykit::pay_request`], whose checks it takes too, a doubtful claim refused as
    /// [`Refusal::MayBePaid`] and never paid over.
    pub async fn pay_under_allowance(
        &mut self,
        request: &RequestRef,
        allowance: &[u8; 32],
    ) -> Result<PayOutcome> {
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_executor()?;
            self.may_attempt(true)
                .map_err(|refusal| Refusal::NotNow(refusal.to_string()))?;
            let under = self.allowance_gate(allowance, true).await??;
            self.pay_request_under(
                request,
                PayOptions::default(),
                Initiator::Automatic,
                Some(under),
            )
            .await
        })
        .await
    }

    /// Become the executor of the Allowance at `record` (Paykit spec 8.3, "Setting up" and
    /// "Handoff"), once its user has confirmed it: sign this app's executor grant over the
    /// record version read, append it to the grant history and put the Allowance on the
    /// consent list, in one write of the record. A handoff from another own executor sends
    /// it an `executor_notice`. The first grant's empty ledger follows, and is owed in the
    /// executor's journal until it lands: a grant batch kept unanswered, or a crash before
    /// the ledger's write, leaves it to be written before the first payment. A grant
    /// signed again by the current executor, such as after a provider loss, takes the
    /// current generation.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `paykit_allowance_execute`; [`Refusal::NotFound`];
    /// [`Refusal::AllowanceEnded`] or [`Refusal::Rejected`]; [`Refusal::Usage`] for an
    /// Allowance not accepted, one this identity is the allowee of, or one that left the
    /// consent list for good; [`Refusal::AllowanceConflicted`] for a conflicted one;
    /// [`Refusal::NotNow`] while the record is divergent or flagged, or can't be read from
    /// the switch now, or the records aren't listed; [`PaykitError::Conflict`] when the
    /// record kept changing; the client's errors.
    pub async fn grant_executor(
        &mut self,
        record: &[u8; 32],
        confirmed: UserConfirmed,
    ) -> Result<()> {
        let _ = confirmed;
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_executor()?;
            if matches!(
                self.local_get::<ConsentEntry>(&key::consent(record))
                    .await?,
                Some(ConsentEntry {
                    consent: Consent::Removed { .. },
                    ..
                })
            ) {
                return Err(Refusal::Usage(
                    "Paykit spec 8.3: the Allowance left the consent list for good".into(),
                )
                .into());
            }
            let mut tries = 0;
            loop {
                match self.write_grant(record).await {
                    Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                        tries += 1;
                        if tries >= GRANT_TRIES {
                            return Err(crate::records::conflict(error));
                        }
                    }
                    other => return other,
                }
            }
        })
        .await
    }

    async fn write_grant(&mut self, record: &[u8; 32]) -> Result<()> {
        let own = self.identity();
        let held = self
            .held_allowance(record)
            .await?
            .ok_or(Refusal::NotFound)?;
        let target = allowance_target(&held.counterparty, &held.allowance_id);
        let shared = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(shared) => shared,
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::Unreachable(why),
                ..
            } => return Err(Refusal::SwitchUnavailable(why).into()),
            RecordRead::Kept { .. } => {
                return Err(Refusal::NotNow("the Allowance record isn't read now".into()).into())
            }
            _ => return Err(PaykitError::corrupt()),
        };
        if shared.divergent || shared.flagged {
            return Err(
                Refusal::NotNow("the Allowance record is divergent or flagged".into()).into(),
            );
        }
        let held = self
            .held_allowance(record)
            .await?
            .ok_or(Refusal::NotFound)?;
        // No grant is written for a conflicted Allowance (Paykit spec 8.3, "Conflicted").
        if held.conflicted.is_some() || self.conflict_kept(record).await? {
            return Err(Refusal::AllowanceConflicted.into());
        }
        if held.end.is_some() || self.allowance_stopped(record).await? {
            return Err(Refusal::AllowanceEnded.into());
        }
        let terms_hash = match &held.response {
            Some(r) if matches!(r.body(), Body::AllowanceAcceptance(_)) => {
                held.terms_hash().ok_or_else(PaykitError::corrupt)?
            }
            Some(_) => return Err(Refusal::Rejected.into()),
            None => {
                return Err(
                    Refusal::Usage("Paykit spec 8.3: the Allowance isn't accepted".into()).into(),
                )
            }
        };
        let Body::AllowanceProposal(proposal) = held.proposal.body() else {
            return Err(PaykitError::corrupt());
        };
        let proposed_here = held.proposal.sender() == own;
        if !matches!(
            (proposal.proposer_role, proposed_here),
            (ProposerRole::Allower, true) | (ProposerRole::Allowee, false)
        ) {
            return Err(Refusal::Usage(
                "Paykit spec 8.3: only the allower's apps pay under an Allowance".into(),
            )
            .into());
        }
        let generation = self.app.own_identity().generation();
        let built_on = shared.version;
        let fields = grant_fields(
            &own,
            &held.counterparty,
            &held.allowance_id,
            &terms_hash,
            generation,
            built_on,
        );
        let refs: Vec<&[u8]> = fields.iter().map(Vec::as_slice).collect();
        let grant = ExecutorGrant {
            app_id: self.app_id(),
            terms_hash,
            confirmed_generation: generation,
            cert: self.app.cert().signed().clone(),
            sig: self.app.sign_protocol(EXECUTOR_GRANT_LABEL, &refs)?,
            origin: Origin::default(),
        };
        let previous = held.current_grant(&own).map(|(g, _)| g.app_id);
        let mut next = held.clone();
        next.grants.push(GrantEntry {
            built_on,
            grant: grant.clone(),
            origin: Origin::default(),
        });
        next.grant = Some(grant);
        let mut queue = Vec::new();
        let mut locals = vec![state::local_put(
            &key::consent(record),
            &ConsentEntry::new(
                held.counterparty,
                &held.allowance_id,
                Consent::Held {
                    confirm_again: false,
                },
            ),
        )?];
        if let Some(previous) = previous.filter(|app| *app != self.app_id()) {
            let body = ExecutorNotice {
                reason: NoticeReason::HandedOff,
                allowance_record: *record,
            }
            .to_body()?;
            let notice = self.sign(own, Kind::ExecutorNotice, Vec::new(), body)?;
            locals.push(state::local_put(
                &key::notice_target(notice.msg_id()),
                &NoticeTarget {
                    msg_id: notice.msg_id().to_owned(),
                    app_id: previous,
                },
            )?);
            queue.push(notice);
        }
        // The first grant's empty ledger goes into the executor's own journal with the grant,
        // unless a ledger record is already at its key, which it takes as one it didn't
        // accept and writes no empty ledger over (Paykit spec 8.3, "A grant implies a
        // ledger").
        // A grant that hands the Allowance on to this app waits for the previous executor's
        // `ledger_handoff` (Paykit spec 8.3, "Handoff"), in this app's own journal with the
        // grant; a grant this app signs again is no handoff.
        if next.grants.len() > 1 && previous != Some(self.app_id()) {
            locals.push(state::local_put(
                &key::ledger_base(record),
                &LedgerBase {
                    record: *record,
                    handoff: Some(HandoffWait {
                        generation: built_on.generation,
                        version: built_on.version,
                        first: None,
                    }),
                    repair: None,
                },
            )?);
        }
        let lt = ledger_target(&held.counterparty, &held.allowance_id);
        let ledger_key = self.app.record_key(&lt)?;
        let ledger_held = self.app.shared_record(&ledger_key).await?.is_some();
        if next.grants.len() == 1 {
            locals.push(state::local_put(
                &key::empty_ledger(record),
                &EmptyLedgerMark {
                    record: *record,
                    sent: ledger_held,
                    batch: None,
                    unaccepted: ledger_held,
                },
            )?);
        }
        let sent: Vec<String> = queue.iter().map(|m| m.msg_id().to_owned()).collect();
        let write = self
            .app
            .record_op(&target, Some(&next.to_json()?), Vec::new())
            .await?;
        let mut batch = OpBatch::new().entry(write).locals(locals);
        for message in queue {
            batch = batch.queue(message);
        }
        match self.app.submit_batch(batch).await {
            Ok(_) => {}
            // Kept: it goes with the app's next write or read, and the empty ledger is
            // written before the first payment.
            Err(error) if error.kept_batch().is_some() => return Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(&target).await?;
                }
                return Err(error.into());
            }
        }
        for msg_id in sent {
            // A notice not delivered now goes with the next flush.
            self.deliver_now(&msg_id).await?;
        }
        if next.grants.len() == 1 {
            self.write_empty_ledger(record, &next).await?;
        } else if ledger_held
            && previous.is_some_and(|app| app != self.app_id())
            && self.kept_ledger(record).await?.is_none()
        {
            // An executor that held a ledger version with an authenticated version before it
            // became the executor, as an app that reads the ledger, keeps it as its last
            // ledger version (Paykit spec 8.3, "Last ledger version"). Only at a handoff, and
            // only when it keeps none: a current executor granting again keeps the version it
            // checked.
            self.keep_ledger(record, &ledger_key).await?;
        }
        Ok(())
    }

    /// The empty ledger the first grant implies (Paykit spec 8.3, "A grant implies a
    /// ledger"), conditioned on the Allowance record version held, while it is owed. Its
    /// outcome is its own, never the grant's: applied, the version is kept from the held
    /// copy the answer left, with no read; kept with no answer, it stays sent, so a missing
    /// ledger is usage history lost; refused, it is owed again, for the next payment. A
    /// ledger at its key, one the app holds or one the switch's `409` shows, is taken as one
    /// the executor didn't accept: no empty ledger goes over it, and the mark goes. Only a
    /// failure of the app's own store is an error.
    async fn write_empty_ledger(
        &mut self,
        record: &[u8; 32],
        allowance: &AllowanceRecord,
    ) -> Result<()> {
        let Some(shared) = self.app.shared_record(record).await? else {
            return Ok(());
        };
        let lt = ledger_target(&allowance.counterparty, &allowance.allowance_id);
        let ledger_key = self.app.record_key(&lt)?;
        if self.app.shared_record(&ledger_key).await?.is_some() {
            return self
                .local_put(
                    &key::empty_ledger(record),
                    &EmptyLedgerMark {
                        record: *record,
                        sent: true,
                        batch: None,
                        unaccepted: true,
                    },
                )
                .await;
        }
        let empty = Ledger::empty(allowance.counterparty, allowance.allowance_id.clone());
        let write = self
            .app
            .record_op(
                &lt,
                Some(&empty.to_json()?),
                vec![Condition::RecordVersion {
                    key: *record,
                    version: shared.version,
                }],
            )
            .await?;
        self.empty_ledger_sent(record, None).await?;
        let owed = EmptyLedgerMark {
            record: *record,
            sent: false,
            batch: None,
            unaccepted: false,
        };
        let unaccepted = EmptyLedgerMark {
            record: *record,
            sent: true,
            batch: None,
            unaccepted: true,
        };
        match self.app.submit_batch(OpBatch::new().entry(write)).await {
            Ok(_) => self.keep_own_ledger(record, &lt).await,
            Err(error) if error.kept_batch().is_some() => {
                self.empty_ledger_sent(record, error.kept_batch()).await
            }
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                // A ledger at the key the app didn't hold, or the record changed since.
                self.app.read_record(&lt).await?;
                if self.app.shared_record(&ledger_key).await?.is_some() {
                    self.local_put(&key::empty_ledger(record), &unaccepted)
                        .await
                } else {
                    self.local_put(&key::empty_ledger(record), &owed).await
                }
            }
            Err(error) if error.code().is_some() => {
                self.local_put(&key::empty_ledger(record), &owed).await
            }
            Err(error) => {
                let error: PaykitError = error.into();
                if crate::hold::local_failure(&error) {
                    return Err(error);
                }
                // No answer: sent without a verified answer, the mark stays sent.
                Ok(())
            }
        }
    }

    /// Confirm again, as the user did, the Allowance at `record` on this app's consent list
    /// after a restore or a `missed` item (Paykit spec 8.3, "After a restore"): the executor
    /// pays under it again. A removal from the list stays for good.
    ///
    /// The same confirmation covers the record's current write the app ignores, if it holds
    /// one (Paykit spec 8.3, "Executor"), and only that write: the host must show the user
    /// that write, and that the record's view is the one the app trusts, before asking.
    /// A write the client refused for its writer's standing, such as a removed app's after
    /// its `distrust_from`, is never covered: the Allowance pays nothing while it is current.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for an Allowance not on the list; [`Refusal::Usage`] for one
    /// that left it for good, or while a `missed` item's mark waits for the next receive.
    pub async fn confirm_allowance_again(
        &mut self,
        record: &[u8; 32],
        confirmed: UserConfirmed,
    ) -> Result<()> {
        let _ = confirmed;
        self.require_executor()?;
        if self
            .local_get::<bool>(key::MISSED_CONSENTS)
            .await?
            .is_some()
        {
            return Err(Refusal::Usage(
                "Paykit spec 8.3: a missed item's mark waits for the next receive".into(),
            )
            .into());
        }
        // Read bound to its key (`state::Stored`): the entry names this Allowance.
        match self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?
        {
            None => Err(Refusal::NotFound.into()),
            Some(ConsentEntry {
                consent: Consent::Removed { .. },
                ..
            }) => Err(Refusal::Usage(
                "Paykit spec 8.3: the Allowance left the consent list for good".into(),
            )
            .into()),
            Some(entry) => {
                let mut tx = Tx::default();
                tx.put(
                    &key::consent(record),
                    &entry.with(Consent::Held {
                        confirm_again: false,
                    }),
                )?;
                // The user's confirmation covers the record's current write the app ignores,
                // if any, and only that one, and only one a confirmation may cover: never a
                // write refused for its writer's standing (Paykit spec 8.3, "Executor").
                let flagged = self
                    .app
                    .shared_record(record)
                    .await?
                    .is_some_and(|held| held.flagged);
                let confirmable = if flagged {
                    self.ignored_write_confirmable(record).await?
                } else {
                    None
                };
                if let Some(latest) = confirmable {
                    tx.put(
                        &key::flag_confirmed(record),
                        &FlagConfirmed {
                            record: *record,
                            generation: latest.generation,
                            version: latest.version,
                        },
                    )?;
                }
                self.local_commit(tx).await
            }
        }
    }

    /// Mark the one-time request `request` manual-only on its payer record (Paykit spec 8.3,
    /// "Manual only"; Paykit wire 5.3): no executor pays it automatically once the write
    /// lands. The mark is never lifted. A write kept unanswered returns `Ok` and lands with
    /// the app's next write or read, before any executor batch, which sends kept batches
    /// first; a kept batch the client later drops leaves no mark.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::Recurring`] for a recurring request, whose periods
    /// carry their own marks; [`PaykitError::Conflict`] when the record changed meanwhile;
    /// the client's errors.
    pub async fn mark_manual_only(&mut self, request: &RequestRef) -> Result<()> {
        Box::pin(async move {
            self.refresh_own()?;
            self.records_known().await?;
            self.require_pay()?;
            let (target, record) = self.payer_request(request).await?;
            let (_, body) = crate::requests::request_of(&record).ok_or(Refusal::NotFound)?;
            if body.is_recurring() {
                return Err(Refusal::Recurring.into());
            }
            if record.manual_only {
                return Ok(());
            }
            let mut next = record.clone();
            next.manual_only = true;
            let write = self
                .app
                .record_op(&target.record_target(), Some(&next.to_json()?), Vec::new())
                .await?;
            match self.app.submit_batch(OpBatch::new().entry(write)).await {
                Ok(_) => Ok(()),
                Err(error) if error.kept_batch().is_some() => Ok(()),
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    self.app.read_record(&target.record_target()).await?;
                    Err(crate::records::conflict(error.into()))
                }
                Err(error) => Err(error.into()),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "d0efffb7-191b-4709-b6c7-e343af5500b4";

    fn kept(record: [u8; 32]) -> KeptLedger {
        let ledger = Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap());
        KeptLedger {
            record,
            generation: 1,
            version: 7,
            written_at: 1,
            item_hash: [3; 32],
            value: ledger.to_json().unwrap(),
            dropped: Vec::new(),
            restored: false,
            term: None,
        }
    }

    fn ledger_with(entries: &[(u8, u8)], failed: &[(u8, u8)]) -> Ledger {
        let mut ledger = Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap());
        for (obligation, attempt) in entries {
            ledger.entries.push(LedgerEntry {
                obligation: [*obligation; 32],
                attempt_id: AttemptId::from_bytes([*attempt; 16]),
                value: paykit_lib::Decimal::parse("0.5").unwrap(),
                asset: paykit_lib::Asset::parse("btc").unwrap(),
                at: 1,
                outcome: None,
            });
        }
        for (obligation, attempt) in failed {
            ledger.failed.push(paykit_lib::allowances::CompactedFailed {
                obligation: [*obligation; 32],
                attempt_id: AttemptId::from_bytes([*attempt; 16]),
                value: paykit_lib::Decimal::parse("0.5").unwrap(),
            });
        }
        ledger
    }

    #[test]
    fn test_executor_attempts_named_in_entries_or_totals_failed_are_the_allowances() {
        let ledger = ledger_with(&[(1, 7)], &[(1, 8)]);
        assert!(!attempted_elsewhere(&[1; 32], &[], &ledger));
        assert!(!attempted_elsewhere(&[1; 32], &[[7; 16]], &ledger));
        assert!(!attempted_elsewhere(&[1; 32], &[[8; 16]], &ledger));
        assert!(!attempted_elsewhere(&[1; 32], &[[7; 16], [8; 16]], &ledger));
    }

    #[test]
    fn test_executor_attempt_no_entry_or_totals_failed_item_names_is_made_elsewhere() {
        let ledger = ledger_with(&[(1, 7)], &[(1, 8)]);
        // Made by hand, or under another Allowance, whose ledger is another.
        assert!(attempted_elsewhere(&[1; 32], &[[9; 16]], &ledger));
        assert!(attempted_elsewhere(&[1; 32], &[[7; 16], [9; 16]], &ledger));
        assert!(attempted_elsewhere(
            &[1; 32],
            &[[7; 16]],
            &ledger_with(&[], &[])
        ));
        // An entry or an item for another obligation names no attempt of this one.
        assert!(attempted_elsewhere(
            &[1; 32],
            &[[7; 16]],
            &ledger_with(&[(3, 7)], &[])
        ));
        assert!(attempted_elsewhere(
            &[1; 32],
            &[[8; 16]],
            &ledger_with(&[], &[(3, 8)])
        ));
    }

    #[test]
    fn test_executor_kept_ledger_reads_at_its_key() {
        assert!(check_kept_ledger(&[1; 32], &kept([1; 32])).is_ok());
    }

    #[test]
    fn test_executor_kept_ledger_refuses_another_record_or_a_state_no_write_gives() {
        assert!(check_kept_ledger(&[1; 32], &kept([9; 32])).is_err());
        let mut unversioned = kept([1; 32]);
        unversioned.version = 0;
        assert!(check_kept_ledger(&[1; 32], &unversioned).is_err());
        let mut no_generation = kept([1; 32]);
        no_generation.generation = 0;
        assert!(check_kept_ledger(&[1; 32], &no_generation).is_err());
        let mut not_a_ledger = kept([1; 32]);
        not_a_ledger.value = b"{}".to_vec();
        assert!(check_kept_ledger(&[1; 32], &not_a_ledger).is_err());
    }

    #[test]
    fn test_executor_kept_ledger_dropped_attempts_read_once_and_unlisted() {
        let attempt = AttemptId::from_bytes([4; 16]);
        let mut dropped = kept([1; 32]);
        dropped.dropped = vec![attempt.to_text()];
        assert!(check_kept_ledger(&[1; 32], &dropped).is_ok());
        let mut unread = kept([1; 32]);
        unread.dropped = vec!["not-an-attempt".into()];
        assert!(check_kept_ledger(&[1; 32], &unread).is_err());
        let mut twice = kept([1; 32]);
        twice.dropped = vec![attempt.to_text(), attempt.to_text()];
        assert!(check_kept_ledger(&[1; 32], &twice).is_err());
        // An attempt the content still lists isn't dropped from it.
        let mut ledger = Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap());
        ledger.failed.push(paykit_lib::allowances::CompactedFailed {
            obligation: [5; 32],
            attempt_id: attempt,
            value: paykit_lib::Decimal::parse("1").unwrap(),
        });
        let mut listed = kept([1; 32]);
        listed.value = ledger.to_json().unwrap();
        assert!(check_kept_ledger(&[1; 32], &listed).is_ok());
        listed.dropped = vec![attempt.to_text()];
        assert!(check_kept_ledger(&[1; 32], &listed).is_err());
    }

    #[test]
    fn test_executor_accepted_ids_keep_only_what_both_allow() {
        let a = PaymentEndpointIdentifier::new("a").unwrap();
        let b = PaymentEndpointIdentifier::new("b").unwrap();
        let c = PaymentEndpointIdentifier::new("c").unwrap();
        assert_eq!(accepted_ids(None, None), None);
        assert_eq!(both_allow(None, None), None);
        let asked = [a.clone(), b.clone()];
        // The request's alone, or the terms' alone.
        assert_eq!(both_allow(Some(&asked), None), Some(asked.to_vec()));
        let allowed = [b.clone(), c.clone()];
        assert_eq!(both_allow(None, Some(&allowed)), Some(allowed.to_vec()));
        // Both: only what both allow, none when they share nothing.
        assert_eq!(both_allow(Some(&asked), Some(&allowed)), Some(vec![b]));
        assert_eq!(both_allow(Some(&[a]), Some(&[c])), Some(Vec::new()));
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_800_000_000_000;

    /// Terms with a rolling 7-day limit and a lifetime limit.
    fn terms() -> AllowanceTerms {
        AllowanceTerms::from_json(&serde_json::json!({
            "asset": "btc", "active_from": null, "expires_at": null,
            "allowed_payment_endpoint_identifiers": null,
            "per_payment_amount": { "minimum": "0.0001", "maximum": "1" },
            "lifetime_amount_limit": "100",
            "period_limits": [{ "amount_limit": "50", "payment_count_limit": null,
                "period": { "kind": "rolling", "every": 7, "unit": "day" } }]
        }))
        .unwrap()
    }

    fn entry(n: u32, at: u64, outcome: Option<EntryOutcome>) -> LedgerEntry {
        let mut attempt = [0x5a; 16];
        attempt[..4].copy_from_slice(&n.to_be_bytes());
        LedgerEntry {
            obligation: [n.to_be_bytes()[3]; 32],
            attempt_id: AttemptId::from_bytes(attempt),
            value: paykit_lib::Decimal::parse("0.0001").unwrap(),
            asset: paykit_lib::Asset::parse("btc").unwrap(),
            at,
            outcome,
        }
    }

    /// A ledger of entries dated `at` with `outcome`, as long as `len` or longer.
    fn filled(at: u64, outcome: Option<EntryOutcome>, len: usize) -> Ledger {
        let mut ledger = Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap());
        let mut n = 0;
        while ledger.to_json().unwrap().len() < len {
            for _ in 0..10 {
                ledger.entries.push(entry(n, at, outcome));
                n += 1;
            }
        }
        ledger
    }

    #[test]
    fn test_executor_next_ledger_compacts_near_the_size_limit() {
        let terms = terms();
        let base = filled(NOW - 30 * DAY_MS, Some(EntryOutcome::Done), COMPACT_FROM);
        let claims = HashMap::new();
        let next = next_ledger(&terms, &base, &claims, NOW, entry(u32::MAX, NOW, None)).unwrap();
        assert_eq!(next.entries.len(), 1);
        assert_eq!(next.count, u64::try_from(base.entries.len()).unwrap());
        let before = usage(&terms, &base, &claims, NOW).unwrap();
        let mut without = next.clone();
        without.entries.clear();
        let after = usage(&terms, &without, &claims, NOW).unwrap();
        assert_eq!(before.lifetime(), after.lifetime());
        assert_eq!(before.periods(), after.periods());
        // Below the bound nothing is compacted.
        let small = filled(NOW - 30 * DAY_MS, Some(EntryOutcome::Done), 1_000);
        let next = next_ledger(&terms, &small, &claims, NOW, entry(u32::MAX, NOW, None)).unwrap();
        assert_eq!(next.entries.len(), small.entries.len() + 1);
    }

    #[test]
    fn test_executor_next_ledger_refuses_young_entries_that_fill_the_record() {
        // Paid within the 7-day limit's window: none can be compacted, and the record can't
        // take another entry.
        let base = filled(
            NOW - DAY_MS,
            Some(EntryOutcome::Done),
            MAX_LEDGER_VALUE - 100,
        );
        let refused = next_ledger(
            &terms(),
            &base,
            &HashMap::new(),
            NOW,
            entry(u32::MAX, NOW, None),
        )
        .unwrap_err();
        assert!(refused.contains("even after compaction"), "{refused}");
    }

    #[test]
    fn test_executor_next_ledger_keeps_room_for_every_outcome() {
        // Entries without an outcome that fit now, but not once each has one: the payment is
        // refused, so a later outcome's write always fits.
        let mut base = filled(NOW - DAY_MS, None, MAX_LEDGER_VALUE - 8_000);
        while base.len_with_outcomes().unwrap() <= MAX_LEDGER_VALUE {
            let n = u32::try_from(base.entries.len()).unwrap();
            base.entries.push(entry(n, NOW - DAY_MS, None));
        }
        // It takes the new entry by its length now, not with an outcome for each.
        assert!(base.to_json().unwrap().len() < MAX_LEDGER_VALUE - 400);
        let mut claims = HashMap::new();
        for e in &base.entries {
            claims.insert(e.attempt_id, ClaimState::Open);
        }
        let refused =
            next_ledger(&terms(), &base, &claims, NOW, entry(u32::MAX, NOW, None)).unwrap_err();
        assert!(refused.contains("even after compaction"), "{refused}");
    }

    #[test]
    fn test_executor_evidence_keeps_an_unknown_failed_attempt_counted() {
        // Another executor's `failed`, its claim gone and its payer record unreadable: it
        // stays counted, and nothing is written for it.
        assert_eq!(
            evidence_counts(Evidence::Unknown, true, true, false),
            (Some(ClaimState::Done), None)
        );
        // Paid by the record's proof: `done`, written, whoever made it.
        for (failed, gone, journaled) in [
            (true, true, false),
            (true, false, true),
            (false, true, false),
        ] {
            assert_eq!(
                evidence_counts(Evidence::Paid, failed, gone, journaled),
                (Some(ClaimState::Done), Some(ClaimState::Done))
            );
        }
        // A record that reads and holds no proof releases it; so does this app's own
        // journal, which shows it unpaid; and a claim still held decides.
        assert_eq!(
            evidence_counts(Evidence::NotPaid, true, true, false),
            (None, None)
        );
        assert_eq!(
            evidence_counts(Evidence::Unknown, true, true, true),
            (None, None)
        );
        assert_eq!(
            evidence_counts(Evidence::Unknown, true, false, false),
            (None, None)
        );
        // Without an outcome and its claim gone, the entry counts as spent already.
        assert_eq!(
            evidence_counts(Evidence::Unknown, false, true, false),
            (None, None)
        );
    }

    #[test]
    fn test_executor_payer_evidence_needs_a_record_that_reads() {
        let attempt = AttemptId::from_bytes([4; 16]);
        let allowance = AllowanceId::parse(ID).unwrap();
        assert_eq!(
            payer_evidence(None, &attempt, &allowance),
            Evidence::Unknown
        );
        let record = crate::activity::empty(paykit_lib::records::ActivitySide::Payer, [2; 32]);
        assert_eq!(
            payer_evidence(Some(&record), &attempt, &allowance),
            Evidence::NotPaid
        );
    }

    #[test]
    fn test_executor_next_ledger_writes_the_outcomes_the_claims_show() {
        let base = Ledger {
            entries: vec![entry(1, NOW - DAY_MS, None), entry(2, NOW - DAY_MS, None)],
            ..Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap())
        };
        let mut claims = HashMap::new();
        claims.insert(base.entries[0].attempt_id, ClaimState::Done);
        claims.insert(
            base.entries[1].attempt_id,
            ClaimState::Failed { by_owner: false },
        );
        let next = next_ledger(&terms(), &base, &claims, NOW, entry(3, NOW, None)).unwrap();
        let outcomes: Vec<_> = next.entries.iter().map(|e| e.outcome).collect();
        assert_eq!(outcomes, vec![Some(EntryOutcome::Done), None, None]);
    }
    #[test]
    fn test_executor_repair_ledger_compacts_near_the_size_limit() {
        // A merge past the record's size, its entries paid long ago: compacted as an
        // executor batch's ledger is, it fits, and counts the same.
        let terms = terms();
        let merged = filled(
            NOW - 30 * DAY_MS,
            Some(EntryOutcome::Done),
            MAX_LEDGER_VALUE + 1_000,
        );
        let claims = HashMap::new();
        let written = repair_ledger(&terms, &merged, &claims, NOW).unwrap();
        assert!(written.entries.is_empty());
        assert_eq!(written.count, u64::try_from(merged.entries.len()).unwrap());
        let before = usage(&terms, &merged, &claims, NOW).unwrap();
        let after = usage(&terms, &written, &claims, NOW).unwrap();
        assert_eq!(before.lifetime(), after.lifetime());
        assert_eq!(before.periods(), after.periods());
        check_successor(&terms, &merged, &written, NOW).unwrap();
    }

    #[test]
    fn test_executor_repair_ledger_keeps_room_for_every_outcome() {
        // Young entries without an outcome that fit now, but not once each has one: the
        // repair is refused, so every later outcome's write fits.
        let mut merged = filled(NOW - DAY_MS, None, MAX_LEDGER_VALUE - 8_000);
        while merged.len_with_outcomes().unwrap() <= MAX_LEDGER_VALUE {
            let n = u32::try_from(merged.entries.len()).unwrap();
            merged.entries.push(entry(n, NOW - DAY_MS, None));
        }
        assert!(merged.to_json().unwrap().len() <= MAX_LEDGER_VALUE);
        let refused = repair_ledger(&terms(), &merged, &HashMap::new(), NOW).unwrap_err();
        assert!(refused.contains("even after compaction"), "{refused}");
        // The outcomes the claims show are written in first.
        let small = Ledger {
            entries: vec![entry(1, NOW - DAY_MS, None)],
            ..Ledger::empty([2; 32], AllowanceId::parse(ID).unwrap())
        };
        let mut claims = HashMap::new();
        claims.insert(small.entries[0].attempt_id, ClaimState::Done);
        let written = repair_ledger(&terms(), &small, &claims, NOW).unwrap();
        assert_eq!(written.entries[0].outcome, Some(EntryOutcome::Done));
    }

    #[test]
    fn test_executor_repair_conditions_cut_at_an_anchored_boundary() {
        // Ten seconds before a monthly limit's boundary, the write's window ends at that
        // boundary, as an executor batch's does: it lands in the period its figures were
        // counted in. Without an anchored limit it is 60 s.
        let anchored = AllowanceTerms::from_json(&serde_json::json!({
            "asset": "btc", "active_from": null, "expires_at": null,
            "allowed_payment_endpoint_identifiers": null,
            "per_payment_amount": null, "lifetime_amount_limit": "100",
            "period_limits": [{ "amount_limit": "50", "payment_count_limit": null,
                "period": { "kind": "anchored", "every": 1, "unit": "month",
                    "anchor": "2026-01-31T00:00:00.000Z" } }]
        }))
        .unwrap();
        let boundary = pubky_switch_core::time::parse_rfc3339("2026-03-31T00:00:00.000Z").unwrap();
        let version = RecordVersion {
            generation: 1,
            version: 4,
        };
        let window = |terms: &AllowanceTerms, at: u64| {
            repair_conditions(terms, &[9; 32], version, at)
                .unwrap()
                .into_iter()
                .find_map(|c| match c {
                    Condition::ServerTimeIn { start, end } => Some((start, end)),
                    _ => None,
                })
                .unwrap()
        };
        let at = boundary - 10_000;
        assert_eq!(window(&anchored, at), (Some(at), Some(boundary)));
        assert_eq!(window(&terms(), at), (Some(at), Some(at + 60_000)));
        let conditions = repair_conditions(&terms(), &[9; 32], version, at).unwrap();
        assert!(conditions.contains(&Condition::RecordVersion {
            key: [9; 32],
            version
        }));
    }

    /// A proof by `from`, of attempt 4, naming `allowance_id`, in a payer record of `to`'s
    /// request.
    fn proof_record(
        from: &crate::testkit::Party,
        to: &crate::testkit::Party,
        allowance_id: Option<AllowanceId>,
    ) -> ActivityRecord {
        let body = paykit_lib::bodies::PaymentProof {
            attempt_id: AttemptId::from_bytes([4; 16]),
            billing_period: None,
            payment_endpoint_identifier: PaymentEndpointIdentifier::new("btc-lightning-bolt11")
                .unwrap(),
            payment_app_id: [0; 16],
            list_version: None,
            amount: None,
            allowance_id,
            label: None,
            payment_reference: None,
            proof: serde_json::from_value(serde_json::json!({ "preimage": "00" })).unwrap(),
        }
        .to_body(1)
        .unwrap();
        let mut record = crate::activity::empty(paykit_lib::records::ActivitySide::Payer, to.id());
        record.proof = Some(from.message(to, "paykit.payment_proof", vec![[7; 32]], body));
        record
    }

    #[test]
    fn test_executor_attempt_excluded_only_on_its_owners_own_proof() {
        use crate::testkit::Party;
        let wallet = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let phone = Party::new("alice", "phone", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new(
            "bob",
            "shop",
            &["paykit_pay", "paykit_contacts", "paykit_request"],
        );
        let allowance = AllowanceId::parse(ID).unwrap();
        let attempt = AttemptId::from_bytes([4; 16]);
        let owner = wallet.app_id();
        let excluded = |record: &ActivityRecord, removed: &[[u8; 16]]| {
            proof_shows_made_otherwise(
                Some(record),
                &wallet.id(),
                &attempt,
                owner,
                removed,
                &allowance,
            )
        };
        // The owner's own proof naming no Allowance: made by hand.
        let by_hand = proof_record(&wallet, &shop, None);
        assert!(excluded(&by_hand, &[]));
        // A proof naming the Allowance counts it, as does one by another own app, one whose
        // signer is removed, and one the counterparty signed leaving `allowance_id` out.
        assert!(!excluded(
            &proof_record(&wallet, &shop, Some(allowance.clone())),
            &[]
        ));
        assert!(!excluded(&proof_record(&phone, &shop, None), &[]));
        assert!(!excluded(&by_hand, &[owner]));
        let from_counterparty = proof_record(&shop, &wallet, None);
        assert_eq!(
            from_counterparty.proof.as_ref().unwrap().sender(),
            shop.id()
        );
        assert!(!excluded(&from_counterparty, &[]));
        assert!(!proof_shows_made_otherwise(
            None,
            &wallet.id(),
            &attempt,
            owner,
            &[],
            &allowance
        ));
        // A payer record holding a proof the counterparty sent is never held as one that
        // reads: the repair refuses on it rather than count from it.
        let mut held = from_counterparty.clone();
        held.counterparty = shop.id();
        let read = held.to_json().ok().map(|json| {
            ActivityRecord::read(
                &json,
                &wallet.id(),
                paykit_lib::records::ActivitySide::Payer,
            )
        });
        assert!(!matches!(read, Some(Ok(_))));
    }

    #[test]
    fn test_executor_proof_naming_the_allowance_counts_as_paid() {
        use crate::testkit::Party;
        let wallet = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let shop = Party::new("bob", "shop", &["paykit_request"]);
        let allowance = AllowanceId::parse(ID).unwrap();
        let amount = PaymentAmount {
            asset: paykit_lib::Asset::parse("btc").unwrap(),
            value: paykit_lib::Decimal::parse("0.5").unwrap(),
        };
        let paid = proof_record(&wallet, &shop, Some(allowance.clone()));
        let entry = paid_under(Some(&paid), &allowance, [8; 32], &amount).unwrap();
        assert_eq!(entry.attempt_id, AttemptId::from_bytes([4; 16]));
        assert_eq!(
            (entry.obligation, entry.outcome),
            ([8; 32], Some(EntryOutcome::Done))
        );
        assert_eq!(entry.value, amount.value);
        assert!(paid_under(
            Some(&proof_record(&wallet, &shop, None)),
            &allowance,
            [8; 32],
            &amount
        )
        .is_none());
        assert!(paid_under(None, &allowance, [8; 32], &amount).is_none());
    }

    #[test]
    fn test_executor_periods_to_read_are_those_whose_claim_may_be_held() {
        use paykit_lib::periods::{AnchoredRule, Unit};
        let start = pubky_switch_core::time::parse_rfc3339("2026-01-01T00:00:00.000Z").unwrap();
        let monthly = |ends_at: Option<u64>| {
            Recurrence::new(
                AnchoredRule::new(start, 1, Unit::Month).unwrap(),
                start,
                ends_at,
            )
            .unwrap()
        };
        let at = pubky_switch_core::time::parse_rfc3339("2026-06-10T00:00:00.000Z").unwrap();
        let secs = |text: &str| pubky_switch_core::time::parse_rfc3339(text).unwrap() / 1000;
        // Without a deadline every period from the first, through the one containing `at`.
        let all = periods_to_read(&monthly(None), None, at).unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(all[0], secs("2026-01-01T00:00:00.000Z"));
        assert_eq!(all[5], secs("2026-06-01T00:00:00.000Z"));
        // With a deadline of 15 days, a claim is gone 30 days after it: May and June stay.
        let deadline = PaymentDeadline::PeriodStart {
            seconds: 15 * 24 * 3600,
        };
        let held = periods_to_read(&monthly(None), Some(&deadline), at).unwrap();
        assert_eq!(
            held,
            vec![
                secs("2026-05-01T00:00:00.000Z"),
                secs("2026-06-01T00:00:00.000Z")
            ]
        );
        // A request that ended: up to its last period.
        let ended = monthly(Some(
            pubky_switch_core::time::parse_rfc3339("2026-03-01T00:00:00.000Z").unwrap(),
        ));
        assert_eq!(periods_to_read(&ended, None, at).unwrap().len(), 2);
        // Before the first period: none.
        assert!(periods_to_read(&monthly(None), None, start - 1)
            .unwrap()
            .is_empty());
        // More periods than a repair reads refuse it.
        let minutely = Recurrence::new(
            AnchoredRule::new(start, 1, Unit::Minute).unwrap(),
            start,
            None,
        )
        .unwrap();
        let refused = periods_to_read(&minutely, None, at).unwrap_err();
        assert!(refused.contains("too many Billing Periods"), "{refused}");
    }
}
