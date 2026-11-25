//! Entries the client stored locally for good, and what Paykit does with each (Dispatch spec
//! 8.2; Paykit spec 8.1): the duty events that tell of them are taken at every receive and
//! kept in Paykit's own state.
//!
//! - **An effect refused for good** (`EffectRefused`, or a `Parked` duty event of any reason
//!   but `Uncertain`, `Released` or `AwaitingAdmission`), such as a record write refused
//!   with `403 permission_missing`, or one over a quota: the message is kept
//!   (`p/se/<sender>/<msg_id>`)
//!   and written into its record by a later write of the app's own, tried at every receive
//!   until it lands, as room frees: a request, a payer's reply, a proof of a payment without
//!   a request into the record of its settlement, a receipt beside the proof it names, and
//!   period details and a Billing Period's proof into the period record, once the request
//!   record they are checked against is held.
//!   Meanwhile a Private Payment List is held over its record (`lh/`), the newer kept, and
//!   placed as any held list is ([`Paykit::place_held_lists`]); a
//!   request is shown unrecorded ([`crate::RequestStatus::Unrecorded`]). A payee's
//!   cancellation needs nothing here: its stop is kept with the cursor move, with no write
//!   at the Dispatch (`kc/`, Paykit spec 8.1). An Allowance proposal or answer is tried
//!   against its `allowances` record. An acceptance that would fill the empty `response`
//!   stays unplaced (Paykit spec 8.3.1). An end is kept as the stop at once (`ae/`, Paykit
//!   spec 8.3). A later write has the effects the message has at its first read, in the
//!   batch that writes it: a receipt's on its claim and in the journal, the notice "marked
//!   failed, was paid" included (`receive::receipt_settles`), and a request's or a list's
//!   note of its sender (`receive::peer_note`). Once written, the stored entry is forgotten,
//!   its message staying processed. One not written within [`crate::KEPT_MESSAGE_MS`] of its
//!   acceptance, such as a reply whose record never comes, is given up: its note goes, its
//!   entry stays stored, and the host is told
//!   ([`crate::NoticeKind::MessageGivenUp`]). One whose batch the client keeps to send again,
//!   whatever the Dispatch answered, is never written again nor given up while the client keeps
//!   that batch, which may still land ([`submitted`], [`held_write`]).
//! - **An effect of unknown fate** (`EffectUncertain`, or `Parked` as `Uncertain`): the same
//!   write compensates for it, since placing a message a record already holds writes
//!   nothing. A copy of the entry the host released after that is declined, its message
//!   processed, so its effect isn't applied twice (`PaykitProtocol`'s receive step).
//! - **A released entry that ran as a duplicate** (`ReleasedDuplicate`): its note goes.
//! - **An input awaiting protocol admission:** its informational parked notice creates no
//!   later-write note. Its unsupported admission-drop duty stays owed; custody alone does
//!   not authorize a later record write (Dispatch spec 8.2).
//! - **An entry dropped unverified** (`StoredEntryDropped`): it counts in its sender's mark
//!   of entries dropped unverified (`ud/<sender>`, `marks.rs`), and the sender is asked at
//!   once to send again what its inbox accepted from the mark's earliest time to its latest,
//!   while it may still keep them, and again at most once a day while the mark lives (Paykit
//!   spec 8.1, 9).
//! - **An entry dropped while it waited for a scope key** (`StoredEntryDropped` with its
//!   `WaitingReason::Key`): this copy is dropped without being processed. The host is told
//!   once that it is given up ([`crate::NoticeKind::MessageGivenUp`]), by the event that
//!   names its `msg_id`,
//!   even while the copy of the records may lack its record, and the drop counts in its
//!   sender's mark of key waits (`kw/<sender>`, `marks.rs`), which asks that sender, on the
//!   same cadence, only once the app holds every key those messages waited for. A copy
//!   sent again is processed as a new message.
//!
//! **Taken at first sight.** The SDK acts on each such event the first time a receive sees
//! it, and keeps a mark of it (`p/dt/<event ID>`) in the same commit as what the event says,
//! so a crash before that commit leaves the event to be taken again, and a later sight does
//! nothing twice, whoever acknowledges it and whenever. A mark goes once its event is
//! acknowledged. An event whose message the SDK can't keep, such as one whose entry is
//! gone, is not taken, and stays for the host. So once [`Paykit::receive`] has returned,
//! the host may acknowledge for Paykit up to the last event its report showed
//! (`App::ack_events_for`), and app-wide (`App::ack_events`) only once every other protocol
//! each event is for has handled it. The SDK also acknowledges for Paykit each event it has
//! taken, one by one (`App::ack_event_with`), so they don't pile up for a host that
//! acknowledges only its own: housekeeping, which decides nothing. An event it doesn't take,
//! such as `AppEvent::ListsUnverified`, which the host shows the user, stays for the host,
//! and never holds back the acknowledgement of a later one Paykit took.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::{ActivityRecord, StoredMessage};
use paykit_lib::Body;
use pubky_dispatch_client::app::{
    AppError, AppEvent, OpBatch, ParkReason, RecordContent, Refusal as ClientRefusal,
};
use pubky_dispatch_core::enc::serde_b64;
use pubky_dispatch_core::scopes::Permission;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, Role};
use crate::error::{PaykitError, Result};
use crate::notices::{KeptNotice, NoticeKind, StoredHandle};
use crate::paykit::Paykit;
use crate::publish::FirstDelivery;
use crate::receive::{
    failure_view, receipt_claim, receipt_deadline, receipt_on_record, receipt_settles,
    DispatchClaim, FailureView, ReceiptClaim, ReceiptSettles,
};
use crate::records::{list_target, KeptMessage};
use crate::requests::{RequestRef, RequestStatus, RequestView};

use crate::state::{key, StateKey, Tx};

/// Whether a kept message is in its record ([`Paykit::place_note`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Written now, or reflected already.
    Done,
    /// Not written now: tried again at the next receive, and given up past its window, a
    /// message waiting for a scope key included.
    Not,
    /// Not written while the record's absence decides nothing yet, or while the client keeps
    /// a batch of its own write, which may still land ([`submitted`]): tried again at the next
    /// receive, and never given up meanwhile. A write that waits behind a batch kept from an
    /// earlier call is [`Placed::Not`].
    Waits,
}

/// What a later write's batch came to, judged first by whether the client still holds a batch
/// of it (Paykit spec 8.1, Stored entries; Dispatch spec 12.5, "Kept batches"):
///
/// - the client keeps the write's own batch to send again (`AppError::Kept`), whatever the
///   Dispatch answered inside it: the write may still land, so it waits ([`Placed::Waits`]),
///   never counted nor given up while the batch is held: given up meanwhile, the message
///   would go while its write may still be applied, and a stop kept beside it, a payee's
///   cancellation or an Allowance end, could go with it. The client saves the held batch's ID
///   ([`held_write`]), and the message isn't written again nor given up while the client
///   keeps that batch ([`Paykit::held_write_kept`]);
/// - otherwise nothing of the write is held, and it was not written now ([`Placed::Not`]),
///   counted toward the message's 30 days as any write not made: the Dispatch answered it with
///   its error code, or it waited behind a batch the client keeps from an earlier call
///   (`AppError::Ahead`), which sent nothing of it and whose answer is that earlier batch's;
/// - not sent now for want of a key (the client's bare `NotNow`): waiting.
///
/// A wallet-wide stop ([`crate::pay::stops_resume`]) and an own Dispatch that doesn't answer
/// ([`crate::pay::dispatch_unavailable`]) are returned, held batch or not. Any other error that
/// isn't the Dispatch's answer, such as a store failure or an answer that doesn't verify, is
/// returned too: the write wasn't evaluated, and nothing is given up on it.
///
/// The receive goes on past a failed upkeep of the app record only on answers that end
/// nothing here: a Dispatch that doesn't answer, returned, or a refusal of the Dispatch client
/// before anything is sent, which no later write meets
/// ([`crate::own_record::own_record_left_owed`]).
pub(crate) fn submitted<T>(result: std::result::Result<T, AppError>) -> Result<Placed> {
    let Err(error) = result else {
        return Ok(Placed::Done);
    };
    if crate::pay::dispatch_unavailable(&error) {
        return Err(error.into());
    }
    let held = matches!(error, AppError::Kept { .. });
    let ahead = matches!(error, AppError::Ahead { .. });
    let bare_not_now = matches!(error, AppError::Refused(ClientRefusal::NotNow(_)));
    let answered = error.code().is_some();
    let error = PaykitError::from(error);
    if crate::pay::stops_resume(&error) {
        return Err(error);
    }
    if held || bare_not_now {
        return Ok(Placed::Waits);
    }
    if ahead || answered {
        return Ok(Placed::Not);
    }
    Err(error)
}

/// Whether `error` is the Dispatch's refusal of a read, an answer with its error code, rather
/// than a Dispatch that doesn't answer or a failure of the app's own.
fn refused(error: &AppError) -> bool {
    !crate::pay::dispatch_unavailable(error) && error.unkept().code().is_some()
}

/// The local key at which the client saves the ID of the batch it keeps of `stored`'s later
/// write ([`OpBatch::keep_id_at`]); the batch removes it when it applies.
pub(crate) fn held_write(stored: &StoredMessage) -> String {
    key::held_write(&stored.sender(), &stored.message().msg_id)
}

/// `batch`, the later write of `stored`, with the client saving the ID of the batch it keeps
/// of it ([`held_write`]) and removing it when the batch applies.
pub(crate) fn holding(batch: OpBatch, stored: &StoredMessage) -> OpBatch {
    let marker = held_write(stored).into_bytes();
    batch
        .keep_id_at(marker.clone())
        .local(pubky_dispatch_client::app::LocalWrite::Delete { key: marker })
}

/// What the client reports of the batch it kept of a kept message's later write
/// ([`Paykit::held_write`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeldWrite {
    /// No batch of it held: none kept, or one dropped or with no status any more.
    None,
    /// Still kept, to send again: it may still land.
    Kept,
    /// Applied: the write is in its record.
    Applied,
}

/// How a kept copy of a message goes other than with its own write
/// ([`Paykit::end_kept_copy`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum KeptEnd<'a> {
    /// Given up at its 30 days, the host told ([`NoticeKind::MessageGivenUp`]) from `origin`.
    GivenUp(crate::notices::GivenUpOrigin),
    /// A stored entry's message, noted by `note`, given up at its 30 days, the host told
    /// with the entry's handle.
    GivenUpStored(&'a StoredNote),
    /// Retired with nothing told: what its write would go into is gone past its window, or
    /// can't take it.
    Retired,
}

