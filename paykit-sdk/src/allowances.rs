//! Allowances (Paykit spec 8.3; Paykit wire 4.15 to 4.17, 5.4): proposing, answering and
//! ending an Allowance, its `allowances` record on either side, the executor's consent list
//! and `executor_notice`.
//!
//! - **The record:** one `allowances` record per Allowance, keyed by the counterparty and
//!   the `allowance_id`, holding the proposal, its response and its end, one of each (Paykit
//!   spec 6 "One message per role"). A message this app sends goes into the record in the
//!   batch that queues it: a proposal opens it, an acceptance, a rejection or an end is
//!   recorded first and sent only if that write is applied (Paykit spec 8.1, 8.3). A
//!   message that would break the record's rules (Paykit wire 5.4) is reflected by its
//!   `msg_id` only, such as an acceptance crossing the proposal's withdrawal. No path stores
//!   a message of this app's without queuing it, so a message this app signed in an
//!   Allowance record is one to be sent; the re-send after a restore builds on that
//!   (`stores_unqueued`).
//! - **Receiving:** a proposal opens the record, gated by the spam rule; an acceptance or a
//!   rejection that would take the record's response is kept by the receive step (`pr/`)
//!   and goes into the proposer's record after the read, once the read's stored entries
//!   are noted, or once the app holds the record (Paykit spec 8.1, "Replies before their
//!   record"); a rejection's stop acts from when it is kept. Every branch holds its entry
//!   while the app's copy of `allowances` isn't listed yet, never deciding from a record's absence (Paykit
//!   spec 8.1, "Records not listed yet"). One whose record the app holds as a value it
//!   can't read is never written over that record, nor dropped at once: it waits
//!   unprocessed while the value waits for its scope key, and is otherwise kept as a stored
//!   entry, tried at each read and written in once the record reads, and given up with the
//!   host's notice 30 days from its acceptance (Paykit spec 8.1, "Stored entries"). An
//!   acceptance over a record seen divergent waits for the user's confirmation with the
//!   kept replies (`pr/`). Answers are placed in the order the inbox accepted them: one
//!   never takes the response while a different, earlier one is still kept to write, but
//!   is kept as the conflict's stop at once (Paykit spec 8.1, 8.3).
//! - **The stop:** a counterparty's `allowance_end` is kept in the app's own state in the
//!   commit that passes its entry, with no write at the switch (`ae/`): at once for an
//!   Allowance whose record the app holds, whatever the record's value, never waiting for
//!   a scope key; for one it holds no record of, as new state, bounded together with the
//!   payees' cancellations kept for requests not held. Such a stop goes 30 days after the
//!   end's acceptance, but never while the copy may lack records or the app keeps a stored
//!   message of the Allowance to write, and the bounds drop none kept so. From then on nothing
//!   is done under the Allowance, whatever the record says, until the record holds an end or
//!   a rejection: the app writes the end there by a later write of its own, with the
//!   `executor_notice`. An end that names its proposal alone, crossing the acceptance this
//!   identity sent, can't go into a record holding that acceptance: the app ends the
//!   Allowance itself, naming both, so the record shows it ended.
//! - **The consent list** (`ac/`): the Allowances this app, as executor, may pay under. An
//!   Allowance seen ended, rejected or conflicted, or named by an `executor_notice`, is
//!   removed for good: a removed entry never comes back. Under a record held as deleted it
//!   pays nothing, its entry kept. A restore marks every entry to be confirmed
//!   again, since the backup may predate an end, and so does a `missed` item, since an end
//!   may be among the entries the service deleted (`mc`, Paykit spec 7.2, 8.3).
//! - **`executor_notice`:** an app that ends an Allowance, or records the counterparty's
//!   end, tells the executor the record's current grant names, through the identity's own
//!   inbox, wrapped to that app only (Paykit wire 4.17).
//!
//! The executor's grant, its ledger and paying under an Allowance build on these records.
//!
//! Security: the stop is the kept end, not the record, so an own app that rewrites the
//! record, or a record write that fails, never lifts it; the record's seen roles keep an own
//! app from dropping a message another app recorded (Paykit wire 5.4, Switch spec 8.2).

use paykit_lib::allowances::AllowanceTerms;
use paykit_lib::bodies::{
    AllowanceProposal, AllowanceReply, ExecutorNotice, NoticeReason, ProposerRole,
};
use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{bounded_msg_ids, AllowanceRecord, StoredMessage};
use paykit_lib::{AllowanceId, Body};
use pubky_switch_client::app::{
    Delivery, LocalWrite, OpBatch, ReceiveContext, RecordContent, RecordTarget, SignedMessage,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::http::ErrorCode;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::hold::RecordSide;
use crate::paykit::Paykit;
use crate::receive::{self, Fail};
use crate::records::{conflict, KeptMessage};
use crate::requests::MessageDelivery;
use crate::state::{self, key, Tx};
use crate::stored::{Placed, StoredNote};

/// How many times a write of this app's is built again after its record changed meanwhile.
const WRITE_TRIES: usize = 3;

/// This identity's role in an Allowance (Paykit wire 4.15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AllowanceRole {
    /// The party whose apps pay.
    Allower,
    /// The party paid.
    Allowee,
}

/// Where an Allowance stands, from the messages its record holds and the stop this app
/// keeps (Paykit spec 8.3, Paykit wire 5.4): never from a member alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AllowanceStatus {
    /// Proposed; no answer yet.
    Proposed,
    /// Accepted: it may authorize automatic payment, while its terms and the executor's
    /// checks pass.
    Accepted,
    /// Rejected by the proposal's recipient.
    Rejected,
    /// Ended by either side, or withdrawn by its proposer; or this app keeps the
    /// counterparty's end, not yet in the record.
    Ended,
    /// Conflicted: a second, different proposal under its Allowance ID, or a second,
    /// different response to its proposal, came, so neither binds and nothing is paid or
    /// answered under it, for good (Paykit spec 8.3, "Conflicted"); the record holds the
    /// second message in `conflicted`, or this app keeps it as the stop, not yet there.
    Conflicted,
}

/// An Allowance this identity holds a record of, as this app's copy shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct AllowanceView {
    /// The record key of its `allowances` record.
    pub record: [u8; 32],
    /// The other identity.
    pub counterparty: [u8; 32],
    /// The Allowance ID.
    pub allowance_id: AllowanceId,
    /// This identity's role. `None` only for a conflicted Allowance whose record is held as
    /// deleted and whose kept conflicting message is a response: the proposal that names
    /// the roles isn't held then.
    pub role: Option<AllowanceRole>,
    /// Whether this identity proposed it.
    pub proposed_here: bool,
    /// The proposed terms. `None` only when [`AllowanceView::role`] is.
    pub terms: Option<AllowanceTerms>,
    /// Where it stands.
    pub status: AllowanceStatus,
    /// The executor app the record's current grant names, if it holds one that verifies.
    pub executor_app_id: Option<[u8; 16]>,
    /// Whether this app's consent list holds it, confirmed since any restore: an executor
    /// pays under an Allowance only then (Paykit spec 8.3).
    pub consented: bool,
    /// Whether the counterparty is on the identity's deny list, as the app last read it
    /// (Paykit spec 8.1, "Blocked counterparties"): nothing new from it is taken, its
    /// requests are paused and nothing is sent to it, so the host shows the Allowance as
    /// blocked. `None` while the copy of the deny list isn't known, at an app holding
    /// `contacts` after a failed read of the lists or a restore: sends are held then too,
    /// and the host can't show it as not blocked.
    pub blocked: Option<bool>,
}

/// An Allowance proposal this app sent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentAllowance {
    /// The record key of its `allowances` record, written in the batch that queues it.
    pub record: [u8; 32],
    /// The fresh Allowance ID.
    pub allowance_id: AllowanceId,
    /// The proposal's message ID.
    pub msg_id: String,
    /// Where the proposal is.
    pub delivery: MessageDelivery,
}

/// An entry of the consent list (`ac/<record key>`, Paykit spec 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Consent {
    /// On the list: the executor may pay under it, unless `confirm_again`, set by a restore
    /// until the user confirms it again.
    Held {
        /// To be confirmed again by the user.
        confirm_again: bool,
    },
    /// Removed for good: the Allowance ended, was rejected, handed off or conflicted.
    Removed {
        /// Why.
        reason: ConsentEnd,
    },
}

impl Consent {
    /// The entry as a restore or a `missed` item leaves it (Paykit spec 8.3, "After a
    /// restore"): one on the list is to be confirmed by the user again before the executor
    /// pays under it; a removal stays for good.
    pub(crate) fn confirm_again(self) -> Self {
        match self {
            Self::Held { .. } => Self::Held {
                confirm_again: true,
            },
            removed => removed,
        }
    }
}

/// An entry of the consent list as kept (`ac/<record key>`): the Allowance it is of, by its
/// counterparty and Allowance ID, which derive the record key its key names, so a read
/// proves the entry is that Allowance's (`state::Stored`), and its [`Consent`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConsentEntry {
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    pub(crate) allowance_id: String,
    pub(crate) consent: Consent,
}

impl ConsentEntry {
    /// The entry of `counterparty`'s Allowance `allowance_id`.
    pub(crate) fn new(
        counterparty: [u8; 32],
        allowance_id: &AllowanceId,
        consent: Consent,
    ) -> Self {
        Self {
            counterparty,
            allowance_id: allowance_id.as_str().to_owned(),
            consent,
        }
    }

    /// The entry with its consent replaced by `consent`.
    pub(crate) fn with(&self, consent: Consent) -> Self {
        Self {
            consent,
            ..self.clone()
        }
    }

    /// The Allowance ID it names: a corrupt store for one that doesn't parse.
    pub(crate) fn allowance_id(&self) -> Result<AllowanceId> {
        AllowanceId::parse(&self.allowance_id).map_err(|_| PaykitError::corrupt())
    }
}

/// Why an Allowance left the consent list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConsentEnd {
    /// It ended.
    Ended,
    /// It was rejected.
    Rejected,
    /// Another app is its executor now.
    HandedOff,
    /// It is conflicted.
    Conflicted,
}

/// The executor app an `executor_notice` is wrapped to (`en/<msg_id>`), kept with the batch
/// that queues it; it names its message, so it is taken only for that one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NoticeTarget {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
}

/// This app's own crossing end recorded while sends to the counterparty were held, whose
/// message is owed (`eo/<record key>`; Paykit spec 8.1, "Blocked counterparties"; 8.3,
/// "Ending"): it names the record and the end's `msg_id`, and is written in the batch that
/// records the end. The message goes from the record once sends to the counterparty aren't
/// held ([`Paykit::send_owed_ends`]). Never restored: a restore queues the record's own end
/// again itself, under the same gate (`hold.rs`, `queue_own_again`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwedEnd {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) msg_id: String,
}

/// The client's target of the `allowances` record of `counterparty`'s Allowance
/// `allowance_id` (Paykit wire 1.5, 5.4).
pub(crate) fn allowance_target(
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
) -> RecordTarget {
    RecordTarget::new(
        crate::activity::switch_collection(Collection::Allowances, None),
        &natural_key::allowance(counterparty, allowance_id),
    )
}

/// The Allowance a proposal, an acceptance, a rejection or an end names.
pub(crate) fn named_allowance(body: &Body) -> Option<&AllowanceId> {
    match body {
        Body::AllowanceProposal(p) => Some(&p.allowance_id),
        Body::AllowanceAcceptance(r) | Body::AllowanceRejection(r) | Body::AllowanceEnd(r) => {
            Some(&r.allowance_id)
        }
        _ => None,
    }
}

/// Whether `body` is an Allowance's acceptance or rejection.
pub(crate) fn is_allowance_response(body: &Body) -> bool {
    matches!(
        body,
        Body::AllowanceAcceptance(_) | Body::AllowanceRejection(_)
    )
}

/// An `allowances` value held for `counterparty`'s Allowance `allowance_id`, as `own` reads
/// it (Paykit wire 5.4 "Binding"): it reads, and names that counterparty and Allowance, from
/// which its key derives. `None`: the reader doesn't act on it.
pub(crate) fn bind_at(
    value: &[u8],
    own: &[u8; 32],
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
) -> Option<AllowanceRecord> {
    let record = AllowanceRecord::read(value, own).ok()?;
    (record.counterparty == *counterparty && record.allowance_id == *allowance_id).then_some(record)
}

/// A new record holding `proposal`, a proposal between this identity and `counterparty`.
fn opened(counterparty: [u8; 32], proposal: StoredMessage) -> Option<AllowanceRecord> {
    let allowance_id = named_allowance(proposal.body())?.clone();
    let record = AllowanceRecord {
        counterparty,
        allowance_id,
        proposal,
        response: None,
        end: None,
        conflicted: None,
        grant: None,
        grants: Vec::new(),
        msg_ids: Vec::new(),
        unknown: Default::default(),
    };
    record.to_json().is_ok().then_some(record)
}

fn stored_ids(record: &AllowanceRecord) -> Vec<String> {
    std::iter::once(&record.proposal)
        .chain(&record.response)
        .chain(&record.end)
        .chain(&record.conflicted)
        .map(|stored| stored.message().msg_id.clone())
        .collect()
}

/// Whether `stored` makes the Allowance of `record` conflicted (Paykit spec 8.3,
/// "Conflicted"): a proposal under its Allowance ID whose hash differs from the record's
/// proposal, from either side, or, beside the record's response, an acceptance or rejection
/// from the proposal's recipient, referring to the record's proposal alone (Paykit wire
/// 4.16), whose hash differs from that response. A re-sent copy, with the same hash, is the
/// message itself. A response referring to anything else is no response of this Allowance
/// ([`misreferenced`]) and conflicts with nothing.
pub(crate) fn conflicts(record: &AllowanceRecord, stored: &StoredMessage) -> bool {
    match stored.body() {
        Body::AllowanceProposal(_) => stored.hash() != record.proposal.hash(),
        Body::AllowanceAcceptance(_) | Body::AllowanceRejection(_) => {
            stored.sender() == record.proposal.message().to
                && !misreferenced(record, stored)
                && record
                    .response
                    .as_ref()
                    .is_some_and(|response| response.hash() != stored.hash())
        }
        _ => false,
    }
}

