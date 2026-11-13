//! Divergent records (Paykit spec 6, "Divergent records"; 8.1; Switch spec 12.6, Switch wire
//! 7.4.9, 7.4.14).
//!
//! After a provider loss, an upload that conflicts with a write made at the new service is
//! kept at the switch, and the record's view shows `divergent` until a writer writes a newer
//! version there. The client holds that flag for the latest view it read
//! ([`SharedRecord::divergent`]) and raises `AppEvent::RecordDivergent` once per change; the
//! app takes the current write as any other, but the upload may hold what it lacks, such as
//! a cancellation, a receipt or a newer list. So:
//!
//! - **A pause mark per record** (`p/dv/<record key>`): kept for each record seen divergent,
//!   with the write shown and whether the user confirmed it. It is set the first time a view
//!   the app reads shows the record divergent, and kept until the user confirms
//!   ([`Paykit::confirm_divergent`]), also once a newer write has cleared the view. A
//!   confirmation keeps the version of the write it was given over: the same divergent view
//!   asks nothing again, a later divergent view of another write pauses the record again,
//!   and the confirmed mark goes at the end of the first receive whose view of the record
//!   shows it no longer divergent, once the messages kept over it are written. Confirming a
//!   record no longer held ends its pause and drops the mark. Marks are of any Paykit
//!   collection, so an Allowance's `allowances` and `allowance_ledgers` records take them
//!   the same way.
//! - **No automatic decision that starts or repeats a payment, or accepts a request**, while
//!   a record is paused or its view divergent, confirmed or not
//!   ([`Pause::blocks_automatic`]): today a resume's repeat of a dispatched payment, against
//!   its request record or the `payment_lists` record of its destination.
//! - **A payment already dispatched is recorded once the user has confirmed**
//!   ([`Pause::dispatched_write_waits`]): its owed acceptance, its outcome and its proof
//!   wait while the record is paused, and go once it is confirmed, also over a view still
//!   divergent, so the payee gets the proof of a payment made.
//! - **By hand only after the user's confirmation** ([`Pause::blocks_by_hand`]): paying a
//!   request, paying to an endpoint of a paused list, single-use or reusable, and a user's
//!   `done` ([`crate::Refusal::Divergent`]).
//! - **A credit only after a confirmation shown for a credit** ([`Pause::blocks_credit`],
//!   Paykit spec 8.1 step 4): a payee app credits no proof while its record, or another
//!   record of the same counterparty, was seen divergent and its user hasn't confirmed it
//!   shown that it may hold a credit of a settlement its current write lacks
//!   ([`DivergentRecord::may_hold_credit`], which only a credit's refusal shows). A plain
//!   confirmation releases payments and writes, not a credit.
//! - **The kept messages go first.** Once the user confirms a record, the messages kept
//!   over it, receipts included, are written at the next receive, before anything is paid,
//!   accepted or credited on it: until they are, every gate above holds the record
//!   ([`DivergentRecord::writes_waiting`]).
//! - **Writes over a divergent view** ([`Pause::write_waits`]): a stop (a cancellation or a
//!   rejection) is written; any other receive-step write is kept here (`p/dw/<record
//!   key>/<role>/<msg_id>`: of each role, the earliest the inbox accepted and the 16 latest
//!   after it, one dropped for this bound dropped as one never processed, its pair forgotten
//!   so a re-send of it is taken, and the message just received, whose pair its own step
//!   writes, kept over the bound until a later one drops it so; one the client won't forget
//!   yet stays kept, over the bound; of lists, the newest alone), and written once the user has confirmed the
//!   record, in the order the inbox accepted them, the earliest into its role and the
//!   others reflected, or given up 30 days after its acceptance with the host's notice, as
//!   a stored entry's message is, but never while its record's copy may be incomplete; an
//!   own write of another kind, such as a receipt, waits for the confirmation. One rule for
//!   every write that adds no stop ([`Pause::adds_no_stop_waits`]): it waits while the view
//!   is divergent and unconfirmed, and while messages kept over the record that the inbox
//!   accepted before it wait to be written, so a message that comes after the confirmation,
//!   in the receive step or as a stored entry, a kept reply or a held list, is kept or waits
//!   behind them and never takes their role.
//! - **A confirmation is of the write shown** ([`DivergentRecord::version`]): one given for
//!   another write than the one the app holds is refused, and the host shows the record
//!   again.
//! - **Pointers are proven** before anything acts on them: a mark and a kept message keep
//!   the collection of their record and the write they were seen at, and a record held at
//!   their key of another collection or at an earlier write isn't theirs. Every reader of a
//!   mark goes through one check ([`Marked::of`]): a mark that doesn't name its record
//!   reads as paused and names nothing. Every gate holds the record, a stop goes over it
//!   with no mark written, the user's confirmation replaces it at the write held, and every
//!   other record goes on. A kept message that doesn't point to its record, kept over a
//!   later write or of another settlement than the rail's names now, isn't written, holds
//!   nothing back, and is given up at its 30 days as any kept message.
//!
//! **Seen at every read.** A view is taken wherever the SDK reads a record to act on it, in
//! the receive step with the cursor move and in its calls. The client's report that a view
//! showed a record divergent (`AppEvent::RecordDivergent`) is a duty event, delivered until
//! acknowledged, through a restart and a flag cleared since: each receive takes it by the
//! record's key and pauses the record whatever its view shows now, in a commit before the
//! one that counts the event taken, so it is acknowledged only once the record is paused. A
//! record its user had confirmed is paused again: the confirmation was of the write shown
//! then, and confirms nothing for a later divergence. The mark keeps the latest event it
//! took, by its ID: the same event delivered again, as after a stop before the commit that
//! counts it taken, changes nothing, so it never pauses again a record confirmed after it;
//! an event raised again under a new ID while a paused mark waits, as after a restore,
//! leaves the pause as it is. At a load, every record held is looked at once too.
//!
//! **Restore.** A backup carries the marks and the kept writes. A restore installs every
//! mark unconfirmed, a confirmation given before it included: the restored app pauses each
//! record it had seen divergent until its user confirms again. A `missed` item changes no
//! mark: it loses inbox entries, not the records' views, which the listing after it reads
//! again.
//!
//! **Two own apps** keep their own marks: one app's confirmation releases nothing in another,
//! which pauses until it has seen the record itself and its own user has confirmed.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::StoredMessage;
use paykit_lib::Body;
use pubky_switch_client::app::{
    AppError, EventId, ReceiveContext, RecordContent, RecordTarget, Refusal as ClientRefusal,
    SharedRecord, UserConfirmed,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::ops::RecordVersion;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, Role};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::receive::{Fail, Settlement};
use crate::records::{list_target, KeptMessage};
use crate::state::{self, key, Tx};

/// A record the app has seen divergent, as the host shows it (Paykit spec 6): from
/// [`Paykit::divergent_records`], a receive's summary, or a refusal
/// ([`crate::Refusal::Divergent`]). [`Paykit::confirm_divergent`] takes its `record`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DivergentRecord {
    /// The record key.
    pub record: [u8; 32],
    /// Its collection: what the upload may hold a newer copy of, such as a cancellation in a
    /// request record or a newer list in a `payment_lists` record.
    pub collection: Collection,
    /// Paused: nothing is paid, accepted or credited on it automatically, and by hand only
    /// once the user confirms ([`Paykit::confirm_divergent`]).
    pub paused: bool,
    /// The latest view the app read shows it divergent now.
    pub divergent: bool,
    /// The write of the record the app holds, the one shown; `None` for a record it holds
    /// no more. [`Paykit::confirm_divergent`] confirms only that write.
    pub version: Option<RecordVersion>,
    /// Shown for a credit (Paykit spec 8.1 step 4): the record may hold a credit of a
    /// settlement its current write lacks, a receipt in its upload, so the app can't tell a
    /// proof isn't credited already. Only a refusal of a credit
    /// ([`Paykit::confirm_payment`]) shows it, and the app keeps that it did, at the write
    /// shown: a confirmation counts for a credit only when given over a record shown with
    /// it, of the write a credit's refusal showed. A plain one, or one whose flag the host
    /// set itself, leaves the credit refused. The showing the app keeps is the refusal it
    /// returned to the host: that the user saw it is the host's word, as the
    /// [`UserConfirmed`] the confirmation takes is.
    pub may_hold_credit: bool,
    /// The user has confirmed it, and the messages kept over it, receipts included, are
    /// written at the next receive ([`Paykit::receive`]): until then nothing is paid,
    /// accepted or credited on it, by hand or automatically.
    pub writes_waiting: bool,
}

/// The version of a record's write, as a mark keeps it: by generation, then `seq`, the
/// order a record's versions only grow in (Switch wire 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SeenVersion {
    generation: u64,
    version: u64,
}

impl SeenVersion {
    fn record_version(self) -> RecordVersion {
        RecordVersion {
            generation: self.generation,
            version: self.version,
        }
    }
}

/// The pause mark of one record (`p/dv/<record key>`). The record and its collection repeat
/// in it, so a reader binds the value to its key, and the version of the write whose view
/// showed it divergent is the proof of what it points to: a record held at that key is of
/// that collection, at that version or a later one ([`Marked::of`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DivergentMark {
    #[serde(with = "serde_b64")]
    record: [u8; 32],
    collection: String,
    /// The write whose view showed the record divergent when it was paused.
    seen: SeenVersion,
    /// The write the user confirmed over, at or after `seen`; `None` while paused.
    confirmed: Option<SeenVersion>,
    /// The write the user confirmed over shown that it may hold a credit
    /// ([`DivergentRecord::may_hold_credit`]); `None` until then. Never without `confirmed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credit_confirmed: Option<SeenVersion>,
    /// The write a refusal of a credit showed the record at, that it may hold a credit
    /// ([`Paykit::refuse_credit_while_paused`]): only a confirmation of that write counts for
    /// a credit, whatever the host passes back. `None` until a credit's refusal names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credit_shown: Option<SeenVersion>,
    /// The latest of the client's divergence reports (`AppEvent::RecordDivergent`) this mark
    /// has taken, by its event ID's stable string form: the same report again, as a restart
    /// delivers one taken before its own commit, or an earlier one of the same store, changes
    /// nothing, so it never pauses again a record the user confirmed after it. `None` for a
    /// mark no report has set or kept since, and after a restore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<String>,
}

impl DivergentMark {
    fn paused(view: &View) -> Self {
        Self {
            record: view.record,
            collection: view.collection.name().to_owned(),
            seen: view.version,
            confirmed: None,
            credit_confirmed: None,
            credit_shown: None,
            event: None,
        }
    }

    /// The mark with `event` the latest report it has taken.
    fn taking(mut self, event: &EventId) -> Self {
        self.event = Some(event.to_string());
        self
    }

    /// Whether `event` is a report this mark has taken already, or an earlier one of the same
    /// store: the client's event IDs order only within one store, so one of another store
    /// is never taken as taken.
    fn has_taken(&self, event: &EventId) -> bool {
        self.event
            .as_deref()
            .and_then(|taken| taken.parse::<EventId>().ok())
            .and_then(|taken| taken.cmp_same_store(event))
            .is_some_and(std::cmp::Ordering::is_ge)
    }

    /// Whether a credit waits for the user's confirmation of this record shown that it may
    /// hold one (Paykit spec 8.1 step 4).
    fn credit_waits(&self) -> bool {
        self.credit_confirmed.is_none()
    }

