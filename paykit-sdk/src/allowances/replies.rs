//! A conflict observed in the lifetime of an ordinary pending proposal.

use super::{allowance_record_target, named_allowance, AnswerOrder, Paykit};
use crate::divergent::SeenVersion;
use crate::error::{PaykitError, Result};
use crate::protocol::{store_failure, view_failure};
use crate::receive::{self, Fail};
use crate::records::KeptMessage;
use crate::state::{self, key, Binder};
use crate::stored::StoredNote;
use paykit_lib::bodies::ProposerRole;
use paykit_lib::collections::Collection;
use paykit_lib::records::{PendingAllowanceRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{
    LocalWrite, ReceiveContext, RecordMarkContext, SharedRecord, StoreFailure,
};
use pubky_switch_core::ops::RecordVersion;
use serde::{Deserialize, Serialize};

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
    Ok(state::delivery_get::<PendingConflict>(delivery, &key::pending_conflict(&key))?.is_some())
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
        message: &StoredMessage,
        order: AnswerOrder,
        kept: &[(StoredMessage, AnswerOrder)],
    ) -> Option<Self> {
        let replies = || {
            kept.iter()
                .map(|(m, at)| (m, *at))
                .chain(std::iter::once((message, order)))
                .filter(|(m, _)| answers(proposal, m))
        };
        let (q, first) = replies().min_by_key(|(_, at)| *at)?;
        if !matches!(q.body(), Body::AllowanceAcceptance(_)) {
            return None;
        }
        let (r, later) = replies()
            .filter(|(m, at)| *at > first && matches!(m.body(), Body::AllowanceRejection(_)))
            .min_by_key(|(_, at)| *at)?;
        Some(Self {
            source: held.version.into(),
            pending: held.content.value()?.to_vec(),
            acceptance: KeptMessage::taken(q, first.1),
            rejection: KeptMessage::taken(r, later.1),
        })
    }

    fn original(&self, own: &[u8; 32]) -> Result<PendingAllowanceRecord> {
        PendingAllowanceRecord::read(&self.pending, own).map_err(|_| PaykitError::corrupt())
    }

    fn source(&self) -> RecordVersion {
        self.source.record_version()
    }

    fn first_order(&self) -> Result<AnswerOrder> {
        self.rejection.order()
    }

    fn reply(&self, own: &[u8; 32], answer: &StoredMessage) -> Result<PendingReply> {
        let original = self.original(own)?;
        Ok(if !answers(original.proposal(), answer) {
            PendingReply::Unrelated
        } else if matches!(answer.body(), Body::AllowanceRejection(_)) {
            PendingReply::Stopped
        } else {
            PendingReply::Waits
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
        if !(p.sender() == *own
            && matches!(q.body(), Body::AllowanceAcceptance(_))
            && matches!(r.body(), Body::AllowanceRejection(_))
            && answers(p, &q)
            && answers(p, &r)
            && self.acceptance.order()? < self.rejection.order()?)
        {
            return Err(PaykitError::corrupt());
        }
        Ok(())
    }
}

/// Only a qualified original can establish a new stop. A previously acquired stop
/// survives an uncertain or unreadable current record.
fn original_for(
    held: &SharedRecord,
    target: &pubky_switch_client::app::RecordTarget,
    key: &[u8; 32],
    own: &[u8; 32],
    answer: &StoredMessage,
) -> Option<PendingAllowanceRecord> {
    if held.key != *key
        || held.collection != target.collection.name
        || held.policy != target.collection.policy
        || !held.authenticated
        || held.doubtful
        || held.not_taken
        || held.flagged
        || held.divergent
        || held.different_write_at_held_version
    {
        return None;
    }
    let pending = PendingAllowanceRecord::read(held.content.value()?, own).ok()?;
    let p = pending.proposal();
    (pending.conflicted().is_none() && p.sender() == *own && answers(p, answer)).then_some(pending)
}

pub(super) enum PendingReply {
    Unrelated,
    Waits,
    Stopped,
}

pub(super) fn observe(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
    answer: &StoredMessage,
    seq: u64,
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
    if let Some(stop) = PendingConflict::from_answers(
        p,
        held,
        answer,
        super::answer_order(answer, seq).map_err(|_| Fail::corrupt())?,
        &kept,
    ) {
        receive::keep_local(cx, receive::local(&at, &stop)?);
        return Ok(if rejected {
            PendingReply::Stopped
        } else {
            PendingReply::Waits
        });
    }
    Ok(PendingReply::Waits)
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
    let Some(stop) = state::cx_get::<PendingConflict>(cx, &key::pending_conflict(&record.key))
        .map_err(Fail::into_store_failure)?
    else {
        return Ok(Vec::new());
    };
    if version <= stop.source.record_version() {
        return Ok(Vec::new());
    }
    let own = cx.identity();
    let pending = stop.original(&own).map_err(view_failure)?;
    let peer = pending.counterparty();
    let permanent = cx
        .record_key(&super::allowance_target(peer, pending.allowance_id()))
        .map_err(store_failure)?;
    let held = state::cx_scan::<KeptMessage>(cx, &key::kept_reply(peer, &permanent, ""))
        .map_err(Fail::into_store_failure)?;
    let stored = state::cx_scan::<StoredNote>(cx, &key::stored_notes_of(peer))
        .map_err(Fail::into_store_failure)?;
    retire(&own, &record.key, pending.proposal(), held, stored).map_err(view_failure)
}

impl Paykit {
    /// Decide the later placement of a signed answer, whether retained as a kept reply
    /// or as a stored entry. Both stores carry its original inbox position.
    pub(super) async fn pending_reply_later(
        &mut self,
        record: &[u8; 32],
        message: &StoredMessage,
        order: AnswerOrder,
    ) -> Result<PendingReply> {
        let peer = message.sender();
        let id = named_allowance(message.body()).ok_or_else(PaykitError::corrupt)?;
        let target = allowance_record_target(Collection::AllowanceProposals, &peer, id);
        let key = self.app.record_key(&target)?;
        let own = self.identity();
        let at = key::pending_conflict(&key);
        if let Some(stop) = self.local_get::<PendingConflict>(&at).await? {
            return stop.reply(&own, message);
        }
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(PendingReply::Unrelated);
        };
        let Some(pending) = original_for(&held, &target, &key, &own, message) else {
            return Ok(PendingReply::Unrelated);
        };
        let kept = Box::pin(self.answers_kept(record, message)).await?;
        if let Some(stop) =
            PendingConflict::from_answers(pending.proposal(), &held, message, order, &kept)
        {
            self.local_put(&at, &stop).await?;
            return stop.reply(&own, message);
        }
        Ok(if matches!(message.body(), Body::AllowanceAcceptance(_)) {
            PendingReply::Waits
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
    mut imported: std::collections::BTreeMap<Vec<u8>, PendingConflict>,
    writes: &mut Vec<LocalWrite>,
) -> Result<()> {
    let own = binder.identity();
    for (key, bytes) in restoring.previous_local_scan(key::PENDING_CONFLICTS.as_bytes())? {
        let old = state::restored::<PendingConflict>(&key, &bytes, binder)?;
        if let Some(other) = imported.get(&key) {
            if other.source() > old.source()
                || (other.source() == old.source() && other.first_order()? < old.first_order()?)
            {
                continue;
            }
        }
        let q = old.unplaced_acceptance_key(binder)?;
        if !writes
            .iter()
            .any(|w| matches!(w, LocalWrite::Put { key,.. } if key == q.as_bytes()))
        {
            for (at, bytes) in restoring.previous_local_scan(q.as_bytes())? {
                if at == q.as_bytes() {
                    let held = state::restored::<KeptMessage>(&at, &bytes, binder)?;
                    if held.stored(own)?.hash() != old.acceptance.stored(own)?.hash() {
                        return Err(PaykitError::corrupt());
                    }
                    writes.push(LocalWrite::Put {
                        key: at,
                        value: bytes,
                    });
                }
            }
        }
        imported.insert(key, old);
    }
    for (key, stop) in imported {
        writes.push(LocalWrite::Put {
            key,
            value: state::encode(&stop)?,
        });
    }
    Ok(())
}
