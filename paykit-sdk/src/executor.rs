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
//!   empty only by the executor of the first grant that kept none.
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
//!   "Handoff"): at a resume it is made after an end, a handoff or a conflict too, through
//!   the method's repeat with the same idempotency key, while what holds while unknown
//!   still holds it ([`Paykit::repeat_under_allowance_held`]).
//! - **Immediately before the dispatch** (Paykit spec 8.1, 8.3): the deny list, the request
//!   record and the block floor are read from the switch again, twice, before the attempt's
//!   `Paying` commit and after it, and the dispatch holds on a block, a stop, a raise of the
//!   floor past the request's date, or a read that fails
//!   (`Paykit::automatic_dispatch_held`); the batch doesn't condition on the floor's version.
//!
//! Security: every executor batch is conditioned on the Allowance record version read, so
//! an end, a rejection or a handoff written since makes it fail; usage is counted from the
//! ledger and the claims, and a ledger the checks refuse stops automatic payment rather
//! than being trusted (fail closed). Limits are enforced here, not by the service.

use std::collections::HashMap;

use paykit_lib::allowances::{
    check_limits, check_successor, usage, AllowanceTerms, ClaimState, Ledger, LedgerEntry,
};
use paykit_lib::bodies::{Body, ExecutorNotice, NoticeReason, PaymentRequest, ProposerRole};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::grant::{grant_fields, ExecutorGrant, EXECUTOR_GRANT_LABEL};
use paykit_lib::kinds::Kind;
use paykit_lib::records::{ActivityRecord, AllowanceRecord, GrantEntry, Origin};
use paykit_lib::{AllowanceId, AttemptId, PaymentAmount, PaymentEndpointIdentifier};
use pubky_switch_client::app::{
    Initiator, OpBatch, RecordContent, RecordRead, RecordTarget, SharedRecord, UserConfirmed,
};
use pubky_switch_core::claims::Outcome;
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

/// The size limit of a shared record value: 256 KiB (Switch wire 6.2).
const MAX_RECORD_VALUE: usize = 256 * 1024;

/// How many times a grant's write is built again after its record changed meanwhile.
const GRANT_TRIES: usize = 3;

fn not_automatic(why: &str) -> PaykitError {
    Refusal::NotAutomatic(why.into()).into()
}

/// The last ledger version the executor wrote or read with an authenticated version, and
/// accepted (`lv/<Allowance record key>`, Paykit spec 8.3 "Last ledger version"): its
/// version, the item hash of its write, and its content, the base the next version is
/// checked against. The Allowance record it names repeats the key's, so a value read back
/// is bound to its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeptLedger {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) generation: u64,
    pub(crate) version: u64,
    #[serde(with = "serde_b64")]
    pub(crate) item_hash: [u8; 32],
    #[serde(with = "serde_b64_vec")]
    pub(crate) value: Vec<u8>,
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

impl KeptLedger {
    fn version(&self) -> RecordVersion {
        RecordVersion {
            generation: self.generation,
            version: self.version,
        }
    }
}

