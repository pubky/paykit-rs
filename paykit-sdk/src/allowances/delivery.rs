//! Current pending and permanent records for a proposal, rejection or withdrawal.

use super::{allowance_record_target, allowance_target, bind_at, named_allowance};
use crate::divergent::SeenVersion;
use crate::protocol::view_failure;
use crate::records::KeptMessage;
use crate::state::{self, key, Binder};
use paykit_lib::bodies::check_body;
use paykit_lib::collections::Collection;
use paykit_lib::kinds::Kind;
use paykit_lib::records::{PendingAllowanceRecord, StoredMessage};
use paykit_lib::AllowanceId;
use pubky_switch_client::app::{
    AppendDecision, DeliveryView, KeptReason, RecordContent, RecordRead, RecordTarget, StoreFailure,
};
use pubky_switch_client::error::StoreError;
use pubky_switch_core::{enc::serde_b64, message::Message, ops::RecordVersion};
use serde::{Deserialize, Serialize};

/// The authenticated deletion that retired the exact proposal this signed reply names.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AppliedReply {
    /// Time of the signed original, to retire the result after its replay window.
    pub(crate) created_at_ms: u64,
    #[serde(with = "serde_b64")]
    pub(crate) reply: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) proposal: [u8; 32],
    retirement: SeenVersion,
}

impl AppliedReply {
    pub(super) fn new(reply: &StoredMessage, version: RecordVersion) -> Self {
        Self {
            created_at_ms: reply.message().created_at_ms,
            reply: reply.hash(),
            proposal: reply.message().refs[0],
            retirement: version.into(),
        }
    }
}

/// The latest generation the followed, signed identity says was lost.
/// An orderly provider move preserves continuity.
pub(super) fn deletion_continues(generation: u64, recovery_from: Option<u64>) -> bool {
    recovery_from.is_none_or(|lost| generation > lost)
}

/// A two-reference end belongs to an accepted Allowance and uses its own delivery rule.
pub(crate) fn pending_delivery(msg: &Message) -> bool {
    match Kind::from_kind(&msg.kind) {
        Some(Kind::AllowanceProposal | Kind::AllowanceAcceptance | Kind::AllowanceRejection) => {
            true
        }
        Some(Kind::AllowanceEnd) => msg.refs.len() == 1,
        _ => false,
    }
}

fn corrupt() -> StoreFailure {
    StoreFailure::of_entry(StoreError::Corrupt)
}

/// Only a local read. A reply still in an unanswered batch has no applied result.
pub(crate) fn reply_is_applied(d: &DeliveryView<'_>) -> Result<bool, StoreFailure> {
    let msg = d.message();
    match Kind::from_kind(&msg.kind) {
        Some(Kind::AllowanceAcceptance | Kind::AllowanceRejection) => (),
        Some(Kind::AllowanceEnd) if msg.refs.len() == 1 => (),
        _ => return Ok(true),
    }
    let applied = state::delivery_get::<AppliedReply>(d, &key::applied_reply(&d.hash()))
        .map_err(view_failure)?;
    Ok(applied.is_some_and(|applied| {
        msg.created_at_ms == applied.created_at_ms && msg.refs.as_slice() == [applied.proposal]
    }))
}

fn allowance_id(d: &DeliveryView<'_>) -> Result<AllowanceId, StoreFailure> {
    let body = check_body(d.message(), d.plaintext().json()).map_err(|_| corrupt())?;
    named_allowance(&body).cloned().ok_or_else(corrupt)
}

fn targets(to: &[u8; 32], id: &AllowanceId) -> [RecordTarget; 2] {
    [
        allowance_record_target(Collection::AllowanceProposals, to, id),
        allowance_target(to, id),
    ]
}

pub(crate) fn append_records(d: &DeliveryView<'_>) -> Result<Vec<RecordTarget>, StoreFailure> {
    let msg = d.message();
    if pending_delivery(msg) {
        return Ok(targets(&msg.to, &allowance_id(d)?).into());
    }
    Ok(Vec::new())
}

