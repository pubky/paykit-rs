//! A conflict observed in the lifetime of an ordinary pending proposal.

use super::{allowance_record_target, named_allowance, HeldAnswer, Paykit};
use crate::divergent::SeenVersion;
use crate::error::{PaykitError, Result};
use crate::protocol::{step_failure, store_failure, view_failure};
use crate::receive::{self, Fail};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder, StateKey};
use crate::stored::StoredNote;
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
    #[serde(with = "pubky_switch_core::enc::serde_b64_vec")]
    pending: Vec<u8>,
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
        if source.is_some_and(|at| at < self.version) {
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
    Waiting,
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
    let permanent = binder.record_key(&super::allowance_target(&message.to, id))?;
    let rejects = |held: &KeptMessage| -> Result<bool> {
        let r = held.stored(binder.identity())?;
        Ok(matches!(r.body(), Body::AllowanceRejection(_))
            && named_allowance(r.body()) == Some(id)
            && r.sender() == message.to
            && r.message().refs.as_slice() == [delivery.hash()]
            && held
                .pending_source
                .is_some_and(|source| retired.as_ref().is_none_or(|end| source > end.version)))
    };
    for (_, kept) in state::bound_view_scan::<KeptMessage>(
        delivery.local(),
        &key::kept_reply(&message.to, &permanent, ""),
        &binder,
    )? {
        if rejects(&kept)? {
            return Ok(true);
        }
    }
    for (_, note) in state::bound_view_scan::<StoredNote>(
        delivery.local(),
        &key::stored_notes_of(&message.to),
        &binder,
    )? {
        if !note.placed && rejects(&note.message)? {
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
            return Opposed::Waiting;
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
            pending: value.to_vec(),
            acceptance: KeptMessage::taken_at(&q.message, first),
            rejection: KeptMessage::taken_at(&r.message, later),
        }))
    }

    pub(super) fn original(&self, own: &[u8; 32]) -> Result<PendingAllowanceRecord> {
        PendingAllowanceRecord::read(&self.pending, own).map_err(|_| PaykitError::corrupt())
    }

    fn source(&self) -> RecordVersion {
        self.source.record_version()
    }

    fn earlier_than(&self, other: &Self) -> Result<bool> {
        let a = self.rejection.order()?.0;
        let b = other.rejection.order()?.0;
        let first = self.rejection.position().ok_or_else(PaykitError::corrupt)?;
        let second = other
            .rejection
            .position()
            .ok_or_else(PaykitError::corrupt)?;
        Ok(a < b || (a == b && first.generation == second.generation && first.seq < second.seq))
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

    pub(super) fn unplaced_acceptance_key(&self, binder: &Binder<'_>) -> Result<String> {
        let own = binder.identity();
        let p = self.original(own)?;
        let a = super::allowance_target(p.counterparty(), p.allowance_id());
        Ok(key::kept_reply(
            p.counterparty(),
            &binder.record_key(&a)?,
            &self.acceptance.stored(own)?.message().msg_id,
        ))
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
            || binder.record_key(&target)? != *key
        {
            return Err(PaykitError::corrupt());
        }
        let p = original.proposal();
        let q = self.acceptance.stored(own)?;
        let r = self.rejection.stored(own)?;
        let first = self
            .acceptance
            .position()
            .ok_or_else(PaykitError::corrupt)?;
        let second = self.rejection.position().ok_or_else(PaykitError::corrupt)?;
        let q_time = self.acceptance.order()?.0;
        let r_time = self.rejection.order()?.0;
        // The writer checked cross-generation order through the account's verified feed history.
        // The restore context has no feed ordering service and preserves the established stop.
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
        Ok(())
    }
}

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
        _ => Err(Fail::Store(StoreFailure::unavailable(
            pubky_switch_client::error::StoreError::Unavailable,
        ))),
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
    notes: Vec<(String, StoredNote)>,
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
    for (at, mut note) in notes {
        if !note.placed && answers(p, &note.bound(&at, own)?) {
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
            state::cx_scan::<StoredNote>(cx, &key::stored_notes_of(peer)).map_err(step_failure)?;
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
                    .filter(|(_, n)| earlier(&n.message))
                    .collect(),
            )
            .map_err(view_failure)?,
        );
    }

    Ok(writes)
}

