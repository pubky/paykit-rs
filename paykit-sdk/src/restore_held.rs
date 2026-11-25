//! Disposition of messages the Dispatch held at restore (Paykit spec 7.2, 9; Dispatch spec 12.4).

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use paykit_lib::derive::natural_key;
use paykit_lib::kinds::{Kind, MessageKind};
use paykit_lib::records::StoredMessage;
use paykit_lib::{Body, Collection, Label};
use pubky_dispatch_client::app::{DeliveryView, QueuedMessage, ReceiveReport, ReplyGrantOnRelease};
use pubky_dispatch_core::message::Message;

use crate::activity::{family, ActivityTarget, KeptSide, SideTag};
use crate::notices::{GivenUpFrom, GivenUpOrigin, KeptNotice};
use crate::protocol::BACKUP_NAME;
use crate::records::KeptMessage;
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

/// Whether a held original is within the one bound on every wait: "A copy reconstructed
/// from stored plaintext may go only until 30 days after its `created_at`, less the clock
/// tolerance" (Paykit spec 7.2). The client gives a carried copy up by its own deadline too,
/// so the earlier of the two applies. Judged on the service time the reads verified (`now`),
/// never the device clock; with none verified yet, it isn't past.
fn within_window(original: &Message, now: Option<u64>) -> bool {
    now.is_none_or(|now| !crate::send_holds::past_replay_window(original.created_at_ms, now))
}

/// The original a held stop or proof is decided on: one within its window whose body
/// parses. With no original read, past its window, or with a body that doesn't parse,
/// `None`: every wait ends, and the message is withdrawn and told (Paykit spec 7.2).
fn decidable(original: Option<&Message>, now: Option<u64>) -> Option<&Message> {
    original.filter(|original| within_window(original, now) && Body::parse(original).is_ok())
}

/// Which records a held decision reads: an activity record (a request's, a period's or a
/// payment's), or an Allowance's and its proposal's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Records {
    /// A request's, a period's or a payment's record.
    Activity,
    /// An Allowance's record and its proposal's.
    Allowance,
}

/// What becomes of the owed mark (`hm`) at the end of a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkAfter {
    /// Nothing is left to decide: the mark goes.
    Gone,
    /// The carried lists are done and held messages remain.
    HeldOnly,
    /// The mark stays as it is, for a later pass.
    Stays,
}

/// The owed mark's fate from the pass's end: `swept` when the end-of-pass listing read and
/// its notices were shown, `mark_stays` when a carried list is left to decide, and the held
/// messages still listed (`None` when they couldn't be). A sweep that couldn't list keeps
/// the mark, so a pending stop notice is shown at a later pass: the mark never goes before
/// the sweep over what the client still holds (Paykit spec 7.2).
fn mark_after(swept: bool, mark_stays: bool, pass: HeldPass, still_held: Option<u32>) -> MarkAfter {
    if !swept || mark_stays {
        return MarkAfter::Stays;
    }
    match still_held {
        Some(0) => MarkAfter::Gone,
        Some(_) if pass == HeldPass::CarriedLists => MarkAfter::HeldOnly,
        Some(_) | None => MarkAfter::Stays,
    }
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
    pubky_dispatch_core::scopes::owns_root(BACKUP_NAME, root)
}

// What each held message writes, and in which order (Paykit spec 7.2, Dispatch spec 12.4).
// Paykit's own notes and the client's release or withdrawal are separate commits, so each
// step is written so that a crash or an error before the next leaves a state the next
// receive takes to the same end, telling each thing once:
// - A stop's routing marks (its side, an end's origin) are written while its evidence is
//   read, before the release. They only route; a replay reads them and decides again.
// - A stop the open record no longer shows: its `StopNotRecorded` notice is written
//   pending (not listed), then the client withdraws it. A pass shows each pending notice
//   whose message the client no longer holds (`show_pending_stops`), at its end and again
//   at the next one's start, so it is shown once, only for a stop that went. A crash before
//   the withdrawal leaves the message held and the notice pending; the next pass decides
//   again, and any other decision removes it. A restore in place keeps the installation's
//   notices over the backup's.
// - A carried list: the withdrawal owed to its recipient and, while its outcome matters,
//   its unknown-delivery notice are written first, then the withdrawal, and before any
//   path can expire it, the expiry step and the outbox's flush (`carried_lists_owed`). Both
//   are kept once, so a replay writes nothing twice. The host is told of the carried
//   list's delivery by the client's give-up event. The notice backs the list calls'
//   `Refusal::ListDeliveryUnknown` and stands until a newer version supersedes it: it
//   goes in the same receive once the fresh withdrawal is reserved there, and stays while
//   none can be, such as without plain `paykit_request` (`Paykit::prune_list_notices`).
// - The client's withdrawal tells its give-up as a duty event the host acknowledges. A
//   withdrawal in this pass reaches the host in the next receive's report, since this
//   report's events were taken right after the read.
// - The owed mark (`hm`) is lowered or removed last, once the client lists nothing it
//   covers; while the client can't list them, it stays. A crash before it only runs the
//   pass again, which finds nothing left to decide.
// - The counts are this receive's, best effort: a receive that fails reports none, and what
//   it decided is told by the client's give-up events and the notices.
// One message that can't be decided now, for an error other than a stop of the app,
// waits for the next receive, never past its window (`within_window`); it never fails
// the pass for the others.
impl Paykit {
    /// The one gate for deciding held messages: Paykit's own first read is complete, its
    /// records are listed and their scope keys held, and nothing stops the app (Paykit
    /// spec 7.2, Dispatch spec 12.4). It is asked only after a read Paykit made, once that
    /// read's stops were placed, so another protocol's read never decides a Paykit message;
    /// what that read can't yet show waits: more pages, a listing owed by Paykit's marks, a
    /// scope key not held. The client reports more work while it owes a listing by its own
    /// measure, such as an upload over its record marks, so `more` covers that one.
    pub(crate) async fn held_decisions_open(&self, report: &ReceiveReport) -> Result<bool> {
        Ok(!report.stopped
            && self.certificate_refused().is_none()
            && self.app.recovery_required().is_none()
            && self.app.alarm().is_none()
            && !report.more
            && self.unlisted().await?.is_none()
            && self.holds_scope_keys())
    }

