//! Disposition of messages the Switch held at restore (Paykit spec 7.2, 9; Switch spec 12.4).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use paykit_lib::derive::natural_key;
use paykit_lib::kinds::{Kind, MessageKind};
use paykit_lib::records::StoredMessage;
use paykit_lib::{Body, Collection, Label};
use pubky_switch_client::app::{
    Delivery, DeliveryView, QueuedMessage, ReceiveReport, ReplyGrantOnRelease,
};
use pubky_switch_core::message::Message;

use crate::activity::{family, ActivityTarget, KeptSide, SideTag};
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::protocol::BACKUP_NAME;
use crate::requests::{expiry_deadline, payment_ended, request_of, Fresh};
use crate::state::key;
use crate::{HeldAtRestore, Paykit, Result};

// queued_messages supplies the authenticated kind and hash. Its delivery callback
// supplies the signed references for locating a stop's record (Paykit wire 5.3, 5.4).
type Originals = BTreeMap<String, Option<Message>>;

#[derive(Default)]
pub(crate) struct HeldInventory(Mutex<Option<Originals>>);

struct InventoryRead<'a>(&'a HeldInventory);

impl HeldInventory {
    fn lock(&self) -> MutexGuard<'_, Option<Originals>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn begin(&self, ids: &[String]) -> InventoryRead<'_> {
        *self.lock() = Some(ids.iter().cloned().map(|id| (id, None)).collect());
        InventoryRead(self)
    }

    pub(crate) fn observe(&self, view: &DeliveryView<'_>) {
        let message = view.message();
        if let Some(slot) = self
            .lock()
            .as_mut()
            .and_then(|rows| rows.get_mut(&message.msg_id))
        {
            *slot = Some(message.clone());
        }
    }
}

