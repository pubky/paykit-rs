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
//!   `msg_id` only, such as an acceptance crossing the proposal's withdrawal.
//! - **Receiving:** a proposal opens the record, gated by the spam rule; an acceptance or a
//!   rejection goes into the proposer's record, or is kept until the app holds it (Paykit
//!   spec 8.1, "Replies before their record"). Every branch holds its entry while the app's
//!   copy of `allowances` isn't listed yet, never deciding from a record's absence (Paykit
//!   spec 8.1, "Records not listed yet").
//! - **The stop:** a counterparty's `allowance_end` is kept in the app's own state in the
//!   commit that passes its entry, with no write at the switch (`ae/`). From then on nothing
//!   is done under the Allowance, whatever the record says, until the record holds an end or
//!   a rejection: the app writes the end there by a later write of its own, with the
//!   `executor_notice`. An end that names its proposal alone, crossing the acceptance this
//!   identity sent, can't go into a record holding that acceptance: the app ends the
//!   Allowance itself, naming both, so the record shows it ended.
//! - **The consent list** (`ac/`): the Allowances this app, as executor, may pay under. An
//!   Allowance seen ended or rejected, or named by an `executor_notice`, is removed for
//!   good: a removed entry never comes back. A restore marks every entry to be confirmed
//!   again, since the backup may predate an end.
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
    LocalWrite, OpBatch, ReceiveContext, RecordContent, RecordTarget, SignedMessage,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::http::ErrorCode;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::hold::RecordSide;
use crate::paykit::Paykit;
use crate::receive::{self, Fail, KEPT_MESSAGE_MS};
use crate::records::{conflict, KeptMessage};
use crate::requests::MessageDelivery;
use crate::state::{self, key, Tx};
use crate::stored::Placed;

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
    /// This identity's role.
    pub role: AllowanceRole,
    /// Whether this identity proposed it.
    pub proposed_here: bool,
    /// The proposed terms.
    pub terms: AllowanceTerms,
    /// Where it stands.
    pub status: AllowanceStatus,
    /// The executor app the record's current grant names, if it holds one that verifies.
    pub executor_app_id: Option<[u8; 16]>,
    /// Whether this app's consent list holds it, confirmed since any restore: an executor
    /// pays under an Allowance only then (Paykit spec 8.3).
    pub consented: bool,
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
    /// Removed for good: the Allowance ended, was rejected or handed off.
    Removed {
        /// Why.
        reason: ConsentEnd,
    },
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
}

/// The executor app an `executor_notice` is wrapped to (`en/<msg_id>`), kept with the batch
/// that queues it; it names its message, so it is taken only for that one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NoticeTarget {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
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
        .map(|stored| stored.message().msg_id.clone())
        .collect()
}

/// Whether `record` reflects the message `msg_id`: holds it, or names it among the others.
pub(crate) fn reflects(record: &AllowanceRecord, msg_id: &str) -> bool {
    stored_ids(record).iter().any(|id| id == msg_id) || record.msg_ids.iter().any(|id| id == msg_id)
}