/// What came of ending a kept copy ([`Paykit::end_kept_copy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    /// Given up, the host told.
    GivenUp,
    /// Retired, nothing told.
    Retired,
    /// Not given up: the client still keeps a batch of its write.
    Waits,
    /// Placed: the client applied a batch of its write; nothing is told.
    Placed,
}

/// What becomes of a message kept to write later, by its write's outcome ([`later_write`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LaterWrite {
    /// In its record: the kept copy goes.
    Done,
    /// Kept, to try again at the next receive.
    Keep,
    /// Given up, the host told ([`NoticeKind::MessageGivenUp`]).
    GiveUp,
}

/// The one rule of the later-write paths, a stored entry's note (`se/`), a reply kept before
/// its record (`pr/`) and a message kept over a divergent record (`dw/`): a message written,
/// or reflected already, is done; one not written now ([`Placed::Not`]: the Dispatch answered
/// its write, the record can't take it, its value doesn't open, or a scope key it waits for
/// hasn't come) is kept within [`crate::KEPT_MESSAGE_MS`] of its acceptance, `accepted_at`,
/// and given up from then on, judged on `service`, the highest service time verified (Paykit
/// spec 8.1: 30 days for a stored entry, a reply before its record, a message kept over a
/// divergent record and an Allowance message alike). One that waits ([`Placed::Waits`]),
/// while its record's absence decides nothing or while the client holds a batch of it, is
/// kept. An error, such as a Dispatch that doesn't answer or a store failure, never reaches
/// this: it is returned, and the message stays kept. A give-up checks once more that the
/// client holds no batch of the write ([`Paykit::give_up_stored`]).
pub(crate) fn later_write(placed: Placed, accepted_at: u64, service: Option<u64>) -> LaterWrite {
    match placed {
        Placed::Done => LaterWrite::Done,
        Placed::Waits => LaterWrite::Keep,
        Placed::Not if past_window(accepted_at, service) => LaterWrite::GiveUp,
        Placed::Not => LaterWrite::Keep,
    }
}

/// Whether a message accepted at `accepted_at` is past the [`crate::KEPT_MESSAGE_MS`] it is
/// kept at most, on `service`, the highest service time verified.
pub(crate) fn past_window(accepted_at: u64, service: Option<u64>) -> bool {
    service.is_some_and(|t| accepted_at.saturating_add(crate::receive::KEPT_MESSAGE_MS) <= t)
}

/// The side of the shared records `target` is of.
pub(crate) fn record_side(target: &ActivityTarget) -> crate::hold::RecordSide {
    match target.side {
        crate::activity::SideTag::Payer => crate::hold::RecordSide::Payer,
        crate::activity::SideTag::Payee => crate::hold::RecordSide::Payee,
    }
}

/// A stored entry's message Paykit writes into its record later (`p/se/<sender>/<msg_id>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredNote {
    /// The entry's local ID at the client, its handle there; it orders nothing.
    pub(crate) stored_id: u64,
    /// The entry's message hash, which the client checks a forget by
    /// ([`pubky_dispatch_client::app::App::forget_parked`]): the note's message's own, so the
    /// ID is followed only to the entry of that message.
    #[serde(with = "serde_b64")]
    pub(crate) hash: [u8; 32],
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) msg_id: String,
    pub(crate) message: KeptMessage,
    /// The effect is in place: written by the app, or found in the record already.
    pub(crate) placed: bool,
}

/// A stored note with the message verified for its physical key during this read.
pub(crate) struct BoundNote {
    pub(crate) note: StoredNote,
    pub(crate) message: StoredMessage,
}

/// The host's notice that `stored`, which no entry keeps stored, was given up at `origin`,
/// raised at `at`, service time.
pub(crate) fn given_up_notice(
    stored: &StoredMessage,
    at: u64,
    origin: crate::notices::GivenUpOrigin,
) -> KeptNotice {
    KeptNotice::given_up(
        stored.sender(),
        &stored.message().msg_id,
        named_request(stored),
        at,
        crate::notices::GivenUpFrom::Origin(origin),
    )
}

impl BoundNote {
    /// Whether the kept effect for this request still needs a record write. A kept
    /// cancellation waits for that write before it can release its stop.
    pub(crate) fn writes_request(&self, request: &[u8; 32]) -> bool {
        !self.note.placed && named_request(&self.message) == Some(*request)
    }
}

impl StoredNote {
    /// The note read whole and bound to `key` (`se/<sender>/<msg_id>`), as every family is
    /// at a restore: its sender and `msg_id` are those the key names, its message verifies
    /// for `own` as that sender's message of that `msg_id`, and the entry's hash it keeps is
    /// that message's. Returns the message.
    ///
    /// # Errors
    ///
    /// A corrupt store, for a note at another key or a message that doesn't verify.
    pub(crate) fn bound(&self, key: &StateKey, own: &[u8; 32]) -> Result<StoredMessage> {
        let StateKey::StoredNote { sender, msg_id } = key else {
            return Err(PaykitError::corrupt());
        };
        let stored = self.message.stored(own)?;
        if (matches!(stored.body(), Body::AllowanceProposal(_))
            || crate::allowances::is_allowance_response(stored.body()))
            && self.message.position().is_none()
        {
            return Err(PaykitError::corrupt());
        }
        if self.sender != *sender
            || self.msg_id != *msg_id
            || stored.sender() != *sender
            || stored.message().msg_id != *msg_id
            || stored.hash() != self.hash
        {
            return Err(PaykitError::corrupt());
        }
        // Every note keeps the acceptance time and the feed `seq` its entry was read at,
        // which order it among the stored entries and the kept replies
        // (`records::KeptOrder`): one without them can't be ordered.
        self.message.order()?;
        Ok(stored)
    }
}

/// The request `stored` is, or names: a request's own hash, the first of a payer's reply's
/// `refs`, the second of a receipt's.
pub(crate) fn named_request(stored: &StoredMessage) -> Option<[u8; 32]> {
    let refs = &stored.message().refs;
    match stored.body() {
        Body::PaymentRequest(_) => Some(stored.hash()),
        Body::PaymentReceipt(_) => refs.get(1).copied(),
        _ => refs.first().copied(),
    }
}

/// Whether a released entry with `note` kept for its message is declined: only once the app
/// has written its effect itself (Dispatch spec 8.2). Without a note, or before the write, the
/// released run applies it.
pub(crate) fn declines(note: Option<&StoredNote>) -> bool {
    note.is_some_and(|note| note.placed)
}

/// Where a kept message goes.
enum Place {
    /// Into this record, in this role; `creates` when the message may open the record.
    Record {
        target: ActivityTarget,
        role: Role,
        creates: bool,
    },
    /// A list, held over its record: the hold to keep, `None` when a list as new is held.
    List(Option<(String, KeptMessage)>),
    /// An Allowance proposal or answer to place in its record later. An acceptance that
    /// would fill the empty `response` stays unplaced (Paykit spec 8.3.1).
    Allowance,
    /// An Allowance's end, kept as the stop at once, unless one is kept already.
    Stop(Option<(String, KeptMessage)>),
    /// Into a period record of a request whose record of this side the app doesn't hold yet:
    /// which period record is decided once it does (Paykit spec 8.2).
    AwaitsRequest(crate::hold::RecordSide),
    /// Nowhere: taken with no effect, such as period details the payer doesn't store
    /// ([`crate::periods::stores_details`]).
    Taken,
    /// The record cannot take the message yet. The message stays within its normal window.
    AwaitsWrite,
    /// Period details whose storing can't be decided yet ([`PeriodPlace::Undecided`]).
    Undecided,
}

/// Where a message naming a request whose record is held goes ([`Paykit::period_destination`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PeriodPlace {
    /// Into the record at `into`: the request record, or the period record of the Billing
    /// Period it names, which it may open (`creates`).
    Record { into: ActivityTarget, creates: bool },
    /// Invalid against its request (Paykit wire 4.4, 4.9): dropped.
    Invalid,
    /// Period details the payer doesn't store ([`crate::periods::stores_details`]): taken
    /// with no effect.
    Taken,
    /// The record cannot take the message yet. Its signed write is disputed or the
    /// proof names period details the record doesn't hold ([`crate::receive::awaits_named_details`]).
    AwaitsWrite,
    /// Period details whose storing can't be decided yet: no service time is verified
    /// ([`crate::periods::Window::NoTime`]), or they would be taken with no effect while the
    /// copy of the payer's records may lack records, a newer write such as another own app's
    /// acceptance among them, so the records the copy holds decide nothing then (Paykit spec
    /// 8.1, "Records not listed yet"). They wait, never given up meanwhile.
    Undecided,
}

/// Bind a parked message to the original admitted sender, recipient and service time.
pub(crate) fn parked_message(
    entry: &pubky_dispatch_client::app::ParkedMessage,
    own: &[u8; 32],
) -> Option<StoredMessage> {
    StoredMessage::new(
        entry.sender,
        &entry.cert,
        entry.plaintext.clone(),
        Some(entry.accepted_at_ms),
        own,
    )
    .ok()
}

impl Paykit {
    /// The note kept at `key`, read whole and bound to it ([`StoredNote::bound`]).
    async fn stored_note(&self, key: &str) -> Result<Option<StoredNote>> {
        self.local_get::<StoredNote>(key).await
    }