/// A current, readable value or a positive observation of absence. A hosted absence in
/// place of a previously held permanent record is missing history.
pub(super) fn current<'a>(
    target: &RecordTarget,
    key: &[u8; 32],
    read: &'a RecordRead,
    ordinary: bool,
) -> Option<Option<&'a [u8]>> {
    match read {
        RecordRead::Kept {
            reason: KeptReason::NotFound,
            record,
        } if record.is_none()
            || (ordinary
                && record.as_ref().is_some_and(|r| {
                    super::replies::qualified_at(r, target, key)
                        && r.content == RecordContent::Deleted
                })) =>
        {
            Some(None)
        }
        RecordRead::Fresh(r) if super::replies::qualified_at(r, target, key) => match &r.content {
            RecordContent::Value(v) => Some(Some(v)),
            RecordContent::Deleted if ordinary => Some(None),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn judge_append(
    d: &DeliveryView<'_>,
    records: &[(RecordTarget, RecordRead)],
) -> Result<AppendDecision, StoreFailure> {
    use AppendDecision::{Hold, Send, Withdraw};
    let msg = d.message();
    let own = d.local().identity();
    if msg.from.identity != own || msg.from.app_id != d.cert().app_id() {
        return Ok(Hold);
    }
    let id = allowance_id(d)?;
    let [(p_target, p_read), (a_target, a_read)] = records else {
        return Ok(Hold);
    };
    let [pending_target, permanent_target] = targets(&msg.to, &id);
    if p_target != &pending_target || a_target != &permanent_target {
        return Ok(Hold);
    }
    let blind = |input: &[u8]| d.blind(input);
    let binder = Binder::new(own, d.cert(), &blind);
    let p_key = binder.record_key(p_target).map_err(view_failure)?;
    let a_key = binder.record_key(a_target).map_err(view_failure)?;
    let (Some(p), Some(a)) = (
        current(p_target, &p_key, p_read, true),
        current(a_target, &a_key, a_read, false),
    ) else {
        return Ok(Hold);
    };
    if super::evidence::holds_delivery(d, &p_key, &a_key).map_err(view_failure)?
        || state::delivery_get::<KeptMessage>(d, &key::conflict(&a_key))
            .map_err(view_failure)?
            .is_some()
        || !state::bound_view_scan::<KeptMessage>(d.local(), &key::allowance_ends(&a_key), &binder)
            .map_err(view_failure)?
            .is_empty()
        || (p.is_some() && a.is_some())
    {
        return Ok(Hold);
    }
    let pending = match p {
        Some(value) => match PendingAllowanceRecord::read(value, &own) {
            Ok(record) if record.counterparty() == &msg.to && record.allowance_id() == &id => {
                Some(record)
            }
            _ => return Ok(Hold),
        },
        None => None,
    };
    let permanent = match a {
        Some(value) => match bind_at(value, &own, &msg.to, &id) {
            Some(record) => Some(record),
            None => return Ok(Hold),
        },
        None => None,
    };
    let kind = Kind::from_kind(&msg.kind);
    let applied = || {
        state::delivery_get::<AppliedReply>(d, &key::applied_reply(&d.hash())).map_err(view_failure)
    };
    if kind == Some(Kind::AllowanceEnd) {
        if pending.is_some() || permanent.is_some() {
            return Ok(Withdraw);
        }
        let retired =
            state::delivery_get::<super::PendingRetirement>(d, &key::pending_retirement(&p_key))
                .map_err(view_failure)?;
        return Ok(match (applied()?.map(|r| r.retirement), retired) {
            (Some(original), Some(current)) => {
                current.withdrawal_after(original, d.recovery_from())
            }
            _ => Hold,
        });
    }
    if pending.as_ref().is_some_and(|r| r.conflicted().is_some())
        || permanent
            .as_ref()
            .is_some_and(|r| r.conflicted.is_some() || r.end.is_some())
    {
        return Ok(Withdraw);
    }
    if let Some(record) = &pending {
        let retired =
            state::delivery_get::<super::PendingRetirement>(d, &key::pending_retirement(&p_key))
                .map_err(view_failure)?;
        let kept =
            state::bound_view_scan(d.local(), &key::kept_reply(&msg.to, &a_key, ""), &binder)
                .map_err(view_failure)?;
        let notes = state::bound_view_scan(d.local(), &key::stored_notes_of(&msg.to), &binder)
            .map_err(view_failure)?;
        if super::pending::awaits_competing_proposal(&own, record, retired.as_ref(), kept, notes)
            .map_err(view_failure)?
        {
            return Ok(Hold);
        }
    }
    let supported = match kind {
        Some(Kind::AllowanceProposal) => pending.is_some_and(|r| r.proposal().hash() == d.hash()),
        Some(Kind::AllowanceAcceptance) => permanent.is_some_and(|r| {
            msg.refs.as_slice() == [r.proposal.hash()]
                && r.response
                    .is_some_and(|accepted| accepted.hash() == d.hash())
        }),
        Some(Kind::AllowanceRejection) => {
            let [proposal] = msg.refs.as_slice() else {
                return Ok(Hold);
            };
            pending.is_none_or(|r| r.proposal().hash() != *proposal)
                && permanent.is_none_or(|r| r.proposal.hash() != *proposal)
        }
        _ => return Ok(Hold),
    };
    if !supported {
        return Ok(Withdraw);
    }
    if kind == Some(Kind::AllowanceRejection)
        && !applied()?.is_some_and(|reply| {
            deletion_continues(
                reply.retirement.record_version().generation,
                d.recovery_from(),
            )
        })
    {
        return Ok(Hold);
    }
    Ok(Send)
}

impl crate::Paykit {
    pub(crate) async fn held_allowance_end(
        &mut self,
        q: &pubky_switch_client::app::QueuedMessage,
        msg: &Message,
        id: &AllowanceId,
    ) -> crate::Result<crate::restore_held::StopEvidence> {
        use crate::restore_held::{exact_stop, StopEvidence::*};

        let target = allowance_target(&q.to, id);
        let key = self.app.record_key(&target)?;
        let read = self.app.read_record_fresh(&target).await?;
        if let RecordRead::Kept { reason, record } = &read {
            if let Some(error) = crate::pay::kept_read_stop(reason) {
                return Err(error);
            }
            // An authenticated absence with nothing retained holds no end: withdrawn.
            // Anything else kept waits (Paykit spec 7.2).
            return Ok(
                if matches!(reason, KeptReason::NotFound) && record.is_none() {
                    Absent
                } else {
                    Unreadable
                },
            );
        }
        // A counterparty's end, answer or conflict kept and not placed yet, for the
        // Allowance or its proposal, waits for its placing (Paykit spec 8.3, "Ends reach the
        // executor", "Conflicted").
        let pending = self.app.record_key(&allowance_record_target(
            Collection::AllowanceProposals,
            &q.to,
            id,
        ))?;
        if self.unplaced_for(&[key, pending], &q.to).await? {
            return Ok(Unreadable);
        }
        // The one order of every held decision: paused waits; a confirmed view is decided
        // on, divergent or not (`Paykit::held_record_paused`).
        if self.held_record_paused(&key).await? {
            return Ok(Unreadable);
        }
        let value = match &read {
            RecordRead::Fresh(r) if super::replies::qualified_confirmed(r, &target, &key) => {
                match &r.content {
                    RecordContent::Value(v) => v.as_slice(),
                    RecordContent::Deleted => return Ok(Absent),
                    // Sealed under a key not held yet, or not read for another reason: the
                    // next read may verify it, so it waits.
                    RecordContent::Unreadable(_) => return Ok(Unreadable),
                }
            }
            _ => return Ok(Unreadable),
        };
        let Some(record) = bind_at(value, &self.identity(), &q.to, id) else {
            return Ok(Unreadable);
        };
        // "An app sends no acceptance, rejection or end for a conflicted Allowance" and "the
        // counterparty is told nothing" (Paykit spec 8.3, Conflicted): withdrawn, no notice.
        if record.conflicted.is_some() {
            return Ok(Absent);
        }
        let accepted = super::acceptance(&record);
        if let Some(end) = &record.end {
            if !exact_stop(end, q, self) {
                return Ok(Absent);
            }
            // Without a durable user origin the original goes under the generated end's
            // counterparty and window checks (Paykit spec 7.2, 8.3).
            let at = key::end_origin(&q.msg_id);
            if self.local_get::<super::EndOrigin>(&at).await?.is_none() {
                let origin = super::EndOrigin::of(
                    key,
                    &record.proposal,
                    accepted,
                    end,
                    super::EndCause::Automatic,
                )?;
                self.local_put(&at, &origin).await?;
            }
            return Ok(Recorded);
        }
        let open = record.conflicted.is_none()
            && accepted.is_some_and(|a| msg.refs.as_slice() == [record.proposal.hash(), a.hash()]);
        Ok(if open { Open } else { Absent })
    }
}