impl Drop for InventoryRead<'_> {
    fn drop(&mut self) {
        self.0.lock().take();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeldRole {
    Message(Kind),
    ProposalWithdrawal,
    Extension,
    Unknown,
}

impl HeldRole {
    fn of(kind: Kind, refs: usize) -> Self {
        if kind == Kind::AllowanceEnd && refs != 2 {
            Self::ProposalWithdrawal
        } else {
            Self::Message(kind)
        }
    }
}

/// The first pass also removes carried lists the installation and backup both owed.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeldPass {
    CarriedLists,
    HeldOnly,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StopEvidence {
    Recorded,
    Open,
    Absent,
    Unreadable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Release,
    Withdraw,
    StopNotRecorded,
    Wait,
}

fn decide(role: HeldRole, evidence: StopEvidence) -> Decision {
    use HeldRole::Message;
    use Kind::*;
    match role {
        Message(PaymentRequestCancellation | PaymentRequestRejection | AllowanceEnd) => {
            match evidence {
                StopEvidence::Recorded => Decision::Release,
                StopEvidence::Open => Decision::StopNotRecorded,
                StopEvidence::Absent => Decision::Withdraw,
                StopEvidence::Unreadable => Decision::Wait,
            }
        }
        Message(
            PaymentRequest
            | PaymentRequestAcceptance
            | PaymentPeriodDetails
            | PaymentProof
            | PaymentReceipt
            | ResendRequest
            | PrivatePaymentList
            | PrivatePaymentListRequest
            | AllowanceProposal
            | AllowanceAcceptance
            | AllowanceRejection,
        )
        | HeldRole::ProposalWithdrawal
        | HeldRole::Extension
        | HeldRole::Unknown => Decision::Withdraw,
        // The spec gives an executor notice no exception (Paykit spec 7.2).
        Message(ExecutorNotice) => Decision::Withdraw,
    }
}

pub(crate) fn exact_stop(s: &StoredMessage, q: &QueuedMessage, app: &Paykit) -> bool {
    s.hash() == q.hash
        && s.message().msg_id == q.msg_id
        && s.sender() == app.identity()
        && s.cert().app_id() == app.app_id()
}

impl Paykit {
    pub(crate) async fn check_held_messages(
        &mut self,
        report: &ReceiveReport,
        confirmed: bool,
    ) -> Result<HeldAtRestore> {
        let mut counts = HeldAtRestore::default();
        let Some(pass) = self.local_get::<HeldPass>(key::HELD_MESSAGES_OWED).await? else {
            return Ok(counts);
        };
        let ids = self.app.held_messages(BACKUP_NAME).await?;
        if !ids.is_empty() || pass == HeldPass::CarriedLists {
            let read = self.protocol.held_inventory.begin(&ids);
            let queued = self.app.queued_messages(BACKUP_NAME).await?;
            let originals = read.0.lock().take().unwrap_or_default();
            drop(read);
            for q in &queued {
                // An unregistered protocol's held messages also appear in this inventory.
                let root = q
                    .kind
                    .split_once('.')
                    .map_or(q.kind.as_str(), |(root, _)| root);
                if !pubky_switch_core::scopes::owns_root(BACKUP_NAME, root) {
                    continue;
                }
                if !confirmed {
                    counts.waiting = counts.waiting.saturating_add(u32::from(q.held));
                    continue;
                }
                if !q.held
                    && (pass != HeldPass::CarriedLists
                        || q.kind != Kind::PrivatePaymentList.as_str()
                        || self.list_has_current_choice(q).await?)
                {
                    continue;
                }
                let msg = originals.get(&q.msg_id).and_then(Option::as_ref);
                // One message that can't be decided now waits for the next receive; it never
                // fails the pass for the others.
                let checked = Box::pin(self.check_held_message(report, q, msg, &mut counts)).await;
                if let Err(error) = checked {
                    crate::pay::unless_stop(error)?;
                    counts.waiting = counts.waiting.saturating_add(1);
                }
            }
        }
        if !confirmed {
            return Ok(counts);
        }
        if counts.waiting == 0 {
            self.local_delete(key::HELD_MESSAGES_OWED).await?;
        } else if pass == HeldPass::CarriedLists {
            self.local_put(key::HELD_MESSAGES_OWED, &HeldPass::HeldOnly)
                .await?;
        }
        Ok(counts)
    }

    async fn check_held_message(
        &mut self,
        report: &ReceiveReport,
        q: &QueuedMessage,
        msg: Option<&Message>,
        counts: &mut HeldAtRestore,
    ) -> Result<()> {
        let role = match MessageKind::parse(&q.kind) {
            Ok(MessageKind::Paykit(kind)) => msg.map_or(HeldRole::Message(kind), |m| {
                HeldRole::of(kind, m.refs.len())
            }),
            Ok(MessageKind::Extension(_)) => HeldRole::Extension,
            Err(_) => HeldRole::Unknown,
        };
        let mut decision = decide(role, StopEvidence::Unreadable);
        if decision == Decision::Wait {
            // Let the Switch give up an expired stop while its hold still prevents delivery.
            if !matches!(
                self.app.flush_message(&q.msg_id).await?,
                Some(Delivery::Queued { .. } | Delivery::Kept { .. })
            ) {
                return Ok(());
            }
            if let Some(original) = msg {
                match self.stop_evidence(report, q, original).await {
                    Ok(evidence) => decision = decide(role, evidence),
                    Err(error) => crate::pay::unless_stop(error)?,
                }
            }
        }
        match decision {
            Decision::Release => {
                // A carried stop adds no reply grant (Paykit spec 7.2).
                self.app
                    .release_message(BACKUP_NAME, &q.msg_id, ReplyGrantOnRelease::Withheld)
                    .await?;
                counts.released = counts.released.saturating_add(1);
            }
            Decision::Withdraw | Decision::StopNotRecorded => {
                if decision == Decision::StopNotRecorded {
                    let request = msg
                        .filter(|_| role != HeldRole::Message(Kind::AllowanceEnd))
                        .and_then(|m| m.refs.first().copied());
                    self.note_unrecorded_stop(q, request).await?;
                }
                if role == HeldRole::Message(Kind::PrivatePaymentList) {
                    self.keep_list_withdrawal(&q.to).await?;
                }
                if self.app.withdraw_message(BACKUP_NAME, &q.msg_id).await? {
                    counts.withdrawn = counts.withdrawn.saturating_add(1);
                }
            }
            Decision::Wait => counts.waiting = counts.waiting.saturating_add(1),
        }
        Ok(())
    }

    async fn note_unrecorded_stop(
        &mut self,
        q: &QueuedMessage,
        request: Option<[u8; 32]>,
    ) -> Result<()> {
        let at = key::unrecorded_stop(&q.to, &q.msg_id);
        if self.kept_notice(&at).await?.is_none() {
            let notice = KeptNotice::given_up(
                q.to,
                &q.msg_id,
                request,
                self.service_time().unwrap_or(0),
                GivenUpFrom::Origin(GivenUpOrigin::UnrecordedStop),
            );
            self.local_put(&at, &notice).await?;
        }
        Ok(())
    }

    async fn stop_evidence(
        &mut self,
        report: &ReceiveReport,
        q: &QueuedMessage,
        msg: &Message,
    ) -> Result<StopEvidence> {
        use StopEvidence::*;
        if msg.kind == Kind::AllowanceEnd.as_str() {
            return match Body::parse(msg)? {
                Body::AllowanceEnd(end) => self.held_allowance_end(q, msg, &end.allowance_id).await,
                // A kind and body that disagree carry no stop: the message is withdrawn.
                _ => Ok(Absent),
            };
        }
        let [hash] = msg.refs.as_slice() else {
            return Ok(Absent);
        };
        let cancellation = msg.kind == Kind::PaymentRequestCancellation.as_str();
        let side_key = key::side(&q.msg_id);
        let side = if cancellation {
            self.local_get::<KeptSide>(&side_key).await?
        } else {
            None
        };
        let mut target = ActivityTarget::payer(natural_key::request_record(&q.to, hash));
        if let Some(payee) = side.as_ref().filter(|s| s.side == SideTag::Payee) {
            let label = payee.label.as_deref().map(Label::parse).transpose()?;
            target = ActivityTarget::payee(
                natural_key::request_record(&self.identity(), hash),
                label.as_ref(),
            );
        }
        let find_side = cancellation && side.is_none();
        let mut read = if find_side && !self.holds("paykit_pay") {
            Fresh::Missing
        } else {
            self.fresh_held_activity(&target).await?
        };
        if find_side && matches!(read, Fresh::Missing) {
            let Some(sale) = self.held_sale(report, q, hash).await? else {
                return Ok(Absent);
            };
            target = sale;
            read = self.fresh_held_activity(&target).await?;
        }
        let record = match read {
            Fresh::Held(record) => record,
            Fresh::Missing => return Ok(Absent),
            Fresh::Unreadable => return Ok(Unreadable),
        };
        let stop = if cancellation {
            &record.cancellation
        } else {
            &record.response
        };
        // The counterparty's own stop of the same request makes ours moot: nothing is owed
        // and the user is not asked to stop again (Paykit spec 7.2).
        let theirs = record.cancellation.iter().chain(&record.response).any(|s| {
            s.sender() == q.to
                && matches!(
                    s.body(),
                    Body::PaymentRequestCancellation(_) | Body::PaymentRequestRejection(_)
                )
        });
        if theirs {
            return Ok(Absent);
        }
        if stop.as_ref().is_some_and(|s| exact_stop(s, q, self)) {
            if find_side {
                // The send gate needs the side even if this restore lacked it (Paykit spec 8.1).
                self.local_put(
                    &side_key,
                    &KeptSide {
                        msg_id: q.msg_id.clone(),
                        side: target.side,
                        label: target.label().map(|l| l.as_str().to_owned()),
                    },
                )
                .await?;
            }
            return Ok(Recorded);
        }
        let open = record.counterparty == q.to
            && !payment_ended(&record, None)
            && (cancellation || record.response.is_none())
            && request_of(&record).is_some_and(|(r, terms)| {
                r.hash() == *hash && !self.deadline_passed(expiry_deadline(&record, terms))
            });
        Ok(if open { Open } else { Absent })
    }

    async fn held_sale(
        &mut self,
        report: &ReceiveReport,
        q: &QueuedMessage,
        hash: &[u8; 32],
    ) -> Result<Option<ActivityTarget>> {
        let inventory = self
            .app
            .current_collection(&family(Collection::PayeeActivity), report)
            .await?
            .ok_or_else(crate::hold::unlisted)?;
        let mut unread = !inventory.uncertain_keys.is_empty();
        for held in &inventory.live {
            let Ok((target, record)) = self.bound_payee_record(held) else {
                unread = true;
                continue;
            };
            if record.counterparty == q.to
                && record.request.as_ref().is_some_and(|r| r.hash() == *hash)
            {
                return Ok(Some(target));
            }
        }
        if unread {
            return Err(crate::requests::unreadable_request());
        }
        Ok(None)
    }

    /// Read the exact record, including its current write and any earlier unapplied stop
    /// (Paykit spec 6, 8.1). A cached value cannot establish absence after a failed read.
    async fn fresh_held_activity(&mut self, target: &ActivityTarget) -> Result<Fresh> {
        let read = self.fresh_request(target).await?;
        let key = self.activity_key(target)?;
        // Nothing is decided on a held message while its record is paused as divergent
        // and unconfirmed (Paykit spec 7.2, 8.1 Blocked counterparties).
        let pause = self.pause_seen(&key).await?;
        if pause.paused || pause.adds_no_stop_waits() {
            return Ok(Fresh::Unreadable);
        }
        if matches!(&read, Fresh::Held(record) if record.lost_to_delete.is_some()) {
            return Ok(Fresh::Unreadable);
        }
        if let Some(held) = self.app.shared_record(&key).await? {
            if !held.authenticated
                || held.flagged
                || held.not_taken
                || held.doubtful
                || crate::records::content_disputed(&held)
                || held.policy != target.record_target().collection.policy
            {
                return Ok(Fresh::Unreadable);
            }
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_held_decide_every_role() {
        use Decision::*;
        use HeldRole::Message;
        use Kind::{AllowanceEnd, PaymentRequestCancellation, PaymentRequestRejection};
        let roles = Kind::ALL.map(|kind| HeldRole::of(kind, 2));
        for role in roles.into_iter().chain([
            HeldRole::ProposalWithdrawal,
            HeldRole::Extension,
            HeldRole::Unknown,
        ]) {
            for (evidence, stop) in [
                (StopEvidence::Recorded, Release),
                (StopEvidence::Open, StopNotRecorded),
                (StopEvidence::Absent, Withdraw),
                (StopEvidence::Unreadable, Wait),
            ] {
                let expected = if matches!(
                    role,
                    Message(PaymentRequestCancellation | PaymentRequestRejection | AllowanceEnd)
                ) {
                    stop
                } else {
                    Withdraw
                };
                assert_eq!(decide(role, evidence), expected, "{role:?} {evidence:?}");
            }
        }
        assert_eq!(HeldRole::of(AllowanceEnd, 1), HeldRole::ProposalWithdrawal);
        assert_eq!(HeldRole::of(AllowanceEnd, 2), Message(AllowanceEnd));
    }
}
