//! A conflict observed in the lifetime of an ordinary pending proposal.

use super::{allowance_record_target, named_allowance, HeldAnswer, Paykit};
use crate::divergent::SeenVersion;
use crate::error::{PaykitError, Result};
use crate::protocol::{step_failure, store_failure, view_failure};
use crate::receive::{self, Fail};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder, StateKey};
use crate::stored::{BoundNote, StoredNote};
use paykit_lib::bodies::ProposerRole;
use paykit_lib::collections::Collection;
use paykit_lib::records::{PendingAllowanceRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{
    FeedPosition, LocalWrite, ReceiveContext, RecordMarkContext, SharedRecord, StoreFailure,
};
use pubky_switch_core::ops::RecordVersion;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::BTreeMap};

/// The readable source at which this app first established the conflict.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingConflict {
    source: SeenVersion,
    /// The latest proven live application of this same original in its observed lifetime.
    current: SeenVersion,
    #[serde(with = "pubky_switch_core::enc::serde_b64_vec")]
    pending: Vec<u8>,
    second: Option<KeptMessage>,
    replies: Option<OpposedReplies>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpposedReplies {
    acceptance: KeptMessage,
    rejection: KeptMessage,
}

/// A verified current deletion bounds every earlier answer at this account key,
/// including answers still parked when the app first observes the deletion.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingRetirement {
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    account: [u8; 32],
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    key: [u8; 32],
    version: SeenVersion,
    reopened: Option<ReopenedProposal>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReopenedProposal {
    version: SeenVersion,
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    proposal: [u8; 32],
}

impl PendingRetirement {
    /// Only the closed lifetime of this withdrawal's applied delete permits its append.
    pub(super) fn withdrawal_after(
        &self,
        original: SeenVersion,
        recovery_from: Option<u64>,
    ) -> pubky_switch_client::app::AppendDecision {
        use pubky_switch_client::app::AppendDecision::{Hold, Send, Withdraw};
        match self.version.cmp(&original) {
            Ordering::Less => Hold,
            Ordering::Equal if self.reopened.is_none() => {
                // The old delete stays authentic after loss. Replacement absence
                // cannot account for a later lifetime at the previous provider.
                if super::delivery::deletion_continues(
                    original.record_version().generation,
                    recovery_from,
                ) {
                    Send
                } else {
                    Hold
                }
            }
            _ => Withdraw,
        }
    }

    pub(super) fn retires(&self, source: SeenVersion) -> bool {
        source < self.version
    }