    /// Whether the record is paused: seen divergent and not confirmed since.
    pub(crate) fn is_paused(&self) -> bool {
        self.confirmed.is_none()
    }

    /// The mark's collection; a mark is read only bound, so it names one of Paykit's.
    fn collection(&self) -> Result<Collection> {
        Collection::from_name(&self.collection).ok_or_else(PaykitError::corrupt)
    }

    /// The mark bound to the key it is read at, `dv/<record>` (`state::Stored`): the record
    /// its key names, of one of Paykit's collections, and a confirmation or a credit's
    /// showing, if any, not before the write it paused, with a credit's confirmation only
    /// beside a confirmation, states no run leaves. The one check for a restore and for every read at runtime; a value that fails
    /// it is a corrupt store, never an absent mark.
    pub(crate) fn bind_to(&self, record: &[u8; 32]) -> Result<()> {
        if self.record != *record
            || self.confirmed.is_some_and(|at| at < self.seen)
            || self.credit_confirmed.is_some_and(|at| at < self.seen)
            || self.credit_shown.is_some_and(|at| at < self.seen)
            || (self.credit_confirmed.is_some() && self.confirmed.is_none())
            || self
                .event
                .as_deref()
                .is_some_and(|event| event.parse::<EventId>().is_err())
        {
            return Err(PaykitError::corrupt());
        }
        self.collection().map(|_| ())
    }

    /// The mark as a restore installs it: paused, a confirmation given before the backup
    /// included, and shown for a credit again only by a refusal after the restore (module
    /// docs, "Restore").
    pub(crate) fn restored(mut self) -> Self {
        self.confirmed = None;
        self.credit_confirmed = None;
        self.credit_shown = None;
        // The restored client raises its reports again under IDs of its own store.
        self.event = None;
        self
    }
}

/// Whether a value that points to the record held as `view`, of `collection`, seen at the
/// write `seen`, points to it: the record held there is of that collection, at that write or
/// a later one, a record's versions only growing. A record not held proves nothing either
/// way: a pause stays.
fn points_to(view: Option<&View>, collection: Collection, seen: SeenVersion) -> bool {
    match view {
        Some(view) => view.collection == collection && view.version >= seen,
        None => true,
    }
}

/// A record's mark as every reader takes it ([`Marked::of`]): read bound to its key, then
/// checked against the record held there before anything is decided on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Marked {
    /// No mark.
    Clear,
    /// A mark that points to the record held at its key.
    Mark(DivergentMark),
    /// A mark that doesn't point to the record held at its key: one of another collection,
    /// or seen at a later write than the one held. It reads as paused and names nothing:
    /// every gate holds the record as paused, a stop goes over it without a mark written,
    /// only the user's confirmation replaces it, and every other record goes on (Paykit spec
    /// 6).
    Mismatch,
}

impl Marked {
    /// The one check of a mark, `mark` as read bound at the key of the record held as
    /// `view`, for every reader: the receive step, the calls, the gates, the loops and the
    /// confirmation. A mismatch is never an error: it is [`Marked::Mismatch`].
    pub(crate) fn of(mark: Option<DivergentMark>, view: Option<&View>) -> Self {
        match mark {
            None => Self::Clear,
            Some(mark)
                if mark
                    .collection()
                    .is_ok_and(|collection| points_to(view, collection, mark.seen)) =>
            {
                Self::Mark(mark)
            }
            Some(_) => Self::Mismatch,
        }
    }

    /// The mark it holds, if it points to its record.
    fn mark(&self) -> Option<&DivergentMark> {
        match self {
            Self::Mark(mark) => Some(mark),
            Self::Clear | Self::Mismatch => None,
        }
    }

    /// What the record allows, the view `view` taken: a mismatch is paused.
    pub(crate) fn pause(&self, view: Option<&View>) -> Pause {
        match self {
            Self::Mismatch => Pause {
                paused: true,
                divergent: view.is_some_and(|view| view.divergent),
                writes_waiting: false,
                credit_waits: true,
            },
            _ => Pause::of(self.mark(), view),
        }
    }

    /// The mark `view` sets ([`after_view`]); none over a mismatch, left as it is.
    fn after_view(&self, view: &View) -> Option<DivergentMark> {
        match self {
            Self::Mismatch => None,
            _ => after_view(self.mark(), view),
        }
    }

    /// The mark the client's report `event` sets ([`after_event`]); none over a mismatch,
    /// which is paused already and stays as it is.
    fn after_event(&self, view: &View, event: &EventId) -> Option<DivergentMark> {
        match self {
            Self::Mismatch => None,
            _ => after_event(self.mark(), view, event),
        }
    }

    /// The mark after the user's confirmation ([`after_confirmation`]): a mismatch is
    /// replaced by a mark confirmed at the write held, or goes for a record not held.
    fn after_confirmation(&self, view: Option<&View>, credit: bool) -> Option<DivergentMark> {
        after_confirmation(self.mark(), view, credit)
    }

    /// Whether the mark goes at a receive's end ([`confirmed_mark_ends`]); a mismatch never.
    fn ends(&self, view: Option<&View>) -> bool {
        self.mark()
            .is_some_and(|mark| confirmed_mark_ends(mark, view))
    }
}

/// A record's view, as the pause rules read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct View {
    record: [u8; 32],
    collection: Collection,
    divergent: bool,
    version: SeenVersion,
}

impl View {
    /// The view of `held`; `None` for a record of a collection that isn't Paykit's.
    pub(crate) fn of(held: &SharedRecord) -> Option<Self> {
        Some(Self {
            record: held.key,
            collection: Collection::from_name(&held.collection)?,
            divergent: held.divergent,
            version: SeenVersion {
                generation: held.version.generation,
                version: held.version.version,
            },
        })
    }
}

/// The mark after `view`, when it changes: a divergent view pauses a record not paused,
/// unless the user confirmed that same write. A view that isn't divergent changes nothing:
/// the pause stays until the user confirms (Paykit spec 6), and a confirmed mark goes only
/// at a receive's end ([`confirmed_mark_ends`]).
pub(crate) fn after_view(mark: Option<&DivergentMark>, view: &View) -> Option<DivergentMark> {
    if !view.divergent {
        return None;
    }
    match mark {
        Some(mark) if mark.is_paused() || mark.confirmed == Some(view.version) => None,
        _ => Some(DivergentMark::paused(view)),
    }
}

/// The mark after the client's report `event` that the record was seen divergent
/// (`RecordDivergent`), whatever its view shows now: a view showed it so, and a later write
/// may have cleared it since, in the same read or before the report is taken. The report
/// carries the record's key, not the write it showed.
/// - A report the mark has taken already, or an earlier one of the same store, changes
///   nothing ([`DivergentMark::has_taken`]).
/// - With no mark, the record is paused, at the write held.
/// - A paused mark stays as it is, the report noted in it.
/// - A confirmed mark is of the write it was given over, and confirms nothing for a later
///   divergence (Paykit spec 6): the record is paused again, at the write held, with no
///   confirmation, a credit's included, whatever the view's flag shows now. Paused, it is
///   ended only by the user's confirmation of the write the app then shows. The one report
///   a confirmed mark keeps is one of the write it confirmed while the view still shows that
///   write divergent: a second upload beside the same write can't be told from the first,
///   and one confirmation covers both.
pub(crate) fn after_event(
    mark: Option<&DivergentMark>,
    view: &View,
    event: &EventId,
) -> Option<DivergentMark> {
    match mark {
        Some(mark) if mark.has_taken(event) => None,
        None => Some(DivergentMark::paused(view).taking(event)),
        Some(mark) if mark.is_paused() => Some(mark.clone().taking(event)),
        Some(mark) if view.divergent && mark.confirmed == Some(view.version) => {
            Some(mark.clone().taking(event))
        }
        Some(_) => Some(DivergentMark::paused(view).taking(event)),
    }
}

/// Whether a confirmed mark goes at a receive's end: the record isn't held, or its view
/// isn't divergent any more, and for a payee's record, whose upload may hold a credit, the
/// user has confirmed it shown so (Paykit spec 8.1 step 4). A paused mark never goes but by
/// the user's confirmation.
pub(crate) fn confirmed_mark_ends(mark: &DivergentMark, view: Option<&View>) -> bool {
    let credit_told = mark.collection != Collection::PayeeActivity.name() || !mark.credit_waits();
    !mark.is_paused() && view.is_none_or(|view| !view.divergent && credit_told)
}

/// The mark after the user's confirmation of the record held as `view`, `mark` before it,
/// given over the record shown for a credit when `credit`: confirmed at the version held, or
/// none for a record no longer held. It counts for a credit only when a credit's refusal
/// showed the record so at that version (`credit_shown`), the mark's own record of what was
/// shown, not the host's say. A credit's confirmation given before stays.
pub(crate) fn after_confirmation(
    mark: Option<&DivergentMark>,
    view: Option<&View>,
    credit: bool,
) -> Option<DivergentMark> {
    let view = view?;
    let shown = mark.and_then(|mark| mark.credit_shown);
    Some(DivergentMark {
        record: view.record,
        collection: view.collection.name().to_owned(),
        seen: mark.map_or(view.version, |mark| mark.seen.min(view.version)),
        confirmed: Some(view.version),
        credit_confirmed: if credit && shown == Some(view.version) {
            Some(view.version)
        } else {
            mark.and_then(|mark| mark.credit_confirmed)
        },
        credit_shown: shown,
        event: mark.and_then(|mark| mark.event.clone()),
    })
}

/// What a record's mark and view allow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pause {
    /// Seen divergent and not confirmed since.
    pub(crate) paused: bool,
    /// The latest view shows it divergent.
    pub(crate) divergent: bool,
    /// Messages kept over it wait to be written: they go first, once the user has confirmed
    /// it, before anything is paid, accepted or credited on it ([`Paykit::pause_of`]).
    pub(crate) writes_waiting: bool,
    /// Seen divergent, and not confirmed since shown that it may hold a credit.
    pub(crate) credit_waits: bool,
}

impl Pause {
    /// The pause of a record with `mark` and `view`, `mark` as `view` leaves it; the kept
    /// writes are the caller's to add ([`Paykit::pause_of`]).
    pub(crate) fn of(mark: Option<&DivergentMark>, view: Option<&View>) -> Self {
        let after = view.and_then(|view| after_view(mark, view));
        let mark = after.as_ref().or(mark);
        Self {
            paused: mark.is_some_and(DivergentMark::is_paused),
            divergent: view.is_some_and(|view| view.divergent),
            writes_waiting: false,
            credit_waits: mark.is_some_and(DivergentMark::credit_waits),
        }
    }

    /// No automatic decision that starts or repeats a payment, or accepts a request: while
    /// the view shows it divergent, the user's confirmation included, while it is paused,
    /// and while the messages kept over it wait to be written (Paykit spec 6). Recording a
    /// payment already dispatched is [`Pause::dispatched_write_waits`].
    pub(crate) fn blocks_automatic(self) -> bool {
        self.paused || self.divergent || self.writes_waiting
    }

    /// A payment, acceptance or `done` by hand waits for the user's confirmation, and then
    /// for the messages kept over the record to be written.
    pub(crate) fn blocks_by_hand(self) -> bool {
        self.paused || self.writes_waiting
    }