/// Whether `stored`, an acceptance or a rejection, refers to anything but `record`'s
/// proposal alone (Paykit wire 4.16): it answers no proposal of this Allowance, so it is
/// dropped, writing nothing and keeping no stop.
pub(crate) fn misreferenced(record: &AllowanceRecord, stored: &StoredMessage) -> bool {
    matches!(
        stored.body(),
        Body::AllowanceAcceptance(_) | Body::AllowanceRejection(_)
    ) && stored.message().refs != [record.proposal.hash()]
}

/// Whether `record` reflects the message `msg_id`: holds it, or names it among the others.
pub(crate) fn reflects(record: &AllowanceRecord, msg_id: &str) -> bool {
    stored_ids(record).iter().any(|id| id == msg_id) || record.msg_ids.iter().any(|id| id == msg_id)
}

/// The order the inbox accepted an Allowance answer in among those of its record (Paykit
/// spec 8.1): its acceptance time, then the feed `seq` its entry was read at
/// ([`crate::records::KeptOrder`]), the same token whichever store keeps the answer, the kept
/// replies (`pr/`) or the stored entries (`se/`), and for the message the receive step is
/// taking; never its `msg_id`, which its sender chooses, nor the store that keeps it.
pub(crate) type AnswerOrder = crate::records::KeptOrder;

/// Whether the response of `record` belongs to another answer than `stored`: one of `kept`,
/// the answers the app keeps to write, is a different response to `record`'s proposal that
/// the inbox accepted before `stored`, at `order`. The earlier of two different answers is
/// the record's response and the later conflicts with it (Paykit spec 8.1, 8.3), so the
/// later one never takes the response while the earlier is still kept, whatever keeps the
/// earlier from its record: its record seen divergent and not confirmed, or a write the
/// switch refuses for now.
fn answered_before(
    record: &AllowanceRecord,
    stored: &StoredMessage,
    order: AnswerOrder,
    kept: &[(StoredMessage, AnswerOrder)],
) -> bool {
    let response = |m: &StoredMessage| {
        is_allowance_response(m.body())
            && m.sender() == record.proposal.message().to
            && !misreferenced(record, m)
    };
    response(stored)
        && kept
            .iter()
            .any(|(other, at)| *at < order && other.hash() != stored.hash() && response(other))
}

/// The order of `stored`, read from the feed at `seq`, whose acceptance time a message the
/// app takes always carries: one without it is a corrupt store.
fn answer_order(stored: &StoredMessage, seq: u64) -> Result<AnswerOrder> {
    Ok((stored.accepted_at().ok_or_else(PaykitError::corrupt)?, seq))
}

/// Place `stored` in `record` (Paykit spec 6 "One message per role", Paykit wire 5.4): a
/// response or an end is stored when its member is empty and the record keeps its rules
/// with it; a second, different proposal or response ([`conflicts`]) goes into
/// `conflicted` while that is empty; anything else, a second, different end included, is
/// reflected by its `msg_id` only. Returns `false` when the record already reflects it, so
/// nothing changes.
pub(crate) fn place(record: &mut AllowanceRecord, stored: StoredMessage) -> bool {
    let msg_id = stored.message().msg_id.clone();
    if reflects(record, &msg_id) {
        return false;
    }
    let mut candidate = record.clone();
    let slot = match stored.body() {
        _ if conflicts(record, &stored) => Some(&mut candidate.conflicted),
        Body::AllowanceAcceptance(_) | Body::AllowanceRejection(_) => Some(&mut candidate.response),
        Body::AllowanceEnd(_) => Some(&mut candidate.end),
        _ => None,
    };
    let stored_now = match slot {
        Some(slot) if slot.is_none() => {
            *slot = Some(stored);
            candidate.to_json().is_ok()
        }
        _ => false,
    };
    let held = stored_ids(record);
    let mut others: Vec<String> = record
        .msg_ids
        .iter()
        .filter(|id| !held.contains(id))
        .cloned()
        .collect();
    if stored_now {
        *record = candidate;
    } else {
        others.push(msg_id);
    }
    record.msg_ids = bounded_msg_ids(&stored_ids(record), &others);
    true
}

/// Whether the record holds the proposal's rejection.
fn rejected(record: &AllowanceRecord) -> bool {
    matches!(
        record.response.as_ref().map(StoredMessage::body),
        Some(Body::AllowanceRejection(_))
    )
}

/// The record's acceptance, if it holds one.
fn acceptance(record: &AllowanceRecord) -> Option<&StoredMessage> {
    record
        .response
        .as_ref()
        .filter(|r| matches!(r.body(), Body::AllowanceAcceptance(_)))
}

/// The status the record shows, `stopped` when this app keeps the counterparty's end,
/// `conflict` when it keeps a conflicting message as the stop. A conflict shows over all
/// else, for good; the record's own end or rejection shows over a kept end, which may yet
/// prove not to be this Allowance's.
fn status(record: &AllowanceRecord, stopped: bool, conflict: bool) -> AllowanceStatus {
    if conflict || record.conflicted.is_some() {
        AllowanceStatus::Conflicted
    } else if record.end.is_some() {
        AllowanceStatus::Ended
    } else if rejected(record) {
        AllowanceStatus::Rejected
    } else if stopped {
        AllowanceStatus::Ended
    } else if acceptance(record).is_some() {
        AllowanceStatus::Accepted
    } else {
        AllowanceStatus::Proposed
    }
}

/// The record key the consent list's entry `ac/<rest>` is kept under, read whole and
/// strictly: any other key is corrupt state.
fn consent_record(rest: &str) -> Result<[u8; 32]> {
    match crate::state::StateKey::parse(format!("{}{rest}", key::CONSENTS).as_bytes()) {
        Some(crate::state::StateKey::Consent(record)) => Ok(record),
        _ => Err(PaykitError::corrupt()),
    }
}

/// The consent list's entry of the Allowance `held`, whose record is `record_key`, removed
/// for good, for `reason`.
fn consent_removed(
    held: &AllowanceRecord,
    record_key: &[u8; 32],
    reason: ConsentEnd,
) -> Result<LocalWrite> {
    state::local_put(
        &key::consent(record_key),
        &ConsentEntry::new(
            held.counterparty,
            &held.allowance_id,
            Consent::Removed { reason },
        ),
    )
}

/// What an app holds at an Allowance's record, in the receive step.
enum Held {
    /// No record, or a delete, and the copy is listed: it holds none. A proposal over a
    /// delete is conflicted all the same.
    None,
    /// A record that binds.
    Record(Box<AllowanceRecord>),
    /// A value that doesn't read or doesn't bind: never written over, since it may hold what
    /// another own app stored; the entry is kept as a stored entry and given up after 30
    /// days ([`stored_entry`], Paykit spec 8.1, "Stored entries").
    Unreadable,
}

/// The receive step of a proposal, an acceptance, a rejection or an end that passed check
/// 10, at an app holding `allowance` (module docs). The entry of a proposal or an answer is
/// held while the record waits for its scope key or the copy of `allowances` isn't listed
/// yet: its absence then decides nothing (Paykit spec 8.1, "Records not listed yet"). An
/// end's stop waits for no key ([`keep_end`]).
pub(crate) fn receive_effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    seq: u64,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
        return Ok(());
    };
    let target = allowance_target(&sender, &allowance_id);
    if let Body::AllowanceEnd(_) = stored.body() {
        return keep_end(cx, &target, &allowance_id, stored, seq, scopes);
    }
    // The request path's own read (`receive::held_or_unlisted_at`, which
    // `receive::held_or_unlisted` is): the entry waits for the record's scope key, and an
    // absent or deleted record is held while the copy may lack records, since a later write
    // may have opened it again (Paykit spec 8.1, "Records not listed yet"). Past that, an
    // answer over a delete is kept before its record, as over no record; a proposal over a
    // delete is taken as conflicted (Paykit spec 8.3, "Ending"): one `allowance_id` has one
    // proposal, and the app can't tell what the deleted record held.
    let value = receive::held_or_unlisted_at(cx, &target, RecordSide::Allowances)?;
    let content = cx.record(&target)?.map(|held| held.content);
    let deleted = content == Some(RecordContent::Deleted);
    let present = receive::holds_record(content.as_ref());
    let held = match value {
        Some(value) => bind_at(&value, &own, &sender, &allowance_id)
            .map_or(Held::Unreadable, |r| Held::Record(Box::new(r))),
        None if present => Held::Unreadable,
        None => Held::None,
    };
    match stored.body() {
        Body::AllowanceProposal(_) => match held {
            // Kept as the stop, and nothing written at the key: a record can't hold
            // `conflicted` without its proposal.
            // A record held as deleted holds nothing the message updates: the stop is new
            // state, which the spam rule gates, so a blocked or flagged sender creates
            // none. Dropping it loses nothing: nothing is paid under a deleted record.
            Held::None if deleted => {
                if receive::admits_new_state(cx, &sender, scopes)? {
                    keep_conflict(cx, &target, stored, scopes)?;
                }
                Ok(())
            }
            Held::None => {
                if receive::admits_new_state(cx, &sender, scopes)? {
                    // Terms that aren't valid were dropped at check 10 (Paykit wire 4.15).
                    let record = opened(sender, stored.clone()).ok_or_else(|| {
                        Fail::Drop("Paykit wire 5.4: the proposal opens no valid record".into())
                    })?;
                    cx.put_record(&target, &record.to_json()?, Vec::new())?;
                    receive::note_peer(cx, &sender, stored)?;
                }
                Ok(())
            }
            Held::Record(record) => put_placed(cx, &target, *record, stored, seq, scopes),
            Held::Unreadable => stored_entry(cx, stored),
        },
        _ => match held {
            // An answer before the app holds its proposal's record, such as one to a
            // proposal another own app sent whose record this app hasn't read yet: kept,
            // as new state the spam rule gates, within the bounds on kept replies, and
            // placed once the record is held (Paykit spec 8.1).
            // Over a record held deleted, the earliest kept is under no bound, as over one
            // held ([`keep_answer`]).
            Held::None => {
                if receive::admits_new_state(cx, &sender, scopes)? {
                    keep_answer(cx, &target, stored, seq, deleted)?;
                }
                Ok(())
            }
            Held::Record(record) => put_placed(cx, &target, *record, stored, seq, scopes),
            Held::Unreadable => stored_entry(cx, stored),
        },
    }
}

/// Keep `stored`, an answer the receive step doesn't place in the Allowance record at
/// `target`, held or not yet, with the replies kept to place
/// (`pr/<sender>/<record key>/<msg_id>`), within their bounds
/// ([`receive::make_room_for_reply`]) and with the feed `seq` it was read at, which orders it
/// among them and among the stored entries ([`crate::records::KeptOrder`]). One kept already
/// is read bound to its key and stays as it is. Admission is the caller's.
///
/// Over a record the app holds, or holds deleted (`over_held`), the earliest Allowance
/// answer kept for that record is under no bound ([`receive::make_room_for_reply`]): it is the one that would
/// take the record's response, and the bounds dropping it would let a later different
/// answer of its sender take the response instead. So when this answer is that earliest
/// one, no room is made for it.
fn keep_answer(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    stored: &StoredMessage,
    seq: u64,
    over_held: bool,
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let record = cx.record_key(target)?;
    let kept = key::kept_reply(&sender, &record, &stored.message().msg_id);
    if state::cx_get::<KeptMessage>(cx, &kept)?.is_none() {
        let order = (stored.accepted_at().ok_or_else(Fail::corrupt)?, seq);
        let earliest = over_held && {
            let mut earlier = false;
            for (_, other) in
                state::cx_scan::<KeptMessage>(cx, &key::kept_reply(&sender, &record, ""))?
            {
                let answer = other.stored(&cx.identity()).map_err(|_| Fail::corrupt())?;
                let at = other.order().map_err(|_| Fail::corrupt())?;
                earlier |= is_allowance_response(answer.body()) && at < order;
            }
            !earlier
        };
        if !earliest {
            receive::make_room_for_reply(cx, &sender, order)?;
        }
        receive::keep_local(cx, receive::local(&kept, &KeptMessage::taken(stored, seq))?);
    }
    Ok(())
}

/// A proposal or an answer whose Allowance record the app holds as a value it can't read
/// for a reason other than a missing key: never written over, since it may hold what another
/// own app stored, and not dropped at once. The entry is stored locally, the
/// cursor passing it (Switch spec 8.2), and its step is tried again at each read, so the
/// message goes into the record once the record reads. Once [`KEPT_MESSAGE_MS`] have passed
/// since its acceptance it is given up, processed with the host's notice as its only write
/// ([`crate::NoticeKind::MessageGivenUp`]), as an activity record's message is (Paykit spec
/// 8.1, "Stored entries"). A value only waiting for its scope key holds the entry
/// unprocessed before this ([`receive::held_or_unlisted_at`]).
fn stored_entry(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
) -> std::result::Result<(), Fail> {
    // Past its window on the highest service time verified (Paykit spec 8.1). A stored
    // message without its acceptance time is a corrupt store, never one past its window.
    let at = stored.accepted_at().ok_or_else(Fail::corrupt)?;
    let over = cx
        .highest_service_time()
        .is_some_and(|t| at.saturating_add(receive::KEPT_MESSAGE_MS) <= t);
    if over {
        let sender = stored.sender();
        let msg_id = &stored.message().msg_id;
        let notice = crate::notices::KeptNotice::given_up(
            sender,
            msg_id,
            None,
            cx.highest_service_time().unwrap_or(0),
            crate::notices::GivenUpFrom::Origin(crate::notices::GivenUpOrigin::ReceiveStep),
        );
        receive::keep_local(
            cx,
            receive::local(&key::message_notice(&sender, msg_id), &notice)?,
        );
        return Ok(());
    }
    Err(Fail::Store(
        pubky_switch_client::app::StoreFailure::of_entry(
            pubky_switch_client::error::StoreError::Corrupt,
        ),
    ))
}