    pub(crate) fn bind(&self, key: &[u8; 32], binder: &Binder<'_>) -> Result<()> {
        if self.key != *key
            || self.account != *binder.identity()
            || self.version.record_version().is_none()
            || self
                .reopened
                .as_ref()
                .is_some_and(|live| live.version <= self.version)
        {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }

    pub(super) fn compare(
        &self,
        source: Option<SeenVersion>,
        position: Option<FeedPosition>,
        order: impl Fn(FeedPosition, FeedPosition) -> Option<Ordering>,
    ) -> Option<Ordering> {
        if source.is_some_and(|at| self.retires(at)) {
            return Some(Ordering::Less);
        }
        let end = self.version.record_version();
        order(
            position?,
            FeedPosition {
                generation: end.generation,
                seq: end.version,
            },
        )
    }
}

enum Opposed {
    Found(Box<PendingConflict>),
    Rejected([u8; 32]),
    Accepted([u8; 32]),
    Unknown,
}

/// A qualified, unresolved conflict at this proposal lifetime stops its outgoing
/// proposal, rejection or withdrawal even on a cold retry (Paykit spec 8.3.1).
pub(crate) fn holds_delivery(
    delivery: &pubky_switch_client::app::DeliveryView<'_>,
) -> Result<bool> {
    let message = delivery.message();
    let body = paykit_lib::bodies::check_body(message, delivery.plaintext().json())?;
    let id = named_allowance(&body).ok_or_else(PaykitError::corrupt)?;
    let target = allowance_record_target(Collection::AllowanceProposals, &message.to, id);
    let blind = |input: &[u8]| delivery.blind(input);
    let binder = Binder::new(delivery.local().identity(), delivery.cert(), &blind);
    let key = binder.record_key(&target)?;
    let permanent = binder.record_key(&super::allowance_target(&message.to, id))?;
    if super::evidence::holds_delivery(delivery, &key, &permanent)? {
        return Ok(true);
    }
    if state::delivery_get::<PendingConflict>(delivery, &key::pending_conflict(&key))?.is_some() {
        return Ok(true);
    }
    if !matches!(body, Body::AllowanceProposal(_)) {
        return Ok(false);
    }
    let retired =
        state::delivery_get::<PendingRetirement>(delivery, &key::pending_retirement(&key))?;
    if retired.as_ref().is_some_and(|end| {
        end.reopened
            .as_ref()
            .is_none_or(|live| live.proposal != delivery.hash())
    }) {
        return Ok(true);
    }
    let rejects = |held: &KeptMessage, r: &StoredMessage| {
        matches!(r.body(), Body::AllowanceRejection(_))
            && named_allowance(r.body()) == Some(id)
            && r.sender() == message.to
            && r.message().refs.as_slice() == [delivery.hash()]
            && held
                .pending_source
                .is_some_and(|source| retired.as_ref().is_none_or(|end| source > end.version))
    };
    for (_, kept) in state::bound_view_scan::<KeptMessage>(
        delivery.local(),
        &key::kept_reply(&message.to, &permanent, ""),
        &binder,
    )? {
        if rejects(&kept, &kept.stored(binder.identity())?) {
            return Ok(true);
        }
    }
    for (_, bound) in state::bound_view_scan::<BoundNote>(
        delivery.local(),
        &key::stored_notes_of(&message.to),
        &binder,
    )? {
        if !bound.note.placed && rejects(&bound.note.message, &bound.message) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn answers(proposal: &StoredMessage, reply: &StoredMessage) -> bool {
    let Body::AllowanceProposal(p) = proposal.body() else {
        return false;
    };
    let answer = match reply.body() {
        Body::AllowanceAcceptance(q)
            if q.executor_app_id.is_none() || p.proposer_role == ProposerRole::Allowee =>
        {
            q
        }
        Body::AllowanceRejection(r) => r,
        _ => return false,
    };
    answer.allowance_id == p.allowance_id
        && reply.sender() == proposal.message().to
        && reply.message().to == proposal.sender()
        && reply.message().refs.as_slice() == [proposal.hash()]
}

impl PendingConflict {
    fn from_answers(
        proposal: &StoredMessage,
        held: &SharedRecord,
        current: HeldAnswer,
        mut kept: Vec<HeldAnswer>,
        lifetime: (Option<&PendingRetirement>, Option<RecordVersion>),
        order: impl Fn(FeedPosition, FeedPosition) -> Option<Ordering>,
    ) -> Opposed {
        let (end, previous) = lifetime;
        kept.push(current);
        let mut matching = Vec::new();
        for reply in &kept {
            if !answers(proposal, &reply.message) {
                continue;
            }
            if let Some(end) = end {
                match end.compare(reply.source, reply.position, &order) {
                    Some(Ordering::Less) => continue,
                    Some(Ordering::Greater) => (),
                    _ => return Opposed::Unknown,
                }
            }
            if reply.position.is_none()
                || reply.source.is_some_and(|s| {
                    s.record_version() != held.version && Some(s.record_version()) != previous
                })
            {
                return Opposed::Unknown;
            }
            matching.push(reply);
        }
        let compare = |a: &HeldAnswer, b: &HeldAnswer| {
            crate::records::compare_received(
                (a.order.0, a.position?),
                (b.order.0, b.position?),
                &order,
            )
        };
        let earliest =
            |items: &[&HeldAnswer]| crate::records::first_received(items, |a, b| compare(a, b));
        let Some(index) = earliest(&matching) else {
            return Opposed::Unknown;
        };
        let q = matching[index];
        if !matches!(q.message.body(), Body::AllowanceAcceptance(_)) {
            let source = q.source.map_or(held.version, SeenVersion::record_version);
            // Unlike the two-reply delivery hold, a rejection-first deletion needs the
            // pending application this reply can actually close (Paykit spec 8.3.1).
            if q.position
                .is_some_and(|position| precedes_reply(source, position, &order))
            {
                return Opposed::Rejected(q.message.hash());
            }
            return Opposed::Unknown;
        }
        let rejections: Vec<_> = matching
            .into_iter()
            .filter(|r| matches!(r.message.body(), Body::AllowanceRejection(_)))
            .collect();
        if rejections.is_empty() {
            let source = q.source.map_or(held.version, SeenVersion::record_version);
            if q.position
                .is_some_and(|position| precedes_reply(source, position, &order))
            {
                return Opposed::Accepted(q.message.hash());
            }
            return Opposed::Unknown;
        }
        let Some(index) = earliest(&rejections) else {
            return Opposed::Unknown;
        };
        let r = rejections[index];
        let (Some(first), Some(later), Some(value)) =
            (q.position, r.position, held.content.value())
        else {
            return Opposed::Unknown;
        };
        if compare(q, r) != Some(Ordering::Less) {
            return Opposed::Unknown;
        }
        Opposed::Found(Box::new(Self {
            source: held.version.into(),
            current: held.version.into(),
            pending: value.to_vec(),
            second: None,
            replies: Some(OpposedReplies {
                acceptance: KeptMessage::taken_at(&q.message, first),
                rejection: KeptMessage::taken_at(&r.message, later),
            }),
        }))
    }

    pub(super) fn original(&self, own: &[u8; 32]) -> Result<PendingAllowanceRecord> {
        PendingAllowanceRecord::read(&self.pending, own).map_err(|_| PaykitError::corrupt())
    }

    fn matches(
        &self,
        own: &[u8; 32],
        current: &PendingAllowanceRecord,
        held: &SharedRecord,
    ) -> Result<bool> {
        Ok(self.current.record_version() == held.version && self.stops(own, current)?)
    }

    /// An explicit discard is conditioned on the selected current write. A later
    /// copy of the very same proposal cannot itself retire this known stop.
    pub(super) fn stops(&self, own: &[u8; 32], pending: &PendingAllowanceRecord) -> Result<bool> {
        Ok(self.original(own)?.proposal().hash() == pending.proposal().hash())
    }

    fn source(&self) -> RecordVersion {
        self.source.record_version()
    }

    fn merge(&mut self, other: Self) -> Result<()> {
        if self.source != other.source || self.pending != other.pending {
            return Err(PaykitError::corrupt());
        }
        self.current = self.current.max(other.current);
        if let Some(new) = other.replies {
            if !self
                .replies
                .as_ref()
                .map(|old| Self::earlier(&old.rejection, &new.rejection))
                .transpose()?
                .unwrap_or(false)
            {
                self.replies = Some(new);
            }
        }
        if let Some(new) = other.second {
            if !self
                .second
                .as_ref()
                .map(|old| Self::earlier(old, &new))
                .transpose()?
                .unwrap_or(false)
            {
                self.second = Some(new);
            }
        }
        Ok(())
    }

    /// An import only compares equal-time receipts within the same generation.
    /// Keep the destination's established conflict if their order is unknown.
    fn earlier(a: &KeptMessage, b: &KeptMessage) -> Result<bool> {
        let a_pos = a.position().ok_or_else(PaykitError::corrupt)?;
        let b_pos = b.position().ok_or_else(PaykitError::corrupt)?;
        Ok(crate::records::compare_received(
            (a.order()?.0, a_pos),
            (b.order()?.0, b_pos),
            |x, y| (x.generation == y.generation).then(|| x.seq.cmp(&y.seq)),
        ) == Some(Ordering::Less))
    }

    fn reply(&self, own: &[u8; 32], answer: &StoredMessage) -> Result<PendingReply> {
        let original = self.original(own)?;
        Ok(if !answers(original.proposal(), answer) {
            PendingReply::Unrelated
        } else if matches!(answer.body(), Body::AllowanceRejection(_)) {
            PendingReply::Stopped
        } else {
            PendingReply::Waits(Some(self.source))
        })
    }

    /// Preserve still-owed replies from the destination of an in-place restore.
    /// Their original inbox receipt and pending source survive alongside the stop.
    fn restore_acceptances(
        &self,
        restoring: &pubky_switch_client::app::RestoreContext<'_>,
        binder: &Binder<'_>,
        replies: &mut BTreeMap<String, KeptMessage>,
    ) -> Result<()> {
        let own = binder.identity();
        let p = self.original(own)?;
        if p.proposal().sender() != *own {
            return Ok(());
        }
        let a = super::allowance_target(p.counterparty(), p.allowance_id());
        let prefix = key::kept_reply(p.counterparty(), &binder.record_key(&a)?, "");
        for (at, bytes) in restoring.previous_local_scan(prefix.as_bytes())? {
            let mut held = state::restored::<KeptMessage>(&at, &bytes, binder)?;
            let message = held.stored(own)?;
            if !matches!(message.body(), Body::AllowanceAcceptance(_))
                || !answers(p.proposal(), &message)
            {
                continue;
            }
            if let Some(pair) = &self.replies {
                let q = pair.acceptance.stored(own)?;
                if q.message().msg_id == message.message().msg_id {
                    if message.hash() != q.hash()
                        || held.order()? != pair.acceptance.order()?
                        || held.position() != pair.acceptance.position()
                    {
                        return Err(PaykitError::corrupt());
                    }
                    held.pending_source.get_or_insert(self.source);
                }
            }
            if held.pending_source == Some(self.source) {
                replies.insert(
                    String::from_utf8(at).map_err(|_| PaykitError::corrupt())?,
                    held,
                );
            }
        }
        Ok(())
    }

    pub(crate) fn bind(&self, key: &[u8; 32], binder: &Binder<'_>) -> Result<()> {
        let own = binder.identity();
        let original = self.original(own)?;
        let target = allowance_record_target(
            Collection::AllowanceProposals,
            original.counterparty(),
            original.allowance_id(),
        );
        if original.conflicted().is_some()
            || self.source.record_version().is_none()
            || self.current.record_version().generation != self.source().generation
            || self.current < self.source
            || binder.record_key(&target)? != *key
        {
            return Err(PaykitError::corrupt());
        }
        let p = original.proposal();
        if self.replies.is_none() && self.second.is_none() {
            return Err(PaykitError::corrupt());
        }
        if let Some(replies) = &self.replies {
            let q = replies.acceptance.stored(own)?;
            let r = replies.rejection.stored(own)?;
            let first = replies
                .acceptance
                .position()
                .ok_or_else(PaykitError::corrupt)?;
            let second = replies
                .rejection
                .position()
                .ok_or_else(PaykitError::corrupt)?;
            let q_time = replies.acceptance.order()?.0;
            let r_time = replies.rejection.order()?.0;
            // The writer verified cross-generation order before creating this stop.
            if !(p.sender() == *own
                && matches!(q.body(), Body::AllowanceAcceptance(_))
                && matches!(r.body(), Body::AllowanceRejection(_))
                && answers(p, &q)
                && answers(p, &r)
                && first != second
                && (q_time < r_time
                    || (q_time == r_time
                        && (first.generation != second.generation || first.seq < second.seq))))
            {
                return Err(PaykitError::corrupt());
            }
        }
        if let Some(second) = &self.second {
            second.order()?;
            second.position().ok_or_else(PaykitError::corrupt)?;
            let mut checked = original;
            if !checked.note_proposal(second.stored(own)?)? {
                return Err(PaykitError::corrupt());
            }
        }
        Ok(())
    }
}

/// Only a qualified original can establish a new stop. A previously acquired stop
/// survives an uncertain or unreadable current record.
pub(super) fn qualified_at(
    held: &SharedRecord,
    target: &pubky_switch_client::app::RecordTarget,
    key: &[u8; 32],
) -> bool {
    held.key == *key
        && held.collection == target.collection.name
        && held.policy == target.collection.policy
        && held.authenticated
        && !held.doubtful
        && !held.not_taken
        && !held.flagged
        && !held.divergent
        && !held.different_write_at_held_version
}

pub(super) fn qualified(
    held: &SharedRecord,
    target: &pubky_switch_client::app::RecordTarget,
    key: &[u8; 32],
    own: &[u8; 32],
) -> Option<PendingAllowanceRecord> {
    if !qualified_at(held, target, key) {
        return None;
    }
    PendingAllowanceRecord::read(held.content.value()?, own).ok()
}

pub(super) fn original_for(
    held: &SharedRecord,
    target: &pubky_switch_client::app::RecordTarget,
    key: &[u8; 32],
    own: &[u8; 32],
    answer: &StoredMessage,
) -> Option<PendingAllowanceRecord> {
    let pending = qualified(held, target, key, own)?;
    let p = pending.proposal();
    (pending.conflicted().is_none() && p.sender() == *own && answers(p, answer)).then_some(pending)
}

/// A restored cursor can replay the old answer after the same proposal has reopened.
/// Compare the actual inbox position with the selected deletion in the account feed.
pub(super) fn before_retirement(
    cx: &ReceiveContext<'_>,
    target: &pubky_switch_client::app::RecordTarget,
    position: FeedPosition,
) -> std::result::Result<bool, Fail> {
    let key = cx.record_key(target)?;
    let Some(end) = state::cx_get::<PendingRetirement>(cx, &key::pending_retirement(&key))? else {
        return Ok(false);
    };
    match end.compare(None, Some(position), |a, b| cx.received_order(a, b)) {
        Some(Ordering::Less) => Ok(true),
        Some(Ordering::Greater) => Ok(false),
        _ => Err(Fail::wait_for("proposal deletion order unknown")),
    }
}

pub(super) enum SecondProposal {
    Undecided,
    Reflected,
    Keep(Box<PendingConflict>),
}

/// A second proposal must belong to the same signed parties, ID and physical
/// key as the selected original. A repeat never replaces either proposal.
pub(super) fn second_proposal(
    own: &[u8; 32],
    held: &SharedRecord,
    target: &pubky_switch_client::app::RecordTarget,
    record: &[u8; 32],
    previous: Option<PendingConflict>,
    message: &StoredMessage,
    position: FeedPosition,
) -> Result<SecondProposal> {
    let Some(mut pending) = qualified(held, target, record, own) else {
        return Ok(SecondProposal::Undecided);
    };
    if pending.counterparty() != &message.sender()
        || Some(pending.allowance_id()) != named_allowance(message.body())
    {
        return Ok(SecondProposal::Undecided);
    }
    if !pending.note_proposal(message.clone())? {
        return Ok(SecondProposal::Reflected);
    }
    let value = held.content.value().ok_or_else(PaykitError::corrupt)?;
    let mut stop = previous.unwrap_or_else(|| PendingConflict {
        source: held.version.into(),
        current: held.version.into(),
        pending: value.to_vec(),
        second: None,
        replies: None,
    });
    if !stop.matches(own, &pending, held)? {
        return Ok(SecondProposal::Undecided);
    }
    if stop.second.is_some() {
        return Ok(SecondProposal::Reflected);
    }
    stop.second = Some(KeptMessage::taken_at(message, position));
    Ok(SecondProposal::Keep(Box::new(stop)))
}

/// Keep the competing envelope in the same commit as its inbox disposition.
pub(super) fn observe_second(
    cx: &mut ReceiveContext<'_>,
    target: &pubky_switch_client::app::RecordTarget,
    held: &SharedRecord,
    message: &StoredMessage,
    position: pubky_switch_client::app::FeedPosition,
) -> std::result::Result<(), Fail> {
    let record = cx.record_key(target)?;
    let at = key::pending_conflict(&record);
    let previous = state::cx_get::<PendingConflict>(cx, &at)?;
    match second_proposal(
        &cx.identity(),
        held,
        target,
        &record,
        previous,
        message,
        position,
    )
    .map_err(|_| Fail::corrupt())?
    {
        SecondProposal::Undecided => super::stored_entry(
            cx,
            message,
            position,
            Some((target, Some(held.version.into()))),
        ),
        SecondProposal::Reflected => Ok(()),
        SecondProposal::Keep(stop) => {
            receive::keep_local(cx, receive::local(&at, &*stop)?);
            Ok(())
        }
    }
}

pub(super) enum PendingReply {
    Undecided,
    Unrelated,
    /// Matched and unplaced. A proven source can be retained for the next retry.
    Waits(Option<SeenVersion>),
    Stopped,
}

/// The proposer records P before sending it. A live version first seen after the
/// original reply can instead be a recreation that reused the same signed P.
pub(super) fn precedes_reply(
    version: RecordVersion,
    reply: FeedPosition,
    order: impl Fn(FeedPosition, FeedPosition) -> Option<Ordering>,
) -> bool {
    order(
        FeedPosition {
            generation: version.generation,
            seq: version.version,
        },
        reply,
    ) == Some(Ordering::Less)
}

pub(super) fn observe(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
    answer: &StoredMessage,
    position: FeedPosition,
) -> std::result::Result<PendingReply, Fail> {
    let own = cx.identity();
    let peer = answer.sender();
    let Some(id) = named_allowance(answer.body()) else {
        return Ok(PendingReply::Unrelated);
    };
    let target = allowance_record_target(Collection::AllowanceProposals, &peer, id);
    let key = cx.record_key(&target)?;
    let at = key::pending_conflict(&key);
    if let Some(stop) = state::cx_get::<PendingConflict>(cx, &at)? {
        return stop.reply(&own, answer).map_err(|_| Fail::corrupt());
    }
    let Some(pending) = original_for(held, &target, &key, &own, answer) else {
        return Ok(PendingReply::Unrelated);
    };
    let p = pending.proposal();
    let rejected = matches!(answer.body(), Body::AllowanceRejection(_));
    let a = super::allowance_target(&peer, id);
    let kept = super::step_answers_kept(cx, &a, answer)?;
    let current = HeldAnswer::new(answer.clone(), &KeptMessage::taken_at(answer, position))
        .map_err(|_| Fail::corrupt())?;
    let end = state::cx_get::<PendingRetirement>(cx, &key::pending_retirement(&key))?;
    if let Opposed::Found(stop) =
        PendingConflict::from_answers(p, held, current, kept, (end.as_ref(), None), |a, b| {
            cx.received_order(a, b)
        })
    {
        receive::keep_local(cx, receive::local(&at, &*stop)?);
        return Ok(if rejected {
            PendingReply::Stopped
        } else {
            PendingReply::Waits(Some(held.version.into()))
        });
    }
    if rejected && !precedes_reply(held.version, position, |a, b| cx.received_order(a, b)) {
        return Ok(PendingReply::Undecided);
    }
    Ok(PendingReply::Waits(Some(held.version.into())))
}

/// Reopen delivery only from a covering observation of this account's live proposal after
/// a verified deletion. Keep the deletion itself to order replies carried by older backups.
pub(crate) fn observe_reopened(
    observation: &pubky_switch_client::app::RecordObservation<'_>,
    cx: &ReceiveContext<'_>,
) -> std::result::Result<Vec<LocalWrite>, StoreFailure> {
    let held = observation.record;
    if held.collection != Collection::AllowanceProposals.name() || !observation.current {
        return Ok(Vec::new());
    }
    let at = key::pending_retirement(&held.key);
    let Some(mut end) = state::cx_get::<PendingRetirement>(cx, &at).map_err(step_failure)? else {
        return Ok(Vec::new());
    };
    if held.version <= end.version.record_version() {
        return Ok(Vec::new());
    }
    let live = held
        .content
        .value()
        .and_then(|v| PendingAllowanceRecord::read(v, &cx.identity()).ok());
    let reopened = if let Some(p) = live {
        let target = allowance_record_target(
            Collection::AllowanceProposals,
            p.counterparty(),
            p.allowance_id(),
        );
        let key = cx.record_key(&target).map_err(store_failure)?;
        (qualified_at(held, &target, &key)
            && p.conflicted().is_none()
            && p.proposal().sender() == cx.identity())
        .then_some(ReopenedProposal {
            version: held.version.into(),
            proposal: p.proposal().hash(),
        })
    } else {
        None
    };
    if reopened == end.reopened {
        return Ok(Vec::new());
    }
    end.reopened = reopened;
    Ok(vec![state::local_put(&at, &end).map_err(view_failure)?])
}

/// The exact discard also settles held replies to its P. A stored entry must still go
/// through its client's ordinary acknowledgment, so mark its effect placed.
fn retire(
    own: &[u8; 32],
    key: &[u8; 32],
    p: &StoredMessage,
    kept: Vec<(String, KeptMessage)>,
    notes: Vec<(String, BoundNote)>,
) -> Result<Vec<LocalWrite>> {
    let mut writes = vec![LocalWrite::Delete {
        key: key::pending_conflict(key).into_bytes(),
    }];
    for (at, message) in kept {
        if answers(p, &message.stored(own)?) {
            writes.push(LocalWrite::Delete {
                key: at.into_bytes(),
            });
        }
    }
    for (at, BoundNote { mut note, message }) in notes {
        if !note.placed && answers(p, &message) {
            note.placed = true;
            writes.push(state::local_put(&at, &note)?);
        }
    }
    Ok(writes)
}

/// Retire the duties of a previously bound P at the client's verified lifetime end.
/// The native record and the local writes are committed together (Paykit spec 8.3.1).
/// A repeated observation has no remaining stop to settle.
pub(crate) fn lifetime_ended(
    record: &RecordMarkContext<'_>,
    version: RecordVersion,
    cx: &ReceiveContext<'_>,
) -> std::result::Result<Vec<LocalWrite>, StoreFailure> {
    let family = Collection::AllowanceProposals;
    if record.collection.name != family.name() || record.collection.policy != family.fixed_policy()
    {
        return Ok(Vec::new());
    }
    let at = key::pending_retirement(&record.key);
    let previous = state::cx_get::<PendingRetirement>(cx, &at).map_err(step_failure)?;
    if previous.is_some_and(|old| old.version.record_version() >= version) {
        return Ok(Vec::new());
    }
    let own = cx.identity();
    let end = PendingRetirement {
        account: own,
        key: record.key,
        version: version.into(),
        reopened: None,
    };
    let mut writes = vec![state::local_put(&at, &end).map_err(view_failure)?];
    let evidence = key::allowance_evidence(&record.key);
    if state::cx_get::<super::ConflictEvidence>(cx, &evidence)
        .map_err(step_failure)?
        .is_some_and(|held| held.retired_by(&end))
    {
        writes.push(LocalWrite::Delete {
            key: evidence.into_bytes(),
        });
    }
    let stop = state::cx_get::<PendingConflict>(cx, &key::pending_conflict(&record.key))
        .map_err(step_failure)?;
    if let Some(stop) = stop.filter(|stop| stop.source() < version) {
        let pending = stop.original(&own).map_err(view_failure)?;
        let peer = pending.counterparty();
        let permanent = cx
            .record_key(&super::allowance_target(peer, pending.allowance_id()))
            .map_err(store_failure)?;
        let held = state::cx_scan::<KeptMessage>(cx, &key::kept_reply(peer, &permanent, ""))
            .map_err(step_failure)?;
        let stored =
            state::cx_scan::<BoundNote>(cx, &key::stored_notes_of(peer)).map_err(step_failure)?;
        let earlier = |message: &KeptMessage| {
            end.compare(message.pending_source, message.position(), |a, b| {
                cx.received_order(a, b)
            }) == Some(Ordering::Less)
        };
        writes.extend(
            retire(
                &own,
                &record.key,
                pending.proposal(),
                held.into_iter().filter(|(_, m)| earlier(m)).collect(),
                stored
                    .into_iter()
                    .filter(|(_, n)| earlier(&n.note.message))
                    .collect(),
            )
            .map_err(view_failure)?,
        );
    }

    Ok(writes)
}

impl Paykit {
    /// If a first attempt was stored before the pending offer could be used,
    /// the later read must still place the second proposal at that pending key.
    pub(super) async fn place_received_second(
        &mut self,
        message: &StoredMessage,
        evidence: &KeptMessage,
        stored_id: Option<u64>,
    ) -> Result<Option<crate::stored::Placed>> {
        use crate::stored::Placed;
        use pubky_switch_client::app::RecordContent;
        let position = evidence.position().ok_or_else(PaykitError::corrupt)?;
        let Some(id) = named_allowance(message.body()) else {
            return Err(PaykitError::corrupt());
        };
        let peer = message.sender();
        let target = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let key = self.app.record_key(&target)?;
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(None);
        };
        if matches!(held.content, RecordContent::Deleted)
            || self
                .app
                .shared_record(&self.app.record_key(&super::allowance_target(&peer, id))?)
                .await?
                .is_some()
        {
            return Ok(None);
        }
        if self
            .side_unlisted(crate::hold::RecordSide::Allowances)
            .await?
        {
            return Ok(Some(Placed::Waits));
        }
        if let Some(end) = self
            .local_get::<PendingRetirement>(&key::pending_retirement(&key))
            .await?
        {
            if let Some(stored_id) = stored_id {
                let Some(entry) = self.app.parked_entry(stored_id).await? else {
                    return Ok(Some(Placed::Waits));
                };
                if !entry.sender_verified
                    || entry.sender != peer
                    || &entry.plaintext != message.plaintext()
                    || entry.generation != position.generation
                    || entry.seq != position.seq
                    || Some(entry.accepted_at_ms) != message.accepted_at()
                {
                    return Ok(Some(Placed::Waits));
                }
            }
            match end.compare(evidence.pending_source, Some(position), |a, b| {
                self.app.received_order(a, b)
            }) {
                None => return Ok(Some(Placed::Waits)),
                Some(std::cmp::Ordering::Less) => return Ok(Some(Placed::Done)),
                Some(_) => {}
            }
        }
        if let Some(source) = evidence.pending_source {
            let Some(pending) = qualified(&held, &target, &key, &self.identity()) else {
                return Ok(Some(Placed::Waits));
            };
            // A single original that happens to equal the competitor cannot prove
            // the shared conflict. Only the original lifetime can take this duty.
            if pending.proposal().hash() == message.hash()
                || (held.version != source.record_version()
                    && self.pending_predecessor(&target, &held).await?
                        != Some(source.record_version()))
            {
                return Ok(Some(Placed::Waits));
            }
        }
        let at = key::pending_conflict(&key);
        let previous = self.local_get::<PendingConflict>(&at).await?;
        let step = second_proposal(
            &self.identity(),
            &held,
            &target,
            &key,
            previous,
            message,
            position,
        )?;
        match step {
            SecondProposal::Undecided => Ok(Some(
                Box::pin(self.expire_allowance_placement(
                    &key,
                    Some(held.version.into()),
                    message,
                    evidence,
                    Vec::new(),
                ))
                .await?,
            )),
            SecondProposal::Reflected => Ok(Some(Placed::Done)),
            SecondProposal::Keep(stop) => {
                self.local_put(&at, &*stop).await?;
                Ok(Some(Placed::Done))
            }
        }
    }

    pub(super) async fn place_pending_seconds(&mut self) -> Result<()> {
        use pubky_switch_client::app::{KeptReason, OpBatch, RecordRead};
        use pubky_switch_core::ops::Condition;
        let own = self.identity();
        for (_, stop) in self
            .local_scan::<PendingConflict>(key::PENDING_CONFLICTS)
            .await?
        {
            let original = stop.original(&own)?;
            let peer = *original.counterparty();
            let id = original.allowance_id().clone();
            let target = allowance_record_target(Collection::AllowanceProposals, &peer, &id);
            let key = self.app.record_key(&target)?;
            let Some(cached) = self.app.shared_record(&key).await? else {
                continue;
            };
            if (stop.second.is_none() && cached.version == stop.current.record_version())
                || qualified(&cached, &target, &key, &own).is_some_and(|r| r.conflicted().is_some())
            {
                continue;
            }
            let RecordRead::Fresh(held) = self.app.read_record_fresh(&target).await? else {
                continue;
            };
            let Some(mut pending) = qualified(&held, &target, &key, &own) else {
                continue;
            };
            if original.check_later_write(&pending).is_err()
                || pending.conflicted().is_some()
                || held.version < stop.current.record_version()
                || (held.version == stop.source()
                    && held.content.value() != Some(stop.pending.as_slice()))
                || (held.version != stop.current.record_version()
                    && self.pending_predecessor(&target, &held).await?
                        != Some(stop.current.record_version()))
            {
                continue;
            }
            let selected = self.app.record_condition(&key).await?;
            let accepted = super::allowance_target(&peer, &id);
            if !matches!(
                self.app.read_record_fresh(&accepted).await?,
                RecordRead::Kept {
                    reason: KeptReason::NotFound,
                    record: None
                }
            ) {
                continue;
            }
            let at = key::pending_conflict(&key);
            let Some(mut current) = self.local_get::<PendingConflict>(&at).await? else {
                continue;
            };
            if current.source != stop.source
                || current.current != stop.current
                || current.pending != stop.pending
            {
                continue;
            }
            if current.current.record_version() != held.version {
                current.current = held.version.into();
                self.local_put(&at, &current).await?;
            }
            let Some(second) = current.second.as_ref() else {
                continue;
            };
            let second = second.stored(&own)?;
            pending.note_proposal(second)?;
            let conditions = vec![
                selected,
                Condition::RecordVersion {
                    key: self.app.record_key(&accepted)?,
                    version: RecordVersion::NONE,
                },
            ];
            let write = self
                .app
                .record_op(&target, Some(&pending.to_json()?), conditions)
                .await?;
            crate::stored::submitted(
                self.submit(OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(write))
                    .await,
            )?;
        }
        Ok(())
    }

    /// Establish a direct live successor of an earlier matched proposal. A readable
    /// copy with the same signed proposal also occurs after deletion and recreation.
    pub(super) async fn pending_predecessor(
        &self,
        target: &pubky_switch_client::app::RecordTarget,
        held: &SharedRecord,
    ) -> Result<Option<RecordVersion>> {
        let app = &self.app;
        if held.version.generation != app.own_identity().generation() {
            return Ok(None);
        }
        let expected = |kept: &pubky_switch_client::app::KeptOp| {
            use paykit_lib::scopes::verify_certificate;
            use pubky_switch_core::{
                chain::TransitionProof,
                ops::{OpKind, SignedOp},
                sealed::RecordValue,
            };
            let identity = app.identity();
            let cert = verify_certificate(&kept.cert, &identity).ok()?;
            if cert.app_id() != held.writer {
                return None;
            }
            let op = SignedOp::verify_with_cert(&kept.op, &cert, &identity).ok()?;
            let OpKind::RecordPut(put) = &op.body().kind else {
                return None;
            };
            if put.key != held.key
                || put.collection != target.collection.id(&identity)
                || put.policy != target.collection.policy
                || put.expected_version.generation != held.version.generation
                || op.body().generation != held.version.generation
            {
                return None;
            }
            let service = app.own_identity().service_key_of(held.version.generation)?;
            let proof = TransitionProof::verify(kept.proof.as_ref()?, &service, &identity).ok()?;
            proof.check_binds(&op).ok()?;
            let at = proof.record_revision().ok()?;
            if at.generation != held.version.generation
                || at.seq != held.version.version
                || Some(proof.envelope().server_time) != held.written_at_ms
            {
                return None;
            }
            let sealed = &put.value_sealed;
            let scope = app.scope_key(sealed.data_epoch, &sealed.scope)?;
            let RecordValue::Json(value) = sealed.open(&scope, &identity, &put.key).ok()? else {
                return None;
            };
            (Some(value.as_slice()) == held.content.value()).then_some(put.expected_version)
        };
        Ok(app.shared_evidence().await?.ops.iter().find_map(expected))
    }

    /// An end stays effective while its matching acceptance cannot open the permanent record.
    /// The retained signed answer is evidence for that stop after the placement duty expires.
    async fn pending_acceptance_stopped(
        &mut self,
        record: &[u8; 32],
        source: RecordVersion,
        reply: &StoredMessage,
        position: FeedPosition,
    ) -> Result<bool> {
        // E can arrive before Q, or before either could be tied to the current P.
        // Keep the signed match with the stop even after Q's placement deadline.
        let prefix = key::allowance_ends(record);
        let mut ends = self.local_scan::<KeptMessage>(&prefix).await?;
        if ends.is_empty() {
            return Ok(false);
        }
        // Each end is already admitted. Promoting one releases its counted slot
        // for the former exempt end of this same sender and record.
        if ends.iter().all(|(_, end)| end.ended_acceptance.is_none()) {
            ends.sort_by_key(|(_, end)| end.order().ok());
            for (suffix, mut end) in ends {
                let Some(at) = end.position() else { continue };
                end.ended_acceptance = super::pending_end_acceptance(
                    &end.stored(&self.identity())?,
                    at,
                    reply,
                    Some(position),
                    source,
                    |a, b| self.app.received_order(a, b),
                );
                if end.ended_acceptance.is_some() {
                    self.local_put(&format!("{prefix}{suffix}"), &end).await?;
                    break;
                }
            }
        }
        Ok(true)
    }

    /// Apply the first eligible reply to the exact current proposal. A rejection deletes
    /// that proposal. An acceptance also opens the permanent pair and the payer’s ledger.
    async fn apply_pending_reply(
        &mut self,
        target: &pubky_switch_client::app::RecordTarget,
        selected: &SharedRecord,
        proposal: &StoredMessage,
        reply: &StoredMessage,
        evidence: &KeptMessage,
    ) -> Result<PendingReply> {
        use pubky_switch_client::app::{KeptReason, OpBatch, RecordRead};
        use pubky_switch_core::ops::Condition;
        let peer = reply.sender();
        let id = named_allowance(reply.body()).ok_or_else(PaykitError::corrupt)?;
        let permanent = super::allowance_target(&peer, id);
        let own = self.identity();
        let position = evidence.position().ok_or_else(PaykitError::corrupt)?;
        let accepting = matches!(reply.body(), Body::AllowanceAcceptance(_));
        let waiting = || {
            if accepting {
                PendingReply::Waits(Some(selected.version.into()))
            } else {
                PendingReply::Undecided
            }
        };
        if accepting
            && (Box::pin(
                self.pending_acceptance_stopped(
                    &self.app.record_key(&permanent)?,
                    evidence
                        .pending_source
                        .map_or(selected.version, SeenVersion::record_version),
                    reply,
                    position,
                ),
            )
            .await?
                || self.pause_at(&selected.key).await?.paused
                || crate::stored::past_window(
                    reply.accepted_at().ok_or_else(PaykitError::corrupt)?,
                    self.service_time(),
                ))
        {
            return Ok(waiting());
        }
        if evidence.pending_source.is_none() {
            // An answer may have arrived before the app first saw its pending record.
            // Bind the custody used by this retry before a competing write or a lost answer.
            let record = self.app.record_key(&permanent)?;
            let original = &reply.message().msg_id;
            let kept_key = key::kept_reply(&peer, &record, original);
            let stored_key = key::stored_note(&peer, original);
            let mut writes = Vec::new();
            let bind = |held: &mut KeptMessage| -> Result<bool> {
                if held.position() != Some(position) || held.stored(&own)?.hash() != reply.hash() {
                    return Err(PaykitError::corrupt());
                }
                Ok(held
                    .pending_source
                    .get_or_insert(selected.version.into())
                    .record_version()
                    == selected.version)
            };
            if let Some(mut held) = self.local_get::<KeptMessage>(&kept_key).await? {
                if !bind(&mut held)? {
                    return Ok(waiting());
                }
                writes.push(state::local_put(&kept_key, &held)?);
            }
            if let Some(mut note) = self
                .local_get::<StoredNote>(&stored_key)
                .await?
                .filter(|n| !n.placed)
            {
                if !bind(&mut note.message)? {
                    return Ok(waiting());
                }
                writes.push(state::local_put(&stored_key, &note)?);
            }
            if writes.is_empty() {
                return Ok(waiting());
            }
            self.app
                .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                .await?;
        }
        for entry in self.app.parked_entries().await? {
            if !entry.readable || !entry.reason.tried_again() {
                continue;
            }
            let Some(entry) = self.app.parked_entry(entry.stored_id).await? else {
                continue;
            };
            if !entry.sender_verified || entry.sender != peer {
                continue;
            }
            let Some(other) = crate::stored::parked_message(&entry, &own) else {
                continue;
            };
            if other.hash() == reply.hash() || !answers(proposal, &other) {
                continue;
            }
            let earlier = crate::records::compare_received(
                (
                    entry.accepted_at_ms,
                    FeedPosition {
                        generation: entry.generation,
                        seq: entry.seq,
                    },
                ),
                (
                    reply.accepted_at().ok_or_else(PaykitError::corrupt)?,
                    position,
                ),
                |a, b| self.app.received_order(a, b),
            );
            if self.fenced_own_proposal(proposal) || earlier != Some(Ordering::Greater) {
                return Ok(waiting());
            }
        }
        let RecordRead::Fresh(current) = self.app.read_record_fresh(target).await? else {
            return Ok(waiting());
        };
        if current.version != selected.version
            || current.content != selected.content
            || original_for(&current, target, &selected.key, &own, reply).is_none()
            || !matches!(
                self.app.read_record_fresh(&permanent).await?,
                RecordRead::Kept {
                    reason: KeptReason::NotFound,
                    record: None
                }
            )
        {
            return Ok(waiting());
        }
        let pending = self.selected_pending(&current)?.0;
        if self
            .competing_proposal_owed(&pending, &selected.key, &self.app.record_key(&permanent)?)
            .await?
        {
            return Ok(waiting());
        }
        let permanent_key = self.app.record_key(&permanent)?;
        let mut conditions = vec![self.app.record_condition(&selected.key).await?];
        let mut writes = Vec::new();
        if accepting {
            if self.conflict_kept(&permanent_key).await? {
                return Ok(waiting());
            }
            if self.fenced_own_proposal(proposal) {
                if self.allowance_end_send_permitted(&peer).await? {
                    if let Some(condition) =
                        Box::pin(self.live_original_after_loss(&current, proposal, reply)).await?
                    {
                        if Box::pin(self.withdraw_fenced_pending(
                            &current,
                            &permanent_key,
                            reply,
                            condition,
                        ))
                        .await?
                        {
                            return Ok(PendingReply::Stopped);
                        }
                    }
                }
                return Ok(waiting());
            }
            let super::pending::Acceptance::Eligible { ledger } =
                self.acceptance(proposal, reply, &peer, id).await?
            else {
                return Ok(waiting());
            };
            let Some(operations) = self
                .acceptance_records(proposal, reply, &peer, id, ledger)
                .await?
            else {
                return Ok(waiting());
            };
            writes = operations;
        } else {
            conditions.push(Condition::RecordVersion {
                key: permanent_key,
                version: RecordVersion::NONE,
            });
        }
        let op = self.app.record_op(target, None, conditions).await?;
        writes.push(op);
        let rejected = if accepting {
            Vec::new()
        } else {
            Box::pin(self.keep_proposal_rejection(
                &permanent_key,
                selected,
                proposal,
                reply,
                &writes,
            ))
            .await?
        };
        let mut retained = state::Tx::default();
        Box::pin(self.keep_original_selection(
            selected,
            proposal,
            reply,
            &mut writes,
            None,
            &mut retained,
        ))
        .await?;
        self.local_commit(retained).await?;
        let track_application = accepting || !rejected.is_empty();
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME).locals(rejected);
        for write in writes {
            batch = batch.entry(write);
        }
        match crate::stored::submitted(self.submit(batch).await) {
            Ok(crate::stored::Placed::Done) => {
                if track_application {
                    Box::pin(self.applied_original_approval(&permanent_key)).await?;
                }
                Ok(PendingReply::Stopped)
            }
            Ok(_) => Ok(waiting()),
            Err(crate::error::PaykitError::Client(error)) if error.kept_batch().is_some() => {
                Ok(waiting())
            }
            Err(error) => Err(error),
        }
    }

    /// Decide the later placement of a signed answer, whether retained as a kept reply
    /// or as a stored entry. Both stores carry its original inbox position.
    pub(super) async fn pending_reply_later(
        &mut self,
        record: &[u8; 32],
        message: &StoredMessage,
        evidence: &KeptMessage,
    ) -> Result<PendingReply> {
        let peer = message.sender();
        let id = named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
        let target = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let key = self.app.record_key(&target)?;
        let own = self.identity();
        let at = key::pending_conflict(&key);
        Box::pin(self.report_fenced_acceptance(record, &target, message, evidence)).await?;
        let end = self
            .local_get::<PendingRetirement>(&key::pending_retirement(&key))
            .await?;
        if let Some(end) = &end {
            match end.compare(evidence.pending_source, evidence.position(), |a, b| {
                self.app.received_order(a, b)
            }) {
                Some(Ordering::Less) => return Ok(PendingReply::Stopped),
                Some(Ordering::Greater) => {
                    if matches!(message.body(), Body::AllowanceAcceptance(_)) {
                        Box::pin(self.request_late_withdrawal(record, message)).await?;
                    }
                }
                _ if evidence.pending_source.is_some()
                    && matches!(message.body(), Body::AllowanceAcceptance(_)) =>
                {
                    return Ok(PendingReply::Waits(None))
                }
                _ => return Ok(PendingReply::Undecided),
            }
        }
        if let Some(stop) = self.local_get::<PendingConflict>(&at).await? {
            return stop.reply(&own, message);
        }
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(PendingReply::Unrelated);
        };
        // A reply kept before this observation still has its ordinary deadline.
        // The sealed record may hold the matching proposal (Paykit spec 8.1).
        if crate::hold::awaits_key(&held.content) {
            return Ok(PendingReply::Waits(None));
        }
        let Some(pending) = original_for(&held, &target, &key, &own, message) else {
            return Ok(PendingReply::Unrelated);
        };
        let kept = Box::pin(self.answers_kept(record, message)).await?;
        let current = HeldAnswer::new(message.clone(), evidence)?;
        let other_source =
            |a: &HeldAnswer| a.source.is_some_and(|s| s.record_version() != held.version);
        let previous = if other_source(&current) || kept.iter().any(other_source) {
            self.pending_predecessor(&target, &held).await?
        } else {
            None
        };
        let sole_acceptance = end.is_none()
            && previous.is_none()
            && matches!(message.body(), Body::AllowanceAcceptance(_))
            && kept.iter().all(|a| a.message.hash() == message.hash());
        match PendingConflict::from_answers(
            pending.proposal(),
            &held,
            current,
            kept,
            (end.as_ref(), previous),
            |a, b| self.app.received_order(a, b),
        ) {
            Opposed::Found(stop) => {
                self.local_put(&at, &*stop).await?;
                return stop.reply(&own, message);
            }
            _ if self.fenced_own_proposal(pending.proposal())
                && matches!(message.body(), Body::AllowanceAcceptance(_)) =>
            {
                if sole_acceptance {
                    return Box::pin(self.apply_pending_reply(
                        &target,
                        &held,
                        pending.proposal(),
                        message,
                        evidence,
                    ))
                    .await;
                }
                return Ok(PendingReply::Waits(None));
            }
            Opposed::Unknown if matches!(message.body(), Body::AllowanceAcceptance(_)) => {
                return Ok(PendingReply::Waits(None));
            }
            Opposed::Unknown => return Ok(PendingReply::Undecided),
            Opposed::Rejected(first) | Opposed::Accepted(first) if first == message.hash() => {
                return Box::pin(self.apply_pending_reply(
                    &target,
                    &held,
                    pending.proposal(),
                    message,
                    evidence,
                ))
                .await;
            }
            Opposed::Rejected(_) => return Ok(PendingReply::Undecided),
            Opposed::Accepted(_) => (),
        }
        Ok(if matches!(message.body(), Body::AllowanceAcceptance(_)) {
            PendingReply::Waits(Some(held.version.into()))
        } else {
            PendingReply::Unrelated
        })
    }
}