    /// A credit waits as [`Pause::blocks_by_hand`] does, and until the user has confirmed
    /// the record shown that it may hold a credit (Paykit spec 8.1 step 4,
    /// [`DivergentRecord::may_hold_credit`]).
    pub(crate) fn blocks_credit(self) -> bool {
        self.blocks_by_hand() || self.credit_waits
    }

    /// A write over it that adds no stop waits for the user's confirmation: while the view
    /// shows it divergent and the user hasn't confirmed it (Paykit spec 6).
    pub(crate) fn write_waits(self) -> bool {
        self.paused && self.divergent
    }

    /// The one rule of every write over it that adds no stop: it waits while
    /// [`Pause::write_waits`] holds, and while messages kept over it wait to be written, so
    /// those go first, in the order the inbox accepted them (Paykit spec 8.1, "Stored
    /// entries"). A message the inbox accepted reads its own `writes_waiting`, of the kept
    /// messages accepted before it ([`waits_behind`]).
    pub(crate) fn adds_no_stop_waits(self) -> bool {
        self.write_waits() || self.writes_waiting
    }

    /// The messages kept over it wait for the user's confirmation: while it is paused. Once
    /// confirmed they are written, before anything else opens on it.
    pub(crate) fn kept_writes_wait(self) -> bool {
        self.paused
    }

    /// A write that records a payment already dispatched, its acceptance owed, its outcome
    /// and its proof, at a payment, a resume or a repair, waits while the record is paused
    /// and while the messages kept over it wait to be written, and goes once the user has
    /// confirmed it and they are written, also over a view still divergent: the payee gets
    /// the proof of a payment made. [`Pause::blocks_automatic`] is for what starts or
    /// repeats a payment, not for recording one made.
    pub(crate) fn dispatched_write_waits(self) -> bool {
        self.paused || self.writes_waiting
    }
}

/// Whether `stored` adds a stop to the record it goes into: a cancellation or a rejection,
/// which go over a divergent view (Paykit spec 6).
pub(crate) fn adds_stop(stored: &StoredMessage) -> bool {
    matches!(
        stored.body(),
        Body::PaymentRequestCancellation(_) | Body::PaymentRequestRejection(_)
    )
}

/// The role a message kept over a divergent record holds there (`p/dw/<record>/<role>`):
/// one kept per record and role, as a record holds one message per role (Paykit spec 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriteRole {
    /// A request, in the payer's request record.
    Request,
    /// A payer's acceptance, in the payee's request record.
    Response,
    /// A payer's proof, in the payee's record of the request or of the settlement.
    Proof,
    /// A payee's receipt, in the payer's record.
    Receipt,
    /// A Private Payment List, in its `payment_lists` record.
    List,
}

impl WriteRole {
    /// The role's name in a key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Proof => "proof",
            Self::Receipt => "receipt",
            Self::List => "list",
        }
    }

    /// The side of the records a message of this role goes into: a request, a receipt and a
    /// list are the payer's, an acceptance and a proof the payee's.
    fn side(self) -> crate::hold::RecordSide {
        match self {
            Self::Request | Self::Receipt | Self::List => crate::hold::RecordSide::Payer,
            Self::Response | Self::Proof => crate::hold::RecordSide::Payee,
        }
    }

    /// The role a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "request" => Self::Request,
            "response" => Self::Response,
            "proof" => Self::Proof,
            "receipt" => Self::Receipt,
            "list" => Self::List,
            _ => return None,
        })
    }

    /// The role of a kept message; `None` for a stop, which is written over a divergent
    /// view and never kept, and for a kind no record of the SDK holds.
    pub(crate) fn of(stored: &StoredMessage) -> Option<Self> {
        Some(match stored.body() {
            Body::PaymentRequest(_) => Self::Request,
            Body::PaymentRequestAcceptance(_) => Self::Response,
            Body::PaymentProof(_) => Self::Proof,
            Body::PaymentReceipt(_) => Self::Receipt,
            Body::PrivatePaymentList(_) => Self::List,
            _ => return None,
        })
    }
}

/// A message kept to write over a divergent record after the user's confirmation
/// (`p/dw/<record key>/<role>`). The record and role repeat in it, so a reader binds the
/// value to its key; the record is the one its message names, derived again at every read;
/// and the version of the divergent write it was kept over is the proof that the record
/// held there later is that record, of its collection, at that write or a later one
/// ([`points_to`]).
///
/// A proof of a payment without a request goes into the payee's record of its settlement:
/// the settlement its rail named when it was kept persists with it, the proof that the
/// record is that proof's, which binds the value without the rail, and is checked against
/// the rail's again before the message is written ([`DivergentWrite::kept`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DivergentWrite {
    #[serde(with = "serde_b64")]
    record: [u8; 32],
    role: WriteRole,
    seen: SeenVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    settlement: Option<String>,
    message: KeptMessage,
}

impl DivergentWrite {
    /// The settlement kept, as bytes.
    fn settlement(&self) -> Result<Option<Vec<u8>>> {
        self.settlement
            .as_deref()
            .map(pubky_switch_core::enc::unb64)
            .transpose()
            .map_err(|_| PaykitError::corrupt())
    }

    /// The kept write bound to the key it is read at, `dw/<record>/<role>`
    /// (`state::Stored`): a message that verifies for `own`, accepted by its inbox, of that
    /// role, whose record, as `record_key` derives it from the message and the settlement
    /// kept, is the one its key names, with a settlement kept exactly for a proof of a
    /// payment without a request. The one check for a restore and for every read at
    /// runtime; a value that fails it is a corrupt store.
    pub(crate) fn bind_to(
        &self,
        record: &[u8; 32],
        role: WriteRole,
        msg_id: &str,
        own: &[u8; 32],
        record_key: &dyn Fn(&RecordTarget) -> Result<[u8; 32]>,
    ) -> Result<()> {
        let stored = self.message.stored(own)?;
        let settlement = self.settlement()?;
        let settled =
            matches!(stored.body(), Body::PaymentProof(_)) && stored.message().refs.is_empty();
        if self.record != *record
            || self.role != role
            || stored.message().msg_id != msg_id
            || WriteRole::of(&stored) != Some(role)
            || stored.accepted_at().is_none()
            || settled != settlement.is_some()
        {
            return Err(PaykitError::corrupt());
        }
        let target =
            write_target(&stored, own, &|_| settlement.clone()).ok_or_else(PaykitError::corrupt)?;
        if target.collection().is_none() || record_key(&target.record_target())? != *record {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    /// The kept write, read bound, as it is written: its message, where it goes by the
    /// app's rail, `settlement`, and the write it was kept over. `None` when the rail names
    /// another settlement, or none, than the one kept: the value points to another record
    /// than the one the rail's would: as one kept over a later write than the record held,
    /// it isn't written, and is given up at its 30 days ([`Paykit::place_divergent_writes`]).
    pub(crate) fn kept(
        &self,
        own: &[u8; 32],
        settlement: Settlement<'_>,
    ) -> Result<Option<KeptWrite>> {
        let message = self.message.stored(own)?;
        if let Body::PaymentProof(proof) = message.body() {
            if message.message().refs.is_empty() && settlement(&proof.proof) != self.settlement()? {
                return Ok(None);
            }
        }
        let target = write_target(&message, own, settlement).ok_or_else(PaykitError::corrupt)?;
        Ok(Some(KeptWrite {
            message,
            target,
            seen: self.seen,
        }))
    }
}

/// A kept write read bound ([`kept_write_at`]): its message, where it goes, and the write
/// it was kept over.
pub(crate) struct KeptWrite {
    pub(crate) message: StoredMessage,
    pub(crate) target: WriteTarget,
    pub(crate) seen: SeenVersion,
}

/// Where a kept message goes, by its kind, as at its first read.
pub(crate) enum WriteTarget {
    /// An activity record, in `role`; `creates` when the message may open the record.
    Activity {
        target: ActivityTarget,
        role: Role,
        creates: bool,
    },
    /// A `payment_lists` record.
    List(RecordTarget),
}

impl WriteTarget {
    fn record_target(&self) -> RecordTarget {
        match self {
            Self::Activity { target, .. } => target.record_target(),
            Self::List(target) => target.clone(),
        }
    }

    /// The collection of the record it goes into.
    fn collection(&self) -> Option<Collection> {
        Collection::from_name(&self.record_target().collection.name)
    }
}

/// The record `stored`, a kept message, goes into, as the receive step places it: a request
/// into the payer's request record, an acceptance or a proof into the payee's record of the
/// request it names, a proof of a payment without a request into the payee's record of its
/// settlement, a receipt into the payer's record, a list into its `payment_lists` record.
pub(crate) fn write_target(
    stored: &StoredMessage,
    own: &[u8; 32],
    settlement: Settlement<'_>,
) -> Option<WriteTarget> {
    let sender = stored.sender();
    let first = stored.message().refs.first().copied();
    let activity = |target, role, creates| {
        Some(WriteTarget::Activity {
            target,
            role,
            creates,
        })
    };
    match stored.body() {
        Body::PaymentRequest(_) => activity(
            ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash())),
            Role::Request,
            true,
        ),
        Body::PaymentProof(proof) if first.is_none() => {
            let settlement = settlement(&proof.proof)?;
            activity(
                ActivityTarget::payee(
                    natural_key::payee_payment_record(
                        &proof.payment_endpoint_identifier,
                        &settlement,
                    ),
                    None,
                ),
                Role::Proof,
                true,
            )
        }
        Body::PaymentRequestAcceptance(_) | Body::PaymentProof(_) => {
            let role = Role::of(stored.body())?;
            activity(
                ActivityTarget::payee(
                    natural_key::request_record(own, &first?),
                    stored.body().label(),
                ),
                role,
                false,
            )
        }
        Body::PaymentReceipt(receipt) => activity(
            crate::receive::receipt_target(stored, own, &receipt.attempt_id),
            Role::Receipt,
            false,
        ),
        Body::PrivatePaymentList(_) => Some(WriteTarget::List(list_target(
            &sender,
            &stored.message().from.app_id,
        ))),
        _ => None,
    }
}

/// How many later messages of one role a kept record reflects besides the one it stores
/// there, the newest: as many as a record's own bounded list of other `msg_id`s (Paykit wire
/// 5.3).
const KEPT_OTHERS: usize = 16;

/// How many messages of one role are kept over one record, all senders together: twice
/// what one sender keeps ([`KEPT_OTHERS`] and its first), so the record's counterparty
/// keeps its whole share beside the messages of others.
const KEPT_CAP: usize = 2 * (KEPT_OTHERS + 1);