impl Paykit {
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

    /// The first rejection closes only the selected, still-current pending application.
    /// A failure to settle its conditional operation leaves the original answers in custody.
    async fn delete_rejected_pending(
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
                    return Ok(PendingReply::Undecided);
                }
                writes.push(state::local_put(&kept_key, &held)?);
            }
            if let Some(mut note) = self
                .local_get::<StoredNote>(&stored_key)
                .await?
                .filter(|n| !n.placed)
            {
                if !bind(&mut note.message)? {
                    return Ok(PendingReply::Undecided);
                }
                writes.push(state::local_put(&stored_key, &note)?);
            }
            if writes.is_empty() {
                return Ok(PendingReply::Undecided);
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
            if earlier != Some(Ordering::Greater) {
                return Ok(PendingReply::Undecided);
            }
        }
        let RecordRead::Fresh(current) = self.app.read_record_fresh(target).await? else {
            return Ok(PendingReply::Undecided);
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
            return Ok(PendingReply::Undecided);
        }
        let pending = self.selected_pending(&current)?.0;
        if self
            .competing_proposal_owed(&pending, &selected.key, &self.app.record_key(&permanent)?)
            .await?
        {
            return Ok(PendingReply::Undecided);
        }
        let conditions = vec![
            Condition::RecordVersion {
                key: selected.key,
                version: selected.version,
            },
            Condition::RecordVersion {
                key: self.app.record_key(&permanent)?,
                version: RecordVersion::NONE,
            },
        ];
        let op = self.app.record_op(target, None, conditions).await?;
        match crate::stored::submitted(
            self.submit(OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(op))
                .await,
        ) {
            Ok(crate::stored::Placed::Done) => Ok(PendingReply::Stopped),
            Ok(_) => Ok(PendingReply::Undecided),
            Err(crate::error::PaykitError::Client(error)) if error.kept_batch().is_some() => {
                Ok(PendingReply::Undecided)
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
        let end = self
            .local_get::<PendingRetirement>(&key::pending_retirement(&key))
            .await?;
        if let Some(end) = &end {
            match end.compare(evidence.pending_source, evidence.position(), |a, b| {
                self.app.received_order(a, b)
            }) {
                Some(Ordering::Less) => return Ok(PendingReply::Stopped),
                Some(Ordering::Greater) => (),
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
            Opposed::Unknown if matches!(message.body(), Body::AllowanceAcceptance(_)) => {
                return Ok(PendingReply::Waits(None));
            }
            Opposed::Unknown => return Ok(PendingReply::Undecided),
            Opposed::Rejected(first) if first == message.hash() => {
                return Box::pin(self.delete_rejected_pending(
                    &target,
                    &held,
                    pending.proposal(),
                    message,
                    evidence,
                ))
                .await;
            }
            Opposed::Rejected(_) => return Ok(PendingReply::Undecided),
            Opposed::Waiting => (),
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
    mut imported: BTreeMap<[u8; 32], PendingConflict>,
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
    let retired = |key: &[u8; 32], source: SeenVersion| {
        retirements.get(key).is_some_and(|r| source < r.version)
    };
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
        let q = old.unplaced_acceptance_key(binder)?;
        if let Some((at, bytes)) = restoring
            .previous_local_scan(q.as_bytes())?
            .into_iter()
            .find(|(at, _)| at == q.as_bytes())
        {
            let mut held = state::restored::<KeptMessage>(&at, &bytes, binder)?;
            if held.stored(binder.identity())?.hash()
                != old.acceptance.stored(binder.identity())?.hash()
                || held.order()? != old.acceptance.order()?
                || held.position() != old.acceptance.position()
            {
                return Err(PaykitError::corrupt());
            }
            held.pending_source.get_or_insert(old.source);
            replies.insert(q, held);
        }
        if let Some(other) = imported.get(&key) {
            if other.source() > old.source()
                || (other.source() == old.source() && other.earlier_than(&old)?)
            {
                continue;
            }
        }
        imported.insert(key, old);
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