/// Install the later live stop from an in-place restore and retain the original Q if
/// its placement was still owed. A fresh installation has no destination state to merge.
pub(crate) fn restore_conflicts(
    restoring: &pubky_switch_client::app::RestoreContext<'_>,
    binder: &Binder<'_>,
    (mut imported, mut unresolved): (
        BTreeMap<[u8; 32], PendingConflict>,
        BTreeMap<[u8; 32], super::ConflictEvidence>,
    ),
    mut prepared: BTreeMap<([u8; 32], [u8; 32]), super::PreparedPendingReply>,
    mut retirements: BTreeMap<[u8; 32], PendingRetirement>,
    mut replies: BTreeMap<String, KeptMessage>,
    writes: &mut Vec<LocalWrite>,
) -> Result<()> {
    for (at, bytes) in restoring.previous_local_scan(key::PENDING_RETIREMENTS.as_bytes())? {
        let receipt = state::restored::<PendingRetirement>(&at, &bytes, binder)?;
        let Some(StateKey::PendingRetirement(key)) = StateKey::parse(&at) else {
            return Err(PaykitError::corrupt());
        };
        if retirements
            .get(&key)
            .is_none_or(|r| r.version < receipt.version)
        {
            retirements.insert(key, receipt);
        }
    }
    unresolved.retain(|key, held| !retirements.get(key).is_some_and(|end| held.retired_by(end)));
    for (at, bytes) in restoring.previous_local_scan(key::ALLOWANCE_EVIDENCE.as_bytes())? {
        let held = state::restored::<super::ConflictEvidence>(&at, &bytes, binder)?;
        let Some(StateKey::AllowanceEvidence(key)) = StateKey::parse(&at) else {
            return Err(PaykitError::corrupt());
        };
        if retirements
            .get(&key)
            .is_some_and(|end| held.retired_by(end))
        {
            continue;
        }
        if let Some(current) = unresolved.get_mut(&key) {
            current.merge(&held, binder.identity())?;
        } else {
            unresolved.insert(key, held);
        }
    }
    for (record, held) in unresolved {
        writes.push(state::local_put(&key::allowance_evidence(&record), &held)?);
    }
    // A backup can predate both messages. Preserve the installation's verified
    // pair, including while its permanent record is still unreadable. The
    // restored greatest deletion and a current permanent read settle it normally.
    let mut retained = BTreeMap::new();
    for (at, value) in restoring.previous_local_scan(key::ALLOWANCE_ENDS.as_bytes())? {
        let held = state::restored::<KeptMessage>(&at, &value, binder)?;
        if held.ended_acceptance.is_some() {
            retained.insert(at, value);
        }
    }
    for (at, bytes) in restoring.previous_local_scan(key::CONFLICTS.as_bytes())? {
        state::restored::<KeptMessage>(&at, &bytes, binder)?;
        retained.insert(at, bytes);
    }
    for (at, value) in restoring.previous_local_scan(key::NOTICES.as_bytes())? {
        if matches!(StateKey::parse(&at), Some(StateKey::MessageNotice { .. })) {
            let notice = state::restored::<crate::notices::KeptNotice>(&at, &value, binder)?;
            if matches!(
                notice.kind,
                crate::NoticeKind::AllowancePlacementExpired
                    | crate::NoticeKind::AllowanceAcceptanceBeforeLoss
                    | crate::NoticeKind::AllowanceWithdrawalConfirmed
            ) {
                retained.insert(at, value);
            }
        }
    }
    writes.retain(|w| !matches!(w, LocalWrite::Put { key, .. } if retained.contains_key(key)));
    writes.extend(
        retained
            .into_iter()
            .map(|(key, value)| LocalWrite::Put { key, value }),
    );
    let retired = |key: &[u8; 32], source: SeenVersion| {
        retirements.get(key).is_some_and(|r| r.retires(source))
    };
    for (at, value) in restoring.previous_local_scan(key::PENDING_REPLIES.as_bytes())? {
        let kept = state::restored::<super::PreparedPendingReply>(&at, &value, binder)?;
        let Some(StateKey::PendingReply { record, reply }) = StateKey::parse(&at) else {
            return Err(PaykitError::corrupt());
        };
        let signed = kept.message.stored(&kept.peer)?;
        if let Some(older) = prepared.get(&(record, reply)) {
            if older.application != kept.application
                || older.source != kept.source
                || older.seals != kept.seals
                || older.fenced_reply(binder.identity())? != kept.fenced_reply(binder.identity())?
            {
                return Err(PaykitError::corrupt());
            }
        }
        if matches!(signed.body(), Body::AllowanceEnd(_)) {
            let origin = key::end_origin(&signed.message().msg_id);
            let previous = restoring.previous_local_scan(origin.as_bytes())?;
            let (_, value) = previous
                .into_iter()
                .find(|(at, _)| at == origin.as_bytes())
                .ok_or_else(PaykitError::corrupt)?;
            let end = state::restored::<super::EndOrigin>(origin.as_bytes(), &value, binder)?;
            if !end.matches(signed.message(), signed.hash()) || end.cause != kept.end_cause() {
                return Err(PaykitError::corrupt());
            }
            writes.push(LocalWrite::Put {
                key: origin.into_bytes(),
                value,
            });
        }
        prepared.insert((record, reply), kept);
    }
    let mut live = std::collections::BTreeSet::new();
    for ((record, hash), answer) in prepared {
        let signed = answer.message.stored(&answer.peer)?;
        let id = named_allowance(signed.body()).ok_or_else(PaykitError::corrupt)?;
        let target = allowance_record_target(Collection::AllowanceProposals, &answer.peer, id);
        let pending = binder.record_key(&target)?;
        // A later lifetime can have its own answer while the earlier one still awaits
        // its exact deletion proof. Two different choices for one live lifetime cannot.
        if !retired(&pending, answer.source) && !live.insert(record) {
            return Err(PaykitError::corrupt());
        }
        writes.push(state::local_put(
            &key::pending_reply(&record, &hash),
            &answer,
        )?);
    }
    // A matched Q may still be owed before the first opposing reply arrives.
    for (at, bytes) in restoring.previous_local_scan(key::KEPT_REPLIES.as_bytes())? {
        let held = state::restored::<KeptMessage>(&at, &bytes, binder)?;
        if held.pending_source.is_some() {
            replies.insert(
                String::from_utf8(at).map_err(|_| PaykitError::corrupt())?,
                held,
            );
        }
    }
    imported.retain(|key, stop| !retired(key, stop.source));
    for (at, bytes) in restoring.previous_local_scan(key::PENDING_CONFLICTS.as_bytes())? {
        let old = state::restored::<PendingConflict>(&at, &bytes, binder)?;
        let Some(StateKey::PendingConflict(key)) = StateKey::parse(&at) else {
            return Err(PaykitError::corrupt());
        };
        if retired(&key, old.source) {
            continue;
        }
        old.restore_acceptances(restoring, binder, &mut replies)?;
        match imported.get_mut(&key) {
            Some(current) if current.source() == old.source() => current.merge(old)?,
            Some(current) if current.source() > old.source() => (),
            _ => {
                imported.insert(key, old);
            }
        }
    }
    for (key, stop) in imported {
        writes.push(state::local_put(&key::pending_conflict(&key), &stop)?);
    }
    for (at, held) in replies {
        let message = held.stored(binder.identity())?;
        let id = named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
        let target = allowance_record_target(Collection::AllowanceProposals, &message.sender(), id);
        let key = binder.record_key(&target)?;
        let source = held.pending_source.ok_or_else(PaykitError::corrupt)?;
        if !retired(&key, source) {
            writes.push(state::local_put(&at, &held)?);
        }
    }
    for (key, mut receipt) in retirements {
        // A backup cannot establish that this record is still the current live proposal.
        receipt.reopened = None;
        writes.push(state::local_put(&key::pending_retirement(&key), &receipt)?);
    }
    Ok(())
}

#[cfg(test)]
mod withdrawal_tests {
    use super::*;
    use pubky_switch_client::app::AppendDecision::{Hold, Send, Withdraw};

    #[test]
    fn test_withdrawal_checks_the_generation_lost_by_the_followed_identity() {
        let original = SeenVersion::from(RecordVersion::new(3, 11));
        let mut closed = PendingRetirement {
            account: [1; 32],
            key: [2; 32],
            version: original,
            reopened: None,
        };
        for (boundary, expected) in [
            (None, Send),
            (Some(2), Send),
            (Some(3), Hold),
            (Some(4), Hold),
        ] {
            assert_eq!(closed.withdrawal_after(original, boundary), expected);
        }
        closed.reopened = Some(ReopenedProposal {
            version: RecordVersion::new(3, 12).into(),
            proposal: [3; 32],
        });
        assert_eq!(closed.withdrawal_after(original, Some(3)), Withdraw);
        closed.reopened = None;
        closed.version = RecordVersion::new(3, 14).into();
        assert_eq!(closed.withdrawal_after(original, Some(3)), Withdraw);
        closed.version = RecordVersion::new(3, 10).into();
        assert_eq!(closed.withdrawal_after(original, None), Hold);
    }
}