/// The kept messages of one record and role, oldest first, that stay once `new` is kept
/// beside `kept` (Paykit spec 6, "One message per role", as the receive step places them):
/// for a list, latest state, only the newest version, and `new` taken with no effect when it
/// is no newer; for any other role, the first, which the record stores in its role, and the
/// newest [`KEPT_OTHERS`] after it, which it only reflects.
///
/// Each sender keeps at most its first and its [`KEPT_OTHERS`] newest: past that, its own
/// earliest go, so no sender pushes out a message of another. With one sender this is the
/// first of the role and the newest after it. All senders together keep at most
/// [`KEPT_CAP`]; past it, the role's first aside:
/// - over a record that `reads`, whose counterparty the receive step has shown is `new`'s
///   sender, the earliest messages of other senders go, never the counterparty's;
/// - over a record that doesn't read, whose counterparty can't be told, the earliest
///   message of the sender keeping the most goes, of the lower identity between senders
///   keeping as many, as past the bound in all on replies before their record.
///
/// Returns whether `new` is kept, and the `msg_id`s of the kept messages that go.
fn kept_after(kept: &[StoredMessage], new: &StoredMessage, reads: bool) -> (bool, Vec<String>) {
    let msg_id = |stored: &StoredMessage| stored.message().msg_id.clone();
    if kept.iter().any(|held| msg_id(held) == msg_id(new)) {
        return (false, Vec::new());
    }
    if matches!(WriteRole::of(new), Some(WriteRole::List)) {
        let newest = kept.iter().map(crate::receive::list_version).max();
        if newest.is_some_and(|newest| newest >= crate::receive::list_version(new)) {
            return (false, Vec::new());
        }
        return (true, kept.iter().map(msg_id).collect());
    }
    let mut all: Vec<&StoredMessage> = kept.iter().chain(std::iter::once(new)).collect();
    all.sort_by_key(|stored| (stored.accepted_at(), msg_id(stored)));
    let first = msg_id(all[0]);
    let new_id = msg_id(new);
    let sender = new.sender();
    // The new message's sender's own, past its share, earliest first, the role's first aside.
    let own = all
        .iter()
        .filter(|stored| stored.sender() == sender)
        .count();
    let mut gone: Vec<String> = all
        .iter()
        .filter(|stored| stored.sender() == sender && msg_id(stored) != first)
        .take(own.saturating_sub(KEPT_OTHERS + 1))
        .map(|stored| msg_id(stored))
        .collect();
    // All senders together, past the cap.
    loop {
        let left: Vec<&StoredMessage> = all
            .iter()
            .copied()
            .filter(|stored| !gone.contains(&msg_id(stored)))
            .collect();
        // The new message stays over the bound when it is the one that would go: its step
        // writes its pair (`step_keep`).
        let kept_count = left.len() + usize::from(gone.contains(&new_id));
        if kept_count <= KEPT_CAP {
            break;
        }
        let evictable =
            |stored: &StoredMessage| msg_id(stored) != first && msg_id(stored) != new_id;
        let pick = if reads {
            left.iter()
                .filter(|stored| evictable(stored))
                .find(|stored| stored.sender() != sender)
        } else {
            let mut counts: std::collections::BTreeMap<[u8; 32], usize> =
                std::collections::BTreeMap::new();
            for stored in &left {
                *counts.entry(stored.sender()).or_default() += 1;
            }
            let mut senders: Vec<([u8; 32], usize)> = counts.into_iter().collect();
            senders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            senders.iter().find_map(|(most, _)| {
                left.iter()
                    .filter(|stored| evictable(stored))
                    .find(|stored| stored.sender() == *most)
            })
        };
        match pick {
            Some(stored) => gone.push(msg_id(stored)),
            None => break,
        }
    }
    let kept_new = !gone.contains(&new_id);
    (kept_new, gone)
}

/// The order the inbox accepted `stored` in, as kept messages are written: by acceptance,
/// then `msg_id`. A message with no acceptance time comes after every one with one.
fn accepted_order(stored: &StoredMessage) -> (u64, &str) {
    (
        stored.accepted_at().unwrap_or(u64::MAX),
        stored.message().msg_id.as_str(),
    )
}