/// Place `stored` in `record` (Paykit spec 6 "One message per role", Paykit wire 5.4): a
/// response or an end is stored when its member is empty and the record keeps its rules
/// with it; anything else, a second proposal under the same Allowance ID included, is
/// reflected by its `msg_id` only. Returns `false` when the record already reflects it, so
/// nothing changes.
pub(crate) fn place(record: &mut AllowanceRecord, stored: StoredMessage) -> bool {
    let msg_id = stored.message().msg_id.clone();
    if reflects(record, &msg_id) {
        return false;
    }
    let mut candidate = record.clone();
    let slot = match stored.body() {
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

/// The status the record shows, `stopped` when this app keeps the counterparty's end.
fn status(record: &AllowanceRecord, stopped: bool) -> AllowanceStatus {
    if stopped || record.end.is_some() {
        AllowanceStatus::Ended
    } else if rejected(record) {
        AllowanceStatus::Rejected
    } else if acceptance(record).is_some() {
        AllowanceStatus::Accepted
    } else {
        AllowanceStatus::Proposed
    }
}

/// The consent list's entry removed for good, for `reason`.
fn consent_removed(record_key: &[u8; 32], reason: ConsentEnd) -> Result<LocalWrite> {
    state::local_put(&key::consent(record_key), &Consent::Removed { reason })
}

/// What an app holds at an Allowance's record, in the receive step.
enum Held {
    /// No record, and the copy is listed: it holds none.
    None,
    /// A record that binds.
    Record(Box<AllowanceRecord>),
    /// A delete, or a value that doesn't read or doesn't bind: never written over, since it
    /// may hold what another own app stored (Paykit spec 8.1, "Stored entries").
    Unreadable,
}

/// The receive step of a proposal, an acceptance, a rejection or an end that passed check
/// 10, at an app holding `allowance` (module docs). The entry is held while the record
/// waits for its scope key or the copy of `allowances` isn't listed yet: its absence then
/// decides nothing (Paykit spec 8.1, "Records not listed yet").
pub(crate) fn receive_effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
        return Ok(());
    };
    let target = allowance_target(&sender, &allowance_id);
    let value = receive::held_or_unlisted_at(cx, &target, RecordSide::Allowances)?;
    let present = cx.record(&target)?.is_some();
    let held = match value {
        Some(value) => bind_at(&value, &own, &sender, &allowance_id)
            .map_or(Held::Unreadable, |r| Held::Record(Box::new(r))),
        None if present => Held::Unreadable,
        None => Held::None,
    };
    match stored.body() {
        Body::AllowanceEnd(_) => keep_end(cx, &target, held, stored, scopes),
        Body::AllowanceProposal(_) => match held {
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
            Held::Record(record) => put_placed(cx, &target, *record, stored),
            Held::Unreadable => Err(unreadable()),
        },
        _ => match held {
            // An answer before the app holds its proposal's record, such as one to a
            // proposal another own app sent whose record this app hasn't read yet: kept,
            // as new state the spam rule gates, within the bounds on kept replies, and
            // placed once the record is held (Paykit spec 8.1).
            Held::None => {
                if receive::admits_new_state(cx, &sender, scopes)? {
                    let record = cx.record_key(&target)?;
                    let kept = key::kept_reply(&sender, &record, &stored.message().msg_id);
                    if cx.local_get(kept.as_bytes())?.is_none() {
                        receive::make_room_for_reply(cx, &sender)?;
                        receive::keep_local(cx, receive::local(&kept, &KeptMessage::of(stored))?);
                    }
                }
                Ok(())
            }
            Held::Record(record) => put_placed(cx, &target, *record, stored),
            Held::Unreadable => Err(unreadable()),
        },
    }
}

fn unreadable() -> Fail {
    Fail::Drop("Paykit wire 5.4: the Allowance record doesn't read; it isn't written over".into())
}

/// Write `record` with `stored` placed, in the step, unless it reflects it already.
fn put_placed(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    mut record: AllowanceRecord,
    stored: &StoredMessage,
) -> std::result::Result<(), Fail> {
    if place(&mut record, stored.clone()) {
        cx.put_record(target, &record.to_json()?, Vec::new())?;
    }
    Ok(())
}

/// A counterparty's end (Paykit spec 8.3, "Ends reach the executor"): kept as the stop in
/// the commit that passes the entry, with no write at the switch, unless the record holds
/// an end or a rejection already. An executor's consent list drops the Allowance in the
/// same commit. One for an Allowance the app holds no record of is new state, which the
/// spam rule gates; one naming another proposal than the record's isn't this Allowance's.
fn keep_end(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    held: Held,
    stored: &StoredMessage,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    match &held {
        Held::Record(record) => {
            if record.end.is_some()
                || rejected(record)
                || reflects(record, &stored.message().msg_id)
            {
                return Ok(());
            }
            if stored.message().refs.first() != Some(&record.proposal.hash()) {
                return Err(Fail::Drop(
                    "Paykit wire 4.16: an end refers to its Allowance's proposal".into(),
                ));
            }
        }
        Held::None => {
            if !receive::admits_new_state(cx, &sender, scopes)? {
                return Ok(());
            }
        }
        Held::Unreadable => {}
    }
    let record_key = cx.record_key(target)?;
    let stop = key::allowance_end(&record_key);
    if cx.local_get(stop.as_bytes())?.is_none() {
        receive::keep_local(cx, receive::local(&stop, &KeptMessage::of(stored))?);
    }
    if scopes.iter().any(|s| s == "allowance_execute") {
        drop_consent(cx, &record_key, ConsentEnd::Ended)?;
    }
    receive::note_peer(cx, &sender, stored)
}

