//! Entries the client stored locally for good, and what Paykit does with each (Switch spec
//! 8.2; Paykit spec 8.1): the duty events that tell of them are taken at every receive and
//! kept in Paykit's own state.
//!
//! - **An effect refused for good** (`EffectRefused`, or a `Parked` duty event of any reason
//!   but `Uncertain`), such as a record write the switch refused (`403
//!   permission_missing`), or one over a quota: the message is kept (`p/se/<sender>/<msg_id>`)
//!   and written into its record by a later write of the app's own, tried at every receive
//!   until it lands, as room frees: a request, a payer's reply, a proof of a payment without
//!   a request into the record of its settlement, a receipt beside the proof it names.
//!   Meanwhile a Private Payment List is held over its record (`lh/`), the newer kept, and a
//!   request is shown unrecorded ([`crate::RequestStatus::Unrecorded`]). A payee's
//!   cancellation needs nothing here: its stop is kept with the cursor move, with no write
//!   at the switch (`kc/`, Paykit spec 8.1). A later write has the effects the message has
//!   at its first read, in the batch that writes it: a receipt's on its claim and in the
//!   journal, the notice "marked failed, was paid" included (`receive::receipt_settles`),
//!   and a request's or a list's note of its sender (`receive::peer_note`). Once written,
//!   the stored entry is forgotten, its message staying processed. One not written within
//!   [`crate::KEPT_MESSAGE_MS`] of its acceptance, such as a reply whose record never comes,
//!   is given up: its note goes, its entry stays stored, and the host is told
//!   ([`crate::NoticeKind::MessageGivenUp`]).
//! - **An effect of unknown fate** (`EffectUncertain`, or `Parked` as `Uncertain`): the same
//!   write compensates for it, since placing a message a record already holds writes
//!   nothing. A copy of the entry the host released after that is declined, its message
//!   processed, so its effect isn't applied twice (`PaykitProtocol`'s receive step).
//! - **A released entry that ran as a duplicate** (`ReleasedDuplicate`): its note goes.
//! - **An entry dropped unverified** (`StoredEntryDropped`): it counts in its sender's mark
//!   of entries dropped unverified (`ud/<sender>`, `marks.rs`), and the sender is asked at
//!   once to send again what its inbox accepted from the mark's earliest time to its latest,
//!   while it may still keep them, and again at most once a day while the mark lives (Paykit
//!   spec 8.1, 9).
//!
//! **Taken at first sight.** The SDK acts on each such event the first time a receive sees
//! it, and keeps a mark of it (`p/dt/<event ID>`) in the same commit as what the event says,
//! so a crash before that commit leaves the event to be taken again, and a later sight does
//! nothing twice, whoever acknowledges it and whenever. A mark goes once its event is
//! acknowledged. An event whose message the SDK can't keep, such as one whose entry is
//! gone, is not taken, and stays for the host. So once [`Paykit::receive`] has returned,
//! the host may acknowledge up to the last event its report showed. The SDK also
//! acknowledges, in order, the events it has taken up to the first one it hasn't, so they
//! don't pile up for a host that acknowledges only its own: housekeeping, which decides
//! nothing.

use paykit_lib::derive::natural_key;
use paykit_lib::records::{ActivityRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{
    AppError, AppEvent, OpBatch, ParkReason, RecordContent, Refusal as ClientRefusal,
};
use pubky_switch_core::enc::serde_b64;
use pubky_switch_core::scopes::Permission;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, Role};
use crate::error::{PaykitError, Result};
use crate::notices::{KeptNotice, NoticeKind, StoredHandle};
use crate::paykit::Paykit;
use crate::receive::{
    failure_view, receipt_claim, receipt_deadline, receipt_on_record, receipt_settles, FailureView,
    ReceiptClaim, ReceiptSettles, SwitchClaim,
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
    /// Not written while the record's absence decides nothing yet: tried again at the next
    /// receive, and never given up meanwhile.
    Waits,
}

/// What a later write's batch came to: written, or refused by the switch, and so not
/// written now and counted toward the message's window, or not sent now for want of a key,
/// and so waiting. Any other error, such as a store failure or a switch that doesn't
/// answer, is returned: the write wasn't evaluated, and nothing may be given up on it.
pub(crate) fn submitted<T>(result: std::result::Result<T, AppError>) -> Result<Placed> {
    match result {
        Ok(_) => Ok(Placed::Done),
        Err(AppError::Refused(ClientRefusal::NotNow(_))) => Ok(Placed::Waits),
        Err(error) if refused(&error) => Ok(Placed::Not),
        Err(error) => Err(error.into()),
    }
}

