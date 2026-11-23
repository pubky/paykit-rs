//! Allowances (Paykit spec 8.3; Paykit wire 4.15 to 4.17, 5.4): proposing, answering and
//! ending an Allowance, its `allowances` record on either side, the executor's consent list
//! and `executor_notice`.
//!
//! - **Pending proposals:** each account stores the original signed proposal in an
//!   ordinary `allowance_proposals` record. The wallet checks its current proposal count
//!   for that counterparty and direction before creating one. The public Allowance key
//!   identifies the permanent `allowances` record. Each pending create, rejection or
//!   withdrawal is conditioned on its absence (Paykit spec 8.3.1).
//! - **Receiving:** the recipient records a new proposal after reading the current
//!   pending inventory. A matching first rejection or withdrawal closes the selected
//!   pending version. A verified acceptance followed by a distinct rejection holds the
//!   proposal while the acceptance awaits placement. The original recipient-signed
//!   acceptance opens a permanent Allowance together with the original ledger on the payer’s account.
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
use paykit_lib::records::{
    bounded_msg_ids, AllowanceRecord, PendingAllowanceRecord, StoredMessage,
};
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
use crate::stored::{BoundNote, Placed};

pub(crate) use delivery::{
    append_records, judge_append, pending_delivery, reply_is_applied, AppliedReply,
};
pub(crate) use original::{restore_originals, OriginalAllowance, OriginalAllowanceApplication};
pub(crate) use pending::PreparedPendingReply;
pub(crate) use replies::holds_delivery as pending_conflict_holds_delivery;
pub(crate) use replies::{
    lifetime_ended, observe_reopened, restore_conflicts, PendingConflict, PendingRetirement,
};
mod application;
mod delivery;
mod evidence;
pub(crate) use evidence::ConflictEvidence;
mod original;
mod pending;
mod replies;

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
    /// The record already holds the proposal's acceptance. This status gives no authority
    /// to pay automatically (Paykit spec 8.3.1). Either side can end the Allowance
    /// ([`Paykit::end_allowance`]).
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
    /// Ordinary placement expired while the record could still establish a conflict.
    AwaitingConflictCheck,
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
    /// Whether this app holds current consent. Once the first executor grant has applied,
    /// [`Paykit::resume_allowance_executor`] records the consent authorized through
    /// [`Paykit::propose_executable_allowance`] or [`Paykit::accept_executable_allowance`]
    /// under the current hold. A clean restore preserves it. A missed item or a restore
    /// without a clean verdict requires confirmation through
    /// [`Paykit::confirm_allowance_executor`] (Paykit spec 8.3).
    pub consented: bool,
    /// The user must confirm this Allowance again before this app pays under it, after a
    /// `missed` item, or a restore until it is judged clean or the user confirms: true
    /// exactly when the Allowance's own state refuses a payment under it with
    /// [`crate::Refusal::AllowanceConfirmationNeeded`], which
    /// [`Paykit::confirm_allowance_executor`] lifts. So the host can ask the user from the
    /// list, without attempting a payment. A payment can still be refused first for the
    /// request's own state, such as a request held after the same restore.
    pub confirmation_needed: bool,
    /// The Allowance with the hold in force when this view was read: the user's
    /// confirmation is given for it ([`Paykit::confirm_allowance_executor`]). Every view
    /// carries one, unlike [`crate::RequestView::held`]: the user's first take-up of an
    /// Allowance is confirmed under it too, with `confirmation_needed` false. Where
    /// `confirmation_needed` is true, the host shows the Allowance before passing it back.
    pub hold: crate::AllowanceHold,
    /// Whether the Allowance's grant, or its proposal before a grant, is of a generation a
    /// provider loss ended (Paykit spec 8.3, "After provider loss").
    pub(crate) granted_before_loss: bool,
    /// Whether the counterparty is on the identity's deny list, as the app last read it
    /// (Paykit spec 8.1, "Blocked counterparties"): nothing new from it is taken, its
    /// requests are paused and nothing is sent to it, so the host shows the Allowance as
    /// blocked. `None` while the copy of the deny list isn't known, at an app holding
    /// `paykit_contacts` after a failed read of the lists or a restore: sends are held then too,
    /// and the host can't show it as not blocked.
    pub blocked: Option<bool>,
}

/// An Allowance proposal this app sent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SentAllowance {
    /// The canonical Allowance key for reading or acting on this proposal.
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// Where this app's consent to execute an Allowance stands ([`Paykit::consent_state`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConsentState {
    /// On the consent list and confirmed since any restore or `missed` item: the executor
    /// may pay under it.
    Held,
    /// On the list, but to be confirmed by the user again, after a restore that isn't clean
    /// or a `missed` item ([`Paykit::confirm_allowance_executor`]).
    ConfirmAgain,
    /// Removed for good, and why.
    Removed(ConsentEnd),
    /// This app holds no consent entry for it, and no first grant of its own: before its
    /// first grant, or after a restore of a backup taken before it.
    Unrecorded,
    /// This app's first grant is kept by the client to send again, its answer lost: the
    /// grant path lands it first ([`Paykit::resume_allowance_executor`]).
    GrantKept,
    /// This app's first grant applied and its consent isn't written yet. With the user's
    /// approval under the current hold (`approved`), resuming writes it; without, the user
    /// confirms it ([`Paykit::confirm_allowance_executor`]).
    Unconsented {
        /// Whether the user approved execution under the current hold.
        approved: bool,
    },
    /// Not held for now, and why: a `missed` item's mark the next receive turns into
    /// [`ConsentState::ConfirmAgain`], or a record not held as it must be yet.
    Waiting(&'static str),
}

/// Why consent waits while a `missed` item's mark is kept.
const WAITS_FOR_MISSED: &str =
    "a missed item holds every Allowance until the next receive marks it for confirmation";
/// Why consent waits while the Allowance record isn't held as it must be.
const WAITS_FOR_RECORD: &str = "the Allowance record isn't held as executing it needs yet";

/// The executor app an `executor_notice` is wrapped to (`en/<msg_id>`), kept with the batch
/// that queues it; it names its message, so it is taken only for that one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NoticeTarget {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
}

/// This app's own crossing end recorded while sends to the counterparty were held, whose
/// message is owed (`eo/<record key>`; Paykit spec 8.1, "Blocked counterparties"; 8.3,
/// "Ending"): it names the record and the end's `msg_id`, and is written in the batch that
/// records the end. The message goes from the record once sends to the counterparty aren't
/// held ([`Paykit::send_owed_ends`]). After restore the signed end can give rise to a
/// possibly-delivered notice. The original installation retains its local delivery duty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwedEnd {
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) msg_id: String,
}

/// Why this app signed an Allowance end (Paykit spec 8.1, 8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndCause {
    /// The user explicitly ended or withdrew the Allowance.
    User,
    /// A withdrawal crossed an acceptance, so the app ended it itself.
    Automatic,
    /// A restore or late acceptance found the closed original proposal.
    RetiredProposal,
    /// The original proposer withdrew its verified live write after a declared loss.
    ProviderLoss,
}

impl EndCause {
    pub(crate) fn permits(self, has_acceptance: bool) -> bool {
        match self {
            Self::User => true,
            Self::Automatic => has_acceptance,
            Self::RetiredProposal | Self::ProviderLoss => !has_acceptance,
        }
    }
}

/// The origin of one exact signed Allowance end (`end/<msg_id>`), recorded before
/// submitting its record and queue or owed mark (Paykit spec 8.1, 8.3). This retains neither
/// message bytes nor a delivery obligation. It does not classify a later copy as an
/// original delivery: recovery copies still follow the re-send rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EndOrigin {
    pub(crate) msg_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) message_hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) own: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) app_id: [u8; 16],
    #[serde(with = "serde_b64")]
    pub(crate) counterparty: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) record: [u8; 32],
    pub(crate) allowance_id: String,
    #[serde(with = "serde_b64")]
    pub(crate) proposal: [u8; 32],
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) acceptance: Option<[u8; 32]>,
    pub(crate) cause: EndCause,
}

impl EndOrigin {
    fn matches(&self, message: &pubky_switch_core::message::Message, hash: [u8; 32]) -> bool {
        let Ok(Body::AllowanceEnd(end)) = Body::parse(message) else {
            return false;
        };
        self.msg_id == message.msg_id
            && self.message_hash == hash
            && self.own == message.from.identity
            && self.app_id == message.from.app_id
            && self.counterparty == message.to
            && self.allowance_id == end.allowance_id.as_str()
            && std::iter::once(self.proposal)
                .chain(self.acceptance)
                .eq(message.refs.iter().copied())
    }

    fn of(
        record: [u8; 32],
        proposal: &StoredMessage,
        accepted: Option<&StoredMessage>,
        end: &StoredMessage,
        cause: EndCause,
    ) -> Result<Self> {
        let (Body::AllowanceProposal(original), Body::AllowanceEnd(body)) =
            (proposal.body(), end.body())
        else {
            return Err(PaykitError::corrupt());
        };
        let counterparty = if proposal.sender() == end.sender() {
            proposal.message().to
        } else if proposal.message().to == end.sender() {
            proposal.sender()
        } else {
            return Err(PaykitError::corrupt());
        };
        let accepted = accepted.map(StoredMessage::hash);
        if body.allowance_id != original.allowance_id
            || end.message().to != counterparty
            || !std::iter::once(proposal.hash()).chain(accepted).eq(end
                .message()
                .refs
                .iter()
                .copied())
            || (accepted.is_none() && end.sender() != proposal.sender())
            || !cause.permits(accepted.is_some())
        {
            return Err(PaykitError::corrupt());
        }
        Ok(Self {
            msg_id: end.message().msg_id.clone(),
            message_hash: end.hash(),
            own: end.sender(),
            app_id: end.cert().app_id(),
            counterparty,
            record,
            allowance_id: original.allowance_id.as_str().to_owned(),
            proposal: proposal.hash(),
            acceptance: accepted,
            cause,
        })
    }
}

/// Authorize this exact counterparty end at each delivery attempt (Paykit spec 3.1, 8.1).
pub(crate) fn may_deliver_end(
    delivery: &pubky_switch_client::app::DeliveryView<'_>,
) -> Result<bool> {
    use pubky_switch_client::app::DeliveryOrigin;

    let message = delivery.message();
    let scopes = delivery.cert().scopes();
    if !scopes.iter().any(|scope| scope == "paykit_allowance") {
        return Ok(false);
    }
    let fact = state::delivery_get::<EndOrigin>(delivery, &key::end_origin(&message.msg_id))?;
    let Some(origin) = fact.filter(|origin| origin.matches(message, delivery.hash())) else {
        return Ok(false);
    };
    if origin.cause == EndCause::ProviderLoss {
        let Some(original) = state::delivery_get::<OriginalAllowance>(
            delivery,
            &key::original_allowance(&origin.record),
        )?
        else {
            return Ok(false);
        };
        if !original.owes_loss_withdrawal(delivery.hash())? {
            return Ok(false);
        }
    }
    if origin.cause == EndCause::User
        && delivery.origin() == DeliveryOrigin::Original
        && !delivery.resend()
    {
        return Ok(true);
    }
    crate::protocol::may_deliver_to_contact(delivery)
}

/// The client's target of the `allowances` record of `counterparty`'s Allowance
/// `allowance_id` (Paykit wire 1.5, 5.4).
pub(crate) fn allowance_target(
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
) -> RecordTarget {
    allowance_record_target(Collection::Allowances, counterparty, allowance_id)
}

