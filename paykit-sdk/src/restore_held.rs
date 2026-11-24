//! Disposition of messages the Switch held at restore (Paykit spec 7.2, 9; Switch spec 12.4).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use paykit_lib::derive::natural_key;
use paykit_lib::kinds::{Kind, MessageKind};
use paykit_lib::records::StoredMessage;
use paykit_lib::{Body, Collection, Label};
use pubky_switch_client::app::{DeliveryView, QueuedMessage, ReceiveReport, ReplyGrantOnRelease};
use pubky_switch_core::message::Message;

use crate::activity::{family, ActivityTarget, KeptSide, SideTag};
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::protocol::BACKUP_NAME;
use crate::requests::{expiry_deadline, payment_ended, request_of, Fresh};
use crate::state::key;
use crate::{Paykit, Result};

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

/// What the record read afresh shows of a carried stop or proof (Paykit spec 7.2).
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
        // A carried proof: released while the payer's record holds it and no receipt for
        // its attempt (Paykit spec 7.2). Its evidence is never `Open`: a proof has no
        // record left open for the user to answer.
        Message(PaymentProof) => match evidence {
            StopEvidence::Recorded => Decision::Release,
            StopEvidence::Open | StopEvidence::Absent => Decision::Withdraw,
            StopEvidence::Unreadable => Decision::Wait,
        },
    }
}

pub(crate) fn exact_stop(s: &StoredMessage, q: &QueuedMessage, app: &Paykit) -> bool {
    s.hash() == q.hash
        && s.message().msg_id == q.msg_id
        && s.sender() == app.identity()
        && s.cert().app_id() == app.app_id()
}

/// What one held message came to in this pass.
enum Outcome {
    Released,
    Withdrawn,
    Waiting,
    /// The client no longer held it, given up meanwhile: nothing to count.
    Gone,
}

/// Whether a kind is Paykit's: the inventory also lists the held messages of kinds no
/// registered protocol owns.
fn paykit_kind(kind: &str) -> bool {
    let root = kind.split_once('.').map_or(kind, |(root, _)| root);
    pubky_switch_core::scopes::owns_root(BACKUP_NAME, root)
}

// What each held message writes, and in which order (Paykit spec 7.2, Switch spec 12.4).
// Paykit's own notes and the client's release or withdrawal are separate commits, so each
// step is written so that a crash or an error before the next leaves a state the next
// receive takes to the same end, telling each thing once:
// - A stop's routing marks (its side, an end's origin) are written while its evidence is
//   read, before the release. They only route; a replay reads them and decides again.
// - A stop the open record no longer shows: its `StopNotRecorded` notice is written
//   first, then the withdrawal. A crash between leaves the message held; the next pass
//   decides again, and any decision other than this one removes the notice it left.
// - A carried list: the withdrawal owed to its recipient and, while its outcome matters,
//   its unknown-delivery notice are written first, then the withdrawal. Both are kept
//   once (`keep_list_withdrawal`, `note_carried_list`), so a replay writes nothing twice.
// - The client's withdrawal tells its give-up as a duty event the host acknowledges. A
//   withdrawal in this pass reaches the host in the next receive's report, since this
//   report's events were taken right after the read.
// - The owed mark (`hm`) is lowered or removed last, once nothing it covers is left: a
//   crash before it only runs the pass again, which finds nothing left to decide.
// One message that can't be decided now, for an error other than a stop of the app,
// waits for the next receive; it never fails the pass for the others.
impl Paykit {
    /// The one gate for deciding held messages: Paykit's own first read is complete, its
    /// records are listed and their scope keys held, and nothing stops the app (Paykit
    /// spec 7.2, Switch spec 12.4).
    pub(crate) async fn held_decisions_open(&self, report: &ReceiveReport) -> Result<bool> {
        Ok(!report.stopped
            && self.certificate_refused().is_none()
            && self.app.recovery_required().is_none()
            && self.app.alarm().is_none()
            && self.confirmed_first_read()
            && self.unlisted().await?.is_none()
            && self.scope_keys_awaited().await?.is_none())
    }

    /// Let the client give up each held Paykit message past its replay deadline, telling
    /// the host, whatever the gate: the deadline doesn't wait for Paykit's first read.
    pub(crate) async fn give_up_expired_held(&mut self) -> Result<()> {
        if self
            .local_get::<HeldPass>(key::HELD_MESSAGES_OWED)
            .await?
            .is_none()
        {
            return Ok(());
        }
        let ids = match self.app.held_messages(BACKUP_NAME).await {
            Ok(ids) => ids,
            Err(error) => return crate::pay::unless_stop(error),
        };
        for id in ids {
            // A held message isn't tried: the flush only gives it up once it is too old.
            if let Err(error) = self.app.flush_message(&id).await {
                crate::pay::unless_stop(error)?;
            }
        }
        Ok(())
    }