/// Whether a message of `kept`, the messages kept over the record held as `view`, read bound,
/// waits to be written: one that still points to the record ([`DivergentWrite::kept`],
/// [`points_to`]) and, when `behind` is given, that its inbox accepted before `behind`, a
/// message written over the same record. One that doesn't point to the record any more holds
/// nothing back (Paykit spec 8.1, "Stored entries").
fn waits_behind(
    kept: impl IntoIterator<Item = DivergentWrite>,
    own: &[u8; 32],
    settlement: Settlement<'_>,
    view: Option<&View>,
    behind: Option<&StoredMessage>,
) -> Result<bool> {
    for kept in kept {
        let Some(kept) = kept.kept(own, settlement)? else {
            continue;
        };
        let collection = kept.target.collection().ok_or_else(PaykitError::corrupt)?;
        if !points_to(view, collection, kept.seen) {
            continue;
        }
        match behind {
            Some(behind) if accepted_order(&kept.message) >= accepted_order(behind) => {}
            _ => return Ok(true),
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------------
// The receive step

/// The mark of the record held as `view` in the receive step, read bound and checked
/// against it ([`Marked::of`]).
fn step_mark(cx: &ReceiveContext<'_>, view: &View) -> std::result::Result<Marked, Fail> {
    let mark = state::cx_get::<DivergentMark>(cx, &key::divergent(&view.record))?;
    Ok(Marked::of(mark, Some(view)))
}

/// In the receive step: take the view of `held`, keeping the mark it sets with the cursor
/// move, and tell what it allows.
fn step_pause(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
) -> std::result::Result<Pause, Fail> {
    let Some(view) = View::of(held) else {
        return Ok(Pause::default());
    };
    let marked = step_mark(cx, &view)?;
    if let Some(after) = marked.after_view(&view) {
        let write =
            state::local_put(&key::divergent(&held.key), &after).map_err(|_| Fail::corrupt())?;
        crate::receive::keep_local(cx, write);
    }
    Ok(marked.pause(Some(&view)))
}

/// In the receive step: take the view of `held` ([`step_pause`]) and tell whether a write of
/// `stored` over it waits, by the one rule of writes that add no stop
/// ([`Pause::adds_no_stop_waits`]): a stop never waits; any other message waits while the
/// view is divergent and unconfirmed, and while messages kept over the record before it
/// wait to be written, and is then kept beside them ([`step_keep`]).
pub(crate) fn step_write_waits(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
    stored: &StoredMessage,
    settlement: Settlement<'_>,
) -> std::result::Result<bool, Fail> {
    let pause = step_pause(cx, held)?;
    if adds_stop(stored) {
        return Ok(false);
    }
    if pause.write_waits() {
        return Ok(true);
    }
    let Some(view) = View::of(held) else {
        return Ok(false);
    };
    let kept = state::cx_scan::<DivergentWrite>(cx, &key::divergent_writes_at(&held.key))?;
    waits_behind(
        kept.into_iter().map(|(_, kept)| kept),
        &cx.identity(),
        settlement,
        Some(&view),
        Some(stored),
    )
    .map_err(|_| Fail::corrupt())
}

/// In the receive step: keep `stored`, a message whose write over `held`, a divergent
/// record, waits for the user's confirmation, with the cursor move (Paykit spec 6, 8.1
/// "Stored entries"). One kept already for the record and role stays, but for a newer list.
pub(crate) fn step_keep(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
    stored: &StoredMessage,
    settlement: Settlement<'_>,
    reads: bool,
) -> std::result::Result<(), Fail> {
    let (Some(role), Some(view)) = (WriteRole::of(stored), View::of(held)) else {
        return Ok(());
    };
    let record = held.key;
    // The messages kept already for the record and role are read bound, and checked against
    // the record, before anything is decided on them: one that doesn't read holds the entry,
    // and is never a reason to drop this one. One that doesn't point to the record, kept
    // over a later write or of another settlement than the rail's, decides nothing here; this
    // one is kept beside it.
    let mut held_messages = Vec::new();
    for (_, kept) in state::cx_scan::<DivergentWrite>(cx, &key::divergent_writes_of(&record, role))?
    {
        let Some(kept) = kept
            .kept(&cx.identity(), settlement)
            .map_err(|_| Fail::corrupt())?
        else {
            continue;
        };
        let collection = kept.target.collection().ok_or_else(Fail::corrupt)?;
        if points_to(Some(&view), collection, kept.seen) {
            held_messages.push(kept.message);
        }
    }
    let (keep, gone) = kept_after(&held_messages, stored, reads);
    // This step's own message, when the bound would drop it (it was accepted before the 16
    // latest kept, or at their time with a lower `msg_id`), can't be dropped as one never
    // processed: the step's outcome writes its pair. It is kept instead, one over the bound,
    // until a later message of the role drops it with its pair forgotten.
    let keep = keep || gone.contains(&stored.message().msg_id);
    for msg_id in gone {
        let Some(dropped) = held_messages
            .iter()
            .find(|held| held.message().msg_id == msg_id)
        else {
            continue;
        };
        // A message kept already and dropped for the bound is dropped as one never processed:
        // its pair is forgotten in this step's commit, so a later re-send of it is taken. One
        // the client won't forget yet stays kept, the bound over by one for this step.
        match cx.forget_processed(dropped.sender(), &msg_id) {
            Ok(_) => {}
            Err(AppError::Refused(ClientRefusal::NotNow(_))) => continue,
            // Past the step's bound on local writes: the context marks the step, and the
            // client stores the entry as too large, applying none of it.
            Err(AppError::Refused(ClientRefusal::Usage(_))) => {}
            Err(error) => return Err(error.into()),
        }
        crate::receive::keep_local(
            cx,
            pubky_switch_client::app::LocalWrite::Delete {
                key: key::divergent_write(&record, role, &msg_id).into_bytes(),
            },
        );
    }
    if !keep {
        return Ok(());
    }
    let kept_key = key::divergent_write(&record, role, &stored.message().msg_id);
    let kept_settlement = match stored.body() {
        Body::PaymentProof(proof) if stored.message().refs.is_empty() => Some(
            pubky_switch_core::enc::b64(&settlement(&proof.proof).ok_or_else(Fail::corrupt)?),
        ),
        _ => None,
    };
    let kept = DivergentWrite {
        record,
        role,
        seen: view.version,
        settlement: kept_settlement,
        message: KeptMessage::of(stored),
    };
    let write = state::local_put(&kept_key, &kept).map_err(|_| Fail::corrupt())?;
    crate::receive::keep_local(cx, write);
    Ok(())
}

impl Paykit {
    /// The mark at `record`, read bound ([`state::Stored`]) and checked against the record
    /// held there, `view` ([`Marked::of`]), before anything is decided on it.
    async fn marked(&self, record: &[u8; 32], view: Option<&View>) -> Result<Marked> {
        let mark = self
            .local_get::<DivergentMark>(&key::divergent(record))
            .await?;
        Ok(Marked::of(mark, view))
    }

    /// The record held at `record`, as a view, and its mark checked against it.
    async fn mark_and_view(&self, record: &[u8; 32]) -> Result<(Marked, Option<View>)> {
        let held = self.app.shared_record(record).await?;
        let view = held.as_ref().and_then(View::of);
        let marked = self.marked(record, view.as_ref()).await?;
        Ok((marked, view))
    }

    /// Every mark, each read bound to its key.
    async fn divergent_marks(&self) -> Result<Vec<DivergentMark>> {
        Ok(self
            .local_scan::<DivergentMark>(key::DIVERGENT)
            .await?
            .into_iter()
            .map(|(_, mark)| mark)
            .collect())
    }

    /// Put or keep the mark `after` of `record`.
    async fn put_mark(&mut self, record: &[u8; 32], after: &DivergentMark) -> Result<()> {
        self.local_put(&key::divergent(record), after).await
    }

    /// Whether messages kept over the record at `record`, held as `view`, wait to be
    /// written: each read bound and checked against the record, one that doesn't point to
    /// it holding nothing back ([`waits_behind`]).
    async fn writes_waiting(&self, record: &[u8; 32], view: Option<&View>) -> Result<bool> {
        self.writes_waiting_before(record, view, None).await
    }

    /// [`Paykit::writes_waiting`], of the messages the inbox accepted before `behind` when it
    /// is given.
    async fn writes_waiting_before(
        &self,
        record: &[u8; 32],
        view: Option<&View>,
        behind: Option<&StoredMessage>,
    ) -> Result<bool> {
        let kept = self
            .local_scan::<DivergentWrite>(&key::divergent_writes_at(record))
            .await?;
        waits_behind(
            kept.into_iter().map(|(_, kept)| kept),
            &self.identity(),
            &|proof| self.rail.settlement_id(proof),
            view,
            behind,
        )
    }

    /// What the record at `record`, held as `view`, allows with `marked`, its kept writes
    /// included: every gate reads a record through here.
    async fn pause_with(
        &self,
        record: &[u8; 32],
        marked: &Marked,
        view: Option<&View>,
    ) -> Result<Pause> {
        Ok(Pause {
            writes_waiting: self.writes_waiting(record, view).await?,
            ..marked.pause(view)
        })
    }

    /// Take the view of `held`, keeping the mark it sets, and tell what it allows.
    pub(crate) async fn pause_of(&mut self, held: &SharedRecord) -> Result<Pause> {
        let Some(view) = View::of(held) else {
            return Ok(Pause::default());
        };
        let marked = self.marked(&held.key, Some(&view)).await?;
        if let Some(after) = marked.after_view(&view) {
            self.put_mark(&held.key, &after).await?;
        }
        self.pause_with(&held.key, &marked, Some(&view)).await
    }

    /// [`Paykit::pause_of`] for the record at `record`: a mark alone for one not held.
    pub(crate) async fn pause_at(&mut self, record: &[u8; 32]) -> Result<Pause> {
        match self.app.shared_record(record).await? {
            Some(held) => self.pause_of(&held).await,
            None => {
                let marked = self.marked(record, None).await?;
                self.pause_with(record, &marked, None).await
            }
        }
    }

    /// What the record at `record` allows, as its mark and view stand, writing nothing: for
    /// the views.
    pub(crate) async fn pause_seen(&self, record: &[u8; 32]) -> Result<Pause> {
        let (marked, view) = self.mark_and_view(record).await?;
        self.pause_with(record, &marked, view.as_ref()).await
    }

    /// The host's view of the record at `record`, `pause` as it allows, shown for a credit
    /// when `credit` ([`DivergentRecord::may_hold_credit`]).
    async fn shown(&self, record: [u8; 32], pause: Pause, credit: bool) -> Result<DivergentRecord> {
        let (marked, view) = self.mark_and_view(&record).await?;
        let collection = match (view, marked.mark()) {
            (Some(view), _) => view.collection,
            (None, Some(mark)) => mark.collection()?,
            (None, None) => return Err(PaykitError::corrupt()),
        };
        Ok(DivergentRecord {
            record,
            collection,
            paused: pause.paused,
            divergent: pause.divergent,
            version: view.map(|view| view.version.record_version()),
            may_hold_credit: credit,
            writes_waiting: pause.writes_waiting && !pause.paused,
        })
    }

    /// Refused for an action by hand while any of `records` is paused, or its kept
    /// messages wait to be written, naming those, for the user to confirm
    /// ([`Refusal::Divergent`]). Each record's view is taken first.
    pub(crate) async fn refuse_while_paused(&mut self, records: &[[u8; 32]]) -> Result<()> {
        self.refuse_where(records, Pause::blocks_by_hand, false)
            .await
    }

    /// Refused while `gate` holds any of `records`, naming those, each shown for a credit
    /// when `credit`. Each record's view is taken first.
    async fn refuse_where(
        &mut self,
        records: &[[u8; 32]],
        gate: fn(Pause) -> bool,
        credit: bool,
    ) -> Result<()> {
        let mut held = Vec::new();
        for record in records {
            let pause = self.pause_at(record).await?;
            if gate(pause) {
                if credit {
                    self.note_credit_shown(record).await?;
                }
                held.push(self.shown(*record, pause, credit).await?);
            }
        }
        if held.is_empty() {
            return Ok(());
        }
        Err(Refusal::Divergent { records: held }.into())
    }

    /// Keep in the mark at `record` that a credit's refusal shows the record, at the write
    /// held, as one that may hold a credit: the one proof [`Paykit::confirm_divergent`]
    /// takes that a confirmation was shown so (Paykit spec 8.1 step 4). A mark that doesn't
    /// point to its record keeps nothing: its confirmation replaces it, and the next
    /// credit's refusal shows the record again.
    async fn note_credit_shown(&mut self, record: &[u8; 32]) -> Result<()> {
        let (marked, view) = self.mark_and_view(record).await?;
        if let (Marked::Mark(mut mark), Some(view)) = (marked, view) {
            if mark.credit_shown != Some(view.version) {
                mark.credit_shown = Some(view.version);
                self.put_mark(record, &mark).await?;
            }
        }
        Ok(())
    }

    /// The refusal of an own write over the divergent record `record`, naming it.
    pub(crate) async fn divergent_refusal(&mut self, record: [u8; 32]) -> Result<PaykitError> {
        let pause = self.pause_at(&record).await?;
        let shown = self.shown(record, pause, false).await?;
        Ok(Refusal::Divergent {
            records: vec![shown],
        }
        .into())
    }

    /// Refused while an own write of another kind than a stop over the record at `record`
    /// waits, by the one rule of writes that add no stop ([`Pause::adds_no_stop_waits`]).
    pub(crate) async fn refuse_while_write_waits(&mut self, record: &[u8; 32]) -> Result<()> {
        self.refuse_where(&[*record], Pause::adds_no_stop_waits, false)
            .await
    }

    /// Whether a write that records a payment already dispatched, its acceptance owed, its
    /// outcome and its proof, over the record at `record` waits
    /// ([`Pause::dispatched_write_waits`]): the one gate of every path that writes one.
    pub(crate) async fn dispatched_write_waits(&mut self, record: &[u8; 32]) -> Result<bool> {
        Ok(self.pause_at(record).await?.dispatched_write_waits())
    }

    /// Whether a later write of `stored` over the record at `record` waits, by the one rule
    /// of writes that add no stop ([`Pause::adds_no_stop_waits`]): a stop never waits; any
    /// other message waits while the record's view is divergent and not confirmed, and while
    /// messages kept over the record that its inbox accepted before `stored` wait to be
    /// written, so those go first. The stored entries, the kept replies, the held lists and
    /// the kept messages themselves are written through here.
    pub(crate) async fn divergent_write_waits(
        &mut self,
        record: &[u8; 32],
        stored: &StoredMessage,
    ) -> Result<bool> {
        if adds_stop(stored) {
            return Ok(false);
        }
        if self.pause_at(record).await?.write_waits() {
            return Ok(true);
        }
        let held = self.app.shared_record(record).await?;
        let view = held.as_ref().and_then(View::of);
        self.writes_waiting_before(record, view.as_ref(), Some(stored))
            .await
    }

    /// A payee app's credit of a proof in `record`, from `counterparty`, waits for the user
    /// while `record` or any other record of the same counterparty it holds, or one whose
    /// counterparty it can't tell, is paused, its
    /// kept messages wait to be written, or it was seen divergent and the user hasn't
    /// confirmed it shown that it may hold a credit of a settlement its current write lacks
    /// (Paykit spec 8.1 step 4): the upload may hold a receipt of the same settlement, so
    /// the app can't tell it isn't credited already. Refused naming them, each shown so
    /// ([`DivergentRecord::may_hold_credit`]): a confirmation of what the refusal shows
    /// counts for the credit, a plain one doesn't.
    pub(crate) async fn refuse_credit_while_paused(
        &mut self,
        record: &[u8; 32],
        counterparty: &[u8; 32],
    ) -> Result<()> {
        let mut records = vec![*record];
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            if held.key == *record {
                continue;
            }
            // A record that doesn't open or bind, a delete among them, may be of the same
            // counterparty, and its upload may hold a receipt: it counts as one of the
            // counterparty's here, failing closed, and holds the credit only while it is
            // paused, divergent or not confirmed for a credit. A failure to read one is the
            // call's error.
            match self.payee_record(&held.key).await {
                Ok((_, other)) if other.counterparty != *counterparty => {}
                Ok(_) | Err(PaykitError::Refused(Refusal::NotFound) | PaykitError::Lib(_)) => {
                    records.push(held.key)
                }
                Err(error) => return Err(error),
            }
        }
        self.refuse_where(&records, Pause::blocks_credit, true)
            .await
    }

    /// The mark the client's report `event` that `record` was seen divergent sets, a duty
    /// event (`AppEvent::RecordDivergent`): a view showed it so, whatever its view shows now
    /// ([`after_event`]). It is kept in its own commit, before the one that counts the event
    /// taken, so the event is acknowledged only once the record is paused, or its mark has
    /// taken it. Taking the same report again changes nothing, so a report delivered twice
    /// never pauses again a record its user confirmed after it; a report of a later
    /// divergence pauses a record its user had confirmed. A mark that doesn't point to its
    /// record is paused already and left as it is ([`Marked::Mismatch`]); a record no
    /// longer held takes no mark.
    pub(crate) async fn take_divergent_event(
        &mut self,
        record: &[u8; 32],
        event: &EventId,
    ) -> Result<()> {
        let (marked, Some(view)) = self.mark_and_view(record).await? else {
            return Ok(());
        };
        if let Some(after) = marked.after_event(&view, event) {
            self.put_mark(record, &after).await?;
        }
        Ok(())
    }

    /// Take the view of every record of Paykit's collections the app holds, once at a load:
    /// a flag a crash left before a receive's events were taken still pauses its record.
    pub(crate) async fn sweep_divergent(&mut self) -> Result<()> {
        for collection in crate::hold::RecordSide::All.collections() {
            for held in self.app.records_of(&collection.name).await? {
                if held.divergent {
                    self.pause_of(&held).await?;
                }
            }
        }
        Ok(())
    }

    /// At a receive's end: the confirmed marks whose record's view isn't divergent any more,
    /// or that the app holds no more, go ([`confirmed_mark_ends`]). A mark that doesn't
    /// point to its record stays.
    pub(crate) async fn end_confirmed_marks(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        for mark in self.divergent_marks().await? {
            let record = mark.record;
            let held = self.app.shared_record(&record).await?;
            let view = held.as_ref().and_then(View::of);
            // Nor while messages kept over it wait to be written: it stays shown until then.
            if Marked::of(Some(mark), view.as_ref()).ends(view.as_ref())
                && !self.writes_waiting(&record, view.as_ref()).await?
            {
                tx.delete(&key::divergent(&record));
            }
        }
        self.local_commit(tx).await
    }

    /// The records paused now, by key, each mark checked against its record: a mark that
    /// doesn't point to it is paused.
    pub(crate) async fn paused_records(&self) -> Result<std::collections::BTreeSet<[u8; 32]>> {
        let mut paused = std::collections::BTreeSet::new();
        for mark in self.divergent_marks().await? {
            let record = mark.record;
            let held = self.app.shared_record(&record).await?;
            let view = held.as_ref().and_then(View::of);
            if Marked::of(Some(mark), view.as_ref())
                .pause(view.as_ref())
                .paused
            {
                paused.insert(record);
            }
        }
        Ok(paused)
    }

    /// The records paused now that weren't in `before`, for a receive's summary.
    pub(crate) async fn newly_paused(
        &self,
        before: &std::collections::BTreeSet<[u8; 32]>,
    ) -> Result<Vec<DivergentRecord>> {
        let mut out = Vec::new();
        for record in self.paused_records().await? {
            if !before.contains(&record) {
                let pause = self.pause_seen(&record).await?;
                out.push(self.shown(record, pause, false).await?);
            }
        }
        Ok(out)
    }

    /// Write every message kept over a divergent record whose record the user has
    /// confirmed, each as a later write of the app's own with its kept copy dropped in the
    /// batch, through the stored entries' later write ([`Paykit::place_note`]) and its one
    /// rule ([`crate::stored::later_write`]); a list goes to its held list (`lh/`), which
    /// the receive places next. They go in the order the inbox accepted them, and one waits
    /// while one accepted before it over the same record waits
    /// ([`Paykit::divergent_write_waits`]). One whose record is still paused waits, and is
    /// given up [`crate::KEPT_MESSAGE_MS`] after its acceptance, on the highest service time
    /// verified, with the host's notice ([`crate::NoticeKind::MessageGivenUp`]) (Paykit spec
    /// 8.1, "Stored entries"). One that doesn't point to its record any more is never
    /// written, and is given up the same way. An error is returned, and the message stays
    /// kept.
    pub(crate) async fn place_divergent_writes(&mut self) -> Result<()> {
        let own = self.identity();
        // Each record's kept messages in the order they were accepted: the first is stored in
        // its role, the later ones reflected, as the receive step would have placed them.
        let mut all = Vec::new();
        for (rest, value) in self
            .local_scan::<DivergentWrite>(key::DIVERGENT_WRITES)
            .await?
        {
            let kept_key = format!("{}{rest}", key::DIVERGENT_WRITES);
            let stored = value.message.stored(&own)?;
            // `None` for one of another settlement than the rail's names now: it doesn't
            // point to its record.
            let kept = value.kept(&own, &|proof| self.rail.settlement_id(proof))?;
            all.push((value.record, value.role, kept_key, stored, kept));
        }
        all.sort_by(|a, b| (a.0, accepted_order(&a.3)).cmp(&(b.0, accepted_order(&b.3))));
        for (record, role, kept_key, stored, kept) in all {
            let accepted_at = stored.accepted_at().ok_or_else(PaykitError::corrupt)?;
            // What it points to is checked before anything is written: the record held at its
            // key is of its target's collection, at the write it was kept over or a later one.
            // The record's mark is judged by the one check below (`Paykit::pause_at`): one
            // that doesn't point to its record is paused.
            let held = self.app.shared_record(&record).await?;
            let view = held.as_ref().and_then(View::of);
            let kept = match kept {
                Some(kept) => {
                    let collection = kept.target.collection().ok_or_else(PaykitError::corrupt)?;
                    points_to(view.as_ref(), collection, kept.seen).then_some(kept)
                }
                None => None,
            };
            // A record that reads names its counterparty: a message kept over it from anyone
            // else, kept while the record didn't read, is never written and holds nothing
            // back. It goes now, as the receive step drops it over a record that reads
            // (`receive::place`), paused or not.
            if let Some(KeptWrite {
                target: WriteTarget::Activity { target, .. },
                ..
            }) = &kept
            {
                if self
                    .held_activity(target)
                    .await?
                    .is_some_and(|read| read.counterparty != stored.sender())
                {
                    self.local_delete(&kept_key).await?;
                    continue;
                }
            }
            // A record absent or deleted while the copy of its side may lack records decides
            // nothing yet: the message waits, never given up meanwhile (Paykit spec 8.1,
            // "Records not listed yet"), as a stored entry's does.
            let content = held.map(|held| held.content);
            let placed = if matches!(content, None | Some(RecordContent::Deleted))
                && self.side_unlisted(role.side()).await?
            {
                crate::stored::Placed::Waits
            } else if let Some(kept) = kept {
                if self.pause_at(&record).await?.kept_writes_wait() {
                    crate::stored::Placed::Not
                } else {
                    let mut tx = Tx::default();
                    tx.delete(&kept_key);
                    match kept.target {
                        WriteTarget::List(_) => {
                            self.hold_kept_list(record, &stored, tx).await?;
                            crate::stored::Placed::Done
                        }
                        WriteTarget::Activity {
                            target,
                            role,
                            creates,
                        } => {
                            self.place_note(&target, role, creates, stored.clone(), tx)
                                .await?
                        }
                    }
                }
            } else {
                // One that doesn't point to its record any more, kept over a later write than
                // the one held or of another settlement than the rail's names now, isn't
                // written: it is given up at its 30 days as any kept message, and nothing
                // waits for it meanwhile ([`waits_behind`]).
                crate::stored::Placed::Not
            };
            if crate::stored::later_write(placed, accepted_at, self.service_time())
                == crate::stored::LaterWrite::GiveUp
            {
                self.give_up_divergent(&kept_key, &stored).await?;
            }
        }
        Ok(())
    }

    /// Move `stored`, a message kept at `kept_key` before its record `held` read (`pr/`),
    /// over that record now that it reads paused as divergent (`dw/`): from then on it is one
    /// of the messages kept over the record (Paykit spec 6), counted under their bound, held
    /// to their order and gates, and written once the user has confirmed the record. Its
    /// processed pair stays as it is, so it is applied once; the copy kept before goes in
    /// the commit that keeps it here. A message kept over the record already, at the same
    /// `msg_id`, stays as it is. A kept message the bound drops is dropped as one never
    /// processed, its pair forgotten in the commit that drops it, as in the receive step
    /// ([`step_keep`]); one the client won't forget yet stays, over the bound, and so does
    /// this one when the bound would drop it, until a later message of its role drops it so.
    pub(crate) async fn keep_over_paused(
        &mut self,
        held: &SharedRecord,
        stored: &StoredMessage,
        kept_key: &str,
    ) -> Result<()> {
        let (Some(role), Some(view)) = (WriteRole::of(stored), View::of(held)) else {
            return Ok(());
        };
        let record = held.key;
        let own = self.identity();
        let mut held_messages = Vec::new();
        for (_, kept) in self
            .local_scan::<DivergentWrite>(&key::divergent_writes_of(&record, role))
            .await?
        {
            let Some(kept) = kept.kept(&own, &|proof| self.rail.settlement_id(proof))? else {
                continue;
            };
            let collection = kept.target.collection().ok_or_else(PaykitError::corrupt)?;
            if points_to(Some(&view), collection, kept.seen) {
                held_messages.push(kept.message);
            }
        }
        let msg_id = stored.message().msg_id.clone();
        let kept_already = held_messages
            .iter()
            .any(|held| held.message().msg_id == msg_id);
        let (keep, gone) = kept_after(&held_messages, stored, true);
        for dropped_id in gone.iter().filter(|gone| **gone != msg_id) {
            let Some(dropped) = held_messages
                .iter()
                .find(|held| held.message().msg_id == *dropped_id)
            else {
                continue;
            };
            let delete = pubky_switch_client::app::LocalWrite::Delete {
                key: key::divergent_write(&record, role, dropped_id).into_bytes(),
            };
            match self
                .app
                .forget_processed(dropped.sender(), dropped_id, &[delete])
                .await
            {
                Ok(_) | Err(AppError::Refused(ClientRefusal::NotNow(_))) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut tx = Tx::default();
        tx.delete(kept_key);
        if !kept_already && (keep || gone.contains(&msg_id)) {
            let settlement = match stored.body() {
                Body::PaymentProof(proof) if stored.message().refs.is_empty() => {
                    let settled = self
                        .rail
                        .settlement_id(&proof.proof)
                        .ok_or_else(PaykitError::corrupt)?;
                    Some(pubky_switch_core::enc::b64(&settled))
                }
                _ => None,
            };
            let kept = DivergentWrite {
                record,
                role,
                seen: view.version,
                settlement,
                message: KeptMessage::of(stored),
            };
            tx.put(&key::divergent_write(&record, role, &msg_id), &kept)?;
        }
        self.local_commit(tx).await
    }

    /// Hold `list`, kept over its divergent record `record` and confirmed since, as a list
    /// held over its record (`lh/`), unless a list as new is held, with `tx`, in one commit:
    /// [`Paykit::place_held_lists`] writes it, on the record as it reads then.
    async fn hold_kept_list(
        &mut self,
        record: [u8; 32],
        list: &StoredMessage,
        mut tx: Tx,
    ) -> Result<()> {
        let held_key = key::held_list(&record);
        // A list held already is read bound to its key: a list of the payee app whose
        // `payment_lists` record the key names (`state::Stored`).
        let newer = match self.local_get::<KeptMessage>(&held_key).await? {
            None => true,
            Some(kept) => {
                crate::receive::list_version(&kept.stored(&self.identity())?)
                    < crate::receive::list_version(list)
            }
        };
        if newer {
            tx.put(&held_key, &KeptMessage::of(list))?;
        }
        self.local_commit(tx).await
    }

    /// Give up `stored`, kept over a divergent record at `kept_key`: the kept copy goes and
    /// the host is told, in one commit; the notice is raised once.
    async fn give_up_divergent(&mut self, kept_key: &str, stored: &StoredMessage) -> Result<()> {
        let mut tx = Tx::default();
        tx.delete(kept_key);
        let notice = key::message_notice(&stored.sender(), &stored.message().msg_id);
        if self.kept_notice(&notice).await?.is_none() {
            let at = self.service_time().unwrap_or(0);
            tx.put(
                &notice,
                &crate::stored::given_up_notice(
                    stored,
                    at,
                    crate::notices::GivenUpOrigin::Divergent,
                ),
            )?;
        }
        self.local_commit(tx).await
    }

    /// The records the app has seen divergent and its user hasn't confirmed, those whose
    /// latest view shows them divergent, and those whose kept messages wait to be written
    /// (Paykit spec 6), by record key: for the host to show, each with what the user is asked
    /// to see, such as a request the payee may have cancelled or a list that may be stale,
    /// and to confirm ([`Paykit::confirm_divergent`]). While one is paused, nothing is paid,
    /// accepted or credited on it automatically, and by hand only once the user confirms it
    /// and its kept messages are written; a write over it that adds no stop waits too. A
    /// record listed here is never shown for a credit
    /// ([`DivergentRecord::may_hold_credit`]): a credit's refusal shows it so, and its
    /// confirmation releases no credit. To credit a proof, the host calls
    /// [`Paykit::confirm_payment`] again, has the user confirm each record its
    /// [`Refusal::Divergent`] names, shown with `may_hold_credit`, exactly as that refusal
    /// gives it, and calls `confirm_payment` once more. The `payment_lists` record of an app
    /// the payee's `identity.json` no longer lists in `apps` is listed too while paused: it
    /// holds back no payment, since no list of that app is paid (Paykit spec 8.4), and
    /// confirming it only ends its pause.
    ///
    /// # Errors
    ///
    /// A store failure, or a mark that doesn't read.
    pub async fn divergent_records(&self) -> Result<Vec<DivergentRecord>> {
        let mut keys = std::collections::BTreeSet::new();
        for mark in self.divergent_marks().await? {
            keys.insert(mark.record);
        }
        for collection in crate::hold::RecordSide::All.collections() {
            for held in self.app.records_of(&collection.name).await? {
                if held.divergent {
                    keys.insert(held.key);
                }
            }
        }
        let mut out = Vec::new();
        for record in keys {
            let pause = self.pause_seen(&record).await?;
            if pause.paused || pause.divergent || pause.writes_waiting {
                out.push(self.shown(record, pause, false).await?);
            }
        }
        Ok(out)
    }

    /// The user has seen `shown`, a divergent record as [`Paykit::divergent_records`] or a
    /// refusal ([`Refusal::Divergent`]) gave it, and what its upload may hold, such as a
    /// cancellation of a request, a receipt, a newer list or entries of a ledger, and lets
    /// its payments and writes go on (Paykit spec 6): the record's pause ends in this app.
    /// The confirmation is of the write shown: given while the view shows it divergent, it
    /// holds for that write, and a later divergent view of another write pauses it again.
    /// The messages kept over it are written at the next receive, before anything is paid,
    /// accepted or credited on it. No automatic payment is started or repeated on it, nor a
    /// request accepted, while its view still shows it divergent; a payment already made is
    /// recorded. Given over a record shown for a credit
    /// ([`DivergentRecord::may_hold_credit`]), of the write a credit's refusal showed, it also
    /// lets a credit go (Paykit spec 8.1 step 4); a plain one doesn't, nor one whose flag the
    /// refusal didn't set. So a payee app's host that confirmed a record from a receive's
    /// summary or [`Paykit::divergent_records`] doesn't stop there to credit a proof: it calls
    /// [`Paykit::confirm_payment`] again, confirms each record the refusal names, shown with
    /// `may_hold_credit`, as the refusal gives it, and calls `confirm_payment` once more,
    /// after a receive while messages kept over one of them wait to be written
    /// ([`DivergentRecord::writes_waiting`]). The app knows only that it returned that refusal
    /// to the host; the
    /// host shows it to the user and passes `UserConfirmed` once the user has seen it. A mark
    /// that doesn't point to the record held is replaced. Confirming a record no longer held
    /// ends its pause and drops the mark. Another own app keeps its own pause until its own
    /// user confirms. A restore from a backup pauses the record again.
    ///
    /// # Errors
    ///
    /// [`PaykitError::Conflict`] when the record the app holds is another write than the one
    /// shown, or no longer held: the host shows it again; [`Refusal::NotFound`] for a record
    /// the app neither has seen divergent nor holds divergent now;
    /// [`Refusal::CertificateRefused`] while the app's certificate fails Paykit's checks; a
    /// store failure, or a mark that doesn't read.
    pub async fn confirm_divergent(
        &mut self,
        shown: &DivergentRecord,
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        let record = &shown.record;
        let (marked, view) = self.mark_and_view(record).await?;
        if marked == Marked::Clear && !view.is_some_and(|view| view.divergent) {
            return Err(Refusal::NotFound.into());
        }
        if view.map(|view| view.version.record_version()) != shown.version {
            return Err(PaykitError::Conflict(
                "the record changed since it was shown".into(),
            ));
        }
        // A mark that doesn't point to its record is replaced by one confirmed at the write
        // held ([`Marked::after_confirmation`]).
        match marked.after_confirmation(view.as_ref(), shown.may_hold_credit) {
            Some(after) => self.put_mark(record, &after).await,
            None => self.local_delete(&key::divergent(record)).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::StateKey;

    const RECORD: [u8; 32] = [7; 32];

    fn view(divergent: bool, version: u64) -> View {
        View {
            record: RECORD,
            collection: Collection::PayerActivity,
            divergent,
            version: SeenVersion {
                generation: 1,
                version,
            },
        }
    }

    fn confirmed(version: u64) -> DivergentMark {
        after_confirmation(None, Some(&view(true, version)), false).unwrap()
    }

    #[test]
    fn test_divergent_view_pauses_a_record_not_paused() {
        let after = after_view(None, &view(true, 3)).unwrap();
        assert!(after.is_paused());
        let pause = Pause::of(None, Some(&view(true, 3)));
        assert!(pause.blocks_automatic() && pause.blocks_by_hand() && pause.write_waits());
    }

    #[test]
    fn test_divergent_clean_view_neither_pauses_nor_ends_a_pause() {
        assert!(after_view(None, &view(false, 3)).is_none());
        let paused = DivergentMark::paused(&view(true, 3));
        assert!(after_view(Some(&paused), &view(false, 4)).is_none());
        let pause = Pause::of(Some(&paused), Some(&view(false, 4)));
        // Kept after a newer write cleared the view: by hand waits, a write doesn't.
        assert!(pause.blocks_by_hand() && pause.blocks_automatic() && !pause.write_waits());
        assert!(!confirmed_mark_ends(&paused, Some(&view(false, 4))));
        assert!(!confirmed_mark_ends(&paused, None));
    }

    #[test]
    fn test_divergent_confirmation_holds_for_its_write_only() {
        let mark = confirmed(3);
        assert!(after_view(Some(&mark), &view(true, 3)).is_none());
        let pause = Pause::of(Some(&mark), Some(&view(true, 3)));
        assert!(!pause.blocks_by_hand() && !pause.write_waits());
        // Still no automatic payment while the view shows it divergent.
        assert!(pause.blocks_automatic());
        // Another write seen divergent pauses it again.
        let again = after_view(Some(&mark), &view(true, 5)).unwrap();
        assert!(again.is_paused());
        assert!(Pause::of(Some(&mark), Some(&view(true, 5))).blocks_by_hand());
    }

    #[test]
    fn test_divergent_confirmed_mark_ends_once_the_view_clears() {
        let mark = confirmed(3);
        assert!(!confirmed_mark_ends(&mark, Some(&view(true, 3))));
        assert!(confirmed_mark_ends(&mark, Some(&view(false, 4))));
        assert!(confirmed_mark_ends(&mark, None));
        assert!(!Pause::of(Some(&mark), Some(&view(false, 4))).blocks_automatic());
    }

    /// The client's event ID `counter` of one store.
    fn event(counter: u64) -> EventId {
        format!("{}-{counter}", "ab".repeat(16)).parse().unwrap()
    }

    #[test]
    fn test_divergent_event_pauses_a_record_without_a_mark_whatever_its_view() {
        assert!(after_event(None, &view(false, 4), &event(1))
            .unwrap()
            .is_paused());
        // A report of the write the user confirmed, while the view shows it divergent, asks
        // nothing again: one confirmation covers it.
        let kept = after_event(Some(&confirmed(3)), &view(true, 3), &event(1)).unwrap();
        assert!(!kept.is_paused());
        // The same report again changes nothing.
        assert!(after_event(Some(&kept), &view(true, 3), &event(1)).is_none());
    }

    #[test]
    fn test_divergent_event_pauses_a_confirmed_record_again_whatever_its_view() {
        // A later divergence's report, its view cleared by a newer write before the report
        // is taken: the earlier confirmation, a credit's included, confirms nothing for it.
        let credit = DivergentMark {
            credit_shown: Some(view(true, 3).version),
            ..DivergentMark::paused(&view(true, 3))
        };
        let confirmed = after_confirmation(Some(&credit), Some(&view(true, 3)), true).unwrap();
        assert!(!confirmed.credit_waits());
        for held in [view(false, 5), view(true, 5)] {
            let again = after_event(Some(&confirmed), &held, &event(2)).unwrap();
            assert!(again.is_paused() && again.credit_waits());
            assert!(!confirmed_mark_ends(&again, Some(&view(false, 6))));
            let pause = Pause::of(Some(&again), Some(&view(false, 6)));
            assert!(pause.blocks_by_hand() && pause.blocks_automatic() && pause.blocks_credit());
            // A paused mark stays as it is at a later report, the report noted.
            let later = after_event(Some(&again), &view(false, 6), &event(3)).unwrap();
            assert_eq!(later.confirmed, None);
            assert_eq!(later.seen, again.seen);
        }
        // A report the mark took before its confirmation, delivered again, pauses nothing:
        // the confirmation came after it.
        let taken = after_event(None, &view(true, 3), &event(4)).unwrap();
        let confirmed = after_confirmation(Some(&taken), Some(&view(true, 3)), false).unwrap();
        assert!(after_event(Some(&confirmed), &view(false, 5), &event(4)).is_none());
        assert!(after_event(Some(&confirmed), &view(false, 5), &event(3)).is_none());
        // A report of another store is never taken as one taken.
        let other: EventId = format!("{}-4", "cd".repeat(16)).parse().unwrap();
        assert!(after_event(Some(&confirmed), &view(false, 5), &other)
            .unwrap()
            .is_paused());
        // A restore drops the report: the restored client's IDs are of its own store.
        assert_eq!(confirmed.restored().event, None);
    }

    #[test]
    fn test_divergent_confirmation_of_a_record_not_held_drops_the_mark() {
        assert!(
            after_confirmation(Some(&DivergentMark::paused(&view(true, 3))), None, false).is_none()
        );
        // Confirmed at the write held, keeping the write it was first seen divergent at.
        let mark = after_confirmation(
            Some(&DivergentMark::paused(&view(true, 3))),
            Some(&view(false, 4)),
            false,
        )
        .unwrap();
        assert_eq!(mark.seen, view(true, 3).version);
        assert_eq!(mark.confirmed, Some(view(false, 4).version));
    }

    #[test]
    fn test_divergent_mark_bound_to_the_record_its_key_names() {
        let mark = DivergentMark::paused(&view(true, 3));
        assert!(mark.bind_to(&RECORD).is_ok());
        // At another record's key.
        assert!(mark.bind_to(&[8; 32]).is_err());
        // Of a collection that isn't Paykit's.
        let foreign = DivergentMark {
            collection: "notes".into(),
            ..mark.clone()
        };
        assert!(foreign.bind_to(&RECORD).is_err());
        // Through the state's reader: a value at a key of another family doesn't read.
        assert!(StateKey::parse(key::divergent(&RECORD).as_bytes())
            .is_some_and(|family| family == StateKey::Divergent(RECORD)));
    }

    #[test]
    fn test_divergent_restore_pauses_a_confirmed_mark() {
        let restored = confirmed(3).restored();
        assert!(restored.is_paused());
        assert!(restored.bind_to(&RECORD).is_ok());
        assert!(DivergentMark::paused(&view(true, 3)).restored().is_paused());
    }

    #[test]
    fn test_divergent_mark_points_to_its_record_by_collection_and_version() {
        let mark = DivergentMark::paused(&view(true, 3));
        let marked = |view: Option<&View>| Marked::of(Some(mark.clone()), view);
        assert_eq!(marked(Some(&view(true, 3))), Marked::Mark(mark.clone()));
        assert_eq!(marked(Some(&view(false, 9))), Marked::Mark(mark.clone()));
        // Not held proves nothing: the pause stays.
        assert_eq!(marked(None), Marked::Mark(mark.clone()));
        // A record held at an earlier write than the one seen divergent isn't it.
        assert_eq!(marked(Some(&view(true, 2))), Marked::Mismatch);
        // Nor is one of another collection.
        let lists = View {
            collection: Collection::PaymentLists,
            ..view(true, 3)
        };
        assert_eq!(marked(Some(&lists)), Marked::Mismatch);
        assert_eq!(Marked::of(None, Some(&view(true, 3))), Marked::Clear);
    }

    #[test]
    fn test_divergent_mismatched_mark_reads_paused_and_names_nothing() {
        // Confirmed, then held at an earlier write: it doesn't point to the record.
        let marked = Marked::of(Some(confirmed(5)), Some(&view(false, 4)));
        assert_eq!(marked, Marked::Mismatch);
        for divergent in [true, false] {
            let held = view(divergent, 4);
            let pause = marked.pause(Some(&held));
            assert!(pause.paused && pause.blocks_by_hand() && pause.blocks_automatic());
            assert!(pause.blocks_credit() && pause.dispatched_write_waits());
            assert_eq!(pause.write_waits(), divergent);
            // No mark is written over it, and it never goes by itself.
            assert!(marked.after_view(&held).is_none());
            assert!(marked.after_event(&held, &event(1)).is_none());
            assert!(!marked.ends(Some(&held)));
        }
        // The user's confirmation replaces it at the write held.
        let replaced = marked
            .after_confirmation(Some(&view(true, 4)), false)
            .unwrap();
        assert_eq!(replaced, confirmed(4));
        assert!(replaced.bind_to(&RECORD).is_ok());
        assert!(marked.after_confirmation(None, false).is_none());
    }

    #[test]
    fn test_divergent_dispatched_write_waits_while_paused_and_goes_once_confirmed() {
        let paused = DivergentMark::paused(&view(true, 3));
        assert!(Pause::of(Some(&paused), Some(&view(true, 3))).dispatched_write_waits());
        // Paused with the view cleared: still waits for the confirmation.
        assert!(Pause::of(Some(&paused), Some(&view(false, 4))).dispatched_write_waits());
        // Confirmed over a view still divergent: the record of a payment made goes, while
        // nothing starts or repeats a payment automatically.
        let pause = Pause::of(Some(&confirmed(3)), Some(&view(true, 3)));
        assert!(!pause.dispatched_write_waits() && pause.blocks_automatic());
        // Messages kept over it go first.
        let waiting = Pause {
            writes_waiting: true,
            ..pause
        };
        assert!(waiting.dispatched_write_waits() && waiting.blocks_by_hand());
        assert!(waiting.blocks_credit() && !waiting.kept_writes_wait());
    }

    #[test]
    fn test_divergent_credit_waits_for_a_confirmation_shown_for_a_credit() {
        let sold = View {
            collection: Collection::PayeeActivity,
            ..view(true, 3)
        };
        let clear = View {
            collection: Collection::PayeeActivity,
            ..view(false, 4)
        };
        let paused = DivergentMark::paused(&sold);
        let plain = after_confirmation(Some(&paused), Some(&sold), false).unwrap();
        let pause = Pause::of(Some(&plain), Some(&clear));
        assert!(!pause.blocks_by_hand() && pause.blocks_credit());
        // The mark stays once the view clears: the credit still waits for it.
        assert!(!confirmed_mark_ends(&plain, Some(&clear)));
        // A confirmation given as for a credit counts only once a credit's refusal has
        // shown the record so, at that write.
        let unshown = after_confirmation(Some(&plain), Some(&sold), true).unwrap();
        assert!(unshown.credit_waits());
        let shown = DivergentMark {
            credit_shown: Some(sold.version),
            ..plain.clone()
        };
        let elsewhere = DivergentMark {
            credit_shown: Some(clear.version),
            ..plain.clone()
        };
        assert!(after_confirmation(Some(&elsewhere), Some(&sold), true)
            .unwrap()
            .credit_waits());
        let credit = after_confirmation(Some(&shown), Some(&sold), true).unwrap();
        assert!(!Pause::of(Some(&credit), Some(&clear)).blocks_credit());
        assert!(confirmed_mark_ends(&credit, Some(&clear)));
        assert!(credit.bind_to(&RECORD).is_ok());
        // A plain confirmation after keeps the credit's.
        let again = after_confirmation(Some(&credit), Some(&sold), false).unwrap();
        assert!(!again.credit_waits());
        // A restore drops it with the confirmation and the showing; one without a
        // confirmation is corrupt.
        let restored = credit.clone().restored();
        assert!(restored.credit_waits() && restored.credit_shown.is_none());
        let crossed = DivergentMark {
            confirmed: None,
            ..credit
        };
        assert!(crossed.bind_to(&RECORD).is_err());
        // A payer's record needs no credit's confirmation to end.
        assert!(confirmed_mark_ends(&confirmed(3), Some(&view(false, 4))));
    }

    #[test]
    fn test_divergent_kept_proof_of_another_settlement_than_the_rails_points_elsewhere() {
        let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
        let proof = alice.message(
            &bob,
            "paykit.payment_proof",
            Vec::new(),
            crate::testkit::object(serde_json::json!({
                "attempt_id": "mAFdg5_0tVNjmrgiTUZW4g",
                "payment_app_id": "brrz3m6g5rci6iqnzke44q4djc",
                "payment_endpoint_identifier": "btc-bitcoin-p2wpkh",
                "amount": { "asset": "btc", "value": "0.1" },
                "proof": { "rail": "fake", "tx": "t1" },
            })),
        );
        let kept = DivergentWrite {
            record: RECORD,
            role: WriteRole::Proof,
            seen: view(true, 3).version,
            settlement: Some(pubky_switch_core::enc::b64(b"t1")),
            message: KeptMessage::of(&proof),
        };
        let rail = |proof: &serde_json::Map<String, serde_json::Value>| {
            paykit_lib::derive::settlement_id(proof)
        };
        assert!(kept.kept(&bob.id(), &rail).unwrap().is_some());
        // The rail names another settlement now, or none: it doesn't point, no error.
        assert!(kept
            .kept(&bob.id(), &|_| Some(b"t2".to_vec()))
            .unwrap()
            .is_none());
        assert!(kept.kept(&bob.id(), &|_| None).unwrap().is_none());
    }

    #[test]
    fn test_divergent_mark_confirmed_before_its_write_is_corrupt() {
        let mark = DivergentMark {
            confirmed: Some(view(true, 2).version),
            ..DivergentMark::paused(&view(true, 3))
        };
        assert!(mark.bind_to(&RECORD).is_err());
    }

    #[test]
    fn test_divergent_kept_after_keeps_the_first_and_the_newest_others() {
        let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
        let reply = || {
            alice.message(
                &bob,
                "paykit.payment_request_acceptance",
                vec![[1; 32]],
                crate::testkit::object(serde_json::json!({ "label": null })),
            )
        };
        let mut kept = Vec::new();
        for _ in 0..=KEPT_OTHERS {
            let new = reply();
            let (keep, gone) = kept_after(&kept, &new, true);
            assert!(keep && gone.is_empty());
            kept.push(new);
        }
        // The same message again is no new one.
        assert_eq!(kept_after(&kept, &kept[3], true), (false, Vec::new()));
        // Past the bound, the earliest of the others goes, the first staying: one time of
        // acceptance here, so the order is the `msg_id`'s.
        let new = reply();
        let (keep, gone) = kept_after(&kept, &new, true);
        let mut all: Vec<String> = kept
            .iter()
            .chain(std::iter::once(&new))
            .map(|stored| stored.message().msg_id.clone())
            .collect();
        all.sort();
        assert_eq!(gone, vec![all[1].clone()]);
        assert_eq!(keep, new.message().msg_id != all[1]);
    }

    #[test]
    fn test_divergent_kept_after_drops_only_the_new_messages_sender() {
        // The bound is full of alice's messages; carol's come. None of alice's goes for them:
        // what goes past the bound is carol's own, the one a step takes kept over the bound
        // until a later message of carol's drops it.
        let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
        let carol = crate::testkit::Party::new("carol", "wallet", &["pay", "contacts"]);
        let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
        let reply = |from: &crate::testkit::Party| {
            from.message(
                &bob,
                "paykit.payment_request_acceptance",
                vec![[1; 32]],
                crate::testkit::object(serde_json::json!({ "label": null })),
            )
        };
        let mut kept: Vec<StoredMessage> = (0..=KEPT_OTHERS).map(|_| reply(&alice)).collect();
        let alices: Vec<String> = kept
            .iter()
            .map(|stored| stored.message().msg_id.clone())
            .collect();
        for _ in 0..3 {
            let new = reply(&carol);
            let (_, gone) = kept_after(&kept, &new, false);
            assert!(gone.iter().all(|id| !alices.contains(id)), "{gone:?}");
            kept.retain(|stored| !gone.contains(&stored.message().msg_id));
            kept.push(new);
        }
    }

    #[test]
    fn test_divergent_kept_after_caps_all_senders_together() {
        let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
        let reply = |from: &crate::testkit::Party| {
            from.message(
                &bob,
                "paykit.payment_request_acceptance",
                vec![[1; 32]],
                crate::testkit::object(serde_json::json!({ "label": null })),
            )
        };
        let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
        let others: Vec<crate::testkit::Party> = (0..32)
            .map(|i| {
                crate::testkit::Party::new(&format!("other{i}"), "wallet", &["pay", "contacts"])
            })
            .collect();
        let id = |stored: &StoredMessage| stored.message().msg_id.clone();
        let first_of = |kept: &[StoredMessage], new: &StoredMessage| {
            kept.iter()
                .chain(std::iter::once(new))
                .map(id)
                .min()
                .unwrap()
        };
        // Over a record that reads, the counterparty's message: past the cap, another
        // sender's goes, never the counterparty's.
        let mut kept: Vec<StoredMessage> = (0..KEPT_OTHERS).map(|_| reply(&alice)).collect();
        kept.extend(others.iter().take(KEPT_CAP - KEPT_OTHERS).map(&reply));
        assert_eq!(kept.len(), KEPT_CAP);
        let new = reply(&alice);
        let (keep, gone) = kept_after(&kept, &new, true);
        assert!(keep);
        assert_eq!(gone.len(), 1, "{gone:?}");
        let went = kept.iter().find(|stored| id(stored) == gone[0]).unwrap();
        assert_ne!(went.sender(), alice.id());
        assert_ne!(gone[0], first_of(&kept, &new));
        // Over a record that doesn't read: past the cap, the earliest of the sender keeping
        // the most goes, the role's first aside.
        let mut kept: Vec<StoredMessage> = (0..3).map(|_| reply(&alice)).collect();
        kept.extend(others.iter().take(KEPT_CAP - 3).map(&reply));
        assert_eq!(kept.len(), KEPT_CAP);
        let new = reply(&others[KEPT_CAP - 3]);
        let (keep, gone) = kept_after(&kept, &new, false);
        assert!(keep);
        assert_eq!(gone.len(), 1, "{gone:?}");
        let went = kept.iter().find(|stored| id(stored) == gone[0]).unwrap();
        assert_eq!(went.sender(), alice.id());
        assert_ne!(gone[0], first_of(&kept, &new));
    }

    #[test]
    fn test_divergent_write_role_never_keeps_a_stop() {
        for (kind, role) in [
            ("paykit.payment_request_cancellation", None),
            ("paykit.payment_request_rejection", None),
        ] {
            let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
            let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
            let stop = alice.message(
                &bob,
                kind,
                vec![[1; 32]],
                crate::testkit::object(serde_json::json!({ "reason": null })),
            );
            assert_eq!(WriteRole::of(&stop), role, "{kind}");
            assert!(adds_stop(&stop), "{kind}");
        }
        for role in [
            WriteRole::Request,
            WriteRole::Response,
            WriteRole::Proof,
            WriteRole::Receipt,
            WriteRole::List,
        ] {
            assert_eq!(WriteRole::from_tag(role.tag()), Some(role));
        }
        assert_eq!(WriteRole::from_tag("cancellation"), None);
    }
}