    /// Give up each held Paykit message past its deadline, telling the host, whatever the
    /// gate: the deadline doesn't wait for Paykit's first read. One past the 30 days from
    /// its `created_at` is withdrawn (`within_window`); the client gives up one
    /// past its own replay deadline at a flush. A non-stop error makes the step best
    /// effort, tried again at the next call; a stop of the wallet is returned, typed.
    pub(crate) async fn give_up_expired_held(&mut self) -> Result<()> {
        let queued = match Box::pin(self.carried_lists_owed()).await {
            Ok(Some(queued)) => queued,
            Ok(None) => return Ok(()),
            // Not listed now: nothing expires before the lists are recorded; tried again at
            // the next call.
            Err(error) => {
                crate::pay::unless_stop(error)?;
                return Ok(());
            }
        };
        let held: Vec<String> = queued
            .iter()
            .filter(|q| q.held && paykit_kind(&q.kind))
            .map(|q| q.msg_id.clone())
            .collect();
        // A stop's notice whose message is no longer held is shown, whether or not the
        // decisions can run now: one a crash left pending after its withdrawal included.
        Box::pin(self.show_pending_stops(&held)).await?;
        if held.is_empty() {
            return Ok(());
        }
        let read = self.protocol.held_inventory.begin(&held);
        let listed = self.app.queued_messages(BACKUP_NAME).await;
        let originals = read.0.lock().take().unwrap_or_default();
        drop(read);
        if let Err(error) = listed {
            crate::pay::unless_stop(error)?;
        }
        for q in queued.iter().filter(|q| q.held && paykit_kind(&q.kind)) {
            // A held acceptance only ever ends withdrawn or given up, never released: its
            // payment is marked first, here, before every pass and flush that can end it,
            // so it is never replaced (`Paykit::acceptance_withdrawn_at_restore`).
            if q.kind == Kind::PaymentRequestAcceptance.as_str() {
                Box::pin(self.acceptance_withdrawn_at_restore(&q.msg_id)).await?;
            }
            let past = originals
                .get(&q.msg_id)
                .and_then(Option::as_ref)
                .is_some_and(|original| !within_window(original, self.service_time()));
            // A held message isn't tried: the flush only gives it up once it is too old. A
            // stopped client gives up nothing, and delivers nothing, until it can act.
            let given_up = if past {
                // Its window ended it, not evidence: a pending notice of an earlier pass
                // stands, and is shown once the message is gone (`show_pending_stops`).
                self.app
                    .withdraw_message(BACKUP_NAME, &q.msg_id)
                    .await
                    .map(drop)
            } else {
                self.app.flush_message(&q.msg_id).await.map(drop)
            };
            if let Err(error) = given_up {
                crate::pay::unless_stop(error)?;
            }
        }
        Ok(())
    }

    /// Record what each carried list owes before anything can expire it: its recipient's
    /// fresh withdrawal and, while its outcome matters, its unknown-delivery notice
    /// (Paykit spec 7.2, 8.4). Every path that can expire a message, the expiry step and
    /// the outbox's flush, runs it first. The messages listed, or `None` with no restore's
    /// messages to decide.
    pub(crate) async fn carried_lists_owed(&mut self) -> Result<Option<Vec<QueuedMessage>>> {
        let Some(pass) = self.local_get::<HeldPass>(key::HELD_MESSAGES_OWED).await? else {
            return Ok(None);
        };
        // A listing that fails is returned: no path expires or flushes a carried list
        // before what it owes is recorded.
        let queued = self.app.queued_messages(BACKUP_NAME).await?;
        for q in queued
            .iter()
            .filter(|q| q.kind == Kind::PrivatePaymentList.as_str())
        {
            // A list the installation and backup both owed isn't held: carried all the same
            // in the first pass, unless the host's choice since made it current.
            let carried = q.held
                || (pass == HeldPass::CarriedLists && !self.list_has_current_choice(q).await?);
            if carried {
                self.keep_list_withdrawal(&q.to).await?;
                self.note_carried_list(q).await?;
            }
        }
        Ok(Some(queued))
    }