    /// The requests received whose record write the Dispatch refused, kept to write later,
    /// as the payer sees them ([`RequestStatus::Unrecorded`]).
    pub(crate) async fn unrecorded_requests(&self) -> Result<Vec<RequestView>> {
        let mut out = Vec::new();
        for (_, bound) in self.local_scan::<BoundNote>(key::STORED_NOTES).await? {
            if bound.note.placed {
                continue;
            }
            let stored = bound.message;
            let Body::PaymentRequest(request) = stored.body() else {
                continue;
            };
            let target = ActivityTarget::payer(natural_key::request_record(
                &stored.sender(),
                &stored.hash(),
            ));
            out.push(RequestView {
                request: RequestRef {
                    payee: stored.sender(),
                    request_hash: stored.hash(),
                },
                record: self.activity_key(&target)?,
                amount: request.amount.clone(),
                payment_reference: request.payment_reference.as_str().to_owned(),
                label: request.label.as_ref().map(|l| l.as_str().to_owned()),
                proposal_expires_at_ms: request.proposal_expires_at,
                recurring: request.is_recurring(),
                recurrence: request.recurrence,
                status: RequestStatus::Unrecorded,
                held: None,
                divergent: false,
                receipt: None,
                manual_only: self.came_back_from_block(&stored).await?,
                periods: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Take Paykit's duty events about stored entries the client hasn't had acknowledged,
    /// from `events`, each at its first sight (module docs, "Taken at first sight");
    /// acknowledge each one taken, by itself, so one left to the host holds back none of
    /// them; drop the marks of events acknowledged. Only Paykit's own local state is
    /// written: the kept messages are written into their records by
    /// [`Paykit::place_stored`].
    pub(crate) async fn take_stored_events(&mut self, events: &[AppEvent]) -> Result<()> {
        let mut listed = std::collections::BTreeSet::new();
        let mut acknowledged = Vec::new();
        for event in events {
            if !self.app.event_is_for(event, crate::protocol::BACKUP_NAME) {
                continue;
            }
            let Some(id) = event.id() else {
                continue;
            };
            let mark = key::taken(&id);
            let taken = match self.local_get::<bool>(&mark).await? {
                Some(_) => true,
                None => self.take_stored_event(event, &mark).await?,
            };
            listed.insert(mark);
            if taken {
                acknowledged.push(id);
            }
        }
        for id in acknowledged {
            self.ack_taken(id).await?;
        }
        let mut gone = Tx::default();
        for (rest, _) in self.local_scan::<bool>(key::TAKEN).await? {
            let mark = format!("{}{rest}", key::TAKEN);
            if !listed.contains(&mark) {
                gone.delete(&mark);
            }
        }
        self.local_commit(gone).await
    }

    /// Acknowledge for Paykit the duty event `id`, which it took, by itself (Dispatch spec 8.2).
    /// One acknowledged already, as by a commit whose outcome was unknown, is no longer owed
    /// to Paykit, and nothing is left to do; one still owed that the client refuses to
    /// acknowledge is the call's error.
    pub(crate) async fn ack_taken(
        &mut self,
        id: pubky_dispatch_client::app::EventId,
    ) -> Result<()> {
        match self
            .app
            .ack_event_with(crate::protocol::BACKUP_NAME, id, &[])
            .await
        {
            Ok(()) => Ok(()),
            Err(AppError::Refused(ClientRefusal::Usage(_)))
                if !self.app.pending_duty_events().await?.iter().any(|duty| {
                    duty.event.id() == Some(id)
                        && duty
                            .owners
                            .iter()
                            .any(|owner| owner == crate::protocol::BACKUP_NAME)
                }) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Take now the client's report of the first delivery of `msg_id` ending, undelivered
    /// or unknown, as the next receive would ([`Paykit::take_stored_events`]). A host can
    /// change or withdraw its list again after native delivery finished and before its next
    /// receive: the previous issue's outcome is recorded while its ID still identifies that
    /// recipient's reservation. A stop ended past its 30 days tells the host at once. Its
    /// duty is acknowledged at the next receive, as any Paykit takes. Whether it was taken.
    pub(crate) async fn take_delivery_duty(&mut self, msg_id: &str) -> Result<bool> {
        for duty in self.app.pending_duty_events().await? {
            if duty
                .owners
                .iter()
                .any(|owner| owner == crate::protocol::BACKUP_NAME)
                && matches!(&duty.event,
                    AppEvent::MessageUndelivered { msg_id: id, resend: false, .. }
                    | AppEvent::DeliveryUnknown { msg_id: id, .. } if id == msg_id)
            {
                let id = duty.event.id().ok_or_else(PaykitError::corrupt)?;
                return self.take_stored_event(&duty.event, &key::taken(&id)).await;
            }
        }
        Ok(false)
    }

    /// Keep what a duty event about a stored entry says, with its mark `mark`, in one
    /// commit (module docs). Whether it was taken: `false` for an event of another kind, or
    /// one whose message can't be kept, which stays for the host.
    async fn take_stored_event(&mut self, event: &AppEvent, mark: &str) -> Result<bool> {
        let mut tx = Tx::default();
        let taken = match event {
            AppEvent::MessagesHeldAtRestore { .. } => {
                let owed = self
                    .local_get::<crate::restore_held::HeldPass>(key::HELD_MESSAGES_OWED)
                    .await?
                    .unwrap_or(crate::restore_held::HeldPass::HeldOnly);
                tx.put(key::HELD_MESSAGES_OWED, &owed)?;
                true
            }
            AppEvent::Parked {
                stored_id, reason, ..
            } if !matches!(reason, ParkReason::Released | ParkReason::AwaitingAdmission) => {
                self.note_stored(*stored_id, &mut tx).await?
            }
            AppEvent::EffectRefused { stored_id, .. }
            | AppEvent::EffectUncertain { stored_id, .. } => {
                self.note_stored(*stored_id, &mut tx).await?
            }
            AppEvent::ReleasedDuplicate { sender, msg_id, .. } => {
                tx.delete(&key::stored_note(sender, msg_id));
                true
            }
            // A copy dropped while it waited for a scope key. Its sender is asked
            // again once the app holds the key, and the host is told once, by the copy that
            // names the message, even while the copy of the records may lack its record.
            AppEvent::StoredEntryDropped {
                sender,
                accepted_at_ms,
                waiting: pubky_dispatch_client::app::WaitingReason::Key { data_epoch },
                message,
                ..
            } => {
                self.count_key_wait(&mut tx, *sender, *accepted_at_ms, *data_epoch)
                    .await?;
                self.give_up_key_wait(&mut tx, *sender, &message.msg_id)
                    .await?;
                true
            }
            AppEvent::StoredEntryDropped {
                sender,
                accepted_at_ms,
                waiting: pubky_dispatch_client::app::WaitingReason::Sender,
                ..
            } => {
                self.count_dropped_unverified(&mut tx, *sender, *accepted_at_ms)
                    .await?;
                true
            }
            // A record a view showed divergent pauses (`divergent.rs`): its mark is its own
            // commit, before this event's, and taking the event again changes nothing.
            AppEvent::RecordDivergent {
                key: record, id, ..
            } => {
                Box::pin(self.take_divergent_event(record, id)).await?;
                true
            }
            // The Dispatch owns the member and its expiry policy, including any pending
            // edit. Normal record upkeep runs after this receive completes.
            AppEvent::MemberChanged { protocol, .. }
                if protocol == paykit_lib::app_record::PROTOCOL_NAME =>
            {
                true
            }
            AppEvent::MessageUndelivered {
                msg_id,
                resend: false,
                ..
            } if self
                .take_list_delivery(&mut tx, msg_id, FirstDelivery::Undelivered)
                .await? =>
            {
                true
            }
            AppEvent::DeliveryUnknown { msg_id, .. }
                if self
                    .take_list_delivery(&mut tx, msg_id, FirstDelivery::Unknown)
                    .await? =>
            {
                true
            }
            // A `resend_request` given up settles its delivery evidence in the same commit,
            // so an overlapping recovery request may go (Paykit spec 7.2).
            _ if self.take_expired_stop(&mut tx, event).await? => {
                self.take_reply_ask_duty(&mut tx, event).await?;
                true
            }
            _ => self.take_reply_ask_duty(&mut tx, event).await?,
        };
        if taken {
            tx.put(mark, &true)?;
            self.local_commit(tx).await?;
        }
        Ok(taken)
    }

    /// Keep, in `tx`, the message of the entry stored at `stored_id` to write it later, and
    /// a list's hold. Whether it is kept, or was already: `false` for an entry gone, one that
    /// doesn't read as a message that verifies for this app, and a kind no record of this
    /// app's holds.
    ///
    /// A message given up stays given up (Paykit spec 8.1): its record write has ended and
    /// the message stays processed. An entry the client tells of again with its notice kept,
    /// as a restore does with the parked entry it carries, only gives the notice the entry's
    /// handle, which its acknowledgement forgets: no note is kept, so nothing writes it.
    async fn note_stored(&mut self, stored_id: u64, tx: &mut Tx) -> Result<bool> {
        let own = self.identity();
        let Some(entry) = self.app.parked_entry(stored_id).await? else {
            return Ok(false);
        };
        let Some(stored) = parked_message(&entry, &own) else {
            return Ok(false);
        };
        let notice_key = key::message_notice(&entry.sender, &stored.message().msg_id);
        // One the client dropped while it waited for a key was never processed: a copy sent
        // again is a new message, kept as any other.
        if let Some(mut given_up) = self
            .kept_notice(&notice_key)
            .await?
            .filter(KeptNotice::keeps_processed)
        {
            let handle = StoredHandle {
                stored_id,
                hash: stored.hash(),
            };
            // A notice raised with no entry stored names its origin instead; once the
            // client stores the entry, the handle replaces it: a notice names one or the
            // other, never both.
            if given_up.stored.as_ref() != Some(&handle) || given_up.origin.is_some() {
                given_up.stored = Some(handle);
                given_up.origin = None;
                tx.put(&notice_key, &given_up)?;
            }
            return Ok(true);
        }
        let note_key = key::stored_note(&entry.sender, &stored.message().msg_id);
        if self.stored_note(&note_key).await?.is_some() {
            return Ok(true);
        }
        match self.place_of(&stored, entry.seq).await? {
            None => return Ok(false),
            // Held as a list whose record doesn't read is held at its first read, and its
            // sender noted with the hold, as there.
            Some(Place::List(hold)) => {
                if let Some((held, kept)) = hold {
                    tx.put(&held, &kept)?;
                }
                let (peer, at) = crate::receive::peer_note(&entry.sender, &stored);
                tx.put(&peer, &at)?;
            }
            Some(Place::Stop(Some((held, kept)))) => {
                tx.put(&held, &kept)?;
            }
            Some(
                Place::Record { .. }
                | Place::Allowance
                | Place::Stop(None)
                | Place::AwaitsRequest(_)
                | Place::Taken
                | Place::AwaitsWrite
                | Place::Undecided,
            ) => {}
        }
        let note = StoredNote {
            stored_id,
            hash: stored.hash(),
            sender: entry.sender,
            msg_id: stored.message().msg_id.clone(),
            message: if matches!(
                stored.body(),
                Body::AllowanceProposal(_)
                    | Body::AllowanceAcceptance(_)
                    | Body::AllowanceRejection(_)
            ) {
                KeptMessage::taken_at(
                    &stored,
                    pubky_dispatch_client::app::FeedPosition {
                        generation: entry.generation,
                        seq: entry.seq,
                    },
                )
            } else {
                KeptMessage::taken(&stored, entry.seq)
            },
            placed: false,
        };
        tx.put(&note_key, &note)?;
        Ok(true)
    }

    /// Whether the stored note `bound`, not placed yet, may go into one of `records`: its
    /// place is one of them, an Allowance's or its proposal's at one of them, or not decided
    /// yet, which may be one of them (Paykit spec 7.2: a held decision waits for it).
    pub(crate) async fn unplaced_note_in(
        &self,
        bound: &BoundNote,
        records: &[[u8; 32]],
    ) -> Result<bool> {
        if bound.note.placed {
            return Ok(false);
        }
        let seq = bound.note.message.order()?.1;
        Ok(match self.place_of(&bound.message, seq).await? {
            Some(Place::Record { target, .. }) => records.contains(&self.activity_key(&target)?),
            Some(Place::Allowance | Place::Stop(_)) => {
                let Some(id) = crate::allowances::named_allowance(bound.message.body()) else {
                    return Ok(true);
                };
                let sender = bound.message.sender();
                let mut named = false;
                for collection in [Collection::Allowances, Collection::AllowanceProposals] {
                    let target =
                        crate::allowances::allowance_record_target(collection, &sender, id);
                    named |= records.contains(&self.app.record_key(&target)?);
                }
                named
            }
            Some(Place::AwaitsRequest(_) | Place::AwaitsWrite | Place::Undecided) => true,
            Some(Place::List(_) | Place::Taken) | None => false,
        })
    }

    /// Where `stored` goes for this app: a request into the payer's request record; a
    /// payer's reply into the payee's record of the request it names, under its label; a
    /// proof of a payment without a request into the payee's record of its settlement,
    /// which it may open (Paykit spec 8.4); a receipt into the payer's record; a list held
    /// over its record, unless a list as new is held. `None` for a kind no record of this
    /// app's holds, or one its scopes don't write.
    async fn place_of(&self, stored: &StoredMessage, seq: u64) -> Result<Option<Place>> {
        let own = self.identity();
        let sender = stored.sender();
        let scopes = self.scopes();
        let pay = self.holds("paykit_pay");
        let first = stored.message().refs.first().copied();
        let record = |target, role, creates| {
            Some(Place::Record {
                target,
                role,
                creates,
            })
        };
        Ok(match stored.body() {
            Body::PaymentRequest(_) if pay => record(
                ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash())),
                Role::Request,
                true,
            ),
            Body::PaymentProof(proof)
                if first.is_none() && crate::protocol::writes_payee_label(&scopes, None) =>
            {
                let Some(settlement) = self
                    .rail
                    .settlement_id(&proof.payment_endpoint_identifier, &proof.proof)
                else {
                    return Ok(None);
                };
                record(
                    ActivityTarget::payee(
                        natural_key::payee_payment_record(
                            &proof.payment_endpoint_identifier,
                            &settlement,
                        ),
                        None,
                    ),
                    Role::Proof,
                    true,
                )
            }
            Body::PaymentRequestAcceptance(_)
            | Body::PaymentRequestRejection(_)
            | Body::PaymentRequestCancellation(_)
            | Body::PaymentProof(_) => {
                let label = stored.body().label().cloned();
                match first {
                    Some(request)
                        if crate::protocol::writes_payee_label(&scopes, label.as_ref()) =>
                    {
                        // The period's place goes on the heap, as `receive`'s steps do: it
                        // nests deep.
                        Box::pin(self.period_place(stored, &request)).await?
                    }
                    _ => None,
                }
            }
            Body::PaymentPeriodDetails(_) if pay => match first {
                Some(request) => Box::pin(self.period_place(stored, &request)).await?,
                None => None,
            },
            Body::PaymentReceipt(receipt) if pay => record(
                crate::receive::receipt_target(stored, &own, &receipt.attempt_id),
                Role::Receipt,
                false,
            ),
            Body::PrivatePaymentList(_) if pay => {
                let target = list_target(&sender, &stored.message().from.app_id);
                let held = key::held_list(&self.app.record_key(&target)?);
                let newer = match self.local_get::<KeptMessage>(&held).await? {
                    None => true,
                    Some(kept) => {
                        crate::receive::list_version(&kept.stored(&own)?)
                            < crate::receive::list_version(stored)
                    }
                };
                Some(Place::List(newer.then(|| (held, KeptMessage::of(stored)))))
            }
            Body::AllowanceProposal(_)
            | Body::AllowanceAcceptance(_)
            | Body::AllowanceRejection(_)
                if self.holds("paykit_allowance") =>
            {
                Some(Place::Allowance)
            }
            // An end's stop is kept at once over a record the app holds, as the receive step
            // keeps it. One for an Allowance not held, absent or deleted, is new state that
            // the spam rule and the bounds gate in the receive step, which no duty event's
            // entry runs again: it is no message any record of this app's holds here, so the
            // event stays for the host, which can release the entry, as for a payee's
            // cancellation of a request not held.
            Body::AllowanceEnd(reply) if self.holds("paykit_allowance") => {
                let target = crate::allowances::allowance_target(&sender, &reply.allowance_id);
                let record = self.app.record_key(&target)?;
                let content = self
                    .app
                    .shared_record(&record)
                    .await?
                    .map(|held| held.content);
                if !crate::receive::holds_record(content.as_ref()) {
                    return Ok(None);
                }
                let stop = key::allowance_end(&record, &stored.message().msg_id);
                let kept = self.local_get::<KeptMessage>(&stop).await?.is_some();
                // With the feed `seq` its entry was read at, which orders it in the pool the
                // kept cancellations and the ends share (`records::KeptOrder`), as the
                // receive step keeps it.
                Some(Place::Stop(
                    (!kept).then(|| (stop, KeptMessage::taken(stored, seq))),
                ))
            }
            _ => None,
        })
    }

    /// Where `stored`, a payer's reply or a payee's period details naming `request`, goes: a
    /// Billing Period's into its period record, once the request record is held to check the
    /// period against, else into the request record ([`Paykit::period_destination`]); `None`
    /// for a period that isn't the request's (Paykit wire 4.4, 5.3).
    async fn period_place(
        &self,
        stored: &StoredMessage,
        request: &[u8; 32],
    ) -> Result<Option<Place>> {
        let own = self.identity();
        let target = crate::receive::kept_request_target(&own, stored, request);
        let periodic = matches!(stored.body(), Body::PaymentPeriodDetails(_))
            || stored.body().billing_period().is_some();
        let role = Role::of(stored.body()).unwrap_or(Role::Response);
        if self
            .activity_base_changed(&self.activity_key(&target)?)
            .await?
        {
            return Ok(Some(Place::AwaitsWrite));
        }
        let Some(record) = self.held_activity(&target).await? else {
            return Ok(Some(if periodic {
                Place::AwaitsRequest(record_side(&target))
            } else {
                Place::Record {
                    target,
                    role,
                    creates: false,
                }
            }));
        };
        Ok(
            match self
                .period_destination(&target, &record, stored, request)
                .await?
            {
                PeriodPlace::Record { into, creates } => Some(Place::Record {
                    target: into,
                    role,
                    creates,
                }),
                PeriodPlace::Invalid => None,
                PeriodPlace::Taken => Some(Place::Taken),
                PeriodPlace::AwaitsWrite => Some(Place::AwaitsWrite),
                PeriodPlace::Undecided => Some(Place::Undecided),
            },
        )
    }

    /// Where `stored`, a message naming `request`, goes now that `record`, the request record
    /// at `target`, is held: the one rule of the later writes, a stored entry's and a message
    /// kept before its record (`pr/`), as the receive step decides it. A message from anyone
    /// but the record's counterparty is [`PeriodPlace::Invalid`]. The period it names
    /// is checked against the request ([`crate::receive::period_or_request`]); details go
    /// only where the payer stores them ([`crate::periods::stores_details`]), and are taken
    /// with no effect only once the copy of the payer's records lacks none; a proof naming
    /// details the period record doesn't hold waits for them
    /// ([`crate::receive::awaits_named_details`]).
    pub(crate) async fn period_destination(
        &self,
        target: &ActivityTarget,
        record: &ActivityRecord,
        stored: &StoredMessage,
        request: &[u8; 32],
    ) -> Result<PeriodPlace> {
        // Only the record's counterparty writes into it or its period records: a message from
        // anyone else is no update of this activity, and goes nowhere.
        if record.counterparty != stored.sender() {
            return Ok(PeriodPlace::Invalid);
        }
        let own = self.identity();
        let Some(into) = crate::receive::period_or_request(&own, target, record, stored, request)
        else {
            return Ok(PeriodPlace::Invalid);
        };
        if self
            .activity_base_changed(&self.activity_key(&into)?)
            .await?
        {
            return Ok(PeriodPlace::AwaitsWrite);
        }
        // Over the request record or the period record seen divergent and not confirmed,
        // whose upload may hold a stop or other details, a write that adds no stop waits for
        // the user's confirmation, nothing decided on the writes held, the period's window
        // included (Paykit spec 6). The order among the messages kept over a record is
        // `place_note`'s (`Paykit::divergent_write_waits`).
        if !crate::divergent::adds_stop(stored) {
            for over in [target, &into] {
                if Box::pin(self.pause_seen(&self.activity_key(over)?))
                    .await?
                    .write_waits()
                {
                    return Ok(PeriodPlace::AwaitsWrite);
                }
            }
        }
        if matches!(stored.body(), Body::PaymentPeriodDetails(_)) {
            let stopped = self.kept_stop(target).await?;
            let period = self.held_activity(&into).await?;
            match crate::periods::stores_details(
                record,
                period.as_ref(),
                stored,
                self.service_time(),
                stopped,
            ) {
                Ok(crate::periods::Window::Takes) => {}
                Ok(crate::periods::Window::Closed) => {
                    return Ok(if self.side_unlisted(record_side(target)).await? {
                        PeriodPlace::Undecided
                    } else {
                        PeriodPlace::Taken
                    });
                }
                Ok(crate::periods::Window::NoTime) => return Ok(PeriodPlace::Undecided),
                // A payment deadline past 64 bits: no attempt on the period can be made.
                Err(_) => return Ok(PeriodPlace::Invalid),
            }
        }
        if into != *target {
            let period = self.held_activity(&into).await?;
            if crate::receive::awaits_named_details(period.as_ref(), into.side.side(), stored) {
                return Ok(PeriodPlace::AwaitsWrite);
            }
        }
        Ok(PeriodPlace::Record {
            creates: into != *target,
            into,
        })
    }

    /// Write every kept message whose record can take it now, each as a later write of the
    /// app's own, built on the version held; then forget the stored entry of each one in
    /// place, and drop its note. A write that fails now is tried again at the next receive;
    /// a message not written within [`crate::KEPT_MESSAGE_MS`] of its acceptance, on the
    /// highest service time verified, is given up, its entry left stored for the host. The
    /// messages are taken in the order their inbox accepted them, by acceptance time and
    /// then by the feed `seq` their entries were read at, as the kept replies are, never in
    /// the order of their `msg_id`s nor of their entries' local IDs.
    pub(crate) async fn place_stored(
        &mut self,
        pending_read: Option<&pubky_dispatch_client::app::ReceiveReport>,
    ) -> Result<()> {
        // Every note in the order its inbox accepted its message, never in the order of the
        // keys, which end in a `msg_id` its sender chooses: by acceptance time, then by the
        // feed `seq` its entry was read at (`records::KeptOrder`, Paykit spec 8.1), the
        // token the kept replies carry too. Each is read bound to its key first.
        let mut notes = Vec::new();
        for (rest, BoundNote { note, message }) in self.local_scan(key::STORED_NOTES).await? {
            let note_key = format!("{}{rest}", key::STORED_NOTES);
            notes.push((note.message.order()?, note_key, note, message));
        }
        notes.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        for (order, note_key, mut note, stored) in notes {
            if !note.placed {
                // A kept message carries its entry's acceptance time; one without it is a
                // corrupt store, never a message past its window.
                let accepted_at = stored.accepted_at().ok_or_else(PaykitError::corrupt)?;
                let placed = match self.place_of(&stored, order.1).await? {
                    Some(Place::List(hold)) | Some(Place::Stop(hold)) => {
                        if let Some((held, kept)) = hold {
                            self.local_put(&held, &kept).await?;
                        }
                        Placed::Done
                    }
                    Some(Place::Allowance) => {
                        Box::pin(self.place_allowance_note(
                            stored.clone(),
                            &note.message,
                            note.stored_id,
                            pending_read,
                        ))
                        .await?
                    }
                    Some(Place::Record {
                        target,
                        role,
                        creates,
                        // An error isn't a refusal: the write wasn't evaluated, so the note stays,
                        // neither counted nor given up, and the receive returns the error.
                    }) => {
                        self.place_note(
                            &target,
                            role,
                            creates,
                            stored.clone(),
                            order,
                            Tx::default(),
                        )
                        .await?
                    }
                    Some(Place::AwaitsRequest(side)) => {
                        if self.side_unlisted(side).await? {
                            Placed::Waits
                        } else {
                            Placed::Not
                        }
                    }
                    Some(Place::Taken) => Placed::Done,
                    Some(Place::AwaitsWrite) => Placed::Not,
                    // Not decided yet: it waits, never given up meanwhile.
                    Some(Place::Undecided) => Placed::Waits,
                    None => Placed::Not,
                };
                let later = later_write(placed, accepted_at, self.service_time());
                note.placed = later == LaterWrite::Done;
                if !note.placed {
                    // Never given up while the client still keeps a batch of its write; placed
                    // when the client applied one (`Paykit::give_up_stored`).
                    if later != LaterWrite::GiveUp
                        || Box::pin(self.give_up_stored(&note_key, &note, &stored)).await?
                            != Ended::Placed
                    {
                        continue;
                    }
                    note.placed = true;
                }
                self.local_put(&note_key, &note).await?;
            }
            // The note's entry, found by its ID and bound to the hash the note keeps: an
            // entry at that ID holding another message is a corrupt store, and nothing is
            // forgotten or dropped. The client forgets by the note's own hash, never the
            // entry's.
            let summary = self
                .app
                .parked_entries()
                .await?
                .into_iter()
                .find(|entry| entry.stored_id == note.stored_id);
            let gone = match summary {
                None => true,
                Some(entry) if entry.hash != note.hash => return Err(PaykitError::corrupt()),
                Some(_) => self
                    .app
                    .forget_parked(note.stored_id, &note.hash)
                    .await
                    .is_ok(),
            };
            if gone {
                self.local_delete(&note_key).await?;
            }
        }
        Ok(())
    }

    /// Tell the host, in `tx`, that the message `msg_id` from `sender`, whose entry the client
    /// dropped while it waited for a scope key, is given up ([`NoticeKind::MessageGivenUp`],
    /// from a key wait, with no entry's handle and no request named: the entry is gone). The
    /// notice is raised once: none is raised over one kept already. A `msg_id` that isn't
    /// one a message carries raises none, since no notice could be kept for it.
    async fn give_up_key_wait(&self, tx: &mut Tx, sender: [u8; 32], msg_id: &str) -> Result<()> {
        if pubky_dispatch_core::json::check_uuid_v4(msg_id).is_err() {
            return Ok(());
        }
        let notice = key::message_notice(&sender, msg_id);
        if self.kept_notice(&notice).await?.is_none() {
            let at = self.service_time().unwrap_or(0);
            tx.put(
                &notice,
                &KeptNotice::given_up(
                    sender,
                    msg_id,
                    None,
                    at,
                    crate::notices::GivenUpFrom::Origin(crate::notices::GivenUpOrigin::KeyWait),
                ),
            )?;
        }
        Ok(())
    }

    /// Give up `stored`, a stored entry's message noted at `note_key` (`se/`): the note goes,
    /// and the host is told ([`NoticeKind::MessageGivenUp`], with the entry's handle)
    /// ([`Paykit::end_kept_copy`]). Returns what came of it: one whose batch the client
    /// applied is placed, the caller noting it placed, with nothing told.
    ///
    /// # Errors
    ///
    /// A store failure, or a notice or marker that doesn't read.
    pub(crate) async fn give_up_stored(
        &mut self,
        note_key: &str,
        note: &StoredNote,
        stored: &StoredMessage,
    ) -> Result<Ended> {
        self.end_kept_copy(note_key, stored, KeptEnd::GivenUpStored(note))
            .await
    }

    /// Give up `stored`, a payer's reply kept at `kept_key` (`pr/`): the kept copy goes and
    /// the host is told once ([`NoticeKind::MessageGivenUp`], from a kept reply, with no
    /// entry's handle) ([`Paykit::end_kept_copy`]).
    pub(crate) async fn give_up(&mut self, kept_key: &str, stored: &StoredMessage) -> Result<()> {
        self.end_kept_copy(
            kept_key,
            stored,
            KeptEnd::GivenUp(crate::notices::GivenUpOrigin::KeptReply),
        )
        .await
        .map(|_| ())
    }

    /// The one way a kept copy of a message, a stored entry's note (`se/`), a reply kept
    /// before its record (`pr/`) or a message kept over a divergent record (`dw/`), goes
    /// other than in the commit of its own write: given up at its 30 days, or retired, as
    /// `end` says. Nothing goes while the client still keeps a batch of its write, which may
    /// still land ([`Paykit::held_write_kept`]; Paykit spec 8.1, Stored entries, Kept
    /// batch): the copy stays, to be decided again at the next receive ([`Ended::Waits`]).
    /// One whose batch the client applied is placed, its write in its record: the copy and
    /// the marker go, nothing told ([`Ended::Placed`]); a stored entry's note stays, for its
    /// caller to note it placed. Otherwise the copy goes in one commit with the marker of a
    /// batch the client no longer keeps, and, when given up, with the host's notice, raised
    /// once.
    ///
    /// The raw delete of a kept copy's key is written here and in the commit of the copy's
    /// own write only: a guard test (`test_stored_kept_copies_removed_only_through_the_gate`)
    /// counts every delete of one, so a new way to remove one goes through this function.
    ///
    /// # Errors
    ///
    /// A store failure, or a notice or marker that doesn't read.
    pub(crate) async fn end_kept_copy(
        &mut self,
        kept_key: &str,
        stored: &StoredMessage,
        end: KeptEnd<'_>,
    ) -> Result<Ended> {
        let mut tx = Tx::default();
        match self.held_give_up(stored, &mut tx).await? {
            Some(Ended::Waits) => return Ok(Ended::Waits),
            Some(Ended::Placed) => {
                if !matches!(end, KeptEnd::GivenUpStored(_)) {
                    tx.delete(kept_key);
                }
                self.local_commit(tx).await?;
                return Ok(Ended::Placed);
            }
            // `held_give_up` answers only `Waits` or `Placed` when a batch is held.
            Some(Ended::GivenUp | Ended::Retired) | None => {}
        }
        tx.delete(kept_key);
        tx.delete(&held_write(stored));
        let notice = key::message_notice(&stored.sender(), &stored.message().msg_id);
        match end {
            KeptEnd::Retired => {
                self.local_commit(tx).await?;
                return Ok(Ended::Retired);
            }
            KeptEnd::GivenUp(origin) => {
                if self.kept_notice(&notice).await?.is_none() {
                    let at = self.service_time().unwrap_or(0);
                    tx.put(&notice, &given_up_notice(stored, at, origin))?;
                }
            }
            KeptEnd::GivenUpStored(note) => {
                let notice = key::message_notice(&note.sender, &note.msg_id);
                // The notice keeps the entry's handle, which its acknowledgement forgets; one
                // kept already, as after a restore onto another store, takes the entry's
                // handle here.
                let handle = StoredHandle {
                    stored_id: note.stored_id,
                    hash: note.hash,
                };
                match self.kept_notice(&notice).await? {
                    None => {
                        let at = self.service_time().unwrap_or(0);
                        let given_up = KeptNotice::given_up(
                            note.sender,
                            &note.msg_id,
                            named_request(stored),
                            at,
                            crate::notices::GivenUpFrom::Stored(handle),
                        );
                        tx.put(&notice, &given_up)?;
                    }
                    Some(mut kept) if kept.stored.as_ref() != Some(&handle) => {
                        kept.stored = Some(handle);
                        kept.origin = None;
                        tx.put(&notice, &kept)?;
                    }
                    Some(_) => {}
                }
            }
        }
        self.local_commit(tx).await?;
        Ok(Ended::GivenUp)
    }

    /// Forget every marker of a batch the client no longer keeps ([`held_write`]): applied,
    /// dropped, or one it holds nothing of. A marker goes with its batch's apply, and with a
    /// given-up message; this takes the rest, as of a message written another way, taken or
    /// retired while its batch was dropped. It is safe: the client writes a marker in the
    /// commit that keeps its batch, so a marker whose batch the client doesn't hold, read in
    /// this same pass, is always stale. A marker that doesn't read goes too: it names no
    /// batch. Run at the end of each receive, so the scan is bounded: no more markers than
    /// the batches the client keeps plus the messages ended since the last receive.
    ///
    /// # Errors
    ///
    /// A store failure, or a kept batch that doesn't read.
    pub(crate) async fn sweep_held_writes(&mut self) -> Result<()> {
        let mut gone = Tx::default();
        let mut any = false;
        for (marker, batch_id) in self.held_write_markers().await? {
            let kept = match batch_id {
                Some(batch_id) => matches!(
                    self.app.kept_batch(batch_id).await?,
                    Some(pubky_dispatch_client::app::KeptBatchStatus::Kept { .. })
                ),
                None => false,
            };
            if !kept {
                gone.delete(&marker);
                any = true;
            }
        }
        if any {
            self.local_commit(gone).await?;
        }
        Ok(())
    }

    /// Whether the client still keeps a batch of `stored`'s later write ([`held_write`]),
    /// which may still land: the message is then neither written again, which would keep
    /// another batch behind it, nor given up ([`submitted`]). A marker whose batch the client
    /// applied, dropped or holds nothing of any more goes, and the write is tried as any.
    ///
    /// The client keeps a batch it sent at most
    /// [`pubky_dispatch_client::app::KEPT_BATCH_ANSWER_MS`] after its first send, the service's
    /// 30 days for a batch's proofs less a one-hour margin, then applies or drops it; one never
    /// sent goes at its first answered send, or is dropped at its first-handoff cutoff. So a
    /// message waits on a held batch at most about 30 days after that batch's first send
    /// (Paykit spec 8.1, Stored entries, Kept batch).
    ///
    /// # Errors
    ///
    /// A store failure, or a marker or kept batch that doesn't read.
    pub(crate) async fn held_write_kept(&mut self, stored: &StoredMessage) -> Result<bool> {
        match self.held_write(stored).await? {
            HeldWrite::Kept => Ok(true),
            HeldWrite::Applied => {
                self.local_delete(&held_write(stored)).await?;
                Ok(false)
            }
            HeldWrite::None => Ok(false),
        }
    }

    /// What the client reports of the batch it kept of `stored`'s later write
    /// ([`held_write`]), in its own terms: still kept, or applied, its operations then
    /// committed as one transaction (Pubky Dispatch spec, Section 10.1, Batches), so the write
    /// is in its record. A marker whose batch the client dropped, or holds no status of any
    /// more, establishes nothing ([`HeldWrite::None`]): it goes, and the record decides as
    /// for a message with no batch kept (Paykit spec 8.1, Stored entries, Kept batch). An
    /// applied batch's marker stays for its caller to drop in the commit that places the
    /// message.
    ///
    /// # Errors
    ///
    /// A store failure, or a marker or kept batch that doesn't read.
    async fn held_write(&mut self, stored: &StoredMessage) -> Result<HeldWrite> {
        let marker = held_write(stored);
        let Some(batch_id) = self.local_get::<[u8; 16]>(&marker).await? else {
            return Ok(HeldWrite::None);
        };
        match self.app.kept_batch(batch_id).await? {
            Some(pubky_dispatch_client::app::KeptBatchStatus::Kept { .. }) => Ok(HeldWrite::Kept),
            Some(pubky_dispatch_client::app::KeptBatchStatus::Applied { .. }) => {
                Ok(HeldWrite::Applied)
            }
            // Dropped, whatever the reason (one dropped as uncertain may have been
            // applied), or no status any more: nothing established.
            _ => {
                self.local_delete(&marker).await?;
                Ok(HeldWrite::None)
            }
        }
    }

    /// Whether a kept message's give-up may go ahead, by the batch the client kept of its
    /// write ([`Paykit::held_write`]): `Some(Ended::Waits)` while the batch is kept,
    /// `Some(Ended::Placed)` once it applied, the marker dropped in `tx`; `None` when no batch
    /// of it is held, and the give-up goes ahead on the record this pass read. That read is
    /// still good when the pass learns of a drop after it: a dropped batch changed nothing in
    /// the record, so the record the pass read is the one the drop leaves, and a message whose
    /// write another path placed there reads as placed, never given up.
    pub(crate) async fn held_give_up(
        &mut self,
        stored: &StoredMessage,
        tx: &mut Tx,
    ) -> Result<Option<Ended>> {
        match self.held_write(stored).await? {
            HeldWrite::Kept => Ok(Some(Ended::Waits)),
            HeldWrite::Applied => {
                tx.delete(&held_write(stored));
                Ok(Some(Ended::Placed))
            }
            HeldWrite::None => Ok(None),
        }
    }

    /// Place `stored` in its `role` of the record at `target`, as a write of the app's own.
    /// Whether it is in place: written now, or reflected already.
    ///
    /// A record that is absent or deleted is opened only by a message that may open it
    /// (`creates`), and a receipt goes only beside the proof it names (Paykit wire 5.3). A
    /// record held as a value that doesn't read, or that the app can't open, is never
    /// written over (Dispatch spec 8.2: a reader doesn't act on a record it can't read, and a
    /// write over one that is only unopenable for now could drop what its writer stored). A
    /// value sealed at a key the app doesn't hold yet (`Unopened::KeyNotHeld`) and a record
    /// whose scope's key at the current data epoch the app doesn't hold yet wait for the key:
    /// tried again at each receive and placed once it comes, and given up as any other
    /// message not written once [`crate::KEPT_MESSAGE_MS`] have passed since its acceptance
    /// (Paykit spec 8.1). A record absent while the copy of its side may lack records
    /// ([`Paykit::side_unlisted`]) waits, never given up meanwhile, and the message is placed
    /// once the records are listed. A value that never opens stays as it is.
    pub(crate) async fn place_note(
        &mut self,
        target: &ActivityTarget,
        role: Role,
        creates: bool,
        stored: StoredMessage,
        order: crate::records::KeptOrder,
        locals: Tx,
    ) -> Result<Placed> {
        // A batch of this write the client still keeps may land: nothing more is written, and
        // nothing is given up, until it applies or the client drops it.
        if self.held_write_kept(&stored).await? {
            return Ok(Placed::Waits);
        }
        let key = self.activity_key(target)?;
        let held = self.app.shared_record(&key).await?;
        if crate::records::activity_base_changed(held.as_ref()) {
            return Ok(Placed::Not);
        }
        let disputed = held.as_ref().is_some_and(crate::records::content_disputed);
        let content = held.map(|record| record.content);
        // Absent or deleted while the copy of its side may lack records: the copy decides
        // nothing yet (Paykit spec 8.1, "Records not listed yet"), whatever key the record's
        // scope waits for, nor whatever waits over the record: the message is neither written
        // nor given up meanwhile.
        if matches!(content, None | Some(RecordContent::Deleted))
            && self.side_unlisted(record_side(target)).await?
        {
            return Ok(Placed::Waits);
        }
        // Over a divergent delete, or a divergent value the app can't read, whose upload may
        // hold what the current write lacks, such as a cancellation, the record is never
        // opened again: the message isn't written, and is given up at its 30 days with the
        // host's notice, confirmed or not (`content_disputed`).
        if disputed {
            return Ok(Placed::Not);
        }
        // Over a divergent record its user hasn't confirmed, only a stop is written, and while
        // messages kept over it before this one wait to be written, they go first: the
        // message waits, within its own window (Paykit spec 6, 8.1 "Stored entries").
        if Box::pin(self.divergent_write_waits(&key, &stored, Some(order))).await? {
            return Ok(Placed::Not);
        }
        // Without the current data epoch's key of the record's scope, nothing can be sealed
        // there yet: the message waits for it, within its window.
        let scope = target.record_target().collection.policy.read_scope;
        if self.app.scope_key(self.app.data_epoch(), &scope).is_none() {
            return Ok(Placed::Not);
        }
        match content {
            None | Some(RecordContent::Deleted) => {
                if !creates {
                    return Ok(Placed::Not);
                }
                // Held deleted at an app that saw its messages: the message is one for the
                // record it saw, written nowhere (Paykit spec 8.1, "Records not listed yet";
                // `receive::place`).
                if content.is_some() && self.app.record_seen(&key).await?.is_some() {
                    return Ok(Placed::Done);
                }
            }
            // A value sealed under a key the app doesn't hold yet: the message waits for it,
            // within its window.
            Some(content) if crate::hold::awaits_key(&content) => return Ok(Placed::Not),
            Some(RecordContent::Unreadable(_)) => return Ok(Placed::Not),
            Some(RecordContent::Value(_)) => {
                let Some(record) = self.held_activity(target).await? else {
                    return Ok(Placed::Not);
                };
                // A record that reads names its counterparty: a message about it from anyone
                // else is never placed, its role held or not (Paykit spec 8.1), and is done
                // with, nothing written. A receipt finds the same in `place_receipt`.
                if role != Role::Receipt && record.counterparty != stored.sender() {
                    self.local_commit(locals).await?;
                    return Ok(Placed::Done);
                }
                if role == Role::Receipt {
                    return Box::pin(self.place_receipt(target, &record, stored, order, locals))
                        .await;
                }
                // A payer's reply never takes the response while a different one its inbox
                // accepted earlier is still kept to write, as at receive: it is only
                // reflected, and the earlier one is the response once written.
                if role == Role::Response
                    && record.response.is_none()
                    && crate::activity::replied_before(
                        &record,
                        &stored,
                        order,
                        &Box::pin(self.replies_kept(&key, &stored)).await?,
                    )
                {
                    return self.write_placed(target, stored, locals, true).await;
                }
            }
        }
        self.write_placed(target, stored, locals, false).await
    }

    /// The payer's replies from the sender of `stored` the app keeps to write into the
    /// request record at `record`, outside the receive step, as the step reads them
    /// (`receive::step_replies_kept`): kept before the record (`pr/`), stored and not yet
    /// placed (`se/`), and kept over it while its view is divergent (`dw/`).
    async fn replies_kept(
        &self,
        record: &[u8; 32],
        stored: &StoredMessage,
    ) -> Result<Vec<(StoredMessage, crate::records::KeptOrder)>> {
        let own = self.identity();
        let sender = stored.sender();
        let mut kept = Vec::new();
        for (_, message) in self
            .local_scan::<KeptMessage>(&key::kept_reply(&sender, record, ""))
            .await?
        {
            kept.push((message.stored(&own)?, message.order()?));
        }
        let prefix = key::stored_notes_of(&sender);
        for (_, BoundNote { note, message }) in self.local_scan(&prefix).await? {
            if !note.placed {
                kept.push((message, note.message.order()?));
            }
        }
        kept.extend(Box::pin(self.responses_kept_over(record)).await?);
        Ok(kept)
    }

    /// A stored receipt's later write, into `record`, the payer's record at `target` it
    /// names, with the same effects as at its first read (`receive::receipt_effect`), in one
    /// batch: beside the proof it names, the attempt settled by evidence, or once the claim
    /// is gone the notice "marked failed, was paid", and the journal entry no longer waiting
    /// for its receipt ([`crate::receive::receipt_settles`]). Done when the batch went, or
    /// when the receipt does nothing here, as at its first read; a claim read or a batch
    /// that fails now is tried again at the next receive.
    async fn place_receipt(
        &mut self,
        target: &ActivityTarget,
        record: &ActivityRecord,
        stored: StoredMessage,
        order: crate::records::KeptOrder,
        locals: Tx,
    ) -> Result<Placed> {
        let Body::PaymentReceipt(receipt) = stored.body() else {
            return Ok(Placed::Not);
        };
        let attempt_id = receipt.attempt_id;
        let mut conditions = Vec::new();
        let current;
        let mut terms = None;
        let record = if let Some(parent) = crate::receive::receipt_parent(&stored) {
            let key = self.activity_key(target)?;
            let parent_key = self.activity_key(&parent)?;
            conditions.push(self.app.record_condition(&key).await?);
            conditions.push(self.app.record_condition(&parent_key).await?);
            let held = self.app.shared_record(&key).await?;
            let parent_held = self.app.shared_record(&parent_key).await?;
            if crate::records::activity_base_changed(held.as_ref())
                || crate::records::activity_base_changed(parent_held.as_ref())
                || Box::pin(self.divergent_write_waits(&parent_key, &stored, Some(order))).await?
            {
                return Ok(Placed::Not);
            }
            let Some(parent) = self.read_activity(&parent, parent_held.as_ref()) else {
                if !crate::receive::holds_activity(parent_held.as_ref())
                    && self.side_unlisted(crate::hold::RecordSide::Payer).await?
                {
                    return Ok(Placed::Waits);
                }
                return Ok(Placed::Not);
            };
            current = self.read_activity(target, held.as_ref());
            let Some(record) = current.as_ref() else {
                return Ok(Placed::Not);
            };
            match crate::receive::period_receipt(&stored, record, &parent) {
                crate::receive::PeriodReceipt::Missing => return Ok(Placed::Not),
                crate::receive::PeriodReceipt::Different => {
                    self.local_commit(locals).await?;
                    return Ok(Placed::Done);
                }
                crate::receive::PeriodReceipt::Matched => {}
            }
            terms = Some(parent);
            record
        } else if stored.message().refs.get(1).is_some() {
            let key = self.activity_key(target)?;
            conditions.push(self.app.record_condition(&key).await?);
            let held = self.app.shared_record(&key).await?;
            if crate::records::activity_base_changed(held.as_ref())
                || crate::receive::waits_for_requested_record(&stored, held.as_ref())
            {
                return Ok(Placed::Not);
            }
            current = self.read_activity(target, held.as_ref());
            let Some(record) = current.as_ref() else {
                return Ok(Placed::Not);
            };
            record
        } else {
            record
        };
        let mut tx = locals;
        let Some(found) = receipt_on_record(record, &stored) else {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        };
        if found.waits_for_proof {
            return Ok(Placed::Not);
        }
        let sender = stored.sender();
        let mut batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME);
        let mut writes = false;
        if found.names_held_proof {
            if let Some(entry) = self
                .placed_write(
                    target,
                    sender,
                    Role::Receipt,
                    stored.clone(),
                    conditions.clone(),
                )
                .await?
            {
                batch = batch.entry(entry);
                writes = true;
            }
        }
        if self.app.cert().has_permission(Permission::ClaimsWrite) {
            // Read bound to its key and to the claim the receipt settles, as at the first read.
            let obligation = receipt_claim(
                &stored,
                &attempt_id,
                |request, start| self.obligation(&sender, request, start),
                |attempt| self.payment_obligation(attempt),
            );
            let journaled = self.journal_entry_on(&attempt_id, &obligation).await?;
            let deadline = receipt_deadline(
                &stored,
                terms.as_ref().unwrap_or(record),
                journaled.as_ref(),
            );
            // The claim view as the first read takes it, by the same rule
            // (`receive::failure_view`): the view the app keeps, or the Dispatch's read only
            // when it asks for it. A read the Dispatch refuses settles nothing.
            let kept = self
                .app
                .claim_evidence(&obligation)
                .await?
                .map(|kept| kept.view);
            let mut view = failure_view(kept, None);
            if matches!(view, FailureView::ReadDispatch) {
                let read = match self.app.read_claim(&obligation).await {
                    Ok(read) => DispatchClaim::Read(read.map(|read| read.view)),
                    Err(error) if refused(&error) => DispatchClaim::Refused,
                    Err(error) => return Err(error.into()),
                };
                view = failure_view(None, Some(read));
            }
            let settles = match view {
                FailureView::View(view) => receipt_settles(&ReceiptClaim {
                    stored: &stored,
                    attempt_id,
                    obligation,
                    deadline,
                    view: view.as_ref(),
                    service_time: self.service_time(),
                    journaled,
                    first_receipt: found.first_receipt,
                })?,
                FailureView::Refused | FailureView::ReadDispatch => ReceiptSettles::default(),
            };
            if let Some((op, deadline)) = settles.claim {
                batch = batch
                    .op_if(op.kind(), conditions)
                    .obligation_deadline(obligation, deadline);
                writes = true;
            }
            if let Some(late) = settles.notice {
                let notice = key::notice(&attempt_id, NoticeKind::PaidAfterFailed);
                if self.kept_notice(&notice).await?.is_none() {
                    tx.put(&notice, &late)?;
                }
            }
            if let Some(entry) = settles.received {
                entry.check_bound(|input| self.app.blind(input))?;
                tx.put(&key::journal(&attempt_id), &entry)?;
            }
        }
        if !writes {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        }
        let batch = holding(batch.locals(tx.into_writes()), &stored);
        submitted(self.submit(batch).await)
    }

    /// Write `stored` into its record at `target`, in its role or, when `reflect_only`, by
    /// its `msg_id` alone, with `locals` in the same batch, or committed alone when the
    /// record reflects it already.
    async fn write_placed(
        &mut self,
        target: &ActivityTarget,
        stored: StoredMessage,
        locals: Tx,
        reflect_only: bool,
    ) -> Result<Placed> {
        let role = Role::of(stored.body()).unwrap_or(Role::Response);
        let sender = stored.sender();
        let held = stored.clone();
        // A request notes its sender in the batch that writes it, as at its first read.
        let peer = (role == Role::Request).then(|| crate::receive::peer_note(&sender, &stored));
        let mut tx = locals;
        if let Some((peer, at)) = peer {
            tx.put(&peer, &at)?;
        }
        // A one-time request that came back after the user's block opens its record paid only
        // by hand, as at its first read (`receive.rs`).
        let manual_only = role == Role::Request && self.came_back_from_block(&stored).await?;
        let Some(entry) = self
            .placed_write_with(
                target,
                sender,
                role,
                stored,
                Vec::new(),
                manual_only,
                reflect_only,
            )
            .await?
        else {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        };
        let batch = OpBatch::for_protocol(crate::protocol::BACKUP_NAME)
            .entry(entry)
            .locals(tx.into_writes());
        submitted(self.submit(holding(batch, &held)).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every removal of a kept copy's key (`se/`, `pr/`, `dw/`) in the SDK's sources, by
    /// the function it is written in: other than in the commit of the copy's own write, a
    /// kept copy goes only through [`Paykit::end_kept_copy`], which asks first whether the
    /// client still keeps a batch of its write (Paykit spec 8.1, Stored entries, Kept
    /// batch). A new delete of one, anywhere, fails this test until it is routed through
    /// that function or listed here with what it is. A kept copy's key is held in a binding
    /// named `kept_key` or `note_key`, or built in place, so the scan sees it: a binding of
    /// one under another name fails too.
    #[test]
    fn test_stored_kept_copies_removed_only_through_the_gate() {
        use std::collections::BTreeMap;
        const KEPT: [&str; 9] = [
            "kept_key",
            "note_key",
            "key::stored_note(",
            "key::kept_reply(",
            "key::divergent_write(",
            "KEPT_REPLIES",
            "STORED_NOTES",
            "DIVERGENT_WRITES",
            "drop_kept",
        ];
        const BUILDERS: [&str; 3] = [
            "key::stored_note(",
            "key::kept_reply(",
            "key::divergent_write(",
        ];
        // (file, function, deletes): what each is.
        let allowed: BTreeMap<(&str, &str), usize> = [
            // The gate.
            (("stored.rs", "end_kept_copy"), 2),
            // Placed: staged with the copy's own write, committed only with it.
            (("receive.rs", "place_kept_replies"), 3),
            (("allowances.rs", "place_kept_allowance_response"), 2),
            (("divergent.rs", "place_divergent_writes"), 1),
            (("allowances/pending.rs", "place_incoming_proposal"), 1),
            (("allowances/pending.rs", "remove_withdrawn_proposal"), 1),
            // Settled: the exact discard of the proposal it answers, committed with the
            // record view that retires the proposal (Paykit spec 8.3.1).
            (("allowances/replies.rs", "retire"), 1),
            // Moved: a kept receipt goes to `dw/` in the commit that keeps it there, its
            // marker kept; and a bound's drop there.
            (("divergent.rs", "keep_over_paused"), 2),
            // Dropped for a bound, as never processed: its pair forgotten in the same
            // commit, so its sender is asked again (Paykit spec 9, Messages dropped for a
            // bound); not a give-up.
            (("receive.rs", "make_room_for_reply"), 1),
            (("divergent.rs", "step_keep"), 1),
            // Cleanup of a note whose entry is done: placed, or released by the client as a
            // duplicate of one it processed.
            (("stored.rs", "place_stored"), 1),
            (("stored.rs", "take_stored_event"), 1),
        ]
        .into_iter()
        .collect();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        let mut dirs = vec![root.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    files.push(path);
                }
            }
        }
        let mut found: BTreeMap<(String, String), usize> = BTreeMap::new();
        for path in files {
            let name = path
                .strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let source = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = source
                .lines()
                .take_while(|line| !line.starts_with("#[cfg(test)]"))
                .collect();
            let mut function = String::new();
            for (at, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                if let Some(rest) = code.split("fn ").nth(1).filter(|_| code.contains("fn ")) {
                    if let Some(end) = rest.find(|c: char| !(c.is_alphanumeric() || c == '_')) {
                        if end > 0 && (code.starts_with("fn ") || code.contains(" fn ")) {
                            function = rest[..end].to_string();
                        }
                    }
                }
                // A kept copy's key bound under a name the scan doesn't see.
                if let Some(binding) = code.strip_prefix("let ") {
                    let builds = BUILDERS.iter().any(|b| binding.contains(b));
                    if builds && !binding.contains(", \"\")") {
                        let bound = binding.split([' ', ':']).next().unwrap_or_default();
                        assert!(
                            ["kept_key", "note_key", "kept"].contains(&bound),
                            "{name}:{}: a kept copy's key bound as `{bound}`",
                            at + 1
                        );
                    }
                }
                let deletes = code.contains(".delete(")
                    || code.contains("local_delete(")
                    || code.contains("Delete {");
                if !deletes {
                    continue;
                }
                let window = if code.contains("Delete {") {
                    lines[at..lines.len().min(at + 3)].join(" ")
                } else {
                    code.to_string()
                };
                if KEPT.iter().any(|k| window.contains(k)) || window.contains("(&kept)") {
                    *found.entry((name.clone(), function.clone())).or_default() += 1;
                }
            }
        }
        let found: BTreeMap<(&str, &str), usize> = found
            .iter()
            .map(|((file, function), n)| ((file.as_str(), function.as_str()), *n))
            .collect();
        assert_eq!(found, allowed);
    }
    use crate::testkit::{object, Party};

    /// The later write's rule over every answer of the Dispatch, bare and with the batch held by
    /// the client (Dispatch spec 6.4, 12.5; Paykit spec 8.1). A batch held is judged first:
    /// whatever the code inside it, the write waits, never counted nor given up, but for a
    /// wallet-wide stop or an own Dispatch that doesn't answer, which are returned. A bare answer
    /// counts toward the 30 days as any write not made. Every answer the receive goes past at
    /// a failed upkeep of the app record is never counted. (A write waiting
    /// behind an earlier kept batch, `AppError::Ahead`, can't be built here, its `KeptAhead`
    /// being the client's own; the flows take it.)
    #[test]
    fn test_stored_later_write_rule_over_every_answer_bare_and_held() {
        use pubky_dispatch_client::error::ClientError;
        use pubky_dispatch_core::http::ErrorCode;
        for code in ErrorCode::ALL {
            let bare = AppError::Switch(ClientError::Switch {
                status: code.status(),
                code,
                message: String::new(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            });
            let held = AppError::Kept {
                batch_id: [7; 16],
                error: Box::new(bare.clone()),
            };
            // Returned, held or bare: the own Dispatch not answering, and the Dispatch's
            // wallet-wide stops, by name.
            let returned = matches!(
                code,
                ErrorCode::Unavailable
                    | ErrorCode::Staged
                    | ErrorCode::NotCurrent
                    | ErrorCode::CertExpired
                    | ErrorCode::RecoveryRequired
            );
            let bare_placed = submitted::<()>(Err(bare.clone()));
            let bare_counted = matches!(bare_placed, Ok(Placed::Not));
            let held_placed = submitted::<()>(Err(held));
            if returned {
                assert!(bare_placed.is_err(), "{code:?} bare");
                assert!(held_placed.is_err(), "{code:?} held");
            } else {
                // Bare, nothing of it held: a write not made, counted (Paykit spec 8.1).
                assert_eq!(bare_placed.unwrap(), Placed::Not, "{code:?} bare");
                // Never counted, never given up, while the client holds a batch of it.
                assert_eq!(held_placed.unwrap(), Placed::Waits, "{code:?} held");
            }
            // A failed upkeep of the app record goes past this answer only if no later write
            // counts it (`own_record::own_record_left_owed`).
            if crate::own_record::own_record_left_owed(&PaykitError::Client(bare)).is_some() {
                assert!(
                    !bare_counted,
                    "{code:?} goes past the upkeep but counts here"
                );
            }
        }
    }

    #[test]
    fn test_stored_held_client_refusals_wait() {
        // A batch kept behind batches kept from earlier calls, and one the client keeps on its
        // own `NotNow`: held, so the write waits; bare `NotNow` waits too.
        for refusal in [
            ClientRefusal::NotNow("not now".into()),
            ClientRefusal::Usage("kept behind".into()),
        ] {
            let held = AppError::Kept {
                batch_id: [7; 16],
                error: Box::new(AppError::Refused(refusal.clone())),
            };
            assert_eq!(
                submitted::<()>(Err(held)).unwrap(),
                Placed::Waits,
                "{refusal:?}"
            );
        }
        assert_eq!(
            submitted::<()>(Err(AppError::Refused(ClientRefusal::NotNow("key".into())))).unwrap(),
            Placed::Waits
        );
    }

    /// A message whose later write's batch the client still keeps is never given up, by
    /// any of the give-up paths, a stored entry's (`se/`), a reply's kept before its record
    /// (`pr/`) and one's kept over a divergent record (`dw/`): nothing is told, and the
    /// batch stays marked.
    #[tokio::test]
    async fn test_stored_give_ups_wait_while_the_batch_is_held() {
        let w = crate::testkit::Dispatches::new();
        let mut wallet = w
            .join(
                &w.alice,
                "wallet",
                crate::AppScopes::new()
                    .pay()
                    .manage_contacts()
                    .build()
                    .unwrap(),
            )
            .await;
        let noted = note(false);
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let stored = bob.message(
            &alice,
            "paykit.payment_request",
            vec![],
            object(serde_json::json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "order-1",
                "proposal_expires_at": null
            })),
        );
        w.hold_write(&mut wallet, &stored).await;
        let note_key = key::stored_note(&noted.sender, &noted.msg_id);
        assert_eq!(
            Box::pin(wallet.give_up_stored(&note_key, &noted, &stored))
                .await
                .unwrap(),
            Ended::Waits
        );
        Box::pin(wallet.give_up("pr/held", &stored)).await.unwrap();
        Box::pin(wallet.give_up_divergent("dw/held", &stored))
            .await
            .unwrap();
        assert!(wallet.notices().await.unwrap().is_empty());
        assert!(wallet.held_write_kept(&stored).await.unwrap());
    }

    /// The client applied the batch of a kept message's write, its operations committed as
    /// one: the message is placed, not given up, and nothing is told. A marker naming a batch
    /// the client holds no status of establishes nothing: the give-up goes ahead, told.
    #[tokio::test]
    async fn test_stored_give_up_places_an_applied_batch_and_gives_up_one_with_no_status() {
        let w = crate::testkit::Dispatches::new();
        let mut wallet = w
            .join(
                &w.alice,
                "wallet",
                crate::AppScopes::new()
                    .pay()
                    .manage_contacts()
                    .build()
                    .unwrap(),
            )
            .await;
        let noted = note(false);
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let stored = bob.message(
            &alice,
            "paykit.payment_request",
            vec![],
            object(serde_json::json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "order-1",
                "proposal_expires_at": null
            })),
        );
        let note_key = key::stored_note(&noted.sender, &noted.msg_id);
        // Kept, then sent again by the client's next read, past the rate limit, and applied.
        w.hold_write(&mut wallet, &stored).await;
        w.clock.advance_ms(11 * 60_000);
        wallet.app_mut().receive(None).await.unwrap();
        assert_eq!(
            Box::pin(wallet.give_up_stored(&note_key, &noted, &stored))
                .await
                .unwrap(),
            Ended::Placed
        );
        assert!(wallet.notices().await.unwrap().is_empty());
        assert_eq!(
            wallet
                .local_get::<[u8; 16]>(&held_write(&stored))
                .await
                .unwrap(),
            None
        );
        // A marker of a batch the client holds nothing of: given up, the host told.
        wallet
            .app_mut()
            .protocol_commit(
                crate::protocol::BACKUP_NAME,
                &[pubky_dispatch_client::app::LocalWrite::Put {
                    key: held_write(&stored).into_bytes(),
                    value: vec![3; 16],
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            Box::pin(wallet.give_up_stored(&note_key, &noted, &stored))
                .await
                .unwrap(),
            Ended::GivenUp
        );
        assert_eq!(wallet.notices().await.unwrap().len(), 1);
    }

    #[test]
    fn test_stored_later_write_gives_up_at_the_window_only_what_isnt_held() {
        let window = crate::receive::KEPT_MESSAGE_MS;
        let at = 1_000;
        assert_eq!(later_write(Placed::Not, at, Some(at + 1)), LaterWrite::Keep);
        assert_eq!(
            later_write(Placed::Not, at, Some(at + window)),
            LaterWrite::GiveUp
        );
        // Waiting, as while the client holds a batch of the write: kept past any bound.
        assert_eq!(
            later_write(Placed::Waits, at, Some(at + 4 * window)),
            LaterWrite::Keep
        );
    }

    fn note(placed: bool) -> StoredNote {
        let alice = Party::new("alice", "wallet", &["paykit_pay", "paykit_contacts"]);
        let bob = Party::new(
            "bob",
            "shop",
            &["paykit_request", "paykit_settlements", "paykit_contacts"],
        );
        let stored = bob.message(
            &alice,
            "paykit.payment_request",
            vec![],
            object(serde_json::json!({
                "amount": { "asset": "btc", "value": "1" },
                "payment_reference": "order-1",
                "proposal_expires_at": null
            })),
        );
        StoredNote {
            stored_id: 1,
            hash: stored.hash(),
            sender: bob.id(),
            msg_id: stored.message().msg_id.clone(),
            message: KeptMessage::taken(&stored, 1),
            placed,
        }
    }

    #[test]
    fn test_stored_released_entry_declined_once_its_effect_is_written() {
        assert!(declines(Some(&note(true))));
    }

    #[test]
    fn test_stored_released_entry_applied_before_its_effect_is_written() {
        assert!(!declines(Some(&note(false))));
        assert!(!declines(None));
    }
}