pub(crate) fn allowance_record_target(
    collection: Collection,
    counterparty: &[u8; 32],
    allowance_id: &AllowanceId,
) -> RecordTarget {
    RecordTarget::new(
        crate::activity::family(collection),
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

/// Kept proposals and answers both wait for Allowance records before placement.
pub(crate) fn placed_with_allowance(body: &Body) -> bool {
    matches!(body, Body::AllowanceProposal(_)) || is_allowance_response(body)
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

struct HeldAnswer {
    message: StoredMessage,
    order: AnswerOrder,
    position: Option<pubky_switch_client::app::FeedPosition>,
    source: Option<crate::divergent::SeenVersion>,
}

impl HeldAnswer {
    fn new(message: StoredMessage, kept: &KeptMessage) -> Result<Self> {
        Ok(Self {
            message,
            order: kept.order()?,
            position: kept.position(),
            source: kept.pending_source,
        })
    }
}

/// Whether the response of `record` belongs to another answer than `stored`: one of `kept`,
/// the answers the app keeps to write, is a different response to `record`'s proposal that
/// the inbox accepted before `stored`, at `order`. The later answer conflicts with the
/// earlier one, even while the app still keeps the earlier answer unplaced (Paykit spec
/// 8.1, 8.3).
fn answered_before(
    record: &AllowanceRecord,
    stored: &StoredMessage,
    order: AnswerOrder,
    kept: &[HeldAnswer],
) -> bool {
    let response = |m: &StoredMessage| {
        is_allowance_response(m.body())
            && m.sender() == record.proposal.message().to
            && !misreferenced(record, m)
    };
    response(stored)
        && kept.iter().any(|other| {
            other.order < order && other.message.hash() != stored.hash() && response(&other.message)
        })
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

/// Whether placing `stored` would fill the empty `response` of `record` with an acceptance
/// (Paykit spec 8.3.1). An acceptance that conflicts with a recorded response or crosses
/// the proposer's withdrawal leaves `response` as it was.
fn would_accept(record: &AllowanceRecord, stored: &StoredMessage) -> bool {
    let mut next = record.clone();
    acceptance(record).is_none() && place(&mut next, stored.clone()) && acceptance(&next).is_some()
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

/// A signed answer can be the protected first reply to the pending proposal at this account.
pub(crate) fn holds_pending_answer(
    cx: &ReceiveContext<'_>,
    reply: &StoredMessage,
) -> std::result::Result<bool, Fail> {
    let id = named_allowance(reply.body()).ok_or_else(Fail::corrupt)?;
    let target = allowance_record_target(Collection::AllowanceProposals, &reply.sender(), id);
    let key = cx.record_key(&target)?;
    Ok(cx.record_at(&key)?.is_some_and(|held| {
        replies::original_for(&held, &target, &key, &cx.identity(), reply).is_some()
    }))
}

/// The receive step can retire a reply before normal placement. A payer proposal
/// from an earlier generation may still need a host notice once the SDK checks
/// the signed loss boundary. Retaining Q here grants no right to change a record.
fn keep_earlier_acceptance_for_report(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    q: &StoredMessage,
    position: pubky_switch_client::app::FeedPosition,
) -> std::result::Result<(), Fail> {
    if matches!(q.body(), Body::AllowanceAcceptance(_)) {
        let key = key::original_allowance(&cx.record_key(target)?);
        let report = if let Some(original) = state::cx_get::<OriginalAllowance>(cx, &key)? {
            original
                .awaits_loss_report(q, position.generation)
                .map_err(|_| Fail::corrupt())?
        } else {
            let id = named_allowance(q.body()).ok_or_else(Fail::corrupt)?;
            let pending = allowance_record_target(Collection::AllowanceProposals, &q.sender(), id);
            let key = cx.record_key(&pending)?;
            cx.record(&pending)?.and_then(|held| replies::original_for(&held, &pending, &key, &cx.identity(), q))
                .is_some_and(|held| matches!(held.proposal().body(), Body::AllowanceProposal(p) if p.proposer_role == ProposerRole::Allower && p.allower_generation < position.generation)
                    && held.proposal().sender() == cx.identity() && held.proposal().cert().app_id() == cx.app_id())
        };
        if report {
            keep_answer(cx, target, q, position, true, None)?;
        }
    }
    Ok(())
}

/// The receive step of a proposal, an acceptance, a rejection or an end that passed check
/// 10, at an app holding `paykit_allowance` (module docs). The entry of a proposal or an answer is
/// held while the record waits for its scope key or the copy of `allowances` isn't listed
/// yet: its absence then decides nothing (Paykit spec 8.1, "Records not listed yet"). An
/// end's stop waits for no key ([`keep_end`]).
pub(crate) fn receive_effect(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    position: pubky_switch_client::app::FeedPosition,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let own = cx.identity();
    let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
        return Ok(());
    };
    let target = allowance_target(&sender, &allowance_id);
    if let Body::AllowanceEnd(_) = stored.body() {
        return keep_end(cx, &target, &allowance_id, stored, position, scopes);
    }
    if cx.record(&target)?.is_none() {
        let pending =
            allowance_record_target(Collection::AllowanceProposals, &sender, &allowance_id);
        let pending_record = receive::record_with_key(cx, &pending)?;
        if matches!(stored.body(), Body::AllowanceProposal(_)) {
            if let Some(held) =
                pending_record.filter(|r| !matches!(r.content, RecordContent::Deleted))
            {
                receive::hold_while_unlisted(cx, RecordSide::Allowances)?;
                if !replies::before_retirement(cx, &pending, position)? {
                    replies::observe_second(cx, &pending, &held, stored, position)?;
                }
                return receive::note_peer(cx, &sender, stored);
            }
        } else if is_allowance_response(stored.body()) {
            if replies::before_retirement(cx, &pending, position)? {
                keep_earlier_acceptance_for_report(cx, &target, stored, position)?;
                return receive::note_peer(cx, &sender, stored);
            }
            if let Some(held) = pending_record {
                match replies::observe(cx, &held, stored, position)? {
                    replies::PendingReply::Undecided => {
                        keep_answer(cx, &target, stored, position, true, None)?;
                        return receive::note_peer(cx, &sender, stored);
                    }
                    replies::PendingReply::Unrelated => {}
                    replies::PendingReply::Waits(version) => {
                        keep_answer(cx, &target, stored, position, true, version)?;
                        return receive::note_peer(cx, &sender, stored);
                    }
                    replies::PendingReply::Stopped => {
                        keep_earlier_acceptance_for_report(cx, &target, stored, position)?;
                        return receive::note_peer(cx, &sender, stored);
                    }
                }
            }
        }
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
                if receive::admits_new_state(cx, &sender, scopes, stored)? {
                    keep_conflict(cx, &target, stored, position, scopes)?;
                }
                Ok(())
            }
            Held::None => {
                if receive::admits_new_state(cx, &sender, scopes, stored)? {
                    // The account read supplies its complete inventory after this inbox step.
                    keep_answer(cx, &target, stored, position, false, None)?;
                    receive::note_peer(cx, &sender, stored)?;
                }
                Ok(())
            }
            Held::Record(record) => put_placed(cx, &target, *record, stored, position, scopes),
            Held::Unreadable => stored_entry(cx, stored, position, Some((&target, None))),
        },
        _ => match held {
            // An answer before the app holds its proposal's record, such as one to a
            // proposal another own app sent whose record this app hasn't read yet: kept,
            // as new state the spam rule gates, within the bounds on kept replies, and
            // placed once the record is held (Paykit spec 8.1).
            // Over a record held deleted, the earliest kept is under no bound, as over one
            // held ([`keep_answer`]).
            Held::None => {
                if receive::admits_new_state(cx, &sender, scopes, stored)? {
                    keep_answer(cx, &target, stored, position, deleted, None)?;
                }
                Ok(())
            }
            Held::Record(record) => put_placed(cx, &target, *record, stored, position, scopes),
            Held::Unreadable => stored_entry(cx, stored, position, Some((&target, None))),
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
    position: pubky_switch_client::app::FeedPosition,
    over_held: bool,
    source: Option<crate::divergent::SeenVersion>,
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let record = cx.record_key(target)?;
    let kept = key::kept_reply(&sender, &record, &stored.message().msg_id);
    if state::cx_get::<KeptMessage>(cx, &kept)?.is_none() {
        let mut message = KeptMessage::taken_at(stored, position);
        message.pending_source = source;
        receive::make_room_for_reply(cx, stored, &message, over_held.then_some(record))?;
        receive::keep_local(cx, receive::local(&kept, &message)?);
    }
    Ok(())
}

/// A proposal or an answer whose Allowance record the app holds as a value it can't read
/// for a reason other than a missing key: never written over, since it may hold what another
/// own app stored, and not dropped at once. The entry is stored locally, the
/// cursor passing it (Switch spec 8.2), and its step is tried again at each read, so the
/// message goes into the record once the record reads. Once [`KEPT_MESSAGE_MS`] have passed
/// since acceptance, ordinary placement may end. Unresolved conflict evidence survives
/// with its own host notice (Paykit spec 8.1, "Stored entries"). A value only waiting for its scope key holds the entry
/// unprocessed before this ([`receive::held_or_unlisted_at`]).
fn stored_entry(
    cx: &mut ReceiveContext<'_>,
    stored: &StoredMessage,
    position: pubky_switch_client::app::FeedPosition,
    unresolved: Option<(&RecordTarget, Option<crate::divergent::SeenVersion>)>,
) -> std::result::Result<(), Fail> {
    // Past its window on the highest service time verified (Paykit spec 8.1). A stored
    // message without its acceptance time is a corrupt store, never one past its window.
    let at = stored.accepted_at().ok_or_else(Fail::corrupt)?;
    let over = cx
        .highest_service_time()
        .is_some_and(|t| at.saturating_add(receive::KEPT_MESSAGE_MS) <= t);
    if over {
        if let Some((target, source)) = unresolved {
            let record = cx.record_key(target)?;
            return evidence::retain(cx, stored, position, &record, source);
        }
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
    Err(Fail::Defer {
        reason: "Allowance record unresolved",
        custody: pubky_switch_client::app::DeferCustody::Immediate,
    })
}

/// Write `record` with `stored` placed, in the step, unless it reflects it already. A
/// message that makes the Allowance conflicted ([`conflicts`]) is kept as the stop instead
/// ([`keep_conflict`]), and an answer that would take the empty response is kept for
/// another try after the read ([`keep_answer`]).
fn put_placed(
    cx: &mut ReceiveContext<'_>,
    target: &RecordTarget,
    mut record: AllowanceRecord,
    stored: &StoredMessage,
    position: pubky_switch_client::app::FeedPosition,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let seq = position.seq;
    if misreferenced(&record, stored) && !reflects(&record, &stored.message().msg_id) {
        return Err(Fail::Drop(
            "Paykit wire 4.16: a response refers to its Allowance's proposal alone".into(),
        ));
    }
    if record.conflicted.is_none()
        && !reflects(&record, &stored.message().msg_id)
        && conflicts(&record, stored)
    {
        return keep_conflict(cx, target, stored, position, scopes);
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
        return keep_conflict(cx, target, stored, position, scopes);
    }
    // The step never writes an answer into the record's empty response. The answer is kept
    // with the replies kept to place (`pr/`), with its place in the inbox's order, and a
    // rejection is written by the placement after the read (an acceptance never is:
    // [`would_accept`]), once that read's stored entries are noted
    // (`se/`), in that order ([`Paykit::place_kept_allowance_responses`]): an earlier answer
    // kept, or stored and noted (`se/`), or tried again by the client before the feed, takes
    // the response first, and a later one conflicts with it ([`answered_before`]). An entry
    // the client stores without a duty event leaves no note, so it is not among them. A
    // rejection's stop acts at once: an app holding `paykit_allowance_execute` removes the
    // Allowance from its consent list for good in this commit, and [`Paykit::consents`]
    // refuses payment while the rejection is kept.
    if is_allowance_response(stored.body())
        && stored.sender() == record.proposal.message().to
        && record.response.is_none()
        && !reflects(&record, &stored.message().msg_id)
    {
        keep_answer(cx, target, stored, position, true, None)?;
        if matches!(stored.body(), Body::AllowanceRejection(_))
            && scopes.iter().any(|s| s == "paykit_allowance_execute")
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
                    Body::AllowanceProposal(_) => stored_entry(cx, stored, position, None),
                    _ => keep_answer(cx, target, stored, position, false, None),
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
) -> std::result::Result<Vec<HeldAnswer>, Fail> {
    let own = cx.identity();
    let sender = stored.sender();
    let record = cx.record_key(target)?;
    let before = state::cx_scan::<KeptMessage>(cx, &key::kept_reply(&sender, &record, ""))?
        .into_iter()
        .map(|(_, kept)| kept)
        .collect();
    let notes = state::cx_scan(cx, &key::stored_notes_of(&sender))?;
    let stop = state::cx_get::<KeptMessage>(cx, &key::conflict(&record))?;
    answers_kept(&own, named_allowance(stored.body()), before, notes, stop)
        .map_err(|_| Fail::corrupt())
}

/// The answers among `before`, the messages kept before one Allowance record, and `notes`,
/// the stored entries' notes of its counterparty with their keys, that are still to write
/// into the record of `allowance_id`, each with its order; and `stop`, the conflict's stop
/// kept for that record (`cf/`), when it is an answer. The stop is an answer the inbox
/// accepted too, at its own place in that order: it stays kept after the earlier answer it
/// conflicted with is given up, so a different answer accepted after it still meets it
/// ([`answered_before`]) and never takes the response.
fn answers_kept(
    own: &[u8; 32],
    allowance_id: Option<&AllowanceId>,
    before: Vec<KeptMessage>,
    notes: Vec<(String, BoundNote)>,
    stop: Option<KeptMessage>,
) -> Result<Vec<HeldAnswer>> {
    let mut kept = Vec::new();
    if let Some(stop) = stop {
        let answer = stop.stored(own)?;
        if is_allowance_response(answer.body()) {
            kept.push(HeldAnswer::new(answer, &stop)?);
        }
    }
    for message in before {
        let answer = message.stored(own)?;
        if is_allowance_response(answer.body()) {
            kept.push(HeldAnswer::new(answer, &message)?);
        }
    }
    for (_, BoundNote { note, message }) in notes {
        if !note.placed
            && is_allowance_response(message.body())
            && named_allowance(message.body()) == allowance_id
        {
            kept.push(HeldAnswer::new(message, &note.message)?);
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
    position: pubky_switch_client::app::FeedPosition,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let record_key = cx.record_key(target)?;
    let kept = state::cx_get::<KeptMessage>(cx, &key::conflict(&record_key))?.is_some();
    let consent = state::cx_get::<ConsentEntry>(cx, &key::consent(&record_key))?;
    let executes = scopes.iter().any(|s| s == "paykit_allowance_execute");
    for write in conflict_writes(
        &record_key,
        stored,
        &KeptMessage::taken_at(stored, position),
        kept,
        consent.as_ref(),
        executes,
    )
    .map_err(|_| Fail::corrupt())?
    {
        receive::keep_local(cx, write);
    }
    receive::note_peer(cx, &stored.sender(), stored)
}

/// The writes a second, different proposal or response makes, the receive step's and a
/// later write's alike (Paykit spec 8.3, "Conflicted"): its stop (`cf/<record key>`) unless
/// one is kept already, the first one kept staying; and, at an app holding
/// `paykit_allowance_execute`, the consent list's entry removed for good, unless it is already.
fn conflict_writes(
    record_key: &[u8; 32],
    stored: &StoredMessage,
    evidence: &KeptMessage,
    kept: bool,
    consent: Option<&ConsentEntry>,
    executes: bool,
) -> Result<Vec<LocalWrite>> {
    let mut writes = Vec::new();
    if !kept {
        writes.push(state::local_put(&key::conflict(record_key), evidence)?);
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
    position: pubky_switch_client::app::FeedPosition,
    scopes: &[String],
) -> std::result::Result<(), Fail> {
    let sender = stored.sender();
    let refs = &stored.message().refs;
    let content = cx.record(target)?.map(|held| held.content);
    let mut evidence = KeptMessage::taken_at(stored, position);
    let record_key = cx.record_key(target)?;
    if refs.len() == 2 && content.is_none() {
        let pending =
            allowance_record_target(Collection::AllowanceProposals, &sender, allowance_id);
        let key = cx.record_key(&pending)?;
        if let Some(mut original) =
            state::cx_get::<OriginalAllowance>(cx, &key::original_allowance(&record_key))?
        {
            let writes = match cx.record(&pending)? {
                Some(live)
                    if live.content == RecordContent::Deleted
                        && replies::qualified_at(&live, &pending, &key)
                        && replies::precedes_reply(live.version, position, |a, b| {
                            cx.received_order(a, b)
                        }) =>
                {
                    original.finish_loss_return(stored, &record_key, live.version)
                }
                None => original.keep_loss_return(stored, &evidence, &record_key, |d| {
                    cx.received_order(
                        pubky_switch_client::app::FeedPosition {
                            generation: d.generation,
                            seq: d.version,
                        },
                        position,
                    )
                }),
                _ => Ok(None),
            }
            .map_err(|_| Fail::corrupt())?;
            if let Some(writes) = writes {
                for write in writes {
                    receive::keep_local(cx, write);
                }
                return receive::note_peer(cx, &sender, stored);
            }
        }
    }
    let ends = state::cx_scan::<KeptMessage>(cx, &key::allowance_ends(&record_key))?;
    if refs.len() == 2
        && !receive::holds_record(content.as_ref())
        && ends.iter().all(|(_, e)| e.ended_acceptance.is_none())
    {
        let pending =
            allowance_record_target(Collection::AllowanceProposals, &sender, allowance_id);
        let key = cx.record_key(&pending)?;
        if let Some(live) = cx.record(&pending)? {
            for answer in step_answers_kept(cx, target, stored)? {
                if answer
                    .source
                    .is_some_and(|source| source.record_version() == live.version)
                    && replies::original_for(&live, &pending, &key, &cx.identity(), &answer.message)
                        .is_some()
                {
                    evidence.ended_acceptance = pending_end_acceptance(
                        stored,
                        position,
                        &answer.message,
                        answer.position,
                        live.version,
                        |a, b| cx.received_order(a, b),
                    );
                    if evidence.ended_acceptance.is_some() {
                        break;
                    }
                }
            }
        }
    }
    let bounded = evidence.ended_acceptance.is_none() && end_bounded(cx, content.as_ref(), stored)?;
    let held = receive::holds_record(content.as_ref()) || !bounded;
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
        if !receive::admits_new_state(cx, &sender, scopes, stored)? {
            return Ok(());
        }
    }
    let stop = key::allowance_end(&record_key, &stored.message().msg_id);
    if state::cx_get::<KeptMessage>(cx, &stop)?.is_none() {
        // A newly matched end takes the exemption. Its predecessor may now
        // count even when the sender already has a full pool.
        if evidence.ended_acceptance.is_some() && !ends.is_empty() {
            receive::make_room_for_stop(cx, &sender, receive::StopAdmission::Matched(record_key))?;
        } else if !ends.is_empty() || (bounded && !keeps_stored_message(cx, &sender, &record_key)?)
        {
            let order = (
                stored.accepted_at().ok_or_else(Fail::corrupt)?,
                position.seq,
            );
            receive::make_room_for_stop(cx, &sender, receive::StopAdmission::Counted(order))?;
        }
        receive::keep_local(cx, receive::local(&stop, &evidence)?);
    }
    if checked && scopes.iter().any(|s| s == "paykit_allowance_execute") {
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
    let Body::AllowanceEnd(body) = end.body() else {
        return Err(Fail::corrupt());
    };
    if !receive::holds_record(content) {
        let refs = &end.message().refs;
        if refs.len() != 1 {
            return Ok(true);
        }
        let peer = end.sender();
        let target =
            allowance_record_target(Collection::AllowanceProposals, &peer, &body.allowance_id);
        let key = cx.record_key(&target)?;
        let Some(held) = cx.record(&target)? else {
            return Ok(true);
        };
        if !receive::holds_record(Some(&held.content)) {
            return Ok(true);
        }
        // A live value without qualified roles can protect the earliest end from eviction.
        // Removing the proposal still requires the exact signed original and a current read.
        let Some(proposal) = held
            .content
            .value()
            .filter(|_| replies::qualified_at(&held, &target, &key))
            .and_then(|value| PendingAllowanceRecord::read(value, &cx.identity()).ok())
        else {
            return Ok(false);
        };
        let matched = proposal.counterparty() == &peer
            && proposal.allowance_id() == &body.allowance_id
            && proposal.proposal().sender() == peer
            && refs.as_slice() == [proposal.proposal().hash()];
        return Ok(!matched);
    }
    let Some(record) = content
        .and_then(RecordContent::value)
        .and_then(|value| bind_at(value, &cx.identity(), &end.sender(), &body.allowance_id))
    else {
        return Ok(false);
    };
    Ok(!names_held(&record, &end.message().refs))
}

/// Both signatures name this proposal, this acceptance and the same parties.
pub(crate) fn end_names_acceptance(end: &StoredMessage, q: &StoredMessage) -> bool {
    let (Body::AllowanceEnd(e), Body::AllowanceAcceptance(a)) = (end.body(), q.body()) else {
        return false;
    };
    let [p] = q.message().refs.as_slice() else {
        return false;
    };
    e.allowance_id == a.allowance_id
        && end.sender() == q.sender()
        && end.message().to == q.message().to
        && end.message().refs.as_slice() == [*p, q.hash()]
}

/// The proposal must already have been live at both inbox positions. A reused
/// signed proposal after deletion does not make either old message current.
fn pending_end_acceptance(
    end: &StoredMessage,
    end_at: pubky_switch_client::app::FeedPosition,
    q: &StoredMessage,
    q_at: Option<pubky_switch_client::app::FeedPosition>,
    source: pubky_switch_core::ops::RecordVersion,
    order: impl Fn(
        pubky_switch_client::app::FeedPosition,
        pubky_switch_client::app::FeedPosition,
    ) -> Option<std::cmp::Ordering>,
) -> Option<Box<KeptMessage>> {
    let q_at = q_at?;
    if !end_names_acceptance(end, q)
        || !replies::precedes_reply(source, q_at, &order)
        || !replies::precedes_reply(source, end_at, order)
    {
        return None;
    }
    let mut proof = KeptMessage::taken_at(q, q_at);
    proof.pending_source = Some(source.into());
    Some(Box::new(proof))
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

/// An `executor_notice` from an own app, at an app holding `paykit_allowance_execute` (Paykit spec
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
    // Only an entry the list holds, or an Allowance this app granted itself or approved for
    // automatic execution, is removed: a notice naming any other record key adds nothing,
    // so own apps' notices can't grow the list. The consent is written only once the grant
    // applied, so a notice before then, while the grant's batch is still kept too, is kept
    // as a removal, which the consent step then leaves as it is (Paykit spec 8.3, "removes
    // it for good ... even before it has added it"). All three are read bound to their key,
    // so the Allowance they name is the one whose record the notice names.
    let record = &notice.allowance_record;
    let named = match state::cx_get::<ConsentEntry>(cx, &key::consent(record))? {
        Some(entry) => Some((
            entry.counterparty,
            entry.allowance_id().map_err(|_| Fail::corrupt())?,
        )),
        None => match state::cx_get::<crate::executor::ExecutorHistory>(
            cx,
            &key::executor_checkpoint(record),
        )?
        .and_then(|history| history.0.first().cloned())
        {
            Some(granted) => Some(
                AllowanceId::parse(&granted.allowance_id)
                    .map(|id| (granted.counterparty, id))
                    .map_err(|_| Fail::corrupt())?,
            ),
            None => state::cx_get::<OriginalAllowance>(cx, &key::original_allowance(record))?
                .map(|approved| approved.approved_for_execution())
                .transpose()
                .map_err(|_| Fail::corrupt())?
                .flatten(),
        },
    };
    let Some((counterparty, allowance_id)) = named else {
        return Ok(());
    };
    drop_consent(cx, record, (counterparty, &allowance_id), reason)
}

/// The `allowances` record the stored entry's message `note` is to be written into (`se/`,
/// `stored.rs`): `None` for a note placed already, or for a message of no Allowance.
fn note_target(bound: &BoundNote) -> Option<RecordTarget> {
    if bound.note.placed {
        return None;
    }
    named_allowance(bound.message.body()).map(|id| allowance_target(&bound.message.sender(), id))
}

/// A first proposal is kept until its bounded admission and conditional create complete.
fn keeps_proposal(messages: Vec<(String, KeptMessage)>, own: &[u8; 32]) -> Result<bool> {
    for (_, message) in messages {
        if matches!(message.stored(own)?.body(), Body::AllowanceProposal(_)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether this app still owes the first pending proposal or a stored inbox message for
/// the same Allowance. Both are bound to the permanent `record` key. Such a message can
/// open the record or give it the response an end names, so the end's stop is neither
/// dropped by the bounds ([`receive::make_room_for_stop`]) nor given up while it is kept
/// (Paykit spec 8.1, 8.3).
pub(crate) fn keeps_stored_message(
    cx: &ReceiveContext<'_>,
    peer: &[u8; 32],
    record: &[u8; 32],
) -> std::result::Result<bool, Fail> {
    let own = cx.identity();
    let prefix = key::kept_reply(peer, record, "");
    if keeps_proposal(state::cx_scan::<KeptMessage>(cx, &prefix)?, &own)
        .map_err(|_| Fail::corrupt())?
    {
        return Ok(true);
    }
    for (_, note) in state::cx_scan(cx, key::STORED_NOTES)? {
        if let Some(target) = note_target(&note) {
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

/// Whether `status` is a stop: an end, a rejection or a conflict, final whatever the record
/// could still establish (Paykit spec 8.3).
fn stopped(status: AllowanceStatus) -> bool {
    matches!(
        status,
        AllowanceStatus::Conflicted | AllowanceStatus::Ended | AllowanceStatus::Rejected
    )
}

/// The status a view shows for `status` while the record could still establish a conflict
/// (`unresolved`): an Allowance that hasn't stopped waits for the conflict check, and a stop
/// stays what it is.
pub(crate) fn shown_status(status: AllowanceStatus, unresolved: bool) -> AllowanceStatus {
    if unresolved && !stopped(status) {
        AllowanceStatus::AwaitingConflictCheck
    } else {
        status
    }
}

impl Paykit {
    pub(crate) fn require_allowance(&self) -> Result<()> {
        if self.holds("paykit_allowance") {
            Ok(())
        } else {
            Err(Refusal::ScopeMissing("paykit_allowance".into()).into())
        }
    }

    /// Contacts and a current permitting deny copy authorize an automatic end or a
    /// reconstructed copy. The original user stop has its separate exception (Paykit spec 8.1).
    async fn allowance_end_send_permitted(&self, peer: &[u8; 32]) -> Result<bool> {
        if !self.holds("paykit_allowance") || !receive::reads_lists(self.app.cert().scopes()) {
            return Ok(false);
        }
        let read = self.local_get::<u64>(key::LISTS_READ).await?;
        if !read.is_some_and(|at| receive::lists_read_is_fresh(at, self.now()))
            || self
                .local_get::<Vec<[u8; 32]>>(key::DENIED)
                .await?
                .is_none()
        {
            return Ok(false);
        }
        Ok(!self.denied_as_last_read(peer).await?)
    }

    /// Reconstruct only the same classified end, under ordinary contact authorization.
    pub(crate) async fn may_queue_stored_end(
        &self,
        message: &pubky_switch_core::message::Message,
        hash: [u8; 32],
    ) -> Result<bool> {
        if !Box::pin(self.allowance_end_send_permitted(&message.to)).await? {
            return Ok(false);
        }
        Ok(self
            .local_get::<EndOrigin>(&key::end_origin(&message.msg_id))
            .await?
            .is_some_and(|origin| origin.matches(message, hash)))
    }

    fn require_allowance_send(&self, kind: Kind) -> Result<()> {
        self.require_allowance()?;
        if !kind.has_allowance_sender_contacts(self.app.cert().scopes()) {
            return Err(Refusal::ScopeMissing("paykit_contacts".into()).into());
        }
        Ok(())
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
    async fn keeps_stored_message(&self, peer: &[u8; 32], record: &[u8; 32]) -> Result<bool> {
        let own = self.identity();
        let prefix = key::kept_reply(peer, record, "");
        if keeps_proposal(self.local_scan::<KeptMessage>(&prefix).await?, &own)? {
            return Ok(true);
        }
        for (_, note) in Box::pin(self.local_scan(key::STORED_NOTES)).await? {
            if let Some(target) = note_target(&note) {
                if self.app.record_key(&target)? == *record {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Condition a first grant on the exact original deletion, or a fresh observation
    /// that the service has forgotten it. The caller separately holds its complete proof.
    pub(crate) async fn pending_condition_for_executor(
        &mut self,
        peer: &[u8; 32],
        id: &AllowanceId,
        deletion: pubky_switch_core::shared::ApplicationRef,
    ) -> Result<Option<pubky_switch_core::ops::Condition>> {
        use pubky_switch_client::app::{KeptReason, RecordRead};
        use pubky_switch_core::ops::{Condition, RecordVersion};
        let target = allowance_record_target(Collection::AllowanceProposals, peer, id);
        let key = self.app.record_key(&target)?;
        if self.unresolved_allowance(&key).await?
            || self
                .local_get::<PendingConflict>(&key::pending_conflict(&key))
                .await?
                .is_some()
        {
            return Ok(None);
        }
        let read = self.app.read_record_fresh(&target).await?;
        if !matches!(delivery::current(&target, &key, &read, true), Some(None)) {
            return Ok(None);
        }
        match read {
            RecordRead::Kept {
                reason: KeptReason::NotFound,
                record,
            } => {
                let original = RecordVersion::new(deletion.generation, deletion.seq);
                Ok(record.is_none_or(|r| r.version == original).then_some(
                    Condition::RecordVersion {
                        key,
                        version: RecordVersion::NONE,
                    },
                ))
            }
            RecordRead::Fresh(_) => {
                let applied = self.app.read_record_application(&target).await?;
                Ok(applied
                    .is_some_and(|current| {
                        current.application == deletion
                            && replies::qualified_at(&current.record, &target, &key)
                            && current.record.content
                                == pubky_switch_client::app::RecordContent::Deleted
                    })
                    .then_some(Condition::RecordCurrent {
                        key,
                        revision: deletion,
                        deleted: true,
                    }))
            }
            _ => Ok(None),
        }
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
    pub(crate) async fn consents_unconfirmed(&self) -> Result<bool> {
        Ok(self
            .local_get::<bool>(key::MISSED_CONSENTS)
            .await?
            .is_some())
    }

    /// Propose an Allowance to `to` (Paykit spec 8.3, Paykit wire 4.15): this identity takes
    /// `role`, `to` the other. The proposal is signed under a fresh Allowance ID, and its
    /// pending record is written in the batch that queues it. The recipient must admit
    /// this identity (Paykit spec 1). A proposal grants no spending authority.
    ///
    /// # Errors
    ///
    /// A seller proposes with this method. The payer uses [`Self::propose_executable_allowance`]
    /// to confirm its terms and automatic payment before signing P. Proposing requires both
    /// `paykit_allowance` and `paykit_contacts`.
    /// [`Refusal::ScopeMissing`] identifies a missing scope. [`Refusal::NotNow`] is returned
    /// while sends to `to` are held because the deny list holds it, or the copy of
    /// the lists isn't known after a read tried now (Paykit spec 8.1, "Blocked counterparties"),
    /// with nothing signed, written or queued; [`PaykitError::Lib`] with `Validation` for terms
    /// that break Paykit wire 4.15, with nothing sent; the client's errors. A failure that leaves
    /// the batch kept (the network, `429`, `503`) is no error: the proposal is reported sent,
    /// [`MessageDelivery::Kept`], and goes with the app's next write or read.
    pub async fn propose_allowance(
        &mut self,
        to: [u8; 32],
        role: AllowanceRole,
        terms: AllowanceTerms,
    ) -> Result<SentAllowance> {
        Box::pin(self.propose_allowance_with_consent(to, role, terms, None)).await
    }

    /// Propose these terms as payer and authorize this wallet as their first executor.
    /// The host shows the counterparty, terms and automatic payments by this app once.
    ///
    /// # Errors
    ///
    /// As [`Self::propose_allowance`]. The payer must also hold execution authority.
    pub async fn propose_executable_allowance(
        &mut self,
        to: [u8; 32],
        terms: AllowanceTerms,
        confirmed: crate::UserConfirmed,
    ) -> Result<SentAllowance> {
        Box::pin(self.propose_allowance_with_consent(
            to,
            AllowanceRole::Allower,
            terms,
            Some(confirmed),
        ))
        .await
    }

    fn require_payer_choice(&self, choice: Option<&crate::UserConfirmed>) -> Result<()> {
        if !self.holds("paykit_allowance_execute") {
            return Err(Refusal::ScopeMissing("paykit_allowance_execute".into()).into());
        }
        if choice.is_none() {
            return Err(Refusal::Usage(
                "confirm the Allowance terms and automatic payment by this wallet".into(),
            )
            .into());
        }
        Ok(())
    }

    pub(crate) async fn propose_allowance_with_consent(
        &mut self,
        to: [u8; 32],
        role: AllowanceRole,
        terms: AllowanceTerms,
        execution: Option<crate::UserConfirmed>,
    ) -> Result<SentAllowance> {
        self.refresh_own()?;
        let count = Box::pin(self.own_pending_count(&to)).await?;
        self.require_first_read()?;
        self.refresh_own()?;
        self.require_allowance_send(Kind::AllowanceProposal)?;
        if role == AllowanceRole::Allower {
            self.require_payer_choice(execution.as_ref())?;
        }
        Box::pin(self.require_sends_allowed(&to)).await?;
        let count = count.ok_or_else(|| {
            Refusal::NotNow("the current pending proposals have not been read".into())
        })?;
        if count >= pending::LIMIT {
            return Err(Refusal::NotNow(
                "eight own proposals to this counterparty are already pending".into(),
            )
            .into());
        }
        let allowance_id = AllowanceId::generate()?;
        let target = allowance_record_target(Collection::AllowanceProposals, &to, &allowance_id);
        let permanent = allowance_target(&to, &allowance_id);
        Box::pin(self.absent_record(&target)).await?;
        Box::pin(self.absent_record(&permanent)).await?;
        let proposer_role = match role {
            AllowanceRole::Allower => ProposerRole::Allower,
            AllowanceRole::Allowee => ProposerRole::Allowee,
        };
        let allower_generation = match role {
            AllowanceRole::Allower => self.app.own_identity().generation(),
            AllowanceRole::Allowee => self.app.identity_of(&to, true).await?.doc.generation(),
        };
        // The writer reads back what it writes by the recipient's rules (Paykit wire 4.15).
        let body = AllowanceProposal {
            allowance_id: allowance_id.clone(),
            proposer_role,
            allower_generation,
            terms,
        }
        .to_body()?;
        let signed = self.sign(to, Kind::AllowanceProposal, Vec::new(), body)?;
        let msg_id = signed.msg_id().to_owned();
        let record = PendingAllowanceRecord::new(&self.identity(), self.own_copy(&signed)?)?;
        let write = self
            .app
            .record_op(
                &target,
                Some(&record.to_json()?),
                vec![pending::absent(self.app.record_key(&permanent)?)],
            )
            .await?;
        if !pending::is_create(&write.1) {
            return Err(Refusal::NotNow(
                "the new Allowance ID already has a pending record".into(),
            )
            .into());
        }
        let mut tx = Tx::default();
        self.note_peer(&mut tx, &to)?;
        let record_key = self.app.record_key(&permanent)?;
        if execution.is_some() {
            // The approval counts under the hold it is given under, so this batch, kept and
            // applied after a newer hold, authorizes no grant (Paykit spec 7.2, 8.3).
            tx.put(
                &key::execution_approval(&record_key, self.current_hold().await?.as_ref()),
                &true,
            )?;
        }
        // Each proposer keeps its own create in the service generation that applies it.
        tx.put(
            &key::original_allowance(&record_key),
            &OriginalAllowance::new(
                to,
                record.proposal(),
                self.app.own_identity().generation(),
                execution.is_some(),
            )
            .with_pending_create(self.identity(), self.app_id(), &write)?,
        )?;
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(write)
            .queue(signed)
            .locals(tx.into_writes());
        let delivery = match Box::pin(self.submit(batch)).await {
            Ok(submitted) => {
                if role == AllowanceRole::Allower {
                    let [proof] = submitted.proofs.as_slice() else {
                        return Err(PaykitError::corrupt());
                    };
                    Box::pin(self.keep_original_create(&record_key, proof)).await?;
                }
                Box::pin(self.delivered(&msg_id)).await?
            }
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

    /// Deliver the original installation's owed crossing ends (Paykit spec 8.1, 8.3).
    /// The record write created the local duty and named the exact end it owes. Keep it
    /// while the record cannot be read, the current sending scopes or deny state hold
    /// delivery, or a store error prevents the decision. A readable record that lacks
    /// the named end is corrupt. A delete, a conflict or the end's replay deadline
    /// retires the duty. A restore reconciles record-only delivery separately (spec 7.2).
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
            // both parties already stop the Allowance on this conflict.
            if self.unresolved_for_allowance(&owed.record, &held).await? {
                continue;
            }
            if self.past_replay_window(end)
                || held.conflicted.is_some()
                || Box::pin(self.conflict_kept(&owed.record)).await?
            {
                Box::pin(self.local_delete(&mark)).await?;
                continue;
            }
            if !Box::pin(self.may_queue_stored_end(end.message(), end.hash())).await? {
                continue;
            }
            Box::pin(self.queue_stored_again(end)).await?;
            Box::pin(self.local_delete(&mark)).await?;
        }
        // The reply placement phase can create a proved withdrawal. Deliver it
        // here, after that read and its conditional write have finished.
        for queued in self
            .app
            .queued_messages(crate::protocol::BACKUP_NAME)
            .await?
        {
            if queued.kind == "paykit.allowance_end"
                && queued.kept_batch.is_none()
                && !queued.resend
            {
                let origin = self
                    .local_get::<EndOrigin>(&key::end_origin(&queued.msg_id))
                    .await?;
                if origin.is_some_and(|o| {
                    o.cause == EndCause::ProviderLoss && o.message_hash == queued.hash
                }) {
                    Box::pin(self.delivered(&queued.msg_id)).await?;
                }
            }
        }
        Ok(())
    }

    /// Retire an end's fact only after every source of its delivery has ended (Switch
    /// spec 11). A positively bound pre-queue owed end keeps it even before the client
    /// has the message. Every retained sent or failed delivery keeps it too.
    ///
    /// The host must hold uninterrupted ownership of the whole App namespace, across all
    /// handles and processes, with prior operations finished and uncertain local writes
    /// resolved. The mutable borrow alone cannot enforce that store contract. No producer
    /// may run between these reads and the local commit; a cancelled future is not a
    /// completed store operation. Errors retain the remaining facts for normal reload.
    pub(crate) async fn retire_end_origins(&mut self) -> Result<()> {
        let origins = self.local_scan::<EndOrigin>(key::END_ORIGINS).await?;
        if origins.is_empty() {
            return Ok(());
        }
        let restore = self
            .local_get::<crate::hold::OwnResendOwed>(key::OWN_RESEND_OWED)
            .await?;
        let queued = self.queued_message_ids().await?;
        for (_, origin) in origins {
            if restore
                .as_ref()
                .is_some_and(|owed| owed.includes(&origin.counterparty))
            {
                continue;
            }
            if self
                .local_get::<PreparedPendingReply>(&key::pending_reply(
                    &origin.record,
                    &origin.message_hash,
                ))
                .await?
                .is_some_and(|prepared| {
                    prepared
                        .message
                        .stored(&origin.counterparty)
                        .is_ok_and(|signed| signed.hash() == origin.message_hash)
                })
            {
                continue;
            }
            if self
                .local_get::<OwedEnd>(&key::owed_end(&origin.record))
                .await?
                .is_some_and(|owed| owed.msg_id == origin.msg_id)
                || queued.contains(&origin.msg_id)
                || self.app.delivery(&origin.msg_id).await?.is_some()
            {
                continue;
            }
            // A crossing end held before queueing is absent from the client's delivery
            // index. Its batch can still install the owed end, even after a lost answer.
            let batch_key = key::end_batch(&origin.msg_id);
            if let Some(id) = self.local_get::<[u8; 16]>(&batch_key).await? {
                if matches!(
                    self.app.kept_batch(id).await?,
                    Some(pubky_switch_client::app::KeptBatchStatus::Kept { .. })
                ) {
                    continue;
                }
            }
            let mut tx = state::Tx::default();
            tx.delete(&key::end_origin(&origin.msg_id))
                .delete(&batch_key);
            self.local_commit(tx).await?;
        }
        Ok(())
    }

    /// Accept the recipient's selected proposal. The accepting wallet records the signed
    /// pair and removes the exact pending proposal atomically. On the payer's account the
    /// same batch creates the spending ledger at zero, bound to this acceptance.
    ///
    /// # Errors
    ///
    /// A stale, stopped or conflicted proposal or an uncertain current record cannot be
    /// accepted. A seller accepts using this method. The payer uses [`Self::accept_executable_allowance`].
    /// Both approval and contacts scopes are required to send the signed reply.
    pub async fn accept_allowance(&mut self, record: &[u8; 32]) -> Result<String> {
        self.accept_allowance_with_consent(record, None).await
    }

    /// Accept the seller's displayed Allowance and authorize this wallet to execute it.
    /// Show the counterparty, terms and automatic payments by this app before confirmation.
    /// Returns the original response ID.
    ///
    /// # Errors
    ///
    /// As [`Self::accept_allowance`]. Only the payer can authorize automatic execution.
    pub async fn accept_executable_allowance(
        &mut self,
        record: &[u8; 32],
        confirmed: crate::UserConfirmed,
    ) -> Result<String> {
        Box::pin(self.accept_allowance_with_consent(record, Some(confirmed))).await
    }

    pub(crate) async fn accept_allowance_with_consent(
        &mut self,
        record: &[u8; 32],
        execution: Option<crate::UserConfirmed>,
    ) -> Result<String> {
        Box::pin(self.answer_allowance(record, Kind::AllowanceAcceptance, execution)).await
    }

    /// Reject the Allowance proposed to this identity at `record` (Paykit spec 8.3, Paykit
    /// wire 4.16): the rejection is recorded, then sent. Only the proposal's recipient
    /// answers it, once. Returns the rejection's `msg_id`. A rejected Allowance has no
    /// end. A rejection is a stop: it goes over a record seen divergent as over any other
    /// (Paykit spec 6). Rejecting requires both `paykit_allowance` and `paykit_contacts`.
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
        Box::pin(self.answer_allowance(record, Kind::AllowanceRejection, None)).await
    }

    /// End the Allowance at `record` (Paykit spec 8.3 "Ending", Paykit wire 4.16): an
    /// accepted one, by either side, naming the proposal and the acceptance; or, before any
    /// answer, the proposal's withdrawal by this identity, its sender. The end is recorded,
    /// then sent, with an `executor_notice` to the executor the record's grant names when
    /// that is another own app (Paykit wire 4.17). The records stay as history. Returns the
    /// end's `msg_id`. An app with `paykit_allowance` can end it even after the signer
    /// removes its contacts permission.
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
        Box::pin(self.answer_allowance(record, Kind::AllowanceEnd, None)).await
    }

    /// Answer the current pending proposal, or record an end of the accepted Allowance
    /// at the same stable host key (Paykit spec 8.3.1).
    async fn answer_allowance(
        &mut self,
        record: &[u8; 32],
        kind: Kind,
        execution: Option<crate::UserConfirmed>,
    ) -> Result<String> {
        self.refresh_own()?;
        Box::pin(self.first_read_before_writing()).await?;
        self.refresh_own()?;
        self.require_allowance_send(kind)?;
        Box::pin(self.records_known()).await?;
        // A record that can't be opened yet may hold a response or an end: nothing is
        // answered or ended over it until it opens (Paykit spec 8.3, one response).
        Box::pin(self.refuse_while_awaiting_key(record)).await?;
        if kind == Kind::AllowanceEnd {
            Box::pin(self.retire_end_origins()).await?;
        }
        if let Some(msg_id) = Box::pin(self.resume_pending_answer(record, kind)).await? {
            return Ok(msg_id);
        }
        if let Some((selected, _)) = Box::pin(self.pending_by_allowance_key(record)).await? {
            return Box::pin(self.answer_pending(&selected, kind, execution, None)).await;
        }
        let mut tries = 0;
        loop {
            let held = self
                .held_allowance(record)
                .await?
                .ok_or(Refusal::NotFound)?;
            // Nothing is sent for a conflicted Allowance (Paykit spec 8.3, "Conflicted").
            if self.unresolved_for_allowance(record, &held).await? {
                return Err(Refusal::NotNow("Allowance conflict check pending".into()).into());
            }
            if held.conflicted.is_some() || Box::pin(self.conflict_kept(record)).await? {
                return Err(Refusal::AllowanceConflicted.into());
            }
            if held.end.is_some() || Box::pin(self.allowance_stopped(record)).await? {
                return Err(Refusal::AllowanceEnded.into());
            }
            if kind != Kind::AllowanceEnd {
                return Err(Refusal::Usage("there is no pending proposal to answer".into()).into());
            }
            let accepted = acceptance(&held).ok_or_else(|| {
                if rejected(&held) {
                    Refusal::Rejected
                } else {
                    Refusal::NotNow("the permanent Allowance has no verified acceptance".into())
                }
            })?;
            let refs = vec![held.proposal.hash(), accepted.hash()];
            let body = AllowanceReply {
                allowance_id: held.allowance_id.clone(),
                executor_app_id: None,
            }
            .to_body(kind)?;
            let signed = self.sign(held.counterparty, kind, refs, body)?;
            let msg_id = signed.msg_id().to_owned();
            let stored = self.own_copy(&signed)?;
            let origin = EndOrigin::of(
                *record,
                &held.proposal,
                Some(accepted),
                &stored,
                EndCause::User,
            )?;
            let mut next = held.clone();
            place(&mut next, stored);
            if !reflects_stored(&next, &msg_id) {
                return Err(PaykitError::corrupt());
            }
            let mut locals = Vec::new();
            // A backup omits unresolved batches. Keep the signed end's origin before
            // the service can apply the record write and its answer can be lost.
            Box::pin(self.local_put(&key::end_origin(&msg_id), &origin)).await?;
            let mut queue = vec![signed];
            self.end_notice(&held, record, &mut queue, &mut locals)?;
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
                        let _ = Box::pin(self.send_message(&notice)).await;
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
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(write)
            .locals(locals);
        if let Some(owed) = owed {
            let marker = key::end_batch(&owed.msg_id).into_bytes();
            batch = batch
                .keep_id_at(marker.clone())
                .local(LocalWrite::Delete { key: marker });
        }
        for message in queue {
            batch = batch.queue(message);
        }
        match Box::pin(self.submit(batch)).await {
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
    /// naming the app it is wrapped to; and, at an app holding `paykit_allowance_execute`, its own
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
        if self.holds("paykit_allowance_execute") {
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
        out.extend(Box::pin(self.pending_views(None, false)).await?);
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
        if let Some(held) = self.app.shared_record(record).await? {
            return self
                .deleted_conflict_view(record, &held.content)
                .await?
                .ok_or_else(|| Refusal::NotFound.into());
        }
        Box::pin(self.pending_views(Some(record), false))
            .await?
            .pop()
            .ok_or_else(|| Refusal::NotFound.into())
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
            confirmation_needed: false,
            granted_before_loss: false,
            hold: self.allowance_hold(record).await?,
            blocked: Box::pin(self.blocked_as_known(&stored.sender())).await?,
        }))
    }

    async fn allowance_view(
        &self,
        record: [u8; 32],
        held: &AllowanceRecord,
    ) -> Result<AllowanceView> {
        let status = status(
            held,
            Box::pin(self.allowance_stopped(&record)).await?,
            Box::pin(self.conflict_kept(&record)).await?,
        );
        let unresolved = !stopped(status) && self.unresolved_for_allowance(&record, held).await?;
        let status = shown_status(status, unresolved);
        let mut view = Box::pin(self.proposal_view(record, &held.proposal, status)).await?;
        view.executor_app_id = held
            .current_grant(&self.identity())
            .map(|(grant, _)| grant.app_id);
        // The generation the user last confirmed it in: its grant's, or before a grant its
        // proposal's. One a provider loss ended pays nothing automatically (Paykit spec 8.3).
        let confirmed = held
            .current_grant(&self.identity())
            .map(|(grant, _)| grant.confirmed_generation)
            .or(match held.proposal.body() {
                Body::AllowanceProposal(proposal) => Some(proposal.allower_generation),
                _ => None,
            });
        view.granted_before_loss = confirmed
            .zip(self.app.own_identity().body().recovery_from)
            .is_some_and(|(confirmed, lost)| confirmed <= lost);
        let consent = Box::pin(self.consent_state(&record)).await?;
        view.consented = consent == ConsentState::Held;
        view.confirmation_needed = matches!(
            crate::executor::standing(self, &view, consent),
            crate::executor::Standing::Confirm
        );
        Ok(view)
    }

    async fn proposal_view(
        &self,
        record: [u8; 32],
        signed: &StoredMessage,
        status: AllowanceStatus,
    ) -> Result<AllowanceView> {
        let Body::AllowanceProposal(proposal) = signed.body() else {
            return Err(PaykitError::corrupt());
        };
        let proposed_here = signed.sender() == self.identity();
        let counterparty = if proposed_here {
            signed.message().to
        } else {
            signed.sender()
        };
        let role = match (proposal.proposer_role, proposed_here) {
            (ProposerRole::Allower, true) | (ProposerRole::Allowee, false) => {
                AllowanceRole::Allower
            }
            _ => AllowanceRole::Allowee,
        };
        Ok(AllowanceView {
            record,
            counterparty,
            allowance_id: proposal.allowance_id.clone(),
            role: Some(role),
            proposed_here,
            terms: Some(proposal.terms.clone()),
            status,
            executor_app_id: None,
            consented: false,
            confirmation_needed: false,
            granted_before_loss: false,
            hold: self.allowance_hold(&record).await?,
            blocked: Box::pin(self.blocked_as_known(&counterparty)).await?,
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
        let mut out: Vec<_> = Box::pin(self.pending_views(None, true))
            .await?
            .into_iter()
            .filter(|view| view.status == AllowanceStatus::Proposed)
            .map(|view| view.counterparty)
            .collect();
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
                if open && !self.unresolved_for_allowance(&held.key, &record).await? {
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
        Ok(Box::pin(self.consent_state(record)).await? == ConsentState::Held)
    }

    /// Where this app's consent to execute the Allowance at `record` stands, by the rules of
    /// [`Paykit::consents`], which holds it only at [`ConsentState::Held`]. The other states
    /// tell why it doesn't: one the user can lift, one a later read can, or one that is final.
    pub(crate) async fn consent_state(&self, record: &[u8; 32]) -> Result<ConsentState> {
        // A stop, a kept conflict and a removal this app recorded are final, whatever the
        // record shows now or a `missed` item's mark holds.
        if Box::pin(self.allowance_stopped(record)).await? {
            return Ok(ConsentState::Removed(ConsentEnd::Ended));
        }
        if Box::pin(self.conflict_kept(record)).await? {
            return Ok(ConsentState::Removed(ConsentEnd::Conflicted));
        }
        if let Some(ConsentEntry {
            consent: Consent::Removed { reason },
            ..
        }) = self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?
        {
            return Ok(ConsentState::Removed(reason));
        }
        if Box::pin(self.consents_unconfirmed()).await? {
            return Ok(ConsentState::Waiting(WAITS_FOR_MISSED));
        }
        let Some(shared) = Box::pin(self.app.shared_record(record)).await? else {
            return Ok(ConsentState::Waiting(WAITS_FOR_RECORD));
        };
        if shared.flagged {
            return Ok(ConsentState::Waiting(WAITS_FOR_RECORD));
        }
        let shown = match Box::pin(self.held_allowance(record)).await? {
            Some(held) if held.end.is_some() => {
                return Ok(ConsentState::Removed(ConsentEnd::Ended))
            }
            Some(held) if held.conflicted.is_some() => {
                return Ok(ConsentState::Removed(ConsentEnd::Conflicted))
            }
            Some(held) if rejected(&held) => {
                return Ok(ConsentState::Removed(ConsentEnd::Rejected))
            }
            Some(held) => {
                if self.unresolved_for_allowance(record, &held).await? {
                    return Ok(ConsentState::Waiting(WAITS_FOR_RECORD));
                }
                held
            }
            None => return Ok(ConsentState::Waiting(WAITS_FOR_RECORD)),
        };
        let own_granted = shown
            .current_grant(&self.identity())
            .is_some_and(|(grant, _)| grant.app_id == self.app_id());
        let Some(held) = self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?
        else {
            return Box::pin(self.unconsented_state(record, own_granted)).await;
        };
        if Box::pin(self.rejection_kept(record, &held.counterparty)).await? {
            return Ok(ConsentState::Removed(ConsentEnd::Rejected));
        }
        Ok(match held.consent {
            Consent::Held {
                confirm_again: false,
            } => ConsentState::Held,
            Consent::Held {
                confirm_again: true,
            } => ConsentState::ConfirmAgain,
            Consent::Removed { reason } => ConsentState::Removed(reason),
        })
    }

    /// Where this app's consent stands with no consent entry kept: its first grant kept to
    /// send again; applied, its consent waiting for resume or the user; or no grant of its
    /// own. The grant's batch writes its history (`xc/`) only as it applies, and leaves its
    /// ID at `gb/` until then.
    async fn unconsented_state(
        &self,
        record: &[u8; 32],
        own_granted: bool,
    ) -> Result<ConsentState> {
        if let Some(id) = self
            .local_get::<[u8; 16]>(&key::grant_batch(record))
            .await?
        {
            if matches!(
                self.app.kept_batch(id).await?,
                Some(pubky_switch_client::app::KeptBatchStatus::Kept { .. })
            ) {
                return Ok(ConsentState::GrantKept);
            }
        }
        let approved = self.execution_approved(record).await?;
        if self
            .local_get::<crate::executor::ExecutorHistory>(&key::executor_checkpoint(record))
            .await?
            .is_none()
        {
            // No grant history here. One the user approved here for automatic execution, not
            // granted yet, whose approval a restore or a `missed` item took since, waits for
            // the user's confirmation, which grants it. One the record shows granted by this
            // app, after the backup this app was restored from, isn't: no confirmation lands
            // a grant this installation has no history of.
            let approved_here = self
                .local_get::<OriginalAllowance>(&key::original_allowance(record))
                .await?
                .filter(OriginalAllowance::opened)
                .map(|original| original.approved_for_execution())
                .transpose()?
                .flatten()
                .is_some();
            return Ok(if approved_here && !approved && !own_granted {
                ConsentState::Unconsented { approved: false }
            } else {
                ConsentState::Unrecorded
            });
        }
        Ok(ConsentState::Unconsented { approved })
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
        if let Some(now) = self.service_time() {
            for (rest, reply) in self
                .local_scan::<AppliedReply>(key::APPLIED_REPLIES)
                .await?
            {
                if now
                    > reply
                        .created_at_ms
                        .saturating_add(pubky_switch_client::app::SENT_RETENTION_MS)
                {
                    tx.delete(&format!("{}{rest}", key::APPLIED_REPLIES));
                }
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
        let mut listed = std::collections::HashSet::new();
        for (rest, entry) in Box::pin(self.local_scan::<ConsentEntry>(key::CONSENTS)).await? {
            let record = consent_record(&rest)?;
            listed.insert(record);
            if !matches!(entry.consent, Consent::Held { .. }) {
                continue;
            }
            let Some(reason) = Box::pin(self.stopped_as(&record)).await? else {
                continue;
            };
            tx.put(
                &key::consent(&record),
                &entry.with(Consent::Removed { reason }),
            )?;
        }
        // An Allowance this app granted, or approved for automatic execution, and sees
        // stopped before its consent is written is kept as a removal all the same (Paykit
        // spec 8.3, "even before it has added it").
        let mut unlisted = Vec::new();
        for (_, history) in
            Box::pin(self.local_scan::<crate::executor::ExecutorHistory>(key::EXECUTOR_CHECKPOINTS))
                .await?
        {
            if let Some(granted) = history.0.first() {
                let id = AllowanceId::parse(&granted.allowance_id)
                    .map_err(|_| PaykitError::corrupt())?;
                unlisted.push((granted.counterparty, id));
            }
        }
        for (_, approved) in
            Box::pin(self.local_scan::<OriginalAllowance>(key::ORIGINAL_ALLOWANCES)).await?
        {
            unlisted.extend(approved.approved_for_execution()?);
        }
        for (counterparty, id) in unlisted {
            let record = self.app.record_key(&allowance_target(&counterparty, &id))?;
            if !listed.insert(record) {
                continue;
            }
            if let Some(reason) = Box::pin(self.stopped_as(&record)).await? {
                tx.put(
                    &key::consent(&record),
                    &ConsentEntry::new(counterparty, &id, Consent::Removed { reason }),
                )?;
            }
        }
        Box::pin(self.local_commit(tx)).await
    }

    /// How the Allowance at `record` has stopped, as the record this app holds or a kept
    /// conflict shows it; `None` while it runs.
    async fn stopped_as(&mut self, record: &[u8; 32]) -> Result<Option<ConsentEnd>> {
        Ok(match Box::pin(self.held_allowance(record)).await? {
            Some(held) if held.conflicted.is_some() => Some(ConsentEnd::Conflicted),
            Some(held) if held.end.is_some() => Some(ConsentEnd::Ended),
            Some(held) if rejected(&held) => Some(ConsentEnd::Rejected),
            _ if Box::pin(self.conflict_kept(record)).await? => Some(ConsentEnd::Conflicted),
            _ => None,
        })
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
        if !self.holds("paykit_allowance")
            || Box::pin(self.scope_keys_awaited()).await?.is_some()
            || Box::pin(self.side_unlisted(RecordSide::Allowances)).await?
        {
            return Ok(());
        }
        Box::pin(self.resolve_allowance_evidence()).await?;
        Box::pin(self.place_pending_seconds()).await?;
        let own = self.identity();
        // Read bound to its key (`state::Stored`). The ends of one Allowance go in the order
        // their inbox accepted them (`records::KeptOrder`): once one ends it, the later ones
        // are reflected or dropped.
        let mut ends = Vec::new();
        for (rest, kept) in Box::pin(self.local_scan::<KeptMessage>(key::ALLOWANCE_ENDS)).await? {
            let end = kept.stored(&own)?;
            let record = rest.split_once('/').map(|(record, _)| record.to_owned());
            ends.push((record, kept.order()?, rest, end, kept));
        }
        ends.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
        for (_, _, rest, end, evidence) in ends {
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
                    if end.message().refs.len() == 1 {
                        if let Some(result) =
                            Box::pin(self.remove_withdrawn_proposal(&end, &evidence, &stop)).await?
                        {
                            if result == Placed::Done {
                                Box::pin(self.local_delete(&stop)).await?;
                            }
                            continue;
                        }
                    }
                    if end.message().refs.len() == 2
                        && Box::pin(self.finish_held_loss_return(
                            &end,
                            &record,
                            &evidence,
                            Some(&stop),
                        ))
                        .await?
                    {
                        continue;
                    }
                    if let Some(source) = evidence
                        .ended_acceptance
                        .as_ref()
                        .and_then(|q| q.pending_source)
                    {
                        let pending = allowance_record_target(
                            Collection::AllowanceProposals,
                            &end.sender(),
                            &allowance_id,
                        );
                        let key = self.app.record_key(&pending)?;
                        let retired = self
                            .local_get::<PendingRetirement>(&key::pending_retirement(&key))
                            .await?;
                        if retired.is_none_or(|r| !r.retires(source)) {
                            continue;
                        }
                        // A sibling may have created the permanent pair in the same batch
                        // that retired P. Keep the end until that record can be checked.
                        let read = self.app.read_record_fresh(&target).await?;
                        if delivery::current(&target, &record, &read, false) == Some(None) {
                            self.local_delete(&stop).await?;
                        }
                        continue;
                    }
                    if stale(&end, self.service_time())?
                        && !Box::pin(self.keeps_stored_message(&end.sender(), &record)).await?
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
                Box::pin(
                    self.app
                        .protocol_commit(crate::protocol::BACKUP_NAME, &locals),
                )
                .await?;
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
        if self.unresolved_for_allowance(&record, &held).await? {
            return Ok(());
        }
        let conflicted = held.conflicted.is_some() || Box::pin(self.conflict_kept(&record)).await?;
        let mut next = held.clone();
        let mut queue = Vec::new();
        let mut owed = None;
        let mut locals = Vec::new();
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
                    Box::pin(self.retire_end_origins()).await?;
                    // The signed end and own-app notice always record the payment stop.
                    // Its separate counterparty delivery needs current authorization.
                    let may_send =
                        Box::pin(self.allowance_end_send_permitted(&held.counterparty)).await?;
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
                    let stored = self.own_copy(&own_end)?;
                    let origin = EndOrigin::of(
                        record,
                        &held.proposal,
                        acceptance(&held),
                        &stored,
                        EndCause::Automatic,
                    )?;
                    Box::pin(self.local_put(&key::end_origin(own_end.msg_id()), &origin)).await?;
                    next = held.clone();
                    place(&mut next, stored);
                    place(&mut next, end.clone());
                    // While sends to the counterparty are held (Paykit spec 8.1, "Blocked
                    // counterparties"), the end is recorded all the same, with its executor
                    // notice to the own apps, so every own app finds the Allowance ended and
                    // a batch built on the record's earlier version fails; only its message
                    // to the counterparty waits, owed (`eo/`), sent from the record once
                    // sends to it aren't held (Paykit spec 8.3, "Ending").
                    if !may_send {
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
                        && !Box::pin(self.keeps_stored_message(&end.sender(), &record)).await?
                    {
                        Box::pin(self.local_delete(stop)).await?;
                    }
                    return Ok(());
                }
                // Any other end isn't one of this Allowance (Paykit wire 4.16).
                _ => return Box::pin(self.local_delete(stop)).await,
            }
        }
        self.end_notice(&held, &record, &mut queue, &mut locals)?;
        locals.push(LocalWrite::Delete {
            key: stop.as_bytes().to_vec(),
        });
        let sent: Vec<String> = queue.iter().map(|m| m.msg_id().to_owned()).collect();
        Box::pin(self.submit_with_record(target, &next, queue, locals, owed.as_ref())).await?;
        // The write went through with the messages queued in its batch: one not delivered
        // now goes with the next flush, and the write's outcome doesn't wait for it.
        for msg_id in sent {
            let _ = Box::pin(self.send_message(&msg_id)).await;
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
    pub(crate) async fn place_kept_allowance_responses(
        &mut self,
        pending_read: Option<&pubky_switch_client::app::ReceiveReport>,
    ) -> Result<()> {
        let own = self.identity();
        let mut kept = Vec::new();
        for (rest, message) in Box::pin(self.local_scan::<KeptMessage>(key::KEPT_REPLIES)).await? {
            // Read bound to its key, as a restore reads it: a message that fails it is a
            // corrupt store, so the receive fails rather than drop, give up or place it.
            let answer = message.stored(&own)?;
            if placed_with_allowance(answer.body()) {
                kept.push((message.order()?, rest, answer, message));
            }
        }
        kept.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for (_, rest, answer, message) in kept {
            let kept_key = format!("{}{rest}", key::KEPT_REPLIES);
            Box::pin(self.place_kept_allowance_response(&kept_key, answer, &message, pending_read))
                .await?;
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
        evidence: &KeptMessage,
        pending_read: Option<&pubky_switch_client::app::ReceiveReport>,
    ) -> Result<()> {
        if Box::pin(self.side_unlisted(RecordSide::Allowances)).await? {
            return Ok(());
        }
        let Some(allowance_id) = named_allowance(reply.body()).cloned() else {
            return Err(PaykitError::corrupt());
        };
        let accepted_at = reply.accepted_at().ok_or_else(PaykitError::corrupt)?;
        if matches!(reply.body(), Body::AllowanceProposal(_)) {
            if let Some(placed) =
                Box::pin(self.place_incoming_proposal(&reply, evidence, None, pending_read)).await?
            {
                match crate::stored::later_write(placed, accepted_at, self.service_time()) {
                    crate::stored::LaterWrite::Done => self.local_delete(kept_key).await?,
                    crate::stored::LaterWrite::GiveUp => self.give_up(kept_key, &reply).await?,
                    _ => {}
                }
                return Ok(());
            }
        }
        let record = self
            .app
            .record_key(&allowance_target(&reply.sender(), &allowance_id))?;
        let content = self
            .app
            .shared_record(&record)
            .await?
            .map(|held| held.content);
        if !receive::holds_record(content.as_ref()) {
            match Box::pin(self.pending_reply_later(&record, &reply, evidence)).await? {
                replies::PendingReply::Stopped => return self.local_delete(kept_key).await,
                replies::PendingReply::Undecided => return Ok(()),
                replies::PendingReply::Waits(version) => {
                    if crate::stored::past_window(accepted_at, self.service_time()) {
                        self.give_up(kept_key, &reply).await?;
                    } else if let Some(version) = version {
                        if let Some(mut kept) = self.local_get::<KeptMessage>(kept_key).await? {
                            if kept.pending_source.is_none_or(|source| source < version) {
                                kept.pending_source = Some(version);
                                self.local_put(kept_key, &kept).await?;
                            }
                        }
                    }
                    return Ok(());
                }
                replies::PendingReply::Unrelated => {}
            }
            if crate::stored::past_window(accepted_at, self.service_time()) {
                Box::pin(self.local_delete(kept_key)).await?;
            }
            return Ok(());
        }
        let drop_kept = LocalWrite::Delete {
            key: kept_key.as_bytes().to_vec(),
        };
        let placed = self
            .place_allowance_later(reply.clone(), evidence, vec![drop_kept])
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
        evidence: &KeptMessage,
        mut locals: Vec<LocalWrite>,
    ) -> Result<()> {
        let kept = Box::pin(self.conflict_kept(record)).await?;
        let consent = self
            .local_get::<ConsentEntry>(&key::consent(record))
            .await?;
        locals.extend(conflict_writes(
            record,
            stored,
            evidence,
            kept,
            consent.as_ref(),
            self.holds("paykit_allowance_execute"),
        )?);
        Ok(Box::pin(
            self.app
                .protocol_commit(crate::protocol::BACKUP_NAME, &locals),
        )
        .await?)
    }

    /// Try to place a stored entry's proposal, acceptance or rejection in its Allowance
    /// record using its inbox order `order` ([`AnswerOrder`]). [`Paykit::place_allowance_later`]
    /// keeps an acceptance that would fill the empty `response` unplaced.
    pub(crate) async fn place_allowance_note(
        &mut self,
        stored: StoredMessage,
        evidence: &KeptMessage,
        stored_id: u64,
        pending_read: Option<&pubky_switch_client::app::ReceiveReport>,
    ) -> Result<Placed> {
        if matches!(stored.body(), Body::AllowanceProposal(_)) {
            if let Some(placed) = Box::pin(self.place_incoming_proposal(
                &stored,
                evidence,
                Some(stored_id),
                pending_read,
            ))
            .await?
            {
                return Ok(placed);
            }
        }
        Box::pin(self.place_allowance_later(stored, evidence, Vec::new())).await
    }

    /// The answers of the Allowance record at `record` that the app keeps to write, from
    /// the sender of `stored` ([`answers_kept`]), outside the receive step.
    async fn answers_kept(
        &self,
        record: &[u8; 32],
        stored: &StoredMessage,
    ) -> Result<Vec<HeldAnswer>> {
        let own = self.identity();
        let sender = stored.sender();
        let before =
            Box::pin(self.local_scan::<KeptMessage>(&key::kept_reply(&sender, record, "")))
                .await?
                .into_iter()
                .map(|(_, kept)| kept)
                .collect();
        let prefix = key::stored_notes_of(&sender);
        let notes = Box::pin(self.local_scan(&prefix)).await?;
        let stop = Box::pin(self.local_get::<KeptMessage>(&key::conflict(record))).await?;
        answers_kept(&own, named_allowance(stored.body()), before, notes, stop)
    }

    /// The later write of an Allowance's proposal, acceptance or rejection kept to write, a
    /// stored entry's (`se/`) or an answer kept before its record (`pr/`), with `locals` in
    /// the commit or batch that places it: the one write both paths share. An answer with
    /// no permanent record is handled by the pending-proposal flow. Only the original
    /// acceptance can create the permanent record.
    ///
    /// - An answer never takes the response while a different one its inbox accepted
    ///   before it, by `order`, is still kept to write ([`answered_before`]): it conflicts
    ///   with that one, so it is done, kept as the conflict's stop (`cf/`) in the commit of
    ///   `locals`, a rejection's stop with it, and goes into `conflicted` once the earlier
    ///   one is the record's response (Paykit spec 8.1, 8.3).
    /// - It waits while the copy of `allowances` may lack records ([`Placed::Waits`]), never
    ///   given up meanwhile, whatever else holds.
    /// - With no permanent record, a stopped or applied pending reply is done
    ///   ([`Placed::Done`]); an undecided reply waits ([`Placed::Waits`]); an unrelated or
    ///   still-unplaced reply isn't written now ([`Placed::Not`]). A proposal without a
    ///   permanent record isn't written by this path either.
    /// - It isn't written now ([`Placed::Not`]) while the current epoch's `paykit_allowance`
    ///   key is missing or the record waits for its key, for an answer over a deleted record,
    ///   or when the switch refuses the write ([`crate::stored::submitted`]). The caller
    ///   applies the retry, retention and give-up rules; this result alone doesn't give up
    ///   the message (Paykit spec 8.1, "Stored entries").
    /// - A permanent value that cannot be read or bound is never overwritten. Before the
    ///   30-day placement window ends, measured from the message's original Switch acceptance
    ///   using verified service time, [`Self::expire_allowance_placement`] returns
    ///   [`Placed::Not`]. At or after expiry it commits retained conflict evidence before
    ///   returning [`Placed::Done`]. First retention queues
    ///   [`crate::NoticeKind::AllowancePlacementExpired`]. The conflict
    ///   hold remains. [`Self::place_allowance_note`] uses the same expiry helper for an
    ///   undecided second proposal.
    /// - One the record reflects already, or that makes the Allowance conflicted, is done:
    ///   the conflict is kept as its stop (`cf/`) in the commit of `locals`.
    /// - An acceptance that would fill an existing permanent record's empty response
    ///   ([`would_accept`]) isn't written now ([`Placed::Not`]). The first acceptance belongs
    ///   to the original conditional batch (Paykit spec 8.3.1).
    /// - A failure of the app's own store, or a switch that doesn't answer, is the call's
    ///   error, never a message not written.
    async fn place_allowance_later(
        &mut self,
        stored: StoredMessage,
        evidence: &KeptMessage,
        locals: Vec<LocalWrite>,
    ) -> Result<Placed> {
        let order = evidence.order()?;
        let Some(allowance_id) = named_allowance(stored.body()).cloned() else {
            return Err(PaykitError::corrupt());
        };
        if Box::pin(self.side_unlisted(RecordSide::Allowances)).await? {
            return Ok(Placed::Waits);
        }
        if self
            .app
            .scope_key(self.app.data_epoch(), "paykit_allowance")
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
            None if is_allowance_response(stored.body()) => {
                match Box::pin(self.pending_reply_later(&record, &stored, evidence)).await? {
                    replies::PendingReply::Stopped => {
                        self.app
                            .protocol_commit(crate::protocol::BACKUP_NAME, &locals)
                            .await?;
                        return Ok(Placed::Done);
                    }
                    replies::PendingReply::Undecided => return Ok(Placed::Waits),
                    replies::PendingReply::Waits(_) | replies::PendingReply::Unrelated => None,
                }
            }
            // Only accepting the original can create the permanent Allowance record.
            None => None,
            // A proposal over a delete is conflicted, kept as the stop with nothing written
            // at the key, as at its first read (Paykit spec 8.3, "Ending"); an answer waits
            // for a record.
            Some(RecordContent::Deleted) => match stored.body() {
                Body::AllowanceProposal(_) => {
                    Box::pin(self.keep_conflict_later(&record, &stored, evidence, locals)).await?;
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
                    Box::pin(
                        self.app
                            .protocol_commit(crate::protocol::BACKUP_NAME, &locals),
                    )
                    .await?;
                    return Ok(Placed::Done);
                }
                Some(held) if held.conflicted.is_none() && conflicts(&held, &stored) => {
                    Box::pin(self.keep_conflict_later(&record, &stored, evidence, locals)).await?;
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
                    Box::pin(self.keep_conflict_later(&record, &stored, evidence, locals)).await?;
                    return Ok(Placed::Done);
                }
                // An existing permanent record cannot acquire its first response here.
                // The first acceptance belongs to the original conditional batch.
                Some(held) if would_accept(&held, &stored) => return Ok(Placed::Not),
                // A proposal waits over a divergent record until the user confirms it, as at
                // receive; a rejection goes.
                Some(held)
                    if !reflects(&held, &stored.message().msg_id)
                        && Box::pin(self.divergent_write_waits(&record, &stored, Some(order)))
                            .await? =>
                {
                    return Ok(Placed::Not);
                }
                Some(mut held) => {
                    if !place(&mut held, stored) {
                        Box::pin(
                            self.app
                                .protocol_commit(crate::protocol::BACKUP_NAME, &locals),
                        )
                        .await?;
                        return Ok(Placed::Done);
                    }
                    Some(held)
                }
                None => {
                    return Box::pin(
                        self.expire_allowance_placement(&record, None, &stored, evidence, locals),
                    )
                    .await
                }
            },
            Some(_) => {
                return Box::pin(
                    self.expire_allowance_placement(&record, None, &stored, evidence, locals),
                )
                .await
            }
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

/// This app's signed Allowance messages whose delivery may need reconciliation after
/// restore (Paykit spec 7.2 and 8.3). A response closes a proposal, and an end closes
/// both the proposal and the rejection. Conflicted Allowances and acceptances are
/// excluded. The caller also checks for stops the app has kept outside the record.
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
        (
            record.response.as_ref(),
            !ended && acceptance(record).is_none(),
        ),
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
/// there establishes original delivery intent.
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

    /// A partial value used only to build the signed proposal and acceptance in memory.
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
            ledger_handoff: None,
            ledger_handoff_unread: None,
            msg_ids: Vec::new(),
            unknown: Default::default(),
        };
        assert!(record.to_json().is_err());
        Some(record)
    }

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
        let alice = Party::new(
            "alice",
            "wallet",
            &["paykit_pay", "paykit_allowance", "paykit_contacts"],
        );
        let bob = Party::new(
            "bob",
            "shop",
            &[
                "paykit_request",
                "paykit_allowance",
                "paykit_settlements",
                "paykit_contacts",
            ],
        );
        let proposal = bob.message(
            &alice,
            "paykit.allowance_proposal",
            vec![],
            object(json!({ "allowance_id": ID, "proposer_role": "allowee", "allower_generation": 1, "terms": terms() })),
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
        let (code, _) = include_str!("allowances.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .unwrap();
        assert_eq!(code.matches(".record_op(").count(), 2);
        assert_eq!(code.matches("cx.put_record(").count(), 1);
        assert_eq!(code.matches("stores_unqueued(").count(), 2);
        assert_eq!(code.matches(".queue(signed)").count(), 1);
        let pending = include_str!("allowances/pending.rs");
        assert!(!pending.contains(".put_record("));
        // In addition to pending writes, the accepting account prepares its permanent
        // Allowance and the payer prepares its original spending ledger.
        assert_eq!(pending.matches(".record_op(").count(), 6);
        assert_eq!(pending.matches("batch.queue(reply)").count(), 1);
        assert!(pending.contains("if is_fenced { batch } else { batch.queue(reply) }"));
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
        let (alice, bob, mut record) = proposed();
        let acceptance = answer(
            &alice,
            &bob,
            "paykit.allowance_acceptance",
            vec![record.proposal.hash()],
        );
        assert!(place(&mut record, as_read(&alice, &bob, &acceptance)));
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

#[cfg(test)]
mod consent_state_tests {
    use super::*;
    use crate::testkit::Party;
    use pubky_switch_core::enc::{b64, hash};
    use serde_json::{json, Value};

    const ID: &str = "d0efffb7-191b-4709-b6c7-e343af5500b4";

    fn read<T: state::Stored>(key: &str, data: &Value) -> Result<T> {
        let party = Party::new(
            "alice",
            "executor",
            &[
                "paykit_pay",
                "paykit_allowance",
                "paykit_contacts",
                "paykit_allowance_execute",
            ],
        );
        state::restored(
            key.as_bytes(),
            &serde_json::to_vec(&json!({"v": 1, "d": data})).unwrap(),
            &state::Binder::new(party.id(), &party.cert, &hash),
        )
    }

    fn consent(consent: Consent) -> (String, Value) {
        let id = AllowanceId::parse(ID).unwrap();
        let target = allowance_target(&[3; 32], &id);
        let record = pubky_switch_core::ident::record_key_from_blinded(
            &target.collection.name,
            &hash(&target.natural_key),
            &target.collection.policy.hash().unwrap(),
        );
        (
            key::consent(&record),
            serde_json::to_value(ConsentEntry::new([3; 32], &id, consent)).unwrap(),
        )
    }

    #[test]
    fn test_consent_state_valid_variants_round_trip() {
        for value in [
            Consent::Held {
                confirm_again: false,
            },
            Consent::Held {
                confirm_again: true,
            },
            Consent::Removed {
                reason: ConsentEnd::Ended,
            },
            Consent::Removed {
                reason: ConsentEnd::Rejected,
            },
            Consent::Removed {
                reason: ConsentEnd::HandedOff,
            },
            Consent::Removed {
                reason: ConsentEnd::Conflicted,
            },
        ] {
            let (key, data) = consent(value);
            let entry = read::<ConsentEntry>(&key, &data).unwrap();
            assert_eq!(entry.consent, value);
            assert_eq!(serde_json::to_value(entry).unwrap(), data);
            let expected = match value {
                Consent::Held { .. } => Consent::Held {
                    confirm_again: true,
                },
                removed => removed,
            };
            assert_eq!(value.confirm_again(), expected);
        }
    }

    #[test]
    fn test_consent_state_entry_unknown_member_refused() {
        let (key, mut data) = consent(Consent::Held {
            confirm_again: false,
        });
        assert!(read::<ConsentEntry>(&key, &data).is_ok());
        data["unexpected"] = json!(true);
        assert!(read::<ConsentEntry>(&key, &data).is_err());
    }

    #[test]
    fn test_consent_state_held_unknown_member_refused() {
        let (key, mut data) = consent(Consent::Held {
            confirm_again: false,
        });
        assert!(read::<ConsentEntry>(&key, &data).is_ok());
        data["consent"]["held"]["unexpected"] = json!(true);
        assert!(read::<ConsentEntry>(&key, &data).is_err());
    }

    #[test]
    fn test_consent_state_removed_unknown_member_refused() {
        let (key, mut data) = consent(Consent::Removed {
            reason: ConsentEnd::Ended,
        });
        assert!(read::<ConsentEntry>(&key, &data).is_ok());
        data["consent"]["removed"]["unexpected"] = json!(true);
        assert!(read::<ConsentEntry>(&key, &data).is_err());
    }

    #[test]
    fn test_consent_state_notice_unknown_member_refused() {
        let target = NoticeTarget {
            msg_id: ID.to_owned(),
            app_id: [4; 16],
        };
        let key = key::notice_target(ID);
        let mut data = serde_json::to_value(&target).unwrap();
        assert_eq!(
            read::<NoticeTarget>(&key, &data).unwrap().app_id,
            target.app_id
        );
        data["unexpected"] = json!(true);
        assert!(read::<NoticeTarget>(&key, &data).is_err());
    }

    #[test]
    fn test_consent_state_owed_end_unknown_member_refused() {
        let end = OwedEnd {
            record: [5; 32],
            msg_id: ID.to_owned(),
        };
        let key = key::owed_end(&end.record);
        let mut data = serde_json::to_value(&end).unwrap();
        assert_eq!(read::<OwedEnd>(&key, &data).unwrap().msg_id, ID);
        data["unexpected"] = json!(true);
        assert!(read::<OwedEnd>(&key, &data).is_err());
    }

    #[test]
    fn test_consent_state_required_members_and_bindings() {
        let (key, data) = consent(Consent::Held {
            confirm_again: false,
        });
        for field in ["counterparty", "allowance_id", "consent"] {
            let mut missing = data.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(read::<ConsentEntry>(&key, &missing).is_err(), "{field}");
            missing[field] = Value::Null;
            assert!(read::<ConsentEntry>(&key, &missing).is_err(), "{field}");
        }
        for bad in [
            json!({"held": {}}),
            json!({"held": {"confirm_again": null}}),
            json!({"held": {"confirm_again": "false"}}),
            json!({"removed": {}}),
            json!({"removed": {"reason": null}}),
            json!({"removed": {"reason": "other"}}),
        ] {
            let mut bad_entry = data.clone();
            bad_entry["consent"] = bad;
            assert!(read::<ConsentEntry>(&key, &bad_entry).is_err());
        }
        assert!(read::<ConsentEntry>(&key::consent(&[8; 32]), &data).is_err());
        let mut wrong = data;
        wrong["counterparty"] = json!(b64(&[9; 32]));
        assert!(read::<ConsentEntry>(&key, &wrong).is_err());
        assert!(read::<NoticeTarget>(
            &key::notice_target(ID),
            &json!({"msg_id": ID, "app_id": b64(&[4; 15])})
        )
        .is_err());
        assert!(read::<NoticeTarget>(
            &key::notice_target(ID),
            &json!({"msg_id": "123e4567-e89b-42d3-a456-426614174000", "app_id": b64(&[4; 16])})
        )
        .is_err());
        assert!(read::<OwedEnd>(
            &key::owed_end(&[5; 32]),
            &json!({"record": b64(&[6; 32]), "msg_id": ID})
        )
        .is_err());
        assert!(read::<OwedEnd>(
            &key::owed_end(&[5; 32]),
            &json!({"record": b64(&[5; 32]), "msg_id": "not-a-message-id"})
        )
        .is_err());
    }

    #[test]
    fn test_consent_state_duplicate_members_refused() {
        assert!(serde_json::from_str::<Consent>(
            r#"{"held":{"confirm_again":false,"confirm_again":true}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<Consent>(
            r#"{"removed":{"reason":"ended","reason":"rejected"}}"#
        )
        .is_err());
        assert!(serde_json::from_str::<NoticeTarget>(&format!(
            r#"{{"msg_id":"{ID}","msg_id":"{ID}","app_id":"{}"}}"#,
            b64(&[4; 16])
        ))
        .is_err());
        assert!(serde_json::from_str::<OwedEnd>(&format!(
            r#"{{"record":"{}","msg_id":"{ID}","msg_id":"{ID}"}}"#,
            b64(&[5; 32])
        ))
        .is_err());
    }
}