/// Whether `error` is the switch's refusal of a request, an answer with its error code,
/// rather than a switch that doesn't answer or a failure of the app's own.
fn refused(error: &AppError) -> bool {
    !crate::pay::switch_unavailable(error) && error.unkept().code().is_some()
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

/// The one rule of both later-write paths, a stored entry's note (`se/`) and a payer's reply
/// kept before its record (`pr/`): a message written, or reflected already, is done; one not
/// written now (the switch refused the batch, the record can't take it, its value doesn't
/// open, or a scope key it waits for hasn't come) is kept within [`crate::KEPT_MESSAGE_MS`]
/// of its acceptance, `accepted_at`, and given up from then on, judged on `service`, the
/// highest service time verified (Paykit spec 8.1); one that waits while its record's
/// absence decides nothing is kept. An error, such as a switch that doesn't answer or a
/// store failure, never reaches this: it is returned, and the message stays kept.
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
fn record_side(target: &ActivityTarget) -> crate::hold::RecordSide {
    match target.side {
        crate::activity::SideTag::Payer => crate::hold::RecordSide::Payer,
        crate::activity::SideTag::Payee => crate::hold::RecordSide::Payee,
    }
}

/// A stored entry's message Paykit writes into its record later (`p/se/<sender>/<msg_id>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredNote {
    /// The entry's local ID at the client.
    pub(crate) stored_id: u64,
    /// The entry's message hash, which the client checks a forget by
    /// ([`pubky_switch_client::app::App::forget_parked`]): the note's message's own, so the
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

/// The write of the host's notice that `stored` was given up (`nt/<sender>/<msg_id>`),
/// raised at `service`, the highest service time verified.
pub(crate) fn given_up_write(
    stored: &StoredMessage,
    service: Option<u64>,
) -> Result<pubky_switch_client::app::LocalWrite> {
    let sender = stored.sender();
    let msg_id = &stored.message().msg_id;
    crate::state::local_put(
        &key::message_notice(&sender, msg_id),
        &given_up_notice(
            stored,
            service.unwrap_or(0),
            crate::notices::GivenUpOrigin::ReceiveStep,
        ),
    )
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

/// Whether `note`, a stored note read bound to its key, keeps a message of `request` still
/// to write: one not in its record yet, whether it waits for a write that failed or for a
/// listing. A payee's cancellation of that request kept as a stop stays while one does
/// ([`Paykit::place_kept_cancellations`]): the request's own entry may be among them, and
/// written without the stop it would be payable.
pub(crate) fn writes_request(
    note: &StoredNote,
    own: &[u8; 32],
    request: &[u8; 32],
) -> Result<bool> {
    let stored = note.message.stored(own)?;
    Ok(!note.placed && named_request(&stored) == Some(*request))
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
    pub(crate) fn bound(&self, key: &str, own: &[u8; 32]) -> Result<StoredMessage> {
        let Some(StateKey::StoredNote { sender, msg_id }) = StateKey::parse(key.as_bytes()) else {
            return Err(PaykitError::corrupt());
        };
        let stored = self.message.stored(own)?;
        if self.sender != sender
            || self.msg_id != msg_id
            || stored.sender() != sender
            || stored.message().msg_id != msg_id
            || stored.hash() != self.hash
        {
            return Err(PaykitError::corrupt());
        }
        Ok(stored)
    }
}

/// The request `stored` is, or names: a request's own hash, the first of a payer's reply's
/// `refs`, the second of a receipt's.
fn named_request(stored: &StoredMessage) -> Option<[u8; 32]> {
    let refs = &stored.message().refs;
    match stored.body() {
        Body::PaymentRequest(_) => Some(stored.hash()),
        Body::PaymentReceipt(_) => refs.get(1).copied(),
        _ => refs.first().copied(),
    }
}

/// Whether a released entry with `note` kept for its message is declined: only once the app
/// has written its effect itself (Switch spec 8.2). Without a note, or before the write, the
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
}

impl Paykit {
    /// The note kept at `key`, read whole and bound to it ([`StoredNote::bound`]).
    async fn stored_note(&self, key: &str) -> Result<Option<StoredNote>> {
        self.local_get::<StoredNote>(key).await
    }

    /// The requests received whose record write the switch refused, kept to write later,
    /// as the payer sees them ([`RequestStatus::Unrecorded`]).
    pub(crate) async fn unrecorded_requests(&self) -> Result<Vec<RequestView>> {
        let own = self.identity();
        let mut out = Vec::new();
        for (rest, note) in self.local_scan::<StoredNote>(key::STORED_NOTES).await? {
            let stored = note.bound(&format!("{}{rest}", key::STORED_NOTES), &own)?;
            if note.placed {
                continue;
            }
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
                status: RequestStatus::Unrecorded,
                divergent: false,
                receipt: None,
            });
        }
        Ok(out)
    }

    /// Take the duty events about stored entries the client hasn't had acknowledged,
    /// `events`, each at its first sight (module docs, "Taken at first sight"); acknowledge,
    /// in order, those taken up to the first that isn't; drop the marks of events
    /// acknowledged. Only Paykit's own local state is written: the kept messages are written
    /// into their records by [`Paykit::place_stored`].
    pub(crate) async fn take_stored_events(&mut self, events: &[AppEvent]) -> Result<()> {
        let mut listed = std::collections::BTreeSet::new();
        let mut prefix = true;
        let mut through = None;
        for event in events {
            let Some(id) = event.id() else {
                continue;
            };
            let mark = key::taken(&id);
            let taken = match self.local_get::<bool>(&mark).await? {
                Some(_) => true,
                None => self.take_stored_event(event, &mark).await?,
            };
            listed.insert(mark);
            prefix &= taken;
            if prefix {
                through = Some(id);
            }
        }
        if let Some(id) = through {
            self.app.ack_events(id).await?;
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

    /// Keep what a duty event about a stored entry says, with its mark `mark`, in one
    /// commit (module docs). Whether it was taken: `false` for an event of another kind, or
    /// one whose message can't be kept, which stays for the host.
    async fn take_stored_event(&mut self, event: &AppEvent, mark: &str) -> Result<bool> {
        let mut tx = Tx::default();
        let taken = match event {
            AppEvent::Parked {
                stored_id, reason, ..
            } if *reason != ParkReason::Released => self.note_stored(*stored_id, &mut tx).await?,
            AppEvent::EffectRefused { stored_id, .. }
            | AppEvent::EffectUncertain { stored_id, .. } => {
                self.note_stored(*stored_id, &mut tx).await?
            }
            AppEvent::ReleasedDuplicate { sender, msg_id, .. } => {
                tx.delete(&key::stored_note(sender, msg_id));
                true
            }
            AppEvent::StoredEntryDropped {
                sender,
                accepted_at_ms,
                ..
            } => {
                self.count_dropped_unverified(&mut tx, *sender, *accepted_at_ms)
                    .await?;
                true
            }
            _ => false,
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
    async fn note_stored(&mut self, stored_id: u64, tx: &mut Tx) -> Result<bool> {
        let own = self.identity();
        let Some(entry) = self.app.parked_entry(stored_id).await? else {
            return Ok(false);
        };
        let Ok(stored) = StoredMessage::new(
            entry.sender,
            &entry.cert,
            entry.plaintext.clone(),
            Some(entry.accepted_at_ms),
            &own,
        ) else {
            return Ok(false);
        };
        let note_key = key::stored_note(&entry.sender, &stored.message().msg_id);
        if self.stored_note(&note_key).await?.is_some() {
            return Ok(true);
        }
        match self.place_of(&stored).await? {
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
            Some(Place::Record { .. }) => {}
        }
        let note = StoredNote {
            stored_id,
            hash: stored.hash(),
            sender: entry.sender,
            msg_id: stored.message().msg_id.clone(),
            message: KeptMessage::of(&stored),
            placed: false,
        };
        tx.put(&note_key, &note)?;
        Ok(true)
    }

    /// Where `stored` goes for this app: a request into the payer's request record; a
    /// payer's reply into the payee's record of the request it names, under its label; a
    /// proof of a payment without a request into the payee's record of its settlement,
    /// which it may open (Paykit spec 8.4); a receipt into the payer's record; a list held
    /// over its record, unless a list as new is held. `None` for a kind no record of this
    /// app's holds, or one its scopes don't write.
    async fn place_of(&self, stored: &StoredMessage) -> Result<Option<Place>> {
        let own = self.identity();
        let sender = stored.sender();
        let scopes = self.scopes();
        let pay = self.holds("pay");
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
                let Some(settlement) = self.rail.settlement_id(&proof.proof) else {
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
                        record(
                            ActivityTarget::payee(
                                natural_key::request_record(&own, &request),
                                label.as_ref(),
                            ),
                            Role::of(stored.body()).unwrap_or(Role::Response),
                            false,
                        )
                    }
                    _ => None,
                }
            }
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
            _ => None,
        })
    }

    /// Write every kept message whose record can take it now, each as a later write of the
    /// app's own, built on the version held; then forget the stored entry of each one in
    /// place, and drop its note. A write that fails now is tried again at the next receive;
    /// a message not written within [`crate::KEPT_MESSAGE_MS`] of its acceptance, on the
    /// highest service time verified, is given up, its entry left stored for the host.
    pub(crate) async fn place_stored(&mut self) -> Result<()> {
        let own = self.identity();
        for (rest, mut note) in self.local_scan::<StoredNote>(key::STORED_NOTES).await? {
            let note_key = format!("{}{rest}", key::STORED_NOTES);
            let stored = note.bound(&note_key, &own)?;
            if !note.placed {
                // A kept message carries its entry's acceptance time; one without it is a
                // corrupt store, never a message past its window.
                let accepted_at = stored.accepted_at().ok_or_else(PaykitError::corrupt)?;
                let placed = match self.place_of(&stored.clone()).await? {
                    Some(Place::List(hold)) => {
                        if let Some((held, kept)) = hold {
                            self.local_put(&held, &kept).await?;
                        }
                        Placed::Done
                    }
                    Some(Place::Record {
                        target,
                        role,
                        creates,
                        // An error isn't a refusal: the write wasn't evaluated, so the note stays,
                        // neither counted nor given up, and the receive returns the error.
                    }) => {
                        self.place_note(&target, role, creates, stored.clone(), Tx::default())
                            .await?
                    }
                    None => Placed::Not,
                };
                let later = later_write(placed, accepted_at, self.service_time());
                note.placed = later == LaterWrite::Done;
                if !note.placed {
                    if later == LaterWrite::GiveUp {
                        // Given up: the host is told, in the commit that drops the note.
                        let mut tx = Tx::default();
                        tx.delete(&note_key);
                        let notice = key::message_notice(&note.sender, &note.msg_id);
                        // The notice keeps the entry's handle, which its acknowledgement
                        // forgets; one kept already, as after a restore onto another store,
                        // takes the entry's handle here.
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
                                    named_request(&stored),
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
                        self.local_commit(tx).await?;
                    }
                    continue;
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

    /// Give up `stored`, a payer's reply kept at `kept_key` (`pr/`): the kept copy goes and
    /// the host is told ([`NoticeKind::MessageGivenUp`], from a kept reply, with no entry's
    /// handle), in one commit. The notice is raised once.
    pub(crate) async fn give_up(&mut self, kept_key: &str, stored: &StoredMessage) -> Result<()> {
        let mut tx = Tx::default();
        tx.delete(kept_key);
        let notice = key::message_notice(&stored.sender(), &stored.message().msg_id);
        if self.kept_notice(&notice).await?.is_none() {
            let at = self.service_time().unwrap_or(0);
            tx.put(
                &notice,
                &given_up_notice(stored, at, crate::notices::GivenUpOrigin::KeptReply),
            )?;
        }
        self.local_commit(tx).await
    }

    /// Place `stored` in its `role` of the record at `target`, as a write of the app's own.
    /// Whether it is in place: written now, or reflected already.
    ///
    /// A record that is absent or deleted is opened only by a message that may open it
    /// (`creates`), and a receipt goes only beside the proof it names (Paykit wire 5.3). A
    /// record held as a value that doesn't read, or that the app can't open, is never
    /// written over (Switch spec 8.2: a reader doesn't act on a record it can't read, and a
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
        locals: Tx,
    ) -> Result<Placed> {
        let key = self.activity_key(target)?;
        // Over a divergent record its user hasn't confirmed, only a stop is written: the
        // message waits, within its window (Paykit spec 6).
        if self.divergent_write_waits(&key, &stored).await? {
            return Ok(Placed::Not);
        }
        let content = self.app.shared_record(&key).await?.map(|held| held.content);
        // Absent or deleted while the copy of its side may lack records: the copy decides
        // nothing yet (Paykit spec 8.1, "Records not listed yet"), whatever key the record's
        // scope waits for.
        if matches!(content, None | Some(RecordContent::Deleted))
            && self.side_unlisted(record_side(target)).await?
        {
            return Ok(Placed::Waits);
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
            }
            // A value sealed under a key the app doesn't hold yet: the message waits for it,
            // within its window.
            Some(content) if crate::hold::awaits_key(&content) => return Ok(Placed::Not),
            Some(RecordContent::Unreadable(_)) => return Ok(Placed::Not),
            Some(RecordContent::Value(_)) => {
                let Some(record) = self.held_activity(target).await? else {
                    return Ok(Placed::Not);
                };
                if role == Role::Receipt {
                    return self.place_receipt(target, &record, stored, locals).await;
                }
            }
        }
        self.write_placed(target, stored, locals).await
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
        locals: Tx,
    ) -> Result<Placed> {
        let Body::PaymentReceipt(receipt) = stored.body() else {
            return Ok(Placed::Not);
        };
        let attempt_id = receipt.attempt_id;
        let mut tx = locals;
        let Some(found) = receipt_on_record(record, &stored) else {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        };
        let sender = stored.sender();
        let mut batch = OpBatch::new();
        let mut writes = false;
        if found.names_held_proof {
            if let Some(entry) = self
                .placed_write(target, sender, Role::Receipt, stored.clone(), Vec::new())
                .await?
            {
                batch = batch.entry(entry);
                writes = true;
            }
        }
        if self.app.cert().has_permission(Permission::ClaimsWrite) {
            // Read bound to its key and to the claim the receipt settles, as at the first read.
            let obligation = receipt_claim(&stored, &attempt_id, |request| {
                self.obligation(&sender, request, 0)
            });
            let journaled = self.journal_entry_on(&attempt_id, &obligation).await?;
            let deadline = receipt_deadline(&stored, record, journaled.as_ref());
            // The claim view as the first read takes it, by the same rule
            // (`receive::failure_view`): the view the app keeps, or the switch's read only
            // when it asks for it. A read the switch refuses settles nothing.
            let kept = self
                .app
                .claim_evidence(&obligation)
                .await?
                .map(|kept| kept.view);
            let mut view = failure_view(kept, None);
            if matches!(view, FailureView::ReadSwitch) {
                let read = match self.app.read_claim(&obligation).await {
                    Ok(read) => SwitchClaim::Read(read.map(|read| read.view)),
                    Err(error) if refused(&error) => SwitchClaim::Refused,
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
                FailureView::Refused | FailureView::ReadSwitch => ReceiptSettles::default(),
            };
            if let Some((op, deadline)) = settles.claim {
                batch = batch
                    .op(op.kind())
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
                tx.put(&key::journal(&attempt_id), &entry)?;
            }
        }
        if !writes {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        }
        submitted(self.app.submit_batch(batch.locals(tx.into_writes())).await)
    }

    /// Write `stored` into its record at `target`, with `locals` in the same batch, or
    /// committed alone when the record reflects it already.
    async fn write_placed(
        &mut self,
        target: &ActivityTarget,
        stored: StoredMessage,
        locals: Tx,
    ) -> Result<Placed> {
        let role = Role::of(stored.body()).unwrap_or(Role::Response);
        let sender = stored.sender();
        // A request notes its sender in the batch that writes it, as at its first read.
        let peer = (role == Role::Request).then(|| crate::receive::peer_note(&sender, &stored));
        let mut tx = locals;
        if let Some((peer, at)) = peer {
            tx.put(&peer, &at)?;
        }
        let Some(entry) = self
            .placed_write(target, sender, role, stored, Vec::new())
            .await?
        else {
            self.local_commit(tx).await?;
            return Ok(Placed::Done);
        };
        submitted(
            self.app
                .submit_batch(OpBatch::new().entry(entry).locals(tx.into_writes()))
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{object, Party};

    fn note(placed: bool) -> StoredNote {
        let alice = Party::new("alice", "wallet", &["pay", "contacts"]);
        let bob = Party::new("bob", "shop", &["request"]);
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
            message: KeptMessage::of(&stored),
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