/// Check a kept ledger version read back from the store or a backup, the same check for
/// both: it names the Allowance record its key names, its version is one a write gives
/// (`seq` at least 1), and its content reads as a ledger. Returns the ledger.
///
/// # Errors
///
/// A corrupt store, never an absence.
pub(crate) fn check_kept_ledger(record: &[u8; 32], kept: &KeptLedger) -> Result<Ledger> {
    if &kept.record != record || kept.version == 0 || kept.generation == 0 {
        return Err(PaykitError::corrupt());
    }
    Ledger::from_json(&kept.value).map_err(|_| PaykitError::corrupt())
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
    /// held must name it; without a record that reads, the kept ledger version must hold an
    /// entry for the attempt. An entry naming another Allowance than its proof is a corrupt
    /// store, checked before money moves and before the outcome's proof or record is built.
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
        let in_ledger = self
            .kept_ledger(&allowance)
            .await?
            .is_some_and(|(_, ledger)| ledger.entries.iter().any(|e| e.attempt_id == attempt));
        if !in_ledger {
            return unproven();
        }
        Ok(Ok(Some(allowance_id)))
    }

    /// The Allowance `named`, of the request record of `payee`, that `attempt` was made
    /// under: the one its ledger record, as held or else as read from the switch once, holds
    /// an entry for the attempt in. `None` for a record naming none, or a ledger not held,
    /// that doesn't read or bind, or holds no such entry: the attempt is then taken as made
    /// by hand, and its proof names no Allowance. For a journal entry rebuilt after a
    /// restore.
    pub(crate) async fn attempt_under(
        &mut self,
        payee: &[u8; 32],
        named: Option<&AllowanceId>,
        attempt: AttemptId,
    ) -> Result<Option<AllowanceId>> {
        let Some(allowance_id) = named else {
            return Ok(None);
        };
        let target = ledger_target(payee, allowance_id);
        let key = self.app.record_key(&target)?;
        if self
            .held_ledger_names(&key, payee, allowance_id, attempt)
            .await?
        {
            return Ok(Some(allowance_id.clone()));
        }
        // The copy a restore holds may predate the attempt's entry: the ledger is read from
        // the switch once. A read that fails leaves the copy as it was; only a failure of the
        // app's own store is an error.
        if let Err(error) = self.app.read_record(&target).await {
            let error: PaykitError = error.into();
            if crate::hold::local_failure(&error) {
                return Err(error);
            }
            return Ok(None);
        }
        Ok(self
            .held_ledger_names(&key, payee, allowance_id, attempt)
            .await?
            .then(|| allowance_id.clone()))
    }

    /// Whether the ledger the app holds at `key`, bound to `payee`'s Allowance
    /// `allowance_id`, holds an entry for `attempt`.
    async fn held_ledger_names(
        &self,
        key: &[u8; 32],
        payee: &[u8; 32],
        allowance_id: &AllowanceId,
        attempt: AttemptId,
    ) -> Result<bool> {
        let Some(held) = self.app.shared_record(key).await? else {
            return Ok(false);
        };
        Ok(held
            .content
            .value()
            .and_then(|value| Ledger::from_json(value).ok())
            .filter(|l| l.counterparty == *payee && &l.allowance_id == allowance_id)
            .is_some_and(|ledger| ledger.entries.iter().any(|e| e.attempt_id == attempt)))
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
        let Some(held) = self.app.shared_record(ledger_key).await? else {
            return Ok(());
        };
        let (Some(value), true) = (held.content.value(), held.authenticated) else {
            return Ok(());
        };
        let Some(op) = self.held_op(ledger_key).await? else {
            return Ok(());
        };
        let kept = KeptLedger {
            record: *record,
            generation: held.version.generation,
            version: held.version.version,
            item_hash: op.item_hash().map_err(|_| PaykitError::corrupt())?,
            value: value.to_vec(),
        };
        check_kept_ledger(record, &kept)?;
        let mut tx = Tx::default();
        tx.put(&key::kept_ledger(record), &kept)?;
        // A version kept answers the first grant's empty ledger.
        tx.delete(&key::empty_ledger(record));
        self.local_commit(tx).await
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
    /// Allowance at `record`, read as held (Paykit spec 8.3): a repeat finishes that payment,
    /// so the Allowance ended, handed off or conflicted since doesn't stop it ("Conflicted",
    /// "Handoff"), and neither does its consent removed for those. What can't be known holds
    /// it: the record not held or not binding, its view or its ledger's divergent or seen so
    /// and not confirmed (Paykit spec 6), its current write one the app ignores and the user
    /// hasn't confirmed or a removed app's after its `distrust_from`, its consent to be
    /// confirmed again after a restore or a `missed` item, or no consent entry at all.
    pub(crate) async fn repeat_under_allowance_held(
        &mut self,
        record: &[u8; 32],
    ) -> Result<std::result::Result<(), Refusal>> {
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
    /// the first grant takes as an empty ledger, until it has written one.
    async fn ledger_base(&mut self, under: &Under) -> Result<Option<Ledger>> {
        let own = self.identity();
        let target = ledger_target(&under.allowance.counterparty, &under.allowance.allowance_id);
        let ledger_key = self.app.record_key(&target)?;
        let kept = self.kept_ledger(&under.record_key).await?;
        let shared = match self.app.read_record_fresh(&target).await? {
            RecordRead::Fresh(shared) => Some(shared),
            RecordRead::Kept {
                reason: pubky_switch_client::app::KeptReason::NotFound,
                ..
            } => None,
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
            if kept.is_none() && first_grant && self.empty_ledger_unsent(&under.record_key).await? {
                return Ok(None);
            }
            return Err(not_automatic("usage history lost: the ledger is missing"));
        };
        if shared.divergent || shared.flagged || shared.doubtful {
            // The view is taken first, so a ledger divergent now is marked, and stays paused
            // until the user confirms it, also once a newer write clears the view.
            self.pause_of(&shared).await?;
            return Err(not_automatic("the ledger is divergent or untrusted"));
        }
        if self.pause_of(&shared).await?.blocks_automatic() {
            return Err(not_automatic(
                "the ledger was seen divergent and the user hasn't confirmed it",
            ));
        }
        if written_by_removed_app(self.app.own_identity(), &shared) {
            return Err(not_automatic(
                "the ledger's write is a removed app's after its distrust_from",
            ));
        }
        let value = match &shared.content {
            RecordContent::Value(value) if !value.is_empty() => value.clone(),
            _ => {
                return Err(not_automatic(
                    "usage history lost: the ledger is deleted or unread",
                ))
            }
        };
        // The binding rule (Paykit wire 5.5): the key derives from the ledger's own
        // counterparty and Allowance, and both are its Allowance record's.
        let ledger = Ledger::from_json(&value)
            .ok()
            .filter(|l| {
                l.counterparty == under.allowance.counterparty
                    && l.allowance_id == under.allowance.allowance_id
            })
            .ok_or_else(|| not_automatic("the ledger doesn't bind to its Allowance"))?;
        if self
            .app
            .record_key(&ledger_target(&ledger.counterparty, &ledger.allowance_id))?
            != ledger_key
        {
            return Err(not_automatic("the ledger doesn't bind to its Allowance"));
        }
        // A ledger that was at its key at the first grant is one the executor didn't accept
        // (Paykit spec 8.3, "A grant implies a ledger"), whoever wrote it.
        if kept.is_none() && self.ledger_unaccepted(&under.record_key).await? {
            return Err(not_automatic(
                "the ledger at its key before the first grant isn't one this executor accepted",
            ));
        }
        let op = self
            .held_op(&ledger_key)
            .await?
            .ok_or_else(|| not_automatic("the ledger's write isn't held"))?;
        written_by_executor(
            &under.allowance,
            &own,
            &under.record_key,
            under.version,
            &op,
            shared.writer,
        )
        .map_err(not_automatic)?;
        // An executor that never held a ledger version, such as a new executor after a
        // handoff that never read the ledger, or one restored from a backup that holds none,
        // builds on the write shown once it passes every other check, and keeps it from then
        // on (Paykit spec 8.3, "Last ledger version"; open question 32). A ledger that was at
        // its key at the first grant is the exception: one the executor didn't accept.
        if let Some((kept, held)) = &kept {
            let item = op.item_hash().map_err(|_| PaykitError::corrupt())?;
            let built_on = match &op.body().kind {
                OpKind::RecordPut(put) => put.expected_version,
                _ => return Err(PaykitError::corrupt()),
            };
            if item != kept.item_hash {
                if built_on < kept.version() {
                    return Err(not_automatic(
                        "the ledger shown is neither the kept version nor built on it",
                    ));
                }
                let at = shared
                    .written_at_ms
                    .or(self.service_time())
                    .ok_or_else(|| not_automatic("no service time verified"))?;
                check_successor(&under.terms, held, &ledger, at)
                    .map_err(|_| not_automatic("the ledger counts less than the version kept"))?;
            }
        }
        ledger
            .check_valid(&under.terms)
            .map_err(|_| not_automatic("the ledger isn't valid"))?;
        self.keep_ledger(&under.record_key, &ledger_key).await?;
        Ok(Some(ledger))
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
        // The first grant's empty ledger, while owed, goes before the first payment.
        self.write_owed_empty_ledger(under).await?;
        let base = self.ledger_base(under).await?;
        // Whether this batch carries the ledger's first write: marked sent just before it
        // goes, after every check, so a refusal here leaves the empty ledger owed.
        let first_ledger_write = base.is_none();
        let mut ledger = base.unwrap_or_else(|| {
            Ledger::empty(
                under.allowance.counterparty,
                under.allowance.allowance_id.clone(),
            )
        });
        let mut claims = HashMap::new();
        let open: Vec<([u8; 32], AttemptId)> = ledger
            .entries
            .iter()
            .filter(|e| e.outcome != Some(paykit_lib::allowances::EntryOutcome::Done))
            .map(|e| (e.obligation, e.attempt_id))
            .chain(ledger.failed.iter().map(|f| (f.obligation, f.attempt_id)))
            .collect();
        for (obligation, attempt_id) in open {
            // A read that fails is the error: never taken as a claim gone, which counts as
            // spent, or as one that releases.
            if let Some(read) = self.app.read_claim(&obligation).await? {
                if let Some(state) = claim_state(&read.view, &attempt_id) {
                    claims.insert(attempt_id, state);
                }
            }
        }
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
        let used = usage(&under.terms, &ledger, &claims, at)
            .map_err(|_| not_automatic("usage can't be counted"))?;
        check_limits(&under.terms, &used, &amount.value)
            .map_err(|limit| not_automatic(&format!("the {} limit has no room", limit.as_str())))?;
        let end = under
            .terms
            .entry_window_end(at)
            .map_err(|_| not_automatic("the entry's window can't be written"))?;
        ledger.entries.push(LedgerEntry {
            obligation,
            attempt_id: *attempt,
            value: amount.value.clone(),
            asset: amount.asset.clone(),
            at,
            outcome: None,
        });
        let value = ledger.to_json()?;
        if value.len() > MAX_RECORD_VALUE {
            return Err(not_automatic("the ledger can't take another entry"));
        }
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
            self.app
                .may_attempt(true)
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
            item_hash: [3; 32],
            value: ledger.to_json().unwrap(),
        }
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
}