/// Remove the Allowance whose record is `record_key` from the consent list for good, in the
/// step, unless it is removed already.
fn drop_consent(
    cx: &mut ReceiveContext<'_>,
    record_key: &[u8; 32],
    reason: ConsentEnd,
) -> std::result::Result<(), Fail> {
    let entry = key::consent(record_key);
    let removed = match cx.local_get(entry.as_bytes())? {
        Some(bytes) => matches!(
            state::decode::<Consent>(&bytes).map_err(|_| Fail::corrupt())?,
            Consent::Removed { .. }
        ),
        None => false,
    };
    if !removed {
        receive::keep_local(cx, receive::local(&entry, &Consent::Removed { reason })?);
    }
    Ok(())
}

/// An `executor_notice` from an own app, at an app holding `allowance_execute` (Paykit spec
/// 8.3): the Allowance leaves its consent list for good. It changes nothing else.
pub(crate) fn notice_effect(
    cx: &mut ReceiveContext<'_>,
    notice: &ExecutorNotice,
) -> std::result::Result<(), Fail> {
    let reason = match notice.reason {
        NoticeReason::Ended => ConsentEnd::Ended,
        NoticeReason::HandedOff => ConsentEnd::HandedOff,
    };
    drop_consent(cx, &notice.allowance_record, reason)
}