    /// The Paykit messages the client still holds after a restore.
    pub(crate) async fn still_held(&self) -> Result<u32> {
        if self
            .local_get::<HeldPass>(key::HELD_MESSAGES_OWED)
            .await?
            .is_none()
        {
            return Ok(0);
        }
        let held = match self.app.queued_messages(BACKUP_NAME).await {
            Ok(queued) => queued
                .iter()
                .filter(|q| q.held && paykit_kind(&q.kind))
                .count(),
            Err(error) => {
                crate::pay::unless_stop(error)?;
                0
            }
        };
        Ok(u32::try_from(held).unwrap_or(u32::MAX))
    }

    /// Decide each held message once the gate is open (Paykit spec 7.2): the counts of
    /// messages released and withdrawn in this pass.
    pub(crate) async fn decide_held_messages(
        &mut self,
        report: &ReceiveReport,
    ) -> Result<(u32, u32)> {
        let (mut released, mut withdrawn) = (0u32, 0u32);
        let Some(pass) = self.local_get::<HeldPass>(key::HELD_MESSAGES_OWED).await? else {
            return Ok((0, 0));
        };
        let ids = self.app.held_messages(BACKUP_NAME).await?;
        // A carried list the installation and backup both owed isn't held, and is
        // decided in the first pass only.
        let mut lists_left = false;
        if !ids.is_empty() || pass == HeldPass::CarriedLists {
            let read = self.protocol.held_inventory.begin(&ids);
            let queued = self.app.queued_messages(BACKUP_NAME).await?;
            let originals = read.0.lock().take().unwrap_or_default();
            drop(read);
            for q in &queued {
                if !paykit_kind(&q.kind) {
                    continue;
                }
                let carried_list = !q.held && q.kind == Kind::PrivatePaymentList.as_str();
                if !q.held {
                    if !carried_list || pass != HeldPass::CarriedLists {
                        continue;
                    }
                    match self.list_has_current_choice(q).await {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => {
                            crate::pay::unless_stop(error)?;
                            lists_left = true;
                            continue;
                        }
                    }
                }
                let msg = originals.get(&q.msg_id).and_then(Option::as_ref);
                match Box::pin(self.check_held_message(report, q, msg)).await {
                    Ok(Outcome::Released) => released = released.saturating_add(1),
                    Ok(Outcome::Withdrawn) => withdrawn = withdrawn.saturating_add(1),
                    Ok(Outcome::Waiting) => lists_left |= carried_list,
                    Ok(Outcome::Gone) => {}
                    Err(error) => {
                        crate::pay::unless_stop(error)?;
                        lists_left |= carried_list;
                    }
                }
            }
        }
        if !lists_left {
            if self.still_held().await? == 0 {
                self.local_delete(key::HELD_MESSAGES_OWED).await?;
            } else if pass == HeldPass::CarriedLists {
                self.local_put(key::HELD_MESSAGES_OWED, &HeldPass::HeldOnly)
                    .await?;
            }
        }
        Ok((released, withdrawn))
    }