/// Write `record` with `stored` placed, in the step, unless it reflects it already. A
/// message that makes the Allowance conflicted ([`conflicts`]) is kept as the stop instead
/// ([`keep_conflict`]), and an answer that would take the empty response is kept to write
/// after the read ([`keep_answer`]).
fn put_placed(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    mut record: AllowanceRecord,
    stored: &StoredMessage,
    seq: u64,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    if misreferenced(&record, stored) && !reflects(&record, &stored.message().msg_id) {
        return Err(Fail::Drop(
            "Paykit wire 4.16: a response refers to its Allowance's proposal alone".into(),
        ));
    }
    if record.conflicted.is_none()
        && !reflects(&record, &stored.message().msg_id)
        && conflicts(&record, stored)
    {
        return keep_conflict(cx, target, stored, scopes);
    }
    // An answer never takes the response while a different one its inbox accepted earlier
    // is still kept to write: it conflicts with that one, so it is kept as the stop at once,
    // a rejection's stop included, and written into `conflicted` once the earlier one is
    // the record's response (Paykit spec 8.1, 8.3).
    if !reflects(&record, &stored.message().msg_id)
        && answered_before(
            &record,
            stored,
            answer_order(stored, seq).map_err(|_| Fail::corrupt())?,
            &step_answers_kept(cx, target, stored)?,
        )
    {
        return keep_conflict(cx, target, stored, scopes);
    }
    // The step never writes an answer into the record's empty response. The answer is kept
    // with the replies kept to place (`pr/`), with its place in the inbox's order, and
    // written by the placement after the read, once that read's stored entries are noted
    // (`se/`), in that order ([`Paykit::place_kept_allowance_responses`]): an earlier answer
    // kept or stored, one of the same read whose entry the client stored included, takes the
    // response first, and a later one conflicts with it ([`answered_before`]). A rejection's stop acts at once: an
    // app holding `allowance_execute` removes the Allowance from its consent list for good
    // in this commit, and [`Paykit::consents`] refuses payment while the rejection is kept.
    if is_allowance_response(stored.body())
        && stored.sender() == record.proposal.message().to
        && record.response.is_none()
        && !reflects(&record, &stored.message().msg_id)
    {
        keep_answer(cx, target, stored, seq, true)?;
        if matches!(stored.body(), Body::AllowanceRejection(_))
            && scopes.iter().any(|s| s == "allowance_execute")
        {
            let allowance_id = named_allowance(stored.body()).ok_or_else(Fail::corrupt)?;
            let record_key = cx.record_key(target)?;
            drop_consent(
                cx,
                &record_key,
                (stored.sender(), allowance_id),
                ConsentEnd::Rejected,
            )?;
        }
        return Ok(());
    }
    // Over a record whose view is divergent and unconfirmed, a write that adds no stop, a
    // proposal or a second acceptance the record only reflects, waits, and is written once
    // the user confirms the record (Paykit spec 6, 8.1 "Stored entries"). A proposal is
    // stored, its entry tried again at each read; an acceptance is kept with the replies
    // kept to place (`pr/`).
    if !reflects(&record, &stored.message().msg_id) {
        if let Some(held) = cx.record(target)? {
            if crate::divergent::step_write_waits(cx, &held, stored, seq, &|_| None)? {
                return match stored.body() {
                    Body::AllowanceProposal(_) => stored_entry(cx, stored),
                    _ => keep_answer(cx, target, stored, seq, false),
                };
            }
        }
    }
    if place(&mut record, stored.clone()) {
        cx.put_record(target, &record.to_json()?, Vec::new())?;
    }
    Ok(())
}

/// The answers of the Allowance record at `target` that the app keeps to write, from the
/// sender of `stored`, in the receive step: those kept before the record (`pr/`) and the
/// stored entries' not yet in it (`se/`), each read bound to its key, with its order.
fn step_answers_kept(
    cx: &ReceiveContext<'_>,
    target: &RecordTarget,
    stored: &StoredMessage,
) -> std::result::Result<Vec<(StoredMessage, AnswerOrder)>, Fail> {
    let own = cx.identity();
    let sender = stored.sender();
    let record = cx.record_key(target)?;
    let before = state::cx_scan::<KeptMessage>(cx, &key::kept_reply(&sender, &record, ""))?
        .into_iter()
        .map(|(_, kept)| kept)
        .collect();
    let notes = state::cx_scan::<StoredNote>(cx, &key::stored_notes_of(&sender))?;
    answers_kept(&own, named_allowance(stored.body()), before, notes).map_err(|_| Fail::corrupt())
}

/// The answers among `before`, the messages kept before one Allowance record, and `notes`,
/// the stored entries' notes of its counterparty with their keys, that are still to write
/// into the record of `allowance_id`, each with its order.
fn answers_kept(
    own: &[u8; 32],
    allowance_id: Option<&AllowanceId>,
    before: Vec<KeptMessage>,
    notes: Vec<(String, StoredNote)>,
) -> Result<Vec<(StoredMessage, AnswerOrder)>> {
    let mut kept = Vec::new();
    for message in before {
        let answer = message.stored(own)?;
        if is_allowance_response(answer.body()) {
            kept.push((answer, message.order()?));
        }
    }
    for (note_key, note) in notes {
        let answer = note.bound(&note_key, own)?;
        if !note.placed
            && is_allowance_response(answer.body())
            && named_allowance(answer.body()) == allowance_id
        {
            kept.push((answer, note.message.order()?));
        }
    }
    Ok(kept)
}

/// A second, different proposal or response, received (Paykit spec 8.3, "Conflicted"):
/// kept durably as the stop in the commit that passes the entry, before anything else is
/// done with it, with no write at the switch (`cf/<record key>`); the app writes it into the
/// record's `conflicted` by a later write of its own ([`Paykit::place_conflicts`]). The first
/// one kept stays: one record holds one `conflicted`. An executor's consent list drops the
/// Allowance for good in the same commit.
///
/// Security: the message passed check 10 and is checked against a record that reads and
/// binds, so only that counterparty's message under that Allowance ID is kept, under its
/// record's key; it is verified again whenever it is read back.
fn keep_conflict(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    stored: &StoredMessage,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let record_key = cx.record_key(target)?;
    let kept = state::cx_get::<KeptMessage>(cx, &key::conflict(&record_key))?.is_some();
    let consent = state::cx_get::<ConsentEntry>(cx, &key::consent(&record_key))?;
    let executes = scopes.iter().any(|s| s == "allowance_execute");
    for write in conflict_writes(&record_key, stored, kept, consent.as_ref(), executes)
        .map_err(|_| Fail::corrupt())?
    {
        receive::keep_local(cx, write);
    }
    receive::note_peer(cx, &stored.sender(), stored)
}

/// The writes a second, different proposal or response makes, the receive step's and a
/// later write's alike (Paykit spec 8.3, "Conflicted"): its stop (`cf/<record key>`) unless
/// one is kept already, the first one kept staying; and, at an app holding
/// `allowance_execute`, the consent list's entry removed for good, unless it is already.
fn conflict_writes(
    record_key: &[u8; 32],
    stored: &StoredMessage,
    kept: bool,
    consent: Option<&ConsentEntry>,
    executes: bool,
) -> Result<Vec<LocalWrite>> {
    let mut writes = Vec::new();
    if !kept {
        writes.push(state::local_put(
            &key::conflict(record_key),
            &KeptMessage::of(stored),
        )?);
    }
    let removed = matches!(
        consent,
        Some(ConsentEntry {
            consent: Consent::Removed { .. },
            ..
        })
    );
    if executes && !removed {
        let allowance_id = named_allowance(stored.body()).ok_or_else(PaykitError::corrupt)?;
        writes.push(state::local_put(
            &key::consent(record_key),
            &ConsentEntry::new(
                stored.sender(),
                allowance_id,
                Consent::Removed {
                    reason: ConsentEnd::Conflicted,
                },
            ),
        )?);
    }
    Ok(writes)
}

/// A counterparty's end (Paykit spec 8.3, "Ends reach the executor"): kept as the stop in
/// the commit that passes the entry, with no write at the switch.
///
/// One for an Allowance whose record the app holds ([`receive::holds_record`]: any content
/// but a delete, a value that doesn't read and one sealed under a scope key the app doesn't
/// hold yet included) always applies and is kept at once: keeping it needs only the
/// record's key, so the stop never waits for the value's key or a listing, and no give-up of
/// the entry can lose it. A value that reads is checked first: an end naming another
/// proposal than the record's isn't this Allowance's. One over a record that holds an end
/// or a rejection already is kept too, so the later write reflects its `msg_id` (Paykit wire
/// 4.16). A sealed value is checked once it opens ([`Paykit::place_allowance_stops`]).
///
/// Every distinct end is kept, each under its `msg_id` (`ae/<record key>/<msg_id>`), none in
/// place of another: over a record that doesn't read, none can be told yet to name what the
/// record holds, so an earlier end naming another proposal or acceptance never keeps a
/// later, right one out. Once the record reads, the first kept that names what it holds
/// ends the Allowance, and the others are reflected or dropped. Only the counterparty's
/// ends share a record's stops: the record key derives from the end's sender.
///
/// One for an Allowance the app holds no record of, or holds deleted, is new state, which
/// the spam rule gates, held while the copy of `allowances` may lack records, when the
/// absence decides nothing. A stop is kept under the bounds of the stops for records not
/// held, counted together with the payees' cancellations kept so
/// ([`receive::make_room_for_stop`]), unless its record is held without a value that reads,
/// or a stored message of the Allowance is kept to write ([`keeps_stored_message`]), and
/// goes [`KEPT_MESSAGE_MS`] after its acceptance while the record still doesn't hold the
/// response it names.
///
/// An executor's consent list drops the Allowance for good in the same commit only once the
/// end checks against a record that reads: one that may yet prove not to be this
/// Allowance's leaves the entry, and the kept stop alone stops payment meanwhile
/// ([`Paykit::consents`]).
fn keep_end(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    allowance_id: &AllowanceId,
    stored: &StoredMessage,
    seq: u64,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let refs = &stored.message().refs;
    let content = cx.record(target)?.map(|held| held.content);
    let held = receive::holds_record(content.as_ref());
    let record = content
        .as_ref()
        .and_then(RecordContent::value)
        .and_then(|value| bind_at(value, &cx.identity(), &sender, allowance_id));
    let mut checked = false;
    if let Some(record) = &record {
        if reflects(record, &stored.message().msg_id) {
            return Ok(());
        }
        if refs.first() != Some(&record.proposal.hash()) {
            return Err(Fail::Drop(
                "Paykit wire 4.16: an end refers to its Allowance's proposal".into(),
            ));
        }
        // Only an end the record shows ends the Allowance removes consent: the proposal's
        // withdrawal by its sender, naming the proposal alone, or an end naming the proposal
        // and the acceptance the record holds (Paykit wire 4.16). Any other is kept as the
        // stop only, which `record_end` judges and drops; consent stays.
        checked = record.end.is_none()
            && !rejected(record)
            && match refs.len() {
                1 => sender == record.proposal.sender(),
                2 => acceptance(record).is_some_and(|accepted| refs[1] == accepted.hash()),
                _ => false,
            };
    }
    if !held {
        receive::hold_while_unlisted(cx, RecordSide::Allowances)?;
        if !receive::admits_new_state(cx, &sender, scopes)? {
            return Ok(());
        }
    }
    let record_key = cx.record_key(target)?;
    let stop = key::allowance_end(&record_key, &stored.message().msg_id);
    if state::cx_get::<KeptMessage>(cx, &stop)?.is_none() {
        // A second end of the Allowance counts under the bounds whatever the record holds
        // (`receive::make_room_for_stop`).
        let second =
            !state::cx_scan::<KeptMessage>(cx, &key::allowance_ends(&record_key))?.is_empty();
        if second
            || (end_bounded(cx, content.as_ref(), stored)?
                && !keeps_stored_message(cx, &record_key)?)
        {
            let order = (stored.accepted_at().ok_or_else(Fail::corrupt)?, seq);
            receive::make_room_for_stop(cx, &sender, order)?;
        }
        receive::keep_local(cx, receive::local(&stop, &KeptMessage::taken(stored, seq))?);
    }
    if checked && scopes.iter().any(|s| s == "allowance_execute") {
        drop_consent(cx, &record_key, (sender, allowance_id), ConsentEnd::Ended)?;
    }
    receive::note_peer(cx, &sender, stored)
}

/// Whether the stop the counterparty's end `end` keeps is under the bounds of the stops for
/// records not held (Paykit spec 8.3, "Ends reach the executor"), judged on `content`, what
/// the app's copy holds at its Allowance record: one for an Allowance whose record the app
/// doesn't hold, absent or deleted, or whose record reads and doesn't hold the proposal and
/// acceptance the end names. One over a record that doesn't read or bind, or that reads and
/// holds what the end names, is the stop of a held Allowance: never counted, never evicted,
/// so no bound can lift it before the record shows the end.
pub(crate) fn end_bounded(
    cx: &ReceiveContext<'_>,
    content: Option<&RecordContent>,
    end: &StoredMessage,
) -> std::result::Result<bool, Fail> {
    if !receive::holds_record(content) {
        return Ok(true);
    }
    let Body::AllowanceEnd(body) = end.body() else {
        return Err(Fail::corrupt());
    };
    let Some(record) = content
        .and_then(RecordContent::value)
        .and_then(|value| bind_at(value, &cx.identity(), &end.sender(), &body.allowance_id))
    else {
        return Ok(false);
    };
    Ok(!names_held(&record, &end.message().refs))
}

/// Whether `record` holds the proposal `refs` names first and, when it names a second, the
/// acceptance it names: an end so named is this Allowance's, over the record that holds it.
fn names_held(record: &AllowanceRecord, refs: &[[u8; 32]]) -> bool {
    refs.first() == Some(&record.proposal.hash())
        && (refs.len() == 1
            || acceptance(record).is_some_and(|accepted| refs.get(1) == Some(&accepted.hash())))
}