/// Whether the kept stop `kept`, the counterparty's end, may go: an Allowance the app holds
/// no record of, or whose record doesn't name the response the end refers to yet, 30 days
/// from its acceptance on the service time verified (Paykit spec 8.1, 9).
fn stale(kept: &StoredMessage, service: Option<u64>) -> bool {
    kept.accepted_at()
        .is_none_or(|at| service.is_some_and(|t| at.saturating_add(KEPT_MESSAGE_MS) <= t))
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
        let Some(held) = self.app.shared_record(record).await? else {
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

    /// Whether this app keeps the counterparty's end of the Allowance at `record` as its
    /// stop.
    pub(crate) async fn allowance_stopped(&self, record: &[u8; 32]) -> Result<bool> {
        Ok(self
            .local_get::<KeptMessage>(&key::allowance_end(record))
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
    /// [`Refusal::ScopeMissing`] without `allowance`; [`PaykitError::Lib`] with `Validation`
    /// for terms that break Paykit wire 4.15, with nothing sent; the client's errors. A
    /// failure that leaves the batch kept (the network, `429`, `503`) is no error: the
    /// proposal is reported sent, [`MessageDelivery::Kept`], and goes with the app's next
    /// write or read.
    pub async fn propose_allowance(
        &mut self,
        to: [u8; 32],
        role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Result<SentAllowance> {
        self.refresh_own()?;
        self.require_allowance()?;
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
        let delivery = match self.app.submit_batch(batch).await {
            Ok(_) => self.delivered(&msg_id).await?,
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

    /// Where `msg_id`, recorded and queued, is after one try to deliver it now: a try that
    /// fails leaves it queued for the next flush.
    async fn delivered(&mut self, msg_id: &str) -> Result<MessageDelivery> {
        let delivery = match self.app.flush_message(msg_id).await {
            Ok(Some(delivery)) => delivery,
            _ => self
                .app
                .delivery(msg_id)
                .await?
                .ok_or_else(PaykitError::corrupt)?,
        };
        Ok(MessageDelivery::of(delivery))
    }

    /// Accept the Allowance proposed to this identity at `record` (Paykit spec 8.3, Paykit
    /// wire 4.16): the acceptance is recorded, then sent. Only the proposal's recipient
    /// answers it, once. Returns the acceptance's `msg_id`.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`]; [`Refusal::AllowanceEnded`] once it ended, or while this app
    /// keeps the counterparty's end; [`Refusal::Usage`] for a proposal this identity sent or
    /// one answered already; as [`Paykit::propose_allowance`].
    pub async fn accept_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        self.answer_allowance(record, Kind::AllowanceAcceptance)
            .await
    }

    /// Reject the Allowance proposed to this identity at `record` (Paykit spec 8.3, Paykit
    /// wire 4.16), as [`Paykit::accept_allowance`] accepts it. A rejected Allowance has no
    /// end.
    ///
    /// # Errors
    ///
    /// As [`Paykit::accept_allowance`].
    pub async fn reject_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        self.answer_allowance(record, Kind::AllowanceRejection)
            .await
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
    /// [`Refusal::NotFound`]; [`Refusal::AllowanceEnded`] once it ended, or while this app
    /// keeps the counterparty's end; [`Refusal::Rejected`] for a rejected one;
    /// [`Refusal::Usage`] for a proposal to this identity not yet answered, which is
    /// rejected instead; as [`Paykit::propose_allowance`].
    pub async fn end_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        self.answer_allowance(record, Kind::AllowanceEnd).await
    }

    /// Record and send this identity's acceptance, rejection or end of the Allowance at
    /// `record`, `kind`, built again from a fresh read after the record changed meanwhile.
    async fn answer_allowance(&mut self, record: &[u8; 32], kind: Kind) -> Result<String> {
        self.refresh_own()?;
        self.records_known().await?;
        self.require_allowance()?;
        let own = self.identity();
        let mut tries = 0;
        loop {
            let held = self
                .held_allowance(record)
                .await?
                .ok_or(Refusal::NotFound)?;
            if held.end.is_some() || self.allowance_stopped(record).await? {
                return Err(Refusal::AllowanceEnded.into());
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
            match self.submit_with_record(&target, &next, queue, locals).await {
                Ok(()) => {
                    self.delivered(&msg_id).await?;
                    // A notice not delivered now goes with the next flush.
                    for notice in notices {
                        let _ = self.app.flush_message(&notice).await;
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
    /// `409 conflict` the record is read again.
    async fn submit_with_record(
        &mut self,
        target: &RecordTarget,
        next: &AllowanceRecord,
        queue: Vec<SignedMessage>,
        locals: Vec<LocalWrite>,
    ) -> Result<()> {
        let write = self
            .app
            .record_op(target, Some(&next.to_json()?), Vec::new())
            .await?;
        let mut batch = OpBatch::new().entry(write).locals(locals);
        for message in queue {
            batch = batch.queue(message);
        }
        match self.app.submit_batch(batch).await {
            Ok(_) => Ok(()),
            Err(error) if error.kept_batch().is_some() => Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    self.app.read_record(target).await?;
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
            locals.push(consent_removed(record, ConsentEnd::Ended)?);
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
    /// doesn't act on it (Paykit wire 5.4).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the records aren't listed since a restore or a certificate
    /// that reads more; a store failure, also one reading a single record: the list is never
    /// partial.
    pub async fn allowances(&self) -> Result<Vec<AllowanceView>> {
        self.refuse_while_unlisted().await?;
        let mut out = Vec::new();
        for held in self.app.records_of(Collection::Allowances.name()).await? {
            if let Some(record) = self.held_allowance(&held.key).await? {
                out.push(self.allowance_view(held.key, &record).await?);
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
        self.refuse_while_unlisted().await?;
        let held = self
            .held_allowance(record)
            .await?
            .ok_or(Refusal::NotFound)?;
        self.allowance_view(*record, &held).await
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
            role,
            proposed_here,
            terms: proposal.terms.clone(),
            status: status(held, self.allowance_stopped(&record).await?),
            executor_app_id: held.current_grant(&own).map(|(grant, _)| grant.app_id),
            consented: self.consents(&record).await?,
        })
    }

    /// The counterparties of every Allowance not ended or rejected that this app holds a
    /// record of, for the re-sends after a restore.
    pub(crate) async fn open_allowance_counterparties(&self) -> Result<Vec<[u8; 32]>> {
        let mut out = Vec::new();
        for held in self.app.records_of(Collection::Allowances.name()).await? {
            if let Some(record) = self.held_allowance(&held.key).await? {
                let open = matches!(
                    status(&record, self.allowance_stopped(&held.key).await?),
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
    /// restore, with no stop kept for it (Paykit spec 8.3, "Executor").
    pub(crate) async fn consents(&self, record: &[u8; 32]) -> Result<bool> {
        let held = self.local_get::<Consent>(&key::consent(record)).await?;
        Ok(held
            == Some(Consent::Held {
                confirm_again: false,
            })
            && !self.allowance_stopped(record).await?)
    }

    /// Remove from the consent list, for good, every Allowance whose record this app holds
    /// ended or rejected: the executor drops it once it sees that, whoever wrote it (Paykit
    /// spec 8.3). Run after each receive.
    pub(crate) async fn sweep_consents(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        for (rest, consent) in self.local_scan::<Consent>(key::CONSENTS).await? {
            if !matches!(consent, Consent::Held { .. }) {
                continue;
            }
            let record = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| PaykitError::corrupt())?;
            let reason = match self.held_allowance(&record).await? {
                Some(held) if held.end.is_some() => ConsentEnd::Ended,
                Some(held) if rejected(&held) => ConsentEnd::Rejected,
                _ if self.allowance_stopped(&record).await? => ConsentEnd::Ended,
                _ => continue,
            };
            tx.put(&key::consent(&record), &Consent::Removed { reason })?;
        }
        self.local_commit(tx).await
    }

    // ---------------------------------------------------------------------------------
    // Later writes of the app's own

    /// Record every kept end whose Allowance record this app holds, each as a later write
    /// of its own with its `executor_notice`, the kept stop dropped in the same batch (Paykit
    /// spec 8.3). Run after each receive. One whose record holds an end or a rejection
    /// already only goes; one whose record isn't held, or doesn't hold the acceptance the
    /// end names yet, stays kept, 30 days from its acceptance; one that can't be written now
    /// stays for the next run. Nothing runs while the copy of `allowances` may lack records.
    pub(crate) async fn place_allowance_stops(&mut self) -> Result<()> {
        if !self.holds("allowance")
            || self.scope_keys_awaited().await?.is_some()
            || self.side_unlisted(RecordSide::Allowances).await?
        {
            return Ok(());
        }
        let own = self.identity();
        for (rest, kept) in self.local_scan::<KeptMessage>(key::ALLOWANCE_ENDS).await? {
            let stop = format!("{}{rest}", key::ALLOWANCE_ENDS);
            let end = kept.stored(&own)?;
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
                None => {
                    if stale(&end, self.service_time()) {
                        self.local_delete(&stop).await?;
                    }
                }
                Some(RecordContent::Value(_)) => {
                    if let Some(held) = self.held_allowance(&record).await? {
                        // A write that fails now is tried at the next run; the stop stands
                        // meanwhile.
                        let _ = self.record_end(&target, record, held, end, &stop).await;
                    }
                }
                // A delete, or a value that doesn't read or waits for its key: never
                // written over; the stop stands.
                Some(_) => {}
            }
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
        if held.end.is_some() || rejected(&held) {
            return self.local_delete(stop).await;
        }
        let refs = end.message().refs.clone();
        let proposal = held.proposal.hash();
        if refs.first() != Some(&proposal) {
            return self.local_delete(stop).await;
        }
        let mut next = held.clone();
        let mut queue = Vec::new();
        place(&mut next, end.clone());
        if !reflects_stored(&next, &end.message().msg_id) {
            match acceptance(&held) {
                // The proposal's withdrawal crossing the acceptance this identity sent: the
                // record can't hold both, so this identity ends the Allowance itself, naming
                // the two, and the withdrawal is reflected (Paykit wire 4.16).
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
                    queue.push(own_end);
                }
                // An end naming an acceptance the record doesn't hold yet waits for it.
                None if refs.len() == 2 && held.response.is_none() => {
                    if stale(&end, self.service_time()) {
                        self.local_delete(stop).await?;
                    }
                    return Ok(());
                }
                // Any other end isn't one of this Allowance (Paykit wire 4.16).
                _ => return self.local_delete(stop).await,
            }
        }
        let mut locals = Vec::new();
        self.end_notice(&held, &record, &mut queue, &mut locals)?;
        locals.push(LocalWrite::Delete {
            key: stop.as_bytes().to_vec(),
        });
        let sent: Vec<String> = queue.iter().map(|m| m.msg_id().to_owned()).collect();
        self.submit_with_record(target, &next, queue, locals)
            .await?;
        for msg_id in sent {
            self.app.flush_message(&msg_id).await?;
        }
        Ok(())
    }

    /// Place the kept acceptance or rejection `reply`, kept at `kept_key` before its
    /// Allowance record was held, once the app holds that record, as a later write of its
    /// own with the kept copy dropped (Paykit spec 8.1). One whose record isn't held goes 30
    /// days from its acceptance.
    pub(crate) async fn place_kept_allowance_response(
        &mut self,
        kept_key: &str,
        reply: StoredMessage,
    ) -> Result<()> {
        if self.side_unlisted(RecordSide::Allowances).await? {
            return Ok(());
        }
        let Some(allowance_id) = named_allowance(reply.body()).cloned() else {
            return self.local_delete(kept_key).await;
        };
        let target = allowance_target(&reply.sender(), &allowance_id);
        let record = self.app.record_key(&target)?;
        let Some(mut held) = self.held_allowance(&record).await? else {
            if stale(&reply, self.service_time()) {
                self.local_delete(kept_key).await?;
            }
            return Ok(());
        };
        let locals = vec![LocalWrite::Delete {
            key: kept_key.as_bytes().to_vec(),
        }];
        if !place(&mut held, reply) {
            return Ok(self.app.protocol_commit(&locals).await?);
        }
        // A write that fails now leaves the reply kept, for the next receive.
        let _ = self
            .submit_with_record(&target, &held, Vec::new(), locals)
            .await;
        Ok(())
    }

    /// A stored entry's proposal, acceptance or rejection (`stored.rs`), written into its
    /// Allowance record by a later write of the app's own: a proposal may open the record,
    /// an answer needs it held. It waits while the record's scope key is missing, while the
    /// copy of `allowances` may lack records, or while the record waits for its key; it is
    /// never written over a value that doesn't read.
    pub(crate) async fn place_allowance_note(&mut self, stored: StoredMessage) -> Result<Placed> {
        let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
            return Ok(Placed::Not);
        };
        if self
            .app
            .scope_key(self.app.data_epoch(), "allowance")
            .is_none()
        {
            return Ok(Placed::Waits);
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
            None if self.side_unlisted(RecordSide::Allowances).await? => return Ok(Placed::Waits),
            None => match stored.body() {
                Body::AllowanceProposal(_) => opened(counterparty, stored),
                _ => None,
            },
            Some(content) if crate::hold::awaits_key(&content) => return Ok(Placed::Waits),
            Some(RecordContent::Value(_)) => match self.held_allowance(&record).await? {
                Some(mut held) => {
                    if !place(&mut held, stored) {
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
        Ok(
            match self
                .submit_with_record(&target, &next, Vec::new(), Vec::new())
                .await
            {
                Ok(()) => Placed::Done,
                Err(_) => Placed::Not,
            },
        )
    }
}

/// Whether `record` holds the message `msg_id` in a member, not only among the others.
fn reflects_stored(record: &AllowanceRecord, msg_id: &str) -> bool {
    stored_ids(record).iter().any(|id| id == msg_id)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
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
        let accepted = StoredMessage::new(
            alice.id(),
            alice.cert.signed(),
            accepted.plaintext().clone(),
            None,
            &bob.id(),
        )
        .unwrap();
        assert!(place(&mut record, accepted.clone()));
        assert!(reflects_stored(&record, &accepted.message().msg_id));
        assert_eq!(status(&record, false), AllowanceStatus::Accepted);
        assert!(record.terms_hash().is_some());
        // Placed again, nothing changes.
        assert!(!place(&mut record, accepted));
    }

    #[test]
    fn test_allowances_place_reflects_a_second_answer_only() {
        let (alice, bob, mut record) = proposed();
        let own = |m: StoredMessage| {
            StoredMessage::new(
                alice.id(),
                alice.cert.signed(),
                m.plaintext().clone(),
                None,
                &bob.id(),
            )
            .unwrap()
        };
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
        assert!(place(&mut record, rejected.clone()));
        assert!(!reflects_stored(&record, &rejected.message().msg_id));
        assert!(reflects(&record, &rejected.message().msg_id));
        assert_eq!(status(&record, false), AllowanceStatus::Accepted);
    }

    #[test]
    fn test_allowances_place_end_after_acceptance_is_stored() {
        let (alice, bob, mut record) = proposed();
        let own = StoredMessage::new(
            alice.id(),
            alice.cert.signed(),
            answer(
                &alice,
                &bob,
                "paykit.allowance_acceptance",
                vec![record.proposal.hash()],
            )
            .plaintext()
            .clone(),
            None,
            &bob.id(),
        )
        .unwrap();
        place(&mut record, own.clone());
        let end = answer(
            &bob,
            &alice,
            "paykit.allowance_end",
            vec![record.proposal.hash(), own.hash()],
        );
        assert!(place(&mut record, end.clone()));
        assert!(reflects_stored(&record, &end.message().msg_id));
        assert_eq!(status(&record, false), AllowanceStatus::Ended);
    }

    #[test]
    fn test_allowances_place_reflects_a_withdrawal_crossing_the_acceptance() {
        let (alice, bob, mut record) = proposed();
        let own = StoredMessage::new(
            alice.id(),
            alice.cert.signed(),
            answer(
                &alice,
                &bob,
                "paykit.allowance_acceptance",
                vec![record.proposal.hash()],
            )
            .plaintext()
            .clone(),
            None,
            &bob.id(),
        )
        .unwrap();
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
        assert_eq!(status(&record, false), AllowanceStatus::Proposed);
        assert_eq!(status(&record, true), AllowanceStatus::Ended);
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
        assert!(!stale(&record.proposal, None));
        assert!(!stale(&record.proposal, Some(at + KEPT_MESSAGE_MS - 1)));
        assert!(stale(&record.proposal, Some(at + KEPT_MESSAGE_MS)));
    }
}