    async fn check_held_message(
        &mut self,
        report: &ReceiveReport,
        q: &QueuedMessage,
        msg: Option<&Message>,
    ) -> Result<Outcome> {
        let role = match MessageKind::parse(&q.kind) {
            Ok(MessageKind::Paykit(kind)) => msg.map_or(HeldRole::Message(kind), |m| {
                HeldRole::of(kind, m.refs.len())
            }),
            Ok(MessageKind::Extension(_)) => HeldRole::Extension,
            Err(_) => HeldRole::Unknown,
        };
        let mut decision = decide(role, StopEvidence::Unreadable);
        if decision == Decision::Wait {
            if let Some(original) = msg {
                match self.stop_evidence(report, q, original).await {
                    Ok(evidence) => decision = decide(role, evidence),
                    Err(error) => crate::pay::unless_stop(error)?,
                }
            }
        }
        let notice = key::unrecorded_stop(&q.to, &q.msg_id);
        match decision {
            Decision::Release => {
                self.local_delete(&notice).await?;
                // A carried stop adds no reply grant (Paykit spec 7.2).
                self.app
                    .release_message(BACKUP_NAME, &q.msg_id, ReplyGrantOnRelease::Withheld)
                    .await?;
                Ok(Outcome::Released)
            }
            Decision::Withdraw | Decision::StopNotRecorded => {
                if decision == Decision::StopNotRecorded {
                    let request = msg
                        .filter(|_| role != HeldRole::Message(Kind::AllowanceEnd))
                        .and_then(|m| m.refs.first().copied());
                    self.note_unrecorded_stop(q, request).await?;
                } else {
                    self.local_delete(&notice).await?;
                }
                if role == HeldRole::Message(Kind::PrivatePaymentList) {
                    self.keep_list_withdrawal(&q.to).await?;
                    self.note_carried_list(q).await?;
                }
                Ok(
                    if self.app.withdraw_message(BACKUP_NAME, &q.msg_id).await? {
                        Outcome::Withdrawn
                    } else {
                        Outcome::Gone
                    },
                )
            }
            Decision::Wait => Ok(Outcome::Waiting),
        }
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
        if msg.kind == Kind::PaymentProof.as_str() {
            return self.proof_evidence(q, msg).await;
        }
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
            // An app that doesn't read the payee's records can't hold this as a payee's
            // cancellation: the payer's record, read afresh, doesn't hold it either.
            if !family(Collection::PayeeActivity).readable_by(self.app.cert()) {
                return Ok(Absent);
            }
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
        // Open: neither paid, rejected nor cancelled, and not expired. A payer's rejection
        // whose request another own app accepted leaves it open, so the user can stop it.
        let open = record.counterparty == q.to
            && !payment_ended(&record, None)
            && request_of(&record).is_some_and(|(r, terms)| {
                r.hash() == *hash && !self.deadline_passed(expiry_deadline(&record, terms))
            });
        Ok(if open { Open } else { Absent })
    }

    /// The payer's record of a carried proof's payment, read afresh (Paykit spec 7.2): a
    /// receipt for its attempt first, which leaves nothing owed, then the exact proof.
    async fn proof_evidence(&mut self, q: &QueuedMessage, msg: &Message) -> Result<StopEvidence> {
        use StopEvidence::*;
        let Body::PaymentProof(proof) = Body::parse(msg)? else {
            // A kind and body that disagree carry no proof: the message is withdrawn.
            return Ok(Absent);
        };
        let key = match (msg.refs.as_slice(), &proof.billing_period) {
            ([hash], Some(period)) => crate::periods::period_key(&q.to, hash, period),
            ([hash], None) => natural_key::request_record(&q.to, hash),
            ([], _) => natural_key::payer_payment_record(&self.identity(), &proof.attempt_id),
            // More than one reference names no record of a proof (Paykit wire 4.10).
            ([_, _, ..], _) => return Ok(Absent),
        };
        let record = match self
            .fresh_held_activity(&ActivityTarget::payer(key))
            .await?
        {
            Fresh::Held(record) => record,
            Fresh::Missing => return Ok(Absent),
            Fresh::Unreadable => return Ok(Unreadable),
        };
        let receipted = record.receipt.as_ref().is_some_and(|receipt| {
            matches!(receipt.body(), Body::PaymentReceipt(r) if r.attempt_id == proof.attempt_id)
        });
        if receipted {
            return Ok(Absent);
        }
        Ok(
            if record
                .proof
                .as_ref()
                .is_some_and(|held| exact_stop(held, q, self))
            {
                Recorded
            } else {
                Absent
            },
        )
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
        use Kind::{
            AllowanceEnd, PaymentProof, PaymentRequestCancellation, PaymentRequestRejection,
        };
        let roles = Kind::ALL.map(|kind| HeldRole::of(kind, 2));
        for role in roles.into_iter().chain([
            HeldRole::ProposalWithdrawal,
            HeldRole::Extension,
            HeldRole::Unknown,
        ]) {
            // Per evidence: Recorded, Open, Absent, Unreadable (Paykit spec 7.2).
            let expected = match role {
                Message(PaymentRequestCancellation | PaymentRequestRejection | AllowanceEnd) => {
                    [Release, StopNotRecorded, Withdraw, Wait]
                }
                Message(PaymentProof) => [Release, Withdraw, Withdraw, Wait],
                _ => [Withdraw, Withdraw, Withdraw, Withdraw],
            };
            let evidence = [
                StopEvidence::Recorded,
                StopEvidence::Open,
                StopEvidence::Absent,
                StopEvidence::Unreadable,
            ];
            for (evidence, expected) in evidence.into_iter().zip(expected) {
                assert_eq!(decide(role, evidence), expected, "{role:?} {evidence:?}");
            }
        }
        assert_eq!(HeldRole::of(AllowanceEnd, 1), HeldRole::ProposalWithdrawal);
        assert_eq!(HeldRole::of(AllowanceEnd, 2), Message(AllowanceEnd));
    }
}