/// Remove the Allowance whose record is `record_key` from the consent list for good, in the
/// step, unless it is removed already.
fn drop_consent(
    cx: &mut ReceiveContext<'_>,
    record_key: &[u8; 32],
    (counterparty, allowance_id): ([u8; 32], &AllowanceId),
    reason: ConsentEnd,
) -> std::result::Result<(), Fail> {
    let entry = key::consent(record_key);
    // Read bound to its key (`state::Stored`): an entry of another Allowance is a corrupt
    // store, never written over.
    let removed = matches!(
        state::cx_get::<ConsentEntry>(cx, &entry)?,
        Some(ConsentEntry {
            consent: Consent::Removed { .. },
            ..
        })
    );
    if !removed {
        let gone = ConsentEntry::new(counterparty, allowance_id, Consent::Removed { reason });
        receive::keep_local(cx, receive::local(&entry, &gone)?);
    }
    Ok(())
}

/// An `executor_notice` from an own app, at an app holding `allowance_execute` (Paykit spec
/// 8.3): the Allowance leaves its consent list for good, if the list holds it. It changes
/// nothing else.
pub(crate) fn notice_effect(
    cx: &mut ReceiveContext<'_>,
    notice: &ExecutorNotice,
) -> std::result::Result<(), Fail> {
    let reason = match notice.reason {
        NoticeReason::Ended => ConsentEnd::Ended,
        NoticeReason::HandedOff => ConsentEnd::HandedOff,
    };
    // Only an entry the list holds is removed: a notice naming any other record key adds
    // nothing, so own apps' notices can't grow the list. The entry is read bound to its key,
    // so the Allowance it names is the one whose record the notice names.
    let Some(entry) = state::cx_get::<ConsentEntry>(cx, &key::consent(&notice.allowance_record))?
    else {
        return Ok(());
    };
    let allowance_id = entry.allowance_id().map_err(|_| Fail::corrupt())?;
    drop_consent(
        cx,
        &notice.allowance_record,
        (entry.counterparty, &allowance_id),
        reason,
    )
}

/// The `allowances` record the stored entry's message `note` is to be written into (`se/`,
/// `stored.rs`): `None` for a note placed already, or for a message of no Allowance.
///
/// The note is bound to its key, `key`: a note naming another sender or `msg_id` than its
/// key, or whose message isn't that one, is corrupt state.
fn note_target(key: &[u8], note: &StoredNote, own: &[u8; 32]) -> Result<Option<RecordTarget>> {
    let Some(crate::state::StateKey::StoredNote { sender, msg_id }) =
        crate::state::StateKey::parse(key)
    else {
        return Err(PaykitError::corrupt());
    };
    let stored = note.message.stored(own)?;
    if note.sender != sender
        || note.msg_id != msg_id
        || stored.sender() != sender
        || stored.message().msg_id != msg_id
    {
        return Err(PaykitError::corrupt());
    }
    if note.placed {
        return Ok(None);
    }
    Ok(named_allowance(stored.body()).map(|id| allowance_target(&stored.sender(), id)))
}