    /// The Paykit messages the client still holds after a restore; `None` when the client
    /// can't list them now, which keeps every mark that covers them.
    pub(crate) async fn still_held(&self) -> Result<Option<u32>> {
        if self
            .local_get::<HeldPass>(key::HELD_MESSAGES_OWED)
            .await?
            .is_none()
        {
            return Ok(Some(0));
        }
        match self.app.queued_messages(BACKUP_NAME).await {
            Ok(queued) => {
                let held = queued
                    .iter()
                    .filter(|q| q.held && paykit_kind(&q.kind))
                    .count();
                Ok(Some(u32::try_from(held).unwrap_or(u32::MAX)))
            }
            Err(error) => {
                crate::pay::unless_stop(error)?;
                Ok(None)
            }
        }
    }

    /// Decide each held message once the gate is open (Paykit spec 7.2): the messages
    /// released and withdrawn in this pass.
    pub(crate) async fn decide_held_messages(
        &mut self,
        report: &ReceiveReport,
    ) -> Result<(u32, u32)> {
        let (mut released, mut withdrawn) = (0u32, 0u32);
        let Some(pass) = self.local_get::<HeldPass>(key::HELD_MESSAGES_OWED).await? else {
            return Ok((0, 0));
        };
        let ids = match self.app.held_messages(BACKUP_NAME).await {
            Ok(ids) => ids,
            Err(error) => {
                crate::pay::unless_stop(error)?;
                return Ok((0, 0));
            }
        };
        // A stop's notice is shown once its message is no longer held: the expiry step,
        // which runs before every pass, shows those of earlier passes, and the end of this
        // pass those of its own withdrawals (`note_unrecorded_stop`).
        // A carried list the installation and backup both owed isn't held, and is
        // decided in the first pass only.
        let mut mark_stays = false;
        if !ids.is_empty() || pass == HeldPass::CarriedLists {
            let read = self.protocol.held_inventory.begin(&ids);
            let queued = self.app.queued_messages(BACKUP_NAME).await;
            let originals = read.0.lock().take().unwrap_or_default();
            drop(read);
            let queued = match queued {
                Ok(queued) => queued,
                Err(error) => {
                    crate::pay::unless_stop(error)?;
                    return Ok((released, withdrawn));
                }
            };
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
                            mark_stays = true;
                            continue;
                        }
                    }
                }
                let msg = originals.get(&q.msg_id).and_then(Option::as_ref);
                match Box::pin(self.check_held_message(report, q, msg)).await {
                    Ok(Outcome::Released) => released = released.saturating_add(1),
                    Ok(Outcome::Withdrawn) => withdrawn = withdrawn.saturating_add(1),
                    Ok(Outcome::Waiting) => mark_stays |= carried_list,
                    Ok(Outcome::Gone) => {}
                    Err(error) => {
                        crate::pay::unless_stop(error)?;
                        mark_stays |= carried_list;
                    }
                }
            }
        }
        // The withdrawals of this pass show their notices before the mark can go
        // (`mark_after`): the sweep runs only while the mark stands.
        let swept = match self.app.held_messages(BACKUP_NAME).await {
            Ok(left) => {
                Box::pin(self.show_pending_stops(&left)).await?;
                true
            }
            Err(error) => {
                crate::pay::unless_stop(error)?;
                false
            }
        };
        let still_held = if swept && !mark_stays {
            self.still_held().await?
        } else {
            None
        };
        match mark_after(swept, mark_stays, pass, still_held) {
            MarkAfter::Gone => self.local_delete(key::HELD_MESSAGES_OWED).await?,
            MarkAfter::HeldOnly => {
                self.local_put(key::HELD_MESSAGES_OWED, &HeldPass::HeldOnly)
                    .await?;
            }
            MarkAfter::Stays => {}
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
        // Whether evidence read now decided it; a pending notice of a stop an earlier pass
        // found unrecorded stands unless it did.
        let mut by_evidence = false;
        // Every wait ends: past its 30 days, with no original read, or with a body that
        // doesn't parse, a stop or proof is withdrawn and told (`within_window`).
        if decision == Decision::Wait {
            decision = match decidable(msg, self.service_time()) {
                Some(original) => match self.stop_evidence(report, q, original).await {
                    Ok(evidence) => {
                        by_evidence = true;
                        decide(role, evidence)
                    }
                    Err(error) => {
                        crate::pay::unless_stop(error)?;
                        Decision::Wait
                    }
                },
                None => Decision::Withdraw,
            };
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
                } else if by_evidence {
                    // The counterparty's stop, or a record that holds neither: nothing to tell.
                    self.local_delete(&notice).await?;
                }
                if role == HeldRole::Message(Kind::PrivatePaymentList) {
                    self.keep_list_withdrawal(&q.to).await?;
                    self.note_carried_list(q).await?;
                }
                // Marked here too, before the withdrawal, whatever the expiry step managed:
                // a withdrawn acceptance is never replaced.
                if role == HeldRole::Message(Kind::PaymentRequestAcceptance) {
                    Box::pin(self.acceptance_withdrawn_at_restore(&q.msg_id)).await?;
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
                // Shown only once the withdrawal is told (`Paykit::show_pending_stop`).
                GivenUpFrom::Origin(GivenUpOrigin::UnrecordedStopPending),
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
        // A message of the counterparty's for this record kept and not placed yet, which may
        // open it or stop it, decides first: it waits, before anything below can return, a
        // record not written yet and a side not known yet included.
        if self
            .unplaced_for(&[self.activity_key(&target)?], &q.to, Records::Activity)
            .await?
        {
            return Ok(Unreadable);
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
            // And for the payee's record the sale names.
            if self
                .unplaced_for(&[self.activity_key(&target)?], &q.to, Records::Activity)
                .await?
            {
                return Ok(Unreadable);
            }
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
        // `refs`: the request hash, then the hash of the period details the payment used,
        // if any (Paykit wire 4.10). A Billing Period's proof is in its period record.
        let key = match (msg.refs.as_slice(), &proof.billing_period) {
            ([hash] | [hash, _], Some(period)) => crate::periods::period_key(&q.to, hash, period),
            ([hash], None) => natural_key::request_record(&q.to, hash),
            ([], _) => natural_key::payer_payment_record(&self.identity(), &proof.attempt_id),
            // Period details with no period, or more references, name no record.
            ([_, _], None) | ([_, _, _, ..], _) => return Ok(Absent),
        };
        let target = ActivityTarget::payer(key);
        // A message of the counterparty's for this record kept and not placed yet, which may
        // open it or stop it, decides first: it waits, a record not written yet included.
        if self
            .unplaced_for(&[self.activity_key(&target)?], &q.to, Records::Activity)
            .await?
        {
            return Ok(Unreadable);
        }
        let record = match self.fresh_held_activity(&target).await? {
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

    /// Whether a message of `counterparty`'s for one of `records` is kept and not placed
    /// in it yet: a held decision waits for its placing, which may bring the counterparty's
    /// stop, receipt or conflict (Paykit spec 7.2, 8.1, 8.3). The one list of every family
    /// that can hold such a message: kept replies (`pr/`), kept payee cancellations (`kc/`),
    /// an Allowance's kept end (`ae/`), conflict (`cf/`), conflict evidence (`ax/`) and a
    /// pending proposal's conflict (`pc/`), and the client's stored entries noted and not
    /// placed (`se/`). Messages kept over a divergent record (`dw/`) pause it, which waits
    /// already (`Paykit::held_record_paused`).
    pub(crate) async fn unplaced_for(
        &self,
        records: &[[u8; 32]],
        counterparty: &[u8; 32],
        kind: Records,
    ) -> Result<bool> {
        for record in records {
            let replies = format!(
                "{}{}/",
                key::kept_replies_of(counterparty),
                pubky_dispatch_core::enc::b64(record)
            );
            if !self.local_scan::<KeptMessage>(&replies).await?.is_empty()
                || !self
                    .local_scan::<KeptMessage>(&key::kept_cancellations(record))
                    .await?
                    .is_empty()
                || !self
                    .local_scan::<KeptMessage>(&key::allowance_ends(record))
                    .await?
                    .is_empty()
                || self
                    .local_get::<KeptMessage>(&key::conflict(record))
                    .await?
                    .is_some()
                || self.unresolved_allowance(record).await?
                || self
                    .local_get::<crate::allowances::PendingConflict>(&key::pending_conflict(record))
                    .await?
                    .is_some()
            {
                return Ok(true);
            }
        }
        for (_, note) in self
            .local_scan::<crate::stored::BoundNote>(&key::stored_notes_of(counterparty))
            .await?
        {
            if self.unplaced_note_in(&note, records, kind).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Show the pending stop notices whose message the client no longer holds (Paykit spec
    /// 7.2): written before the withdrawal, each is shown once its message is withdrawn or
    /// given up, whatever crash or restore came between.
    async fn show_pending_stops(&mut self, held: &[String]) -> Result<()> {
        let prefix = key::unrecorded_stop_prefix();
        let mut tx = crate::state::Tx::default();
        let mut any = false;
        for (rest, mut kept) in self.local_scan::<KeptNotice>(&prefix).await? {
            let pending = kept.origin == Some(GivenUpOrigin::UnrecordedStopPending);
            if pending && kept.msg_id.as_ref().is_some_and(|id| !held.contains(id)) {
                kept.origin = Some(GivenUpOrigin::UnrecordedStop);
                tx.put(&format!("{prefix}{rest}"), &kept)?;
                any = true;
            }
        }
        if any {
            self.local_commit(tx).await?;
        }
        Ok(())
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

    /// Whether a held message's record is paused, which decides nothing about it, in the one
    /// order every held decision reads its record (Paykit spec 7.2, 8.1 Blocked
    /// counterparties): seen divergent and not confirmed since, or with messages kept over
    /// it still to be written, it waits. Once its user confirmed it, the view the user
    /// confirmed is decided on, divergent or not.
    pub(crate) async fn held_record_paused(&self, key: &[u8; 32]) -> Result<bool> {
        let pause = self.pause_seen(key).await?;
        Ok(pause.paused || pause.writes_waiting)
    }

    /// Read the exact record, including its current write and any earlier unapplied stop
    /// (Paykit spec 6, 8.1). A cached value cannot establish absence after a failed read.
    async fn fresh_held_activity(&mut self, target: &ActivityTarget) -> Result<Fresh> {
        let read = self.fresh_request(target).await?;
        let key = self.activity_key(target)?;
        if self.held_record_paused(&key).await? {
            return Ok(Fresh::Unreadable);
        }
        // Not found at the service while this store retains the record, not as a delete:
        // the two disagree, so the decision waits, as an Allowance end's does
        // (`Paykit::held_allowance_end`), bounded by its 30 days.
        if matches!(read, Fresh::Missing)
            && self.app.shared_record(&key).await?.is_some_and(|held| {
                held.content != pubky_dispatch_client::app::RecordContent::Deleted
            })
        {
            return Ok(Fresh::Unreadable);
        }
        // A record that carries `lost_to_delete` is decided on what it holds. Paykit spec
        // 7.2 asks only that "the record read afresh ... holds that exact message" (and no
        // receipt for its attempt, or no stop of the counterparty's). The loss marker's
        // rule is about paying through the record: a reader takes a payer's record "as one
        // whose occurrence ... is paid never, automatically or by hand" (Paykit wire 5,
        // `lost_to_delete`). Releasing the original proof or stop pays nothing again.
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

    #[tokio::test]
    async fn test_restore_held_unplaced_for_every_family() {
        use crate::allowances::{allowance_record_target, allowance_target};
        use crate::stored::StoredNote;
        use crate::testkit::{object, Dispatches};
        use crate::AppScopes;
        use pubky_dispatch_client::testing::Fault;
        use pubky_dispatch_core::http::ErrorCode;
        use serde_json::json;

        let w = Dispatches::new();
        let mut wallet = Box::pin(w.join(
            &w.alice,
            "wallet",
            AppScopes::new().pay().allowance().build().unwrap(),
        ))
        .await;
        let shop = Box::pin(w.join(
            &w.bob,
            "shop",
            AppScopes::new().request().allowance().build().unwrap(),
        ))
        .await;
        let alice = wallet.identity();
        let bob = shop.identity();
        let generation = wallet.app.own_identity().generation();
        wallet.app_mut().invite(&bob).await.unwrap();
        let sign = |kind, refs, body| shop.sign(alice, kind, refs, object(body)).unwrap();
        let id = paykit_lib::AllowanceId::generate().unwrap();
        let proposal = |limit: &str| {
            sign(
                Kind::AllowanceProposal,
                vec![],
                json!({
                    "allowance_id": id.as_str(), "proposer_role": "allowee",
                    "allower_generation": generation,
                    "terms": {
                        "asset": "btc", "lifetime_amount_limit": limit, "period_limits": [],
                        "per_payment_amount": null, "active_from": null, "expires_at": null,
                        "allowed_payment_endpoint_identifiers": null
                    }
                }),
            )
        };
        let original = proposal("0.10");
        let second = proposal("0.20");
        let deferred = proposal("0.30");
        let end = sign(
            Kind::AllowanceEnd,
            vec![original.hash()],
            json!({ "allowance_id": id.as_str() }),
        );
        let request = sign(
            Kind::PaymentRequest,
            vec![],
            json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "order-1", "proposal_expires_at": null
            }),
        );
        let cancel = sign(
            Kind::PaymentRequestCancellation,
            vec![request.hash()],
            json!({}),
        );
        let payer = ActivityTarget::payer(natural_key::request_record(&bob, &request.hash()));
        let request_key = wallet.activity_key(&payer).unwrap();
        let allowance = wallet.app.record_key(&allowance_target(&bob, &id)).unwrap();
        let target = allowance_record_target(Collection::AllowanceProposals, &bob, &id);
        let pending = wallet.app.record_key(&target).unwrap();
        let cert = shop.app.cert();
        let reply_key = key::kept_reply(&bob, &allowance, original.msg_id());
        let cancel_key = key::kept_cancellation(&request_key, &cert.hash(), generation);
        let end_key = key::allowance_end(&allowance, end.msg_id());
        let note_key = key::stored_note(&bob, request.msg_id());
        let rows = [
            ("pr/", allowance, reply_key, &original),
            ("kc/", request_key, cancel_key, &cancel),
            ("ae/", allowance, end_key, &end),
            ("pc/", pending, key::pending_conflict(&pending), &second),
            ("cf/", allowance, key::conflict(&allowance), &second),
            ("se/", request_key, note_key, &request),
            ("ax/", pending, key::allowance_evidence(&pending), &deferred),
        ];
        let mut competing = None;
        for (prefix, record, at, message) in rows {
            // Read the inbox before Paykit places the messages kept by that read.
            let receive = async |app: &mut Paykit| {
                let seq = w
                    .alice
                    .switch()
                    .send_plaintext(bob, cert, message.plaintext(), &[app.app_id()])
                    .unwrap_or_else(|e| panic!("{prefix} delivery: {e}"));
                Box::pin(app.app_mut().receive(None))
                    .await
                    .unwrap_or_else(|e| panic!("{prefix} receive: {e}"));
                seq
            };
            match prefix {
                "cf/" => {
                    // A proposal of the counterparty's for the Allowance, kept as received
                    // (the `pr/` row's), now at the permanent Allowance key as its conflict.
                    wallet
                        .local_put(&at, competing.as_ref().expect("cf/ competing proposal"))
                        .await
                        .unwrap();
                }
                "ax/" => {
                    // A competing proposal cannot be placed over the divergent original.
                    w.alice
                        .switch()
                        .make_divergent([0x5a; 16], pending, [1; 32])
                        .unwrap();
                    Box::pin(wallet.receive(None)).await.unwrap();
                    receive(&mut wallet).await;
                    w.clock.advance_ms(crate::KEPT_MESSAGE_MS);
                    wallet.app_mut().rotate().await.unwrap();
                    wallet.app_mut().flush_cursor().await.unwrap();
                    Box::pin(wallet.receive(None)).await.unwrap();
                }
                "se/" => {
                    // The refused request write leaves the verified entry in the client's store.
                    wallet.app_mut().flush_cursor().await.unwrap();
                    w.alice.switch().fail_next(Fault::Error {
                        path: "/ops".into(),
                        code: ErrorCode::PermissionMissing,
                        current: serde_json::Value::Null,
                    });
                    receive(&mut wallet).await;
                    let duties = wallet.app_mut().unacknowledged_events().await.unwrap();
                    Box::pin(wallet.take_stored_events(&duties)).await.unwrap();
                }
                _ => {
                    receive(&mut wallet).await;
                }
            }

            let unplaced = async |app: &Paykit, key, peer| {
                let kind = if key == request_key {
                    Records::Activity
                } else {
                    Records::Allowance
                };
                app.unplaced_for(&[key], &peer, kind)
                    .await
                    .unwrap_or_else(|e| panic!("{prefix} {e}"))
            };
            assert!(
                wallet.local_holds(&at).await.unwrap(),
                "{prefix} missing entry"
            );
            assert!(
                unplaced(&wallet, record, bob).await,
                "{prefix} matching record"
            );
            let other = if record == allowance {
                pending
            } else {
                allowance
            };
            assert!(
                !unplaced(&wallet, other, bob).await,
                "{prefix} other record"
            );
            if matches!(prefix, "pr/" | "se/") {
                assert!(
                    !unplaced(&wallet, record, alice).await,
                    "{prefix} other sender"
                );
            }
            match prefix {
                // Placing the original removes the kept reply and opens the pending record.
                "pr/" => {
                    competing = wallet.local_get::<KeptMessage>(&at).await.unwrap();
                    Box::pin(wallet.receive(None)).await.unwrap();
                }
                "se/" => {
                    let mut note = wallet.local_get::<StoredNote>(&at).await.unwrap().unwrap();
                    note.placed = true;
                    wallet.local_put(&at, &note).await.unwrap();
                }
                _ => wallet.local_delete(&at).await.unwrap(),
            }
            assert!(
                !unplaced(&wallet, record, bob).await,
                "{prefix} after placement or removal"
            );
        }
    }

    /// Keep the payee's signed message as an unplaced stored entry at a pay-only wallet.
    async fn unplaced_payee_note(
        kind: Kind,
        request: [u8; 32],
        body: serde_json::Value,
    ) -> (Paykit, [u8; 32]) {
        use crate::stored::StoredNote;
        use crate::testkit::{object, Dispatches};
        use crate::AppScopes;

        let w = Dispatches::new();
        let mut wallet =
            Box::pin(w.join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())).await;
        let shop =
            Box::pin(w.join(&w.bob, "shop", AppScopes::new().request().build().unwrap())).await;
        let bob = shop.identity();
        let signed = shop
            .sign(wallet.identity(), kind, vec![request], object(body))
            .unwrap();
        wallet.app_mut().invite(&bob).await.unwrap();
        // An unreadable notice parks this verified entry before it can write a reply or stop.
        let notice = key::message_notice(&bob, signed.msg_id());
        wallet.local_put(&notice, &false).await.unwrap();
        let seq = w
            .alice
            .switch()
            .send_plaintext(bob, shop.app.cert(), signed.plaintext(), &[wallet.app_id()])
            .unwrap();
        Box::pin(wallet.app_mut().receive(None)).await.unwrap();
        wallet.local_delete(&notice).await.unwrap();
        let parked = wallet.app_mut().parked_entries().await.unwrap();
        let id = parked.iter().find(|p| p.seq == seq).unwrap().stored_id;
        let entry = wallet.app_mut().parked_entry(id).await.unwrap().unwrap();
        let stored = crate::stored::parked_message(&entry, &wallet.identity()).unwrap();
        let note = StoredNote {
            stored_id: id,
            hash: stored.hash(),
            sender: bob,
            msg_id: signed.msg_id().to_owned(),
            message: KeptMessage::taken(&stored, seq),
            placed: false,
        };
        wallet
            .local_put(&key::stored_note(&bob, signed.msg_id()), &note)
            .await
            .unwrap();
        (wallet, bob)
    }

    // Every family `unplaced_for` reads has its row in the family table of
    // `test_restore_held_unplaced_for_every_family`: a row removed, or a family read with no
    // row, fails here, so each row keeps its family's check guarded.
    #[test]
    fn test_restore_held_family_table_has_a_row_per_family() {
        let source = include_str!("restore_held.rs");
        let start = source
            .find("pub(crate) async fn unplaced_for(")
            .expect("unplaced_for");
        let body = &source[start..];
        let body = &body[..body.find("\n    }\n").expect("its end")];
        let table = &source[source.find("let rows = [").expect("the family table")..];
        let table = &table[..table.find("];").expect("its end")];
        // What `unplaced_for` calls to read each family, and the family's prefix.
        let families = [
            ("key::kept_replies_of(", "pr/"),
            ("key::kept_cancellations(", "kc/"),
            ("key::allowance_ends(", "ae/"),
            ("key::conflict(", "cf/"),
            ("self.unresolved_allowance(", "ax/"),
            ("key::pending_conflict(", "pc/"),
            ("key::stored_notes_of(", "se/"),
        ];
        for (call, prefix) in families {
            assert!(body.contains(call), "unplaced_for no longer reads {prefix}");
            assert!(
                table.contains(&format!("(\"{prefix}\"")),
                "the family table has no row for {prefix}"
            );
        }
        // Each `key::` family `unplaced_for` reads is one of those: a new one needs its row.
        let read = families
            .iter()
            .filter(|(call, _)| call.starts_with("key::"))
            .count();
        assert_eq!(body.matches("key::").count(), read, "a family with no row");
    }

    #[tokio::test]
    async fn test_restore_held_unplaced_undecided_place_only_for_activity() {
        let (wallet, bob) = Box::pin(unplaced_payee_note(
            Kind::PaymentPeriodDetails,
            [1; 32],
            serde_json::json!({
                "billing_period": {
                    "starts_at": "2026-02-28T00:00:00.000Z",
                    "ends_at": "2026-03-31T00:00:00.000Z"
                },
                "expires_at": "2026-03-31T00:00:00.000Z",
                "payment_endpoints": { "btc-lightning-bolt11": "lnbc1period" }
            }),
        ))
        .await;
        // The request is missing, so even a different activity record must wait.
        let other = ActivityTarget::payer(natural_key::request_record(&bob, &[2; 32]));
        let activity = wallet.activity_key(&other).unwrap();
        let id = paykit_lib::AllowanceId::generate().unwrap();
        let target = crate::allowances::allowance_target(&bob, &id);
        let allowance = wallet.app.record_key(&target).unwrap();
        assert!(wallet
            .unplaced_for(&[activity], &bob, Records::Activity)
            .await
            .unwrap());
        assert!(!wallet
            .unplaced_for(&[allowance], &bob, Records::Allowance)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn test_restore_held_unplaced_payee_cancellation_at_a_pay_only_payer() {
        let request = [1; 32];
        let (wallet, bob) = Box::pin(unplaced_payee_note(
            Kind::PaymentRequestCancellation,
            request,
            serde_json::json!({}),
        ))
        .await;
        for (hash, matches) in [(request, true), ([2; 32], false)] {
            let target = ActivityTarget::payer(natural_key::request_record(&bob, &hash));
            let record = wallet.activity_key(&target).unwrap();
            assert_eq!(
                wallet
                    .unplaced_for(&[record], &bob, Records::Activity)
                    .await
                    .unwrap(),
                matches
            );
        }
    }

    // send_holds.rs leaves a restore-held stop to the restore's verified service clock.
    #[tokio::test]
    async fn test_restore_held_stop_expiry_leaves_a_held_stop_on_the_device_clock() {
        use crate::testkit::{object, Dispatches};
        use crate::{AppScopes, PaykitConfig, RequestRef};
        use pubky_dispatch_client::app::SENT_RETENTION_MS;
        use pubky_dispatch_client::clock::TestClock;
        use pubky_dispatch_client::store::MemoryStore;
        use std::sync::Arc;

        let w = Dispatches::new();
        let mut wallet =
            Box::pin(w.join(&w.alice, "wallet", AppScopes::new().pay().build().unwrap())).await;
        let shop =
            Box::pin(w.join(&w.bob, "shop", AppScopes::new().request().build().unwrap())).await;
        let bob = shop.identity();
        wallet.app_mut().invite(&bob).await.unwrap();
        let request = shop
            .sign(
                wallet.identity(),
                Kind::PaymentRequest,
                vec![],
                object(serde_json::json!({
                    "amount": { "asset": "btc", "value": "1" },
                    "payment_reference": "order-1", "proposal_expires_at": null,
                    "payment_endpoints": { "btc-lightning-bolt11": "lnbc1invoice" }
                })),
            )
            .unwrap();
        w.alice
            .switch()
            .send_plaintext(
                bob,
                shop.app.cert(),
                request.plaintext(),
                &[wallet.app_id()],
            )
            .unwrap();
        Box::pin(wallet.receive(None)).await.unwrap();
        let request = RequestRef {
            payee: bob,
            request_hash: request.hash(),
        };
        wallet.app_mut().hold_queued(&bob).await.unwrap();
        let signed_at = wallet.now();
        let stop = Box::pin(wallet.reject_request(&request, None))
            .await
            .unwrap();
        wallet.app_mut().release_queued(&bob).await.unwrap();
        // A later backup leaves the Dispatch replay deadline beyond the device time below.
        w.clock.advance_ms(2 * 3_600_000);
        let backup = wallet.backup().await.unwrap();
        let rail = wallet.rail.clone();
        let timer = wallet.timer.clone();
        drop(wallet);

        let past = signed_at + SENT_RETENTION_MS + 3_600_000;
        let mut env = w.alice.env(Arc::new(MemoryStore::new()));
        env.clock = Arc::new(TestClock::new(past));
        let mut wallet = Box::pin(Paykit::restore(
            env,
            PaykitConfig::default(),
            &backup,
            rail,
            timer,
        ))
        .await
        .unwrap();
        // Give the ordinary expiry a current deny-list copy, read now and denying no one,
        // without a service read that would verify a time.
        wallet.local_put(key::LISTS_READ, &past).await.unwrap();
        wallet
            .local_put(key::LISTS_READ_UNDER, &0u64)
            .await
            .unwrap();
        wallet
            .local_put(key::DENIED, &Vec::<[u8; 32]>::new())
            .await
            .unwrap();
        assert!(!wallet.delivery_held(&bob).await.unwrap());
        assert_eq!(wallet.service_time(), None);
        assert!(wallet.local_holds(&key::queued_stop(&stop)).await.unwrap());
        assert!(wallet
            .app
            .held_messages(BACKUP_NAME)
            .await
            .unwrap()
            .contains(&stop));
        Box::pin(wallet.give_up_expired_stops()).await.unwrap();
        assert!(wallet
            .app
            .held_messages(BACKUP_NAME)
            .await
            .unwrap()
            .contains(&stop));

        // At the same clock reading the ordinary expiry does withdraw the released copy.
        wallet
            .app_mut()
            .release_message(BACKUP_NAME, &stop, ReplyGrantOnRelease::Withheld)
            .await
            .unwrap();
        Box::pin(wallet.give_up_expired_stops()).await.unwrap();
        assert!(wallet.app.delivery(&stop).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_restore_held_decidable_original() {
        use crate::testkit::{object, Dispatches};
        use crate::AppScopes;
        use pubky_dispatch_client::app::SENT_RETENTION_MS;
        use serde_json::json;

        let w = Dispatches::new();
        let shop =
            Box::pin(w.join(&w.bob, "shop", AppScopes::new().request().build().unwrap())).await;
        let signed = shop
            .sign(
                w.alice.identity().public(),
                Kind::PaymentRequestCancellation,
                vec![[7; 32]],
                object(json!({})),
            )
            .unwrap();
        let original = Message::parse(signed.plaintext().json(), &[]).unwrap();
        let within = original.created_at_ms + 1;
        let past = original.created_at_ms + SENT_RETENTION_MS;
        assert!(decidable(Some(&original), Some(within)).is_some());
        // With no service time verified yet, the window isn't past.
        assert!(decidable(Some(&original), None).is_some());
        // No original read: withdrawn and told.
        assert!(decidable(None, Some(within)).is_none());
        assert!(decidable(None, None).is_none());
        // Past its 30 days, less the clock tolerance.
        assert!(!within_window(&original, Some(past)));
        assert!(decidable(Some(&original), Some(past)).is_none());
        // A body that doesn't parse for its kind.
        let mut malformed = Message::parse(signed.plaintext().json(), &[]).unwrap();
        malformed.kind = Kind::PaymentRequest.as_str().to_owned();
        assert!(decidable(Some(&malformed), Some(within)).is_none());
    }

    #[test]
    fn test_restore_held_mark_stays_when_the_sweep_cannot_list() {
        for pass in [HeldPass::CarriedLists, HeldPass::HeldOnly] {
            for held in [None, Some(0), Some(2)] {
                assert_eq!(mark_after(false, false, pass, held), MarkAfter::Stays);
            }
        }
    }

    #[test]
    fn test_restore_held_mark_goes_after_a_sweep_listing_nothing_held() {
        for pass in [HeldPass::CarriedLists, HeldPass::HeldOnly] {
            assert_eq!(mark_after(true, false, pass, Some(0)), MarkAfter::Gone);
            // A carried list left to decide keeps it.
            assert_eq!(mark_after(true, true, pass, Some(0)), MarkAfter::Stays);
        }
    }

    #[test]
    fn test_restore_held_mark_stays_while_messages_are_held() {
        assert_eq!(
            mark_after(true, false, HeldPass::CarriedLists, Some(2)),
            MarkAfter::HeldOnly
        );
        assert_eq!(
            mark_after(true, false, HeldPass::HeldOnly, Some(2)),
            MarkAfter::Stays
        );
        // Not listed after a sweep that listed: kept for a later pass.
        assert_eq!(
            mark_after(true, false, HeldPass::HeldOnly, None),
            MarkAfter::Stays
        );
    }

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