/// Whether the app keeps a stored entry's message of the Allowance whose record is `record`,
/// still to write into it (`se/`, `stored.rs`), read in the receive step. Such a message can
/// open the record or give it the response an end names, so the end's stop is neither
/// dropped by the bounds ([`receive::make_room_for_stop`]) nor given up while it is kept
/// (Paykit spec 8.1, 8.3).
pub(crate) fn keeps_stored_message(
    cx: &ReceiveContext<'_>,
    record: &[u8; 32],
) -> std::result::Result<bool, Fail> {
    let own = cx.identity();
    for (name, note) in state::cx_scan::<StoredNote>(cx, key::STORED_NOTES)? {
        if let Some(target) =
            note_target(name.as_bytes(), &note, &own).map_err(|_| Fail::corrupt())?
        {
            if cx.record_key(&target)? == *record {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether the kept stop `kept`, the counterparty's end, may go: an Allowance the app holds
/// no record of, or whose record doesn't name the response the end refers to yet, 30 days
/// from its acceptance on the service time verified (Paykit spec 8.1, 9). Its callers keep
/// it meanwhile while the copy of `allowances` may lack records, or while a stored message
/// of the Allowance is kept to write ([`Paykit::keeps_stored_message`]). Every kept message
/// carries its acceptance time: one without is a corrupt store, never one past its window.
fn stale(kept: &StoredMessage, service: Option<u64>) -> Result<bool> {
    let at = kept.accepted_at().ok_or_else(PaykitError::corrupt)?;
    Ok(crate::stored::past_window(at, service))
}

impl Paykit {
    pub(crate) fn require_allowance(&self) -> Result<()> {
        if self.holds("allowance") {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("allowance".into()).into())
        }
    }

    /// The `allowances` record this app holds at `record`, bound to its key (Paykit wire
    /// 5.4); `None` when it holds none, or one that doesn't bind or isn't under the fixed
    /// policy.
    pub(crate) async fn held_allowance(
        &self,
        record: &[u8; 32],
    ) -> Result<Option<AllowanceRecord>> {
        let Some(held) = Box::pin(self.app.shared_record(record)).await? else {
            return Ok(None);
        };
        let collection = Collection::Allowances;
        if held.collection != collection.name() || held.policy != collection.fixed_policy() {
            return Ok(None);
        }
        let Some(value) = held.content.value() else {
            return Ok(None);
        };
        let Ok(bound) = AllowanceRecord::read(value, &self.identity()) else {
            return Ok(None);
        };
        let target = allowance_target(&bound.counterparty, &bound.allowance_id);
        Ok((self.app.record_key(&target)? == *record).then_some(bound))
    }

    /// [`keeps_stored_message`], outside the receive step.
    async fn keeps_stored_message(&self, record: &[u8; 32]) -> Result<bool> {
        let own = self.identity();
        for (rest, note) in Box::pin(self.local_scan::<StoredNote>(key::STORED_NOTES)).await? {
            let key = format!("{}{rest}", key::STORED_NOTES);
            if let Some(target) = note_target(key.as_bytes(), &note, &own)? {
                if self.app.record_key(&target)? == *record {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Whether this app keeps the counterparty's end of the Allowance at `record` as its
    /// stop.
    pub(crate) async fn allowance_stopped(&self, record: &[u8; 32]) -> Result<bool> {
        // Each read bound to its key, as a restore reads it (`state::Stored`).
        Ok(
            !Box::pin(self.local_scan::<KeptMessage>(&key::allowance_ends(record)))
                .await?
                .is_empty(),
        )
    }

    /// Whether this app keeps a conflicting proposal or response of the Allowance at
    /// `record` as its stop (`cf/`), not yet in the record.
    pub(crate) async fn conflict_kept(&self, record: &[u8; 32]) -> Result<bool> {
        Box::pin(self.allowance_stop(&key::conflict(record))).await
    }

    /// Whether a stop is kept at `stop`, read bound to its key as a restore reads it
    /// (`state::Stored`): one that doesn't verify, isn't of its family's kinds, or names
    /// another record is a corrupt store, never no stop.
    async fn allowance_stop(&self, stop: &str) -> Result<bool> {
        Ok(Box::pin(self.local_get::<KeptMessage>(stop))
            .await?
            .is_some())
    }

    /// Whether a `missed` item's mark (`mc`) is set. It holds only `true`: any other value
    /// is a corrupt store (`state::Stored`).
    async fn consents_unconfirmed(&self) -> Result<bool> {
        Ok(self
            .local_get::<bool>(key::MISSED_CONSENTS)
            .await?
            .is_some())
    }

    /// Propose an Allowance to `to` (Paykit spec 8.3, Paykit wire 4.15): this identity takes
    /// `role`, `to` the other. The proposal is signed under a fresh Allowance ID, and its
    /// `allowances` record written in the batch that queues it, then it is sent. The
    /// recipient must admit this identity (Paykit spec 1). A proposal grants nothing: only
    /// its acceptance does.
    ///
    /// # Errors
    ///
    /// [`Refusal::ScopeMissing`] without `allowance`; [`Refusal::NotNow`] while sends to `to`
    /// are held, at an app holding `contacts`: the deny list holds it, or the copy of the
    /// lists isn't known after a read tried now (Paykit spec 8.1, "Blocked counterparties"),
    /// with nothing signed, written or queued; [`PaykitError::Lib`] with `Validation` for
    /// terms that break Paykit wire 4.15, with nothing sent; the client's errors. A
    /// failure that leaves the batch kept (the network, `429`, `503`) is no error: the
    /// proposal is reported sent, [`MessageDelivery::Kept`], and goes with the app's next
    /// write or read.
    pub async fn propose_allowance(
        &mut self,
        to: [u8; 32],
        role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Result<SentAllowance> {
        Box::pin(self.propose_allowance_inner(to, role, terms)).await
    }

    async fn propose_allowance_inner(
        &mut self,
        to: [u8; 32],
        role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Result<SentAllowance> {
        self.refresh_own()?;
        self.require_allowance()?;
        Box::pin(self.refuse_while_sends_held(&to)).await?;
        let allowance_id = AllowanceId::generate()?;
        let proposer_role = match role {
            AllowanceRole::Allower => ProposerRole::Allower,
            AllowanceRole::Allowee => ProposerRole::Allowee,
        };
        // The writer reads back what it writes by the recipient's rules (Paykit wire 4.15).
        let body = AllowanceProposal {
            allowance_id: allowance_id.clone(),
            proposer_role,
            terms,
        }
        .to_body()?;
        let signed = self.sign(to, Kind::AllowanceProposal, Vec::new(), body)?;
        let msg_id = signed.msg_id().to_owned();
        let target = allowance_target(&to, &allowance_id);
        let record = opened(to, self.own_copy(&signed)?).ok_or_else(PaykitError::corrupt)?;
        if stores_unqueued(
            None,
            &record,
            std::slice::from_ref(&msg_id),
            &self.identity(),
            self.app_id(),
        ) {
            return Err(PaykitError::corrupt());
        }
        let write = self
            .app
            .record_op(&target, Some(&record.to_json()?), Vec::new())
            .await?;
        let mut tx = Tx::default();
        self.note_peer(&mut tx, &to)?;
        let batch = OpBatch::new()
            .entry(write)
            .queue(signed)
            .locals(tx.into_writes());
        let record_key = self.app.record_key(&target)?;
        let delivery = match Box::pin(self.app.submit_batch(batch)).await {
            Ok(_) => Box::pin(self.delivered(&msg_id)).await?,
            Err(error) => match error.kept_batch() {
                Some(batch_id) => MessageDelivery::Kept { batch_id },
                None => return Err(error.into()),
            },
        };
        Ok(SentAllowance {
            record: record_key,
            allowance_id,
            msg_id,
            delivery,
        })
    }

    /// Send each own crossing end recorded while sends to its counterparty were held (`eo/`,
    /// [`OwedEnd`]) once they aren't, from the signed plaintext its record stores, with the
    /// client's `queue_stored`, and drop the mark after it (Paykit spec 8.1, "Blocked
    /// counterparties"; 8.3, "Ending"). A mark stays while sends to the counterparty are held,
    /// or while its record isn't held as a value that reads and binds. One whose record is
    /// held as deleted goes with nothing sent: no record holds the end to send; so does one
    /// whose Allowance is conflicted by then, in the record or as a kept stop, for which
    /// nothing is sent. A mark over a record that reads and binds without that end of this
    /// app's is a corrupt store: the mark is written in the batch that records the end, and
    /// the client keeps an end the app has seen against any later write (Switch spec 8.2), so
    /// no writer leaves it so, and dropping it would leave the counterparty without the end. As a message queued again from its
    /// record it goes only until 30 days after its `created_at`, less the clock tolerance
    /// (Paykit spec 8.3, "Ending"; 7.2, "What a restore loses"): one the client refuses for
    /// good past that is neither sent nor told, as at a restore's re-send
    /// ([`Paykit::queue_stored_again`]), and the end in the record stands either way. A store
    /// failure is the call's error, and the mark stays. The mark was read bound to its key
    /// (`state::Stored`).
    pub(crate) async fn send_owed_ends(&mut self) -> Result<()> {
        let own = self.identity();
        let app_id = self.app_id();
        for (_, owed) in Box::pin(self.local_scan::<OwedEnd>(key::OWED_ENDS)).await? {
            let mark = key::owed_end(&owed.record);
            let Some(held) = Box::pin(self.held_allowance(&owed.record)).await? else {
                let deleted = self
                    .app
                    .shared_record(&owed.record)
                    .await?
                    .is_some_and(|held| held.content == RecordContent::Deleted);
                if deleted {
                    Box::pin(self.local_delete(&mark)).await?;
                }
                continue;
            };
            let end = held.end.as_ref().filter(|end| {
                end.message().msg_id == owed.msg_id
                    && end.sender() == own
                    && end.message().from.app_id == app_id
            });
            let Some(end) = end else {
                return Err(PaykitError::corrupt());
            };
            // Nothing is sent for a conflicted Allowance (Paykit spec 8.3, "Conflicted"), as
            // a restore's re-send sends nothing for one: the conflict ended it on both sides.
            if held.conflicted.is_some() || Box::pin(self.conflict_kept(&owed.record)).await? {
                Box::pin(self.local_delete(&mark)).await?;
                continue;
            }
            if Box::pin(self.sends_held(&held.counterparty)).await? {
                continue;
            }
            Box::pin(self.queue_stored_again(end)).await?;
            Box::pin(self.local_delete(&mark)).await?;
        }
        Ok(())
    }

    /// Accept the Allowance proposed to this identity at `record` (Paykit spec 8.3, Paykit
    /// wire 4.16): the acceptance is recorded, then sent. Only the proposal's recipient
    /// answers it, once. Returns the acceptance's `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], also for a record held as deleted; [`Refusal::AllowanceEnded`]
    /// once it ended, or while this app keeps the counterparty's end;
    /// [`Refusal::AllowanceConflicted`] for a conflicted one, for good; [`Refusal::NotNow`]
    /// while the shared records aren't listed, the record waits for its scope key, or sends
    /// to the counterparty are held, as [`Paykit::propose_allowance`] says, with nothing
    /// changed; [`Refusal::Divergent`] while the record was seen divergent and the user
    /// hasn't confirmed it, or messages kept over it wait to be written, naming it, with
    /// nothing signed, written or sent: an acceptance adds no stop (Paykit spec 6);
    /// [`Refusal::Usage`] for a proposal this identity sent or one answered already; as
    /// [`Paykit::propose_allowance`].
    pub async fn accept_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        Box::pin(self.answer_allowance(record, Kind::AllowanceAcceptance)).await
    }

    /// Reject the Allowance proposed to this identity at `record` (Paykit spec 8.3, Paykit
    /// wire 4.16), as [`Paykit::accept_allowance`] accepts it. A rejected Allowance has no
    /// end. A rejection is a stop: it goes over a record seen divergent as over any other
    /// (Paykit spec 6).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], also for a record held as deleted; [`Refusal::AllowanceEnded`]
    /// once it ended, or while this app keeps the counterparty's end;
    /// [`Refusal::AllowanceConflicted`] for a conflicted one, for good; [`Refusal::NotNow`]
    /// while the shared records aren't listed, the record waits for its scope key, or sends
    /// to the counterparty are held, as [`Paykit::propose_allowance`] says, with nothing
    /// changed; [`Refusal::Usage`] for a proposal this identity sent or one answered already;
    /// as [`Paykit::propose_allowance`]. Never [`Refusal::Divergent`].
    pub async fn reject_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        Box::pin(self.answer_allowance(record, Kind::AllowanceRejection)).await
    }

    /// End the Allowance at `record` (Paykit spec 8.3 "Ending", Paykit wire 4.16): an
    /// accepted one, by either side, naming the proposal and the acceptance; or, before any
    /// answer, the proposal's withdrawal by this identity, its sender. The end is recorded,
    /// then sent, with an `executor_notice` to the executor the record's grant names when
    /// that is another own app (Paykit wire 4.17). The records stay as history. Returns the
    /// end's `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`], also for a record held as deleted; [`Refusal::AllowanceEnded`]
    /// once it ended, or while this app keeps the counterparty's end; [`Refusal::Rejected`]
    /// for a rejected one; [`Refusal::AllowanceConflicted`] for a conflicted one, for good;
    /// [`Refusal::NotNow`] while the shared records aren't listed or the record waits for its
    /// scope key; never for the deny list: an end stops payment, so it is recorded and sent
    /// at once to a counterparty blocked or not, as a payee's cancellation is (Paykit spec
    /// 8.1, "Blocked counterparties"); [`Refusal::Usage`] for a proposal to this identity not
    /// yet answered, which is rejected instead; as [`Paykit::propose_allowance`].
    pub async fn end_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        Box::pin(self.answer_allowance(record, Kind::AllowanceEnd)).await
    }

    /// Record and send this identity's acceptance, rejection or end of the Allowance at
    /// `record`, `kind`, built again from a fresh read after the record changed meanwhile.
    async fn answer_allowance(&mut self, record: &[u8; 32], kind: Kind) -> Result<String> {
        self.refresh_own()?;
        Box::pin(self.records_known()).await?;
        self.require_allowance()?;
        // A record that can't be opened yet may hold a response or an end: nothing is
        // answered or ended over it until it opens (Paykit spec 8.3, one response).
        Box::pin(self.refuse_while_awaiting_key(record)).await?;
        let own = self.identity();
        let mut tries = 0;
        loop {
            let held = self
                .held_allowance(record)
                .await?
                .ok_or(Refusal::NotFound)?;
            // Nothing is sent for a conflicted Allowance (Paykit spec 8.3, "Conflicted").
            if held.conflicted.is_some() || Box::pin(self.conflict_kept(record)).await? {
                return Err(Refusal::AllowanceConflicted.into());
            }
            if held.end.is_some() || Box::pin(self.allowance_stopped(record)).await? {
                return Err(Refusal::AllowanceEnded.into());
            }
            // A proposal, acceptance or rejection to a counterparty whose sends are held is
            // refused, which moves no money in either role (Paykit spec 8.1, "Blocked
            // counterparties"). The user's end is never held: it stops payment, so it is
            // recorded and sent at once, as a payee's cancellation is (Paykit spec 8.1,
            // "What stops or settles a payment still goes"; 8.3).
            if kind != Kind::AllowanceEnd {
                Box::pin(self.refuse_while_sends_held(&held.counterparty)).await?;
            }
            // An acceptance adds no stop: it waits for the user's confirmation of a record
            // seen divergent (Paykit spec 6). A rejection and an end are stops and go.
            if kind == Kind::AllowanceAcceptance {
                Box::pin(self.refuse_while_write_waits(record)).await?;
            }
            let proposal = held.proposal.hash();
            let to_here = held.proposal.message().to == own;
            let refs = match kind {
                Kind::AllowanceEnd => {
                    match (&held.response, acceptance(&held)) {
                        (_, Some(accepted)) => vec![proposal, accepted.hash()],
                        (Some(_), None) => return Err(Refusal::Rejected.into()),
                        (None, None) if !to_here => vec![proposal],
                        (None, None) => return Err(Refusal::Usage(
                            "Paykit wire 4.16: the proposal's recipient rejects it, never ends it \
                             before answering"
                                .into(),
                        )
                        .into()),
                    }
                }
                _ if !to_here => {
                    return Err(Refusal::Usage(
                        "Paykit wire 4.16: only the proposal's recipient answers it".into(),
                    )
                    .into())
                }
                _ if held.response.is_some() => {
                    return Err(
                        Refusal::Usage("Paykit wire 5.4: the proposal is answered".into()).into(),
                    )
                }
                _ => vec![proposal],
            };
            let body = AllowanceReply {
                allowance_id: held.allowance_id.clone(),
                executor_app_id: None,
            }
            .to_body(kind)?;
            let signed = self.sign(held.counterparty, kind, refs, body)?;
            let msg_id = signed.msg_id().to_owned();
            let mut next = held.clone();
            place(&mut next, self.own_copy(&signed)?);
            if !reflects_stored(&next, &msg_id) {
                return Err(PaykitError::corrupt());
            }
            let mut queue = vec![signed];
            let mut locals = Vec::new();
            if kind == Kind::AllowanceEnd {
                self.end_notice(&held, record, &mut queue, &mut locals)?;
            }
            let target = allowance_target(&held.counterparty, &held.allowance_id);
            let notices: Vec<String> = queue
                .iter()
                .skip(1)
                .map(|m| m.msg_id().to_owned())
                .collect();
            match Box::pin(self.submit_with_record(&target, &next, queue, locals, None)).await {
                Ok(()) => {
                    Box::pin(self.delivered(&msg_id)).await?;
                    // A notice not delivered now goes with the next flush.
                    for notice in notices {
                        let _ = Box::pin(self.app.flush_message(&notice)).await;
                    }
                    return Ok(msg_id);
                }
                Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                    tries += 1;
                    if tries >= WRITE_TRIES {
                        return Err(conflict(error));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Write `next` at `target` and queue `queue`, with `locals`, in one batch: the
    /// messages go out only if the write is applied. A batch kept for a switch not
    /// answering now counts as written: it goes with the app's next write or read. After
    /// `409 conflict` the record is read again. This and [`Paykit::propose_allowance`] are
    /// the only writes of an Allowance record: a write storing a message this app signed
    /// that isn't queued with it is refused before anything is sent ([`stores_unqueued`]),
    /// but for a crossing end owed while sends to the counterparty are held, `owed`, whose
    /// mark goes in the same batch.
    async fn submit_with_record(
        &mut self,
        target: &RecordTarget,
        next: &AllowanceRecord,
        queue: Vec<SignedMessage>,
        mut locals: Vec<LocalWrite>,
        owed: Option<&OwedEnd>,
    ) -> Result<()> {
        let before = Box::pin(self.held_allowance(&self.app.record_key(target)?)).await?;
        let mut queued: Vec<String> = queue.iter().map(|m| m.msg_id().to_owned()).collect();
        // A crossing end owed while sends are held counts as queued: its mark goes in this
        // batch, and its message goes from the record later ([`Paykit::send_owed_ends`]).
        if let Some(owed) = owed {
            locals.push(state::local_put(&key::owed_end(&owed.record), owed)?);
            queued.push(owed.msg_id.clone());
        }
        if stores_unqueued(
            before.as_ref(),
            next,
            &queued,
            &self.identity(),
            self.app_id(),
        ) {
            return Err(PaykitError::corrupt());
        }
        let write = self
            .app
            .record_op(target, Some(&next.to_json()?), Vec::new())
            .await?;
        let mut batch = OpBatch::new().entry(write).locals(locals);
        for message in queue {
            batch = batch.queue(message);
        }
        match Box::pin(self.app.submit_batch(batch)).await {
            Ok(_) => Ok(()),
            Err(error) if error.kept_batch().is_some() => Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    Box::pin(self.app.read_record(target)).await?;
                }
                Err(error.into())
            }
        }
    }

    /// The `executor_notice` an end of the Allowance `held`, at `record`, sends to its
    /// executor (Paykit spec 8.3, Paykit wire 4.17), added to `queue`, with the local write
    /// naming the app it is wrapped to; and, at an app holding `allowance_execute`, its own
    /// consent list's removal, added to `locals`. No notice when the record holds no current
    /// grant, or names this app.
    fn end_notice(
        &self,
        held: &AllowanceRecord,
        record: &[u8; 32],
        queue: &mut Vec<SignedMessage>,
        locals: &mut Vec<LocalWrite>,
    ) -> Result<()> {
        let own = self.identity();
        if self.holds("allowance_execute") {
            locals.push(consent_removed(held, record, ConsentEnd::Ended)?);
        }
        let Some((grant, _)) = held.current_grant(&own) else {
            return Ok(());
        };
        if grant.app_id == self.app_id() {
            return Ok(());
        }
        let body = ExecutorNotice {
            reason: NoticeReason::Ended,
            allowance_record: *record,
        }
        .to_body()?;
        let notice = self.sign(own, Kind::ExecutorNotice, Vec::new(), body)?;
        let wrapped = NoticeTarget {
            msg_id: notice.msg_id().to_owned(),
            app_id: grant.app_id,
        };
        locals.push(state::local_put(
            &key::notice_target(notice.msg_id()),
            &wrapped,
        )?);
        queue.push(notice);
        Ok(())
    }

    /// Every Allowance this identity holds a record of, from the `allowances` records this
    /// app holds (Paykit spec 6, 8.3). A record that doesn't bind is left out, as a reader
    /// doesn't act on it (Paykit wire 5.4). A record held as deleted over which this app
    /// keeps a conflicting proposal shows as conflicted, from that proposal, with no record
    /// behind the view, so the host can show it (Paykit spec 8.3, "Ending").
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the records aren't listed since a restore, a `missed` item
    /// or another certificate, or while one of them waits for its scope key; a store
    /// failure, also one reading a single record: the list is never partial.
    pub async fn allowances(&self) -> Result<Vec<AllowanceView>> {
        Box::pin(self.allowances_inner()).await
    }

    async fn allowances_inner(&self) -> Result<Vec<AllowanceView>> {
        Box::pin(self.refuse_while_unlisted()).await?;
        let mut out = Vec::new();
        for held in Box::pin(self.app.records_of(Collection::Allowances.name())).await? {
            // A record waiting for its scope key would be left out: the list would be
            // partial, so it is refused for now instead.
            if crate::hold::awaits_key(&held.content) {
                return Err(crate::hold::value_awaits_key());
            }
            if let Some(record) = Box::pin(self.held_allowance(&held.key)).await? {
                out.push(Box::pin(self.allowance_view(held.key, &record)).await?);
            } else if let Some(view) =
                Box::pin(self.deleted_conflict_view(&held.key, &held.content)).await?
            {
                out.push(view);
            }
        }
        Ok(out)
    }

    /// The Allowance at `record`, as [`Paykit::allowances`] shows it.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; as [`Paykit::allowances`].
    pub async fn allowance(&self, record: &[u8; 32]) -> Result<AllowanceView> {
        Box::pin(self.allowance_inner(record)).await
    }

    async fn allowance_inner(&self, record: &[u8; 32]) -> Result<AllowanceView> {
        Box::pin(self.refuse_while_unlisted()).await?;
        Box::pin(self.refuse_while_awaiting_key(record)).await?;
        if let Some(held) = Box::pin(self.held_allowance(record)).await? {
            return Box::pin(self.allowance_view(*record, &held)).await;
        }
        let content = self
            .app
            .shared_record(record)
            .await?
            .map(|held| held.content)
            .ok_or(Refusal::NotFound)?;
        Ok(self
            .deleted_conflict_view(record, &content)
            .await?
            .ok_or(Refusal::NotFound)?)
    }

    /// The Allowance whose record this app holds as deleted and over which it keeps a
    /// conflicting proposal or response as the stop (Paykit spec 8.3, "Conflicted",
    /// "Ending"), as the host shows it: conflicted, from the kept message alone. No record is
    /// behind this view: the counterparty and Allowance ID are the kept message's, nothing
    /// is consented, no executor is named, and nothing is answered, ended or paid under it.
    /// A kept proposal gives the role and terms; a kept response, the counterparty's answer
    /// to this identity's proposal, gives neither, since the proposal isn't held. `None` for
    /// a record not held as deleted, or with no such stop.
    async fn deleted_conflict_view(
        &self,
        record: &[u8; 32],
        content: &RecordContent,
    ) -> Result<Option<AllowanceView>> {
        if *content != RecordContent::Deleted {
            return Ok(None);
        }
        // Read bound to its key (`state::Stored`): a message of the Allowance this record key
        // derives from.
        let Some(kept) = self
            .local_get::<KeptMessage>(&key::conflict(record))
            .await?
        else {
            return Ok(None);
        };
        let stored = kept.stored(&self.identity())?;
        let (allowance_id, role, terms, proposed_here) = match stored.body() {
            Body::AllowanceProposal(proposal) => {
                let role = match proposal.proposer_role {
                    ProposerRole::Allower => AllowanceRole::Allowee,
                    ProposerRole::Allowee => AllowanceRole::Allower,
                };
                (
                    proposal.allowance_id.clone(),
                    Some(role),
                    Some(proposal.terms.clone()),
                    false,
                )
            }
            Body::AllowanceAcceptance(reply) | Body::AllowanceRejection(reply) => {
                (reply.allowance_id.clone(), None, None, true)
            }
            _ => return Ok(None),
        };
        Ok(Some(AllowanceView {
            record: *record,
            counterparty: stored.sender(),
            allowance_id,
            role,
            proposed_here,
            terms,
            status: AllowanceStatus::Conflicted,
            executor_app_id: None,
            consented: false,
            blocked: Box::pin(self.blocked_as_known(&stored.sender())).await?,
        }))
    }

    async fn allowance_view(
        &self,
        record: [u8; 32],
        held: &AllowanceRecord,
    ) -> Result<AllowanceView> {
        let own = self.identity();
        let Body::AllowanceProposal(proposal) = held.proposal.body() else {
            return Err(PaykitError::corrupt());
        };
        let proposed_here = held.proposal.sender() == own;
        let role = match (proposal.proposer_role, proposed_here) {
            (ProposerRole::Allower, true) | (ProposerRole::Allowee, false) => {
                AllowanceRole::Allower
            }
            _ => AllowanceRole::Allowee,
        };
        Ok(AllowanceView {
            record,
            counterparty: held.counterparty,
            allowance_id: held.allowance_id.clone(),
            role: Some(role),
            proposed_here,
            terms: Some(proposal.terms.clone()),
            status: status(
                held,
                Box::pin(self.allowance_stopped(&record)).await?,
                Box::pin(self.conflict_kept(&record)).await?,
            ),
            executor_app_id: held.current_grant(&own).map(|(grant, _)| grant.app_id),
            consented: Box::pin(self.consents(&record)).await?,
            blocked: Box::pin(self.blocked_as_known(&held.counterparty)).await?,
        })
    }

    /// Whether the copy of the deny list holds `pubky`, for the views: `None` while the copy
    /// isn't known ([`Paykit::deny_copy_known`]).
    async fn blocked_as_known(&self, pubky: &[u8; 32]) -> Result<Option<bool>> {
        if !Box::pin(self.deny_copy_known()).await? {
            return Ok(None);
        }
        Ok(Some(Box::pin(self.denied_as_last_read(pubky)).await?))
    }

    /// [`Refusal::NotNow`] while the record at `record` waits for its scope key: the app
    /// can't say yet what it holds ([`crate::hold::value_awaits_key`]).
    async fn refuse_while_awaiting_key(&self, record: &[u8; 32]) -> Result<()> {
        if let Some(held) = Box::pin(self.app.shared_record(record)).await? {
            if crate::hold::awaits_key(&held.content) {
                return Err(crate::hold::value_awaits_key());
            }
        }
        Ok(())
    }

    /// The counterparties of every Allowance not ended or rejected that this app holds a
    /// record of, for the re-sends after a restore or a `missed` item.
    pub(crate) async fn open_allowance_counterparties(&self) -> Result<Vec<[u8; 32]>> {
        let mut out = Vec::new();
        for held in Box::pin(self.app.records_of(Collection::Allowances.name())).await? {
            if let Some(record) = Box::pin(self.held_allowance(&held.key)).await? {
                let open = matches!(
                    status(
                        &record,
                        Box::pin(self.allowance_stopped(&held.key)).await?,
                        Box::pin(self.conflict_kept(&held.key)).await?,
                    ),
                    AllowanceStatus::Proposed | AllowanceStatus::Accepted
                );
                if open {
                    out.push(record.counterparty);
                }
            }
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------------------
    // The consent list

    /// Whether this app's consent list holds the Allowance at `record`, confirmed since any
    /// restore or `missed` item (Paykit spec 8.3, "Executor"). None is confirmed while a
    /// `missed` item's mark (`mc`) waits for [`Paykit::confirm_consents_again`], nor while
    /// this app keeps a counterparty's end, rejection or conflicting message of the Allowance
    /// as a stop, whatever the entry says: a stop the receive step kept, or one a stored entry
    /// kept, stops payment before the record shows it. Nor is one whose record isn't held
    /// now as a value that reads and binds and shows no end, rejection or conflict, read
    /// here, so a record that changed outside a receive counts at once: one held as deleted
    /// pays nothing, and neither does one whose current write the app ignores (Switch spec
    /// 8.2; Paykit spec 8.3, "Executor", "Ending"): the user's confirmation of such a write
    /// comes with the executor, so until then it pays nothing.
    pub(crate) async fn consents(&self, record: &[u8; 32]) -> Result<bool> {
        if Box::pin(self.consents_unconfirmed()).await?
            || Box::pin(self.allowance_stopped(record)).await?
            || Box::pin(self.conflict_kept(record)).await?
        {
            return Ok(false);
        }
        let Some(shared) = Box::pin(self.app.shared_record(record)).await? else {
            return Ok(false);
        };
        if shared.flagged {
            return Ok(false);
        }
        match Box::pin(self.held_allowance(record)).await? {
            Some(held) if held.end.is_none() && !rejected(&held) && held.conflicted.is_none() => {}
            _ => return Ok(false),
        }
        let Some(held) = self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?
        else {
            return Ok(false);
        };
        if Box::pin(self.rejection_kept(record, &held.counterparty)).await? {
            return Ok(false);
        }
        Ok(held.consent
            == Consent::Held {
                confirm_again: false,
            })
    }

    /// Whether this app keeps a rejection from `counterparty` of the Allowance at `record`
    /// to write (`pr/`), not yet in the record: the receive step keeps every answer that
    /// would take the record's empty response for the placement after the read, and a
    /// rejection is a stop from when it is kept.
    async fn rejection_kept(&self, record: &[u8; 32], counterparty: &[u8; 32]) -> Result<bool> {
        let own = self.identity();
        for (_, kept) in
            Box::pin(self.local_scan::<KeptMessage>(&key::kept_reply(counterparty, record, "")))
                .await?
        {
            if matches!(kept.stored(&own)?.body(), Body::AllowanceRejection(_)) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// After a `missed` item, which sets `mc` in the commit that passes it: mark every
    /// Allowance on the consent list to be confirmed by the user again, as a restore marks
    /// it ([`Consent::confirm_again`]), and clear the mark, in one commit. An end may be among
    /// the entries the service deleted (Paykit spec 7.2, 8.3). Run after each receive; a
    /// commit that fails leaves the mark, under which nothing is consented, for the next.
    pub(crate) async fn confirm_consents_again(&mut self) -> Result<()> {
        if !Box::pin(self.consents_unconfirmed()).await? {
            return Ok(());
        }
        let mut tx = Tx::default();
        for (rest, entry) in Box::pin(self.local_scan::<ConsentEntry>(key::CONSENTS)).await? {
            let again = entry.consent.confirm_again();
            if again != entry.consent {
                tx.put(&format!("{}{rest}", key::CONSENTS), &entry.with(again))?;
            }
        }
        tx.delete(key::MISSED_CONSENTS);
        Box::pin(self.local_commit(tx)).await
    }

    /// Drop each executor notice's target (`en/<msg_id>`) once the client reports its notice
    /// accepted or failed: the target only tells the outbox which app the notice is wrapped
    /// to, so it is needed no longer. One whose notice the client doesn't know, as after a
    /// restore, stays, since that notice may still be sealed and sent. Each target is read
    /// bound to its key (`state::Stored`), so one naming another notice is a corrupt store,
    /// returned by the receive, rather than the hook's silent "send to no one". Run after
    /// each receive.
    pub(crate) async fn sweep_notice_targets(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        for (rest, target) in Box::pin(self.local_scan::<NoticeTarget>(key::NOTICE_TARGETS)).await?
        {
            if matches!(
                Box::pin(self.app.delivery(&target.msg_id)).await?,
                Some(Delivery::Accepted { .. } | Delivery::Failed { .. })
            ) {
                tx.delete(&format!("{}{rest}", key::NOTICE_TARGETS));
            }
        }
        Box::pin(self.local_commit(tx)).await
    }

    /// Remove from the consent list, for good, every Allowance whose record this app holds
    /// ended, rejected or conflicted, or whose conflicting message it keeps as the stop: the
    /// executor drops it once it sees that, whoever wrote it (Paykit spec 8.3). A kept end
    /// alone removes nothing: it may yet prove not to be this Allowance's, and
    /// [`Paykit::consents`] refuses payment while it is kept. Nor does a record held as
    /// deleted: its entry stays, and only payment is refused, so a delete a faulty own app
    /// made loses nothing the user consented to. Run after each receive.
    pub(crate) async fn sweep_consents(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        for (rest, entry) in Box::pin(self.local_scan::<ConsentEntry>(key::CONSENTS)).await? {
            let record = consent_record(&rest)?;
            if !matches!(entry.consent, Consent::Held { .. }) {
                continue;
            }
            let reason = match Box::pin(self.held_allowance(&record)).await? {
                Some(held) if held.conflicted.is_some() => ConsentEnd::Conflicted,
                Some(held) if held.end.is_some() => ConsentEnd::Ended,
                Some(held) if rejected(&held) => ConsentEnd::Rejected,
                _ if Box::pin(self.conflict_kept(&record)).await? => ConsentEnd::Conflicted,
                _ => continue,
            };
            tx.put(
                &key::consent(&record),
                &entry.with(Consent::Removed { reason }),
            )?;
        }
        Box::pin(self.local_commit(tx)).await
    }

    // ---------------------------------------------------------------------------------
    // Later writes of the app's own

    /// Record every kept end whose Allowance record this app holds, each as a later write
    /// of its own with its `executor_notice`, the kept stop dropped in the same batch (Paykit
    /// spec 8.3). Run after each receive. One whose record holds an end or a rejection
    /// already only goes; one whose record isn't held, or doesn't hold the acceptance the
    /// end names yet, stays kept, 30 days from its acceptance, and longer while a stored
    /// message of the Allowance is kept to write; one that can't be written now stays for
    /// the next run. Nothing runs while the copy of `allowances` may lack records.
    pub(crate) async fn place_allowance_stops(&mut self) -> Result<()> {
        if !self.holds("allowance")
            || Box::pin(self.scope_keys_awaited()).await?.is_some()
            || Box::pin(self.side_unlisted(RecordSide::Allowances)).await?
        {
            return Ok(());
        }
        let own = self.identity();
        // Read bound to its key (`state::Stored`). The ends of one Allowance go in the order
        // their inbox accepted them (`records::KeptOrder`): once one ends it, the later ones
        // are reflected or dropped.
        let mut ends = Vec::new();
        for (rest, kept) in Box::pin(self.local_scan::<KeptMessage>(key::ALLOWANCE_ENDS)).await? {
            let end = kept.stored(&own)?;
            let record = rest.split_once('/').map(|(record, _)| record.to_owned());
            ends.push((record, kept.order()?, rest, end));
        }
        ends.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (_, _, rest, end) in ends {
            let stop = format!("{}{rest}", key::ALLOWANCE_ENDS);
            let Some(allowance_id) = named_allowance(end.body()).cloned() else {
                return Err(PaykitError::corrupt());
            };
            let target = allowance_target(&end.sender(), &allowance_id);
            let record = self.app.record_key(&target)?;
            let content = self
                .app
                .shared_record(&record)
                .await?
                .map(|held| held.content);
            match content {
                // No record held, or a delete: the stop goes 30 days after its acceptance.
                None | Some(RecordContent::Deleted) => {
                    if stale(&end, self.service_time())?
                        && !Box::pin(self.keeps_stored_message(&record)).await?
                    {
                        Box::pin(self.local_delete(&stop)).await?;
                    }
                }
                Some(RecordContent::Value(_)) => {
                    if let Some(held) = Box::pin(self.held_allowance(&record)).await? {
                        // A write the switch refuses now is tried at the next run; the stop
                        // stands meanwhile. Any other failure is the call's error.
                        written(
                            Box::pin(self.record_end(&target, record, held, end, &stop)).await,
                        )?;
                    }
                }
                // A value that doesn't read or waits for its key: never written over; the
                // stop stands.
                Some(_) => {}
            }
        }
        Box::pin(self.place_conflicts()).await
    }

    /// Write every kept conflicting proposal or response into its Allowance record's
    /// `conflicted`, each as a later write of its own with the kept stop dropped in the same
    /// batch (Paykit spec 8.3, "Conflicted"); one whose record holds a `conflicted` already
    /// only goes, since the record then shows the conflict for good (Paykit wire 5.4, Seen
    /// roles). One whose record isn't held as a value that reads and binds, or that can't be
    /// written now, stays: the Allowance shows conflicted meanwhile, and nothing is paid or
    /// sent under it. One over a record held as deleted, a proposal there, stays for good and
    /// is written nowhere: a record can't hold `conflicted` without its proposal, and one
    /// written back since with that same proposal doesn't conflict with it, so it isn't
    /// cleared then either (Paykit spec 8.3, "Conflicted", "Ending").
    async fn place_conflicts(&mut self) -> Result<()> {
        let own = self.identity();
        for (rest, kept) in Box::pin(self.local_scan::<KeptMessage>(key::CONFLICTS)).await? {
            let stop = format!("{}{rest}", key::CONFLICTS);
            // Read bound to its key (`state::Stored`).
            let message = kept.stored(&own)?;
            let Some(allowance_id) = named_allowance(message.body()).cloned() else {
                return Err(PaykitError::corrupt());
            };
            let target = allowance_target(&message.sender(), &allowance_id);
            let record = self.app.record_key(&target)?;
            let Some(held) = Box::pin(self.held_allowance(&record)).await? else {
                continue;
            };
            let locals = vec![LocalWrite::Delete {
                key: stop.as_bytes().to_vec(),
            }];
            if held.conflicted.is_some() {
                Box::pin(self.app.protocol_commit(&locals)).await?;
                continue;
            }
            // A kept message the record doesn't conflict with stays the stop for good: a
            // proposal taken over a delete, its record since written back with that same
            // proposal, is still conflicted, and `conflicted` can't hold the record's own
            // proposal (Paykit spec 8.3, "Conflicted", "Ending").
            if !conflicts(&held, &message) {
                continue;
            }
            let mut next = held;
            place(&mut next, message.clone());
            if !reflects_stored(&next, &message.message().msg_id) {
                // It no longer conflicts with what the record holds, or doesn't fit its
                // rules: the stop stands, as the stop of the conflict it was.
                continue;
            }
            // A write the switch refuses now is tried at the next run; the stop stands
            // meanwhile. Any other failure is the call's error.
            written(
                Box::pin(self.submit_with_record(&target, &next, Vec::new(), locals, None)).await,
            )?;
        }
        Ok(())
    }

    /// Record the counterparty's end `end`, kept at `stop`, in `held`, the Allowance record
    /// at `record`.
    async fn record_end(
        &mut self,
        target: &RecordTarget,
        record: [u8; 32],
        held: AllowanceRecord,
        end: StoredMessage,
        stop: &str,
    ) -> Result<()> {
        let refs = end.message().refs.clone();
        let proposal = held.proposal.hash();
        if refs.first() != Some(&proposal) {
            return Box::pin(self.local_delete(stop)).await;
        }
        // An end over a record that holds an end or a rejection changes nothing but is
        // reflected by its `msg_id` (Paykit wire 4.16), with the stop dropped in the same
        // batch.
        if held.end.is_some() || rejected(&held) {
            let mut next = held;
            if !place(&mut next, end) {
                return Box::pin(self.local_delete(stop)).await;
            }
            let locals = vec![LocalWrite::Delete {
                key: stop.as_bytes().to_vec(),
            }];
            return self
                .submit_with_record(target, &next, Vec::new(), locals, None)
                .await;
        }
        let conflicted = held.conflicted.is_some() || Box::pin(self.conflict_kept(&record)).await?;
        let mut next = held.clone();
        let mut queue = Vec::new();
        let mut owed = None;
        place(&mut next, end.clone());
        if !reflects_stored(&next, &end.message().msg_id) {
            match acceptance(&held) {
                // The proposal's withdrawal crossing the acceptance this identity sent: the
                // record can't hold both, so this identity ends the Allowance itself, naming
                // the two, and the withdrawal is reflected (Paykit wire 4.16).
                // Nothing is sent for a conflicted Allowance (Paykit spec 8.3): the
                // withdrawal is only reflected.
                Some(_) if conflicted && refs == [proposal] => {
                    next = held.clone();
                    place(&mut next, end.clone());
                }
                Some(accepted) if refs == [proposal] && end.sender() == held.proposal.sender() => {
                    let body = AllowanceReply {
                        allowance_id: held.allowance_id.clone(),
                        executor_app_id: None,
                    }
                    .to_body(Kind::AllowanceEnd)?;
                    let own_end = self.sign(
                        held.counterparty,
                        Kind::AllowanceEnd,
                        vec![proposal, accepted.hash()],
                        body,
                    )?;
                    next = held.clone();
                    place(&mut next, self.own_copy(&own_end)?);
                    place(&mut next, end.clone());
                    // While sends to the counterparty are held (Paykit spec 8.1, "Blocked
                    // counterparties"), the end is recorded all the same, with its executor
                    // notice to the own apps, so every own app finds the Allowance ended and
                    // a batch built on the record's earlier version fails; only its message
                    // to the counterparty waits, owed (`eo/`), sent from the record once
                    // sends to it aren't held (Paykit spec 8.3, "Ending").
                    if Box::pin(self.sends_held(&held.counterparty)).await? {
                        owed = Some(OwedEnd {
                            record,
                            msg_id: own_end.msg_id().to_owned(),
                        });
                    } else {
                        queue.push(own_end);
                    }
                }
                // An end naming an acceptance the record doesn't hold, none yet or another
                // than the one it holds, waits for it, 30 days from its acceptance.
                accepted
                    if refs.len() == 2
                        && !rejected(&held)
                        && accepted.is_none_or(|a| refs.get(1) != Some(&a.hash())) =>
                {
                    if stale(&end, self.service_time())?
                        && !Box::pin(self.keeps_stored_message(&record)).await?
                    {
                        Box::pin(self.local_delete(stop)).await?;
                    }
                    return Ok(());
                }
                // Any other end isn't one of this Allowance (Paykit wire 4.16).
                _ => return Box::pin(self.local_delete(stop)).await,
            }
        }
        let mut locals = Vec::new();
        self.end_notice(&held, &record, &mut queue, &mut locals)?;
        locals.push(LocalWrite::Delete {
            key: stop.as_bytes().to_vec(),
        });
        let sent: Vec<String> = queue.iter().map(|m| m.msg_id().to_owned()).collect();
        Box::pin(self.submit_with_record(target, &next, queue, locals, owed.as_ref())).await?;
        // The write went through with the messages queued in its batch: one not delivered
        // now goes with the next flush, and the write's outcome doesn't wait for it.
        for msg_id in sent {
            let _ = Box::pin(self.app.flush_message(&msg_id)).await;
        }
        Ok(())
    }

    /// Place every Allowance answer kept (`pr/`) in the order its inbox accepted it
    /// ([`AnswerOrder`]), then its key, each by [`Paykit::place_kept_allowance_response`]:
    /// the answers the receive step kept for a record it holds, which it never writes into
    /// the record's response itself, those kept before their record, and those waiting over
    /// a divergent one. Run after each receive, after the read's stored entries are noted,
    /// so an earlier answer stored (`se/`) is seen ([`answered_before`]) whatever keeps it
    /// from its record.
    pub(crate) async fn place_kept_allowance_responses(&mut self) -> Result<()> {
        let own = self.identity();
        let mut kept = Vec::new();
        for (rest, message) in Box::pin(self.local_scan::<KeptMessage>(key::KEPT_REPLIES)).await? {
            // Read bound to its key, as a restore reads it: a message that fails it is a
            // corrupt store, so the receive fails rather than drop, give up or place it.
            let answer = message.stored(&own)?;
            if is_allowance_response(answer.body()) {
                kept.push((message.order()?, rest, answer));
            }
        }
        kept.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for (order, rest, answer) in kept {
            let kept_key = format!("{}{rest}", key::KEPT_REPLIES);
            Box::pin(self.place_kept_allowance_response(&kept_key, answer, order)).await?;
        }
        Ok(())
    }

    /// Place the kept acceptance or rejection `reply`, kept at `kept_key` at its order `order`
    /// by the receive step, over its Allowance record or before it was held, or while it
    /// waited over it for the user's confirmation, once the app holds that record, as a
    /// later write of its own with the kept copy dropped (Paykit spec 8.1). Nothing runs
    /// while the copy of `allowances` may lack records. One whose record isn't held, or is
    /// held deleted, goes 30 days from its acceptance with no notice: its sender keeps it no
    /// longer either. One over a held record goes through the stored entries' later write
    /// of an Allowance message ([`Paykit::place_allowance_later`]) and its one rule
    /// ([`crate::stored::later_write`]), as a payer's kept reply does: one not written now,
    /// its record sealed under a scope key not held yet, a value that doesn't read or bind,
    /// or a write the switch refuses, is kept, and given up past its window with the host's
    /// notice ([`crate::NoticeKind::MessageGivenUp`]) in the commit that drops it. A write
    /// that can't be built or sent now, for a store failure or a switch that doesn't answer,
    /// is the call's error, and the reply stays kept. The reply was read bound to its key
    /// (`state::Stored`): one kept under another sender, record or `msg_id` is a corrupt
    /// store.
    pub(crate) async fn place_kept_allowance_response(
        &mut self,
        kept_key: &str,
        reply: StoredMessage,
        order: AnswerOrder,
    ) -> Result<()> {
        if Box::pin(self.side_unlisted(RecordSide::Allowances)).await? {
            return Ok(());
        }
        let Some(allowance_id) = named_allowance(reply.body()).cloned() else {
            return Err(PaykitError::corrupt());
        };
        let accepted_at = reply.accepted_at().ok_or_else(PaykitError::corrupt)?;
        let record = self
            .app
            .record_key(&allowance_target(&reply.sender(), &allowance_id))?;
        let content = self
            .app
            .shared_record(&record)
            .await?
            .map(|held| held.content);
        if !receive::holds_record(content.as_ref()) {
            if crate::stored::past_window(accepted_at, self.service_time()) {
                Box::pin(self.local_delete(kept_key)).await?;
            }
            return Ok(());
        }
        let drop_kept = LocalWrite::Delete {
            key: kept_key.as_bytes().to_vec(),
        };
        let placed = self
            .place_allowance_later(reply.clone(), order, vec![drop_kept])
            .await?;
        if crate::stored::later_write(placed, accepted_at, self.service_time())
            == crate::stored::LaterWrite::GiveUp
        {
            self.give_up(kept_key, &reply).await?;
        }
        Ok(())
    }

    /// A conflicting message met at a later write, kept as the receive step keeps it
    /// ([`conflict_writes`]), in the commit of `locals`.
    async fn keep_conflict_later(
        &mut self,
        record: &[u8; 32],
        stored: &StoredMessage,
        mut locals: Vec<LocalWrite>,
    ) -> Result<()> {
        let kept = Box::pin(self.conflict_kept(record)).await?;
        let consent = self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?;
        locals.extend(conflict_writes(
            record,
            stored,
            kept,
            consent.as_ref(),
            self.holds("allowance_execute"),
        )?);
        Ok(Box::pin(self.app.protocol_commit(&locals)).await?)
    }

    /// A stored entry's proposal, acceptance or rejection (`stored.rs`), read from the feed
    /// at the order `order` ([`AnswerOrder`]), written into its Allowance record by a later
    /// write of the app's own ([`Paykit::place_allowance_later`]).
    pub(crate) async fn place_allowance_note(
        &mut self,
        stored: StoredMessage,
        order: AnswerOrder,
    ) -> Result<Placed> {
        Box::pin(self.place_allowance_later(stored, order, Vec::new())).await
    }

    /// The answers of the Allowance record at `record` that the app keeps to write, from
    /// the sender of `stored` ([`answers_kept`]), outside the receive step.
    async fn answers_kept(
        &self,
        record: &[u8; 32],
        stored: &StoredMessage,
    ) -> Result<Vec<(StoredMessage, AnswerOrder)>> {
        let own = self.identity();
        let sender = stored.sender();
        let before =
            Box::pin(self.local_scan::<KeptMessage>(&key::kept_reply(&sender, record, "")))
                .await?
                .into_iter()
                .map(|(_, kept)| kept)
                .collect();
        let prefix = key::stored_notes_of(&sender);
        let notes = Box::pin(self.local_scan::<StoredNote>(&prefix))
            .await?
            .into_iter()
            .map(|(rest, note)| (format!("{prefix}{rest}"), note))
            .collect();
        answers_kept(&own, named_allowance(stored.body()), before, notes)
    }

    /// The later write of an Allowance's proposal, acceptance or rejection kept to write, a
    /// stored entry's (`se/`) or an answer kept before its record (`pr/`), with `locals` in
    /// the commit or batch that places it: the one write both paths share. A proposal may
    /// open the record, an answer needs it held.
    ///
    /// - An answer never takes the response while a different one its inbox accepted
    ///   before it, by `order`, is still kept to write ([`answered_before`]): it conflicts
    ///   with that one, so it is done, kept as the conflict's stop (`cf/`) in the commit of
    ///   `locals`, a rejection's stop with it, and goes into `conflicted` once the earlier
    ///   one is the record's response (Paykit spec 8.1, 8.3).
    /// - It waits while the copy of `allowances` may lack records ([`Placed::Waits`]), never
    ///   given up meanwhile, whatever else holds.
    /// - It isn't written now ([`Placed::Not`]) while the current epoch's `allowance` key is
    ///   missing or the record waits for its key, over a value that doesn't read or bind,
    ///   never written over, for an answer whose record is absent or deleted (given up past
    ///   its window with the notice, as Stored entries says), or when the switch
    ///   refuses the write ([`crate::stored::submitted`]): tried again, and given up once
    ///   [`crate::KEPT_MESSAGE_MS`] have passed since its acceptance (Paykit spec 8.1).
    /// - One the record reflects already, or that makes the Allowance conflicted, is done:
    ///   the conflict is kept as its stop (`cf/`) in the commit of `locals`.
    /// - A failure of the app's own store, or a switch that doesn't answer, is the call's
    ///   error, never a message not written.
    async fn place_allowance_later(
        &mut self,
        stored: StoredMessage,
        order: AnswerOrder,
        locals: Vec<LocalWrite>,
    ) -> Result<Placed> {
        let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
            return Err(PaykitError::corrupt());
        };
        if Box::pin(self.side_unlisted(RecordSide::Allowances)).await? {
            return Ok(Placed::Waits);
        }
        if self
            .app
            .scope_key(self.app.data_epoch(), "allowance")
            .is_none()
        {
            return Ok(Placed::Not);
        }
        let counterparty = stored.sender();
        let target = allowance_target(&counterparty, &allowance_id);
        let record = self.app.record_key(&target)?;
        let next = match self
            .app
            .shared_record(&record)
            .await?
            .map(|held| held.content)
        {
            // A record absent is opened only by a message that may open it, a proposal, as
            // the request path's later write does (`Paykit::place_note`).
            None => match stored.body() {
                Body::AllowanceProposal(_) => opened(counterparty, stored),
                _ => None,
            },
            // A proposal over a delete is conflicted, kept as the stop with nothing written
            // at the key, as at its first read (Paykit spec 8.3, "Ending"); an answer waits
            // for a record.
            Some(RecordContent::Deleted) => match stored.body() {
                Body::AllowanceProposal(_) => {
                    Box::pin(self.keep_conflict_later(&record, &stored, locals)).await?;
                    return Ok(Placed::Done);
                }
                _ => None,
            },
            Some(content) if crate::hold::awaits_key(&content) => return Ok(Placed::Not),
            Some(RecordContent::Value(_)) => match Box::pin(self.held_allowance(&record)).await? {
                // A response to no proposal of this Allowance is dropped, as at receive.
                Some(held)
                    if misreferenced(&held, &stored)
                        && !reflects(&held, &stored.message().msg_id) =>
                {
                    Box::pin(self.app.protocol_commit(&locals)).await?;
                    return Ok(Placed::Done);
                }
                Some(held) if held.conflicted.is_none() && conflicts(&held, &stored) => {
                    Box::pin(self.keep_conflict_later(&record, &stored, locals)).await?;
                    return Ok(Placed::Done);
                }
                // An answer never takes the response while a different one its inbox
                // accepted earlier is still kept to write, as at receive: it conflicts with
                // that one, so its stop is kept at once, a rejection's included, and it is
                // written into `conflicted` once the earlier one is the record's response.
                Some(held)
                    if !reflects(&held, &stored.message().msg_id)
                        && answered_before(
                            &held,
                            &stored,
                            order,
                            &Box::pin(self.answers_kept(&record, &stored)).await?,
                        ) =>
                {
                    Box::pin(self.keep_conflict_later(&record, &stored, locals)).await?;
                    return Ok(Placed::Done);
                }
                // A proposal or an acceptance waits over a divergent record until the user
                // confirms it, as at receive; a rejection goes.
                Some(held)
                    if !reflects(&held, &stored.message().msg_id)
                        && Box::pin(self.divergent_write_waits(&record, &stored, Some(order)))
                            .await? =>
                {
                    return Ok(Placed::Not);
                }
                Some(mut held) => {
                    if !place(&mut held, stored) {
                        Box::pin(self.app.protocol_commit(&locals)).await?;
                        return Ok(Placed::Done);
                    }
                    Some(held)
                }
                None => None,
            },
            Some(_) => None,
        };
        let Some(next) = next else {
            return Ok(Placed::Not);
        };
        written(Box::pin(self.submit_with_record(&target, &next, Vec::new(), locals, None)).await)
    }
}

/// What a later write of an Allowance record came to, by the rule every later write follows
/// ([`crate::stored::submitted`]): written, refused by the switch and so not written now, or
/// not sent now for want of a key. A failure of the app's own store, a switch that doesn't
/// answer, or a write that can't be built is the error, never a write not made.
fn written(result: Result<()>) -> Result<Placed> {
    match result {
        Ok(()) => Ok(Placed::Done),
        Err(PaykitError::Client(error)) => crate::stored::submitted::<()>(Err(error)),
        Err(error) => Err(error),
    }
}

/// The messages of the Allowance record `record` this app signed, `app_id` of identity `own`,
/// that it queues again after a restore (Paykit spec 7.2, "What a restore loses"; 8.3): each
/// one the record stores to be sent, since every message this app stores in an Allowance
/// record it also queues in that batch, or, for a crossing end recorded while sends to the
/// counterparty were held, marks owed in it ([`stores_unqueued`]), while the counterparty
/// hasn't answered it and no later stop covers it. The proposal goes while the record holds
/// no response and no end; an acceptance or a rejection while it holds no end; an end, a
/// stop, always. Nothing goes
/// for a conflicted Allowance (Paykit spec 8.3, "Conflicted"); the caller leaves out one
/// whose conflict or counterparty's end this app keeps as a stop not in the record yet.
pub(crate) fn own_allowance_to_send_again<'a>(
    record: &'a AllowanceRecord,
    own: &[u8; 32],
    app_id: &[u8; 16],
) -> Vec<&'a StoredMessage> {
    if record.conflicted.is_some() {
        return Vec::new();
    }
    let ended = record.end.is_some();
    let candidates = [
        (Some(&record.proposal), record.response.is_none() && !ended),
        (record.response.as_ref(), !ended),
        (record.end.as_ref(), true),
    ];
    candidates
        .into_iter()
        .filter_map(|(stored, send)| stored.filter(|_| send))
        .filter(|stored| stored.sender() == *own && stored.cert().app_id() == *app_id)
        .collect()
}

/// Whether `next` would store a message this app signed, as `own` and `app_id`, that
/// `before` doesn't hold and `queued` doesn't name: a write that stores without sending,
/// which no path makes (Paykit spec 8.3, "Recorded first"). Every message of this app's in
/// an Allowance record is written in the batch that queues it, or, for a crossing end
/// recorded while sends to the counterparty are held, in the batch that marks it owed
/// (`eo/`, which `queued` then names; Paykit spec 8.3, "Ending"), so one signed by this app
/// there is one to be sent: the re-send after a restore can rely on that.
fn stores_unqueued(
    before: Option<&AllowanceRecord>,
    next: &AllowanceRecord,
    queued: &[String],
    own: &[u8; 32],
    app_id: [u8; 16],
) -> bool {
    let held = before.map(stored_ids).unwrap_or_default();
    std::iter::once(&next.proposal)
        .chain(&next.response)
        .chain(&next.end)
        .chain(&next.conflicted)
        .filter(|m| m.sender() == *own && m.message().from.app_id == app_id)
        .map(|m| &m.message().msg_id)
        .any(|id| !held.contains(id) && !queued.contains(id))
}

/// Whether `record` holds the message `msg_id` in a member, not only among the others.
fn reflects_stored(record: &AllowanceRecord, msg_id: &str) -> bool {
    stored_ids(record).iter().any(|id| id == msg_id)
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::receive::KEPT_MESSAGE_MS;
    use crate::testkit::{object, Party};

    fn terms() -> serde_json::Value {
        json!({
            "asset": "btc",
            "per_payment_amount": null,
            "period_limits": [],
            "lifetime_amount_limit": "0.10",
            "active_from": null,
            "expires_at": null,
            "allowed_payment_endpoint_identifiers": null
        })
    }

    const ID: &str = "d0efffb7-191b-4709-b6c7-e343af5500b4";

    /// Bob's proposal to alice as allowee, alice's record of it, and the two parties.
    fn proposed() -> (Party, Party, AllowanceRecord) {
        let alice = Party::new("alice", "wallet", &["pay", "allowance", "contacts"]);
        let bob = Party::new("bob", "shop", &["request", "allowance"]);
        let proposal = bob.message(
            &alice,
            "paykit.allowance_proposal",
            vec![],
            object(json!({ "allowance_id": ID, "proposer_role": "allowee", "terms": terms() })),
        );
        let record = opened(bob.id(), proposal).unwrap();
        (alice, bob, record)
    }

    /// `message`, which `from` signed, as `to`'s record stores it: with no acceptance time.
    fn as_read(from: &Party, to: &Party, message: &StoredMessage) -> StoredMessage {
        StoredMessage::new(
            from.id(),
            from.cert.signed(),
            message.plaintext().clone(),
            None,
            &to.id(),
        )
        .unwrap()
    }

    fn answer(from: &Party, to: &Party, kind: &str, refs: Vec<[u8; 32]>) -> StoredMessage {
        from.message(to, kind, refs, object(json!({ "allowance_id": ID })))
    }

    #[test]
    fn test_allowances_place_stores_an_answer_once() {
        let (alice, bob, mut record) = proposed();
        let accepted = answer(
            &alice,
            &bob,
            "paykit.allowance_acceptance",
            vec![record.proposal.hash()],
        );
        // Placed by alice's own app: her record holds it as she sent it.
        let accepted = as_read(&alice, &bob, &accepted);
        assert!(place(&mut record, accepted.clone()));
        assert!(reflects_stored(&record, &accepted.message().msg_id));
        assert_eq!(status(&record, false, false), AllowanceStatus::Accepted);
        assert!(record.terms_hash().is_some());
        // Placed again, nothing changes.
        assert!(!place(&mut record, accepted));
    }

    #[test]
    fn test_allowances_place_puts_a_second_answer_in_conflicted() {
        let (alice, bob, mut record) = proposed();
        let own = |m: StoredMessage| as_read(&alice, &bob, &m);
        let proposal = record.proposal.hash();
        let accepted = own(answer(
            &alice,
            &bob,
            "paykit.allowance_acceptance",
            vec![proposal],
        ));
        let rejected = own(answer(
            &alice,
            &bob,
            "paykit.allowance_rejection",
            vec![proposal],
        ));
        assert!(place(&mut record, accepted.clone()));
        assert!(conflicts(&record, &rejected));
        assert!(place(&mut record, rejected.clone()));
        // A rejection after the acceptance conflicts: it goes into `conflicted`, and the
        // Allowance is conflicted for good (Paykit spec 8.3).
        assert_eq!(
            record.conflicted.as_ref().map(StoredMessage::hash),
            Some(rejected.hash())
        );
        assert_eq!(status(&record, false, false), AllowanceStatus::Conflicted);
        // A re-sent copy of the acceptance is the acceptance itself.
        assert!(!conflicts(&record, &accepted));
        // A third response is reflected only: one record holds one `conflicted`.
        let third = own(answer(
            &alice,
            &bob,
            "paykit.allowance_acceptance",
            vec![proposal],
        ));
        assert!(place(&mut record, third.clone()));
        assert!(!reflects_stored(&record, &third.message().msg_id));
        assert!(reflects(&record, &third.message().msg_id));
    }

    #[test]
    fn test_allowances_stores_unqueued_refuses_an_own_message_not_sent() {
        let (alice, bob, mut record) = proposed();
        let proposal = record.proposal.hash();
        let own = alice.id();
        let accepted = answer(&alice, &bob, "paykit.allowance_acceptance", vec![proposal]);
        let accepted = as_read(&alice, &bob, &accepted);
        let before = record.clone();
        assert!(place(&mut record, accepted.clone()));
        let app = accepted.message().from.app_id;
        // Queued with the write: fine. Not queued: a store-only write, refused.
        let id = accepted.message().msg_id.clone();
        assert!(!stores_unqueued(Some(&before), &record, &[id], &own, app));
        assert!(stores_unqueued(Some(&before), &record, &[], &own, app));
        // Held already, or another app's message: no send owed by this write.
        assert!(!stores_unqueued(Some(&record), &record, &[], &own, app));
        assert!(!stores_unqueued(Some(&before), &record, &[], &own, [9; 16]));
    }

    /// Every write of an Allowance record goes through the two functions that check it
    /// queues the messages it stores ([`stores_unqueued`]): a store-only path added later
    /// fails here.
    #[test]
    fn test_allowances_record_writes_only_through_the_queuing_paths() {
        let source = include_str!("allowances.rs");
        let (code, _) = source.split_at(source.find("#[cfg(test)]").unwrap());
        assert_eq!(code.matches(".record_op(").count(), 2);
        assert_eq!(code.matches("put_record(").count(), 2);
        assert_eq!(code.matches("stores_unqueued(").count(), 3);
    }

    #[test]
    fn test_allowances_conflicts_not_for_a_first_answer_or_an_end() {
        let (alice, bob, record) = proposed();
        let proposal = record.proposal.hash();
        let accepted = answer(&alice, &bob, "paykit.allowance_acceptance", vec![proposal]);
        let accepted = as_read(&alice, &bob, &accepted);
        // A first response doesn't conflict, nor does the record's own proposal.
        assert!(!conflicts(&record, &accepted));
        assert!(!conflicts(&record, &record.proposal));
    }

    #[test]
    fn test_allowances_place_end_after_acceptance_is_stored() {
        let (alice, bob, mut record) = proposed();
        let own = as_read(
            &alice,
            &bob,
            &answer(
                &alice,
                &bob,
                "paykit.allowance_acceptance",
                vec![record.proposal.hash()],
            ),
        );
        place(&mut record, own.clone());
        let end = answer(
            &bob,
            &alice,
            "paykit.allowance_end",
            vec![record.proposal.hash(), own.hash()],
        );
        assert!(place(&mut record, end.clone()));
        assert!(reflects_stored(&record, &end.message().msg_id));
        assert_eq!(status(&record, false, false), AllowanceStatus::Ended);
    }

    #[test]
    fn test_allowances_place_reflects_a_withdrawal_crossing_the_acceptance() {
        let (alice, bob, mut record) = proposed();
        let own = as_read(
            &alice,
            &bob,
            &answer(
                &alice,
                &bob,
                "paykit.allowance_acceptance",
                vec![record.proposal.hash()],
            ),
        );
        place(&mut record, own);
        // The proposal's withdrawal names the proposal alone: the record can't hold it
        // beside the acceptance (Paykit wire 4.16), so it is only reflected.
        let withdrawal = answer(
            &bob,
            &alice,
            "paykit.allowance_end",
            vec![record.proposal.hash()],
        );
        assert!(place(&mut record, withdrawal.clone()));
        assert!(!reflects_stored(&record, &withdrawal.message().msg_id));
        assert!(reflects(&record, &withdrawal.message().msg_id));
        assert!(record.to_json().is_ok());
    }

    #[test]
    fn test_allowances_status_ended_while_a_stop_is_kept() {
        let (_, _, record) = proposed();
        assert_eq!(status(&record, false, false), AllowanceStatus::Proposed);
        assert_eq!(status(&record, true, false), AllowanceStatus::Ended);
    }

    #[test]
    fn test_allowances_bind_at_refuses_another_allowance() {
        let (alice, bob, record) = proposed();
        let value = record.to_json().unwrap();
        let id = AllowanceId::parse(ID).unwrap();
        assert!(bind_at(&value, &alice.id(), &bob.id(), &id).is_some());
        assert!(bind_at(&value, &alice.id(), &alice.id(), &id).is_none());
        let other = AllowanceId::parse("123e4567-e89b-42d3-a456-426614174000").unwrap();
        assert!(bind_at(&value, &alice.id(), &bob.id(), &other).is_none());
        assert!(bind_at(b"{}", &alice.id(), &bob.id(), &id).is_none());
    }

    #[test]
    fn test_allowances_stale_after_thirty_days_on_service_time() {
        let (_, _, record) = proposed();
        let at = record.proposal.accepted_at().unwrap();
        assert!(!stale(&record.proposal, None).unwrap());
        assert!(!stale(&record.proposal, Some(at + KEPT_MESSAGE_MS - 1)).unwrap());
        assert!(stale(&record.proposal, Some(at + KEPT_MESSAGE_MS)).unwrap());
    }

    #[test]
    fn test_allowances_stale_without_its_acceptance_time_is_corrupt() {
        let (alice, _, record) = proposed();
        let mut value = serde_json::to_value(KeptMessage::of(&record.proposal)).unwrap();
        value["accepted_at"] = Value::Null;
        let kept: KeptMessage = serde_json::from_value(value).unwrap();
        let message = kept.stored(&alice.id()).unwrap();
        assert!(message.accepted_at().is_none());
        assert!(stale(&message, Some(u64::MAX)).is_err());
    }
}
