//! Entries the client stored locally for good, and what Paykit does with each (Switch spec
//! 8.2; Paykit spec 8.1): the duty events that tell of them are taken at every receive,
//! kept in Paykit's own state, and then acknowledged by the SDK, not left to the host.
//!
//! - **An effect refused for good** (`EffectRefused`, or a `Parked` duty event of any reason
//!   but `Uncertain`), such as a record write the switch refused (`403
//!   permission_missing`), or one over a quota: the message is kept (`p/se/<sender>/<msg_id>`)
//!   and written into its record by a later write of the app's own, tried at every receive
//!   until it lands, as room frees. Meanwhile a Private Payment List is held over its record
//!   (`lh/`), and a request is shown unrecorded ([`crate::RequestStatus::Unrecorded`]). A
//!   payee's cancellation needs nothing here: its stop is kept with the cursor move, with no
//!   write at the switch (`kc/`, Paykit spec 8.1). Once written, the stored entry is
//!   forgotten, its message staying processed.
//! - **An effect of unknown fate** (`EffectUncertain`, or `Parked` as `Uncertain`): the same
//!   write compensates for it, since placing a message a record already holds writes
//!   nothing. A copy of the entry the host released after that is declined, its message
//!   processed, so its effect isn't applied twice (`PaykitProtocol`'s receive step).
//! - **A released entry that ran as a duplicate** (`ReleasedDuplicate`): its note goes.
//! - **An entry dropped unverified** (`StoredEntryDropped`): a `resend_request` for the
//!   messages its sender's inbox accepted in that millisecond is queued to the sender, who
//!   may still keep it (Paykit spec 9).
//!
//! The SDK acknowledges these events in order, up to the first duty event it doesn't take,
//! such as one about a kept batch or the account, which stays for the host: the client
//! acknowledges every event up to an ID at once. A stored entry's event after such a one is
//! kept again at each receive, which changes nothing, and an entry dropped unverified is
//! asked for once the events before it are acknowledged. A host acknowledges only the
//! events it takes itself, up to the last of them, so it never acknowledges these unseen.
//!
//! Kinds a record doesn't hold, a proof of a payment without a request and an entry that
//! doesn't read as a message are left stored, for the host ([`pubky_switch_client::app::App::parked_entries`]).

use paykit_lib::derive::natural_key;
use paykit_lib::records::StoredMessage;
use paykit_lib::Body;
use pubky_switch_client::app::{AppEvent, OpBatch, ParkReason};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, Role};
use crate::error::Result;
use crate::paykit::Paykit;
use crate::protocol::QueuedAsk;
use crate::records::{list_target, KeptMessage};
use crate::requests::{RequestRef, RequestStatus, RequestView};
use crate::state::key;

/// A stored entry's message Paykit writes into its record later (`p/se/<sender>/<msg_id>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StoredNote {
    /// The entry's local ID at the client.
    pub(crate) stored_id: u64,
    #[serde(with = "serde_b64")]
    pub(crate) sender: [u8; 32],
    pub(crate) msg_id: String,
    pub(crate) message: KeptMessage,
    /// The effect is in place: written by the app, or found in the record already.
    pub(crate) placed: bool,
}

/// Where a kept message goes: its record and role.
enum Place {
    /// Into this record, in this role.
    Record(ActivityTarget, Role),
    /// Held elsewhere already: the kept stop or the held list.
    Held,
}

impl Paykit {
    /// The requests received whose record write the switch refused, kept to write later,
    /// as the payer sees them ([`RequestStatus::Unrecorded`]).
    pub(crate) async fn unrecorded_requests(&self) -> Result<Vec<RequestView>> {
        let own = self.identity();
        let mut out = Vec::new();
        for (_, note) in self.local_scan::<StoredNote>(key::STORED_NOTES).await? {
            if note.placed {
                continue;
            }
            let stored = note.message.stored(&own)?;
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
            });
        }
        Ok(out)
    }

    /// Take the duty events about stored entries (module docs), and acknowledge those
    /// taken, in order; then write the kept messages that can be written now.
    pub(crate) async fn take_stored_entries(&mut self) -> Result<()> {
        let events = self.app.unacknowledged_events().await?;
        // Acknowledged up to the first event the SDK doesn't take; every one it takes is
        // kept first, the later ones again at the next receive, which changes nothing.
        let mut taken = None;
        let mut prefix = true;
        for event in &events {
            let Some(id) = event.id() else {
                continue;
            };
            let ours = self.take_stored_event(event, prefix).await?;
            prefix &= ours;
            if prefix {
                taken = Some(id);
            }
        }
        if let Some(id) = taken {
            self.app.ack_events(id).await?;
        }
        self.place_stored().await
    }

    /// Keep what a duty event about a stored entry says (module docs). Whether it is one,
    /// taken. An entry dropped unverified is asked for only with `acknowledged`, when its
    /// event is acknowledged right after, so the request goes once.
    async fn take_stored_event(&mut self, event: &AppEvent, acknowledged: bool) -> Result<bool> {
        match event {
            AppEvent::Parked {
                stored_id, reason, ..
            } if *reason != ParkReason::Released => self.note_stored(*stored_id).await?,
            AppEvent::EffectRefused { stored_id, .. }
            | AppEvent::EffectUncertain { stored_id, .. } => self.note_stored(*stored_id).await?,
            AppEvent::ReleasedDuplicate { sender, msg_id, .. } => {
                self.local_delete(&key::stored_note(sender, msg_id)).await?;
            }
            AppEvent::StoredEntryDropped {
                sender,
                accepted_at_ms,
                ..
            } if acknowledged => {
                let ask = QueuedAsk {
                    sender: *sender,
                    from_ms: *accepted_at_ms,
                    until_ms: Some(*accepted_at_ms),
                };
                self.local_put(&key::ask(sender, *accepted_at_ms), &ask)
                    .await?;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Keep the message of the entry stored at `stored_id` to write it later, once: an entry
    /// forgotten already, one that doesn't read as a message that verifies for this app, and
    /// a kind no record of this app's holds are left to the host.
    async fn note_stored(&mut self, stored_id: u64) -> Result<()> {
        let own = self.identity();
        let Some(entry) = self.app.parked_entry(stored_id).await? else {
            return Ok(());
        };
        let Ok(stored) = StoredMessage::new(
            entry.sender,
            &entry.cert,
            entry.plaintext.clone(),
            Some(entry.accepted_at_ms),
            &own,
        ) else {
            return Ok(());
        };
        let note_key = key::stored_note(&entry.sender, &stored.message().msg_id);
        if self.local_get::<StoredNote>(&note_key).await?.is_some()
            || self.place_of(&stored).await?.is_none()
        {
            return Ok(());
        }
        let note = StoredNote {
            stored_id,
            sender: entry.sender,
            msg_id: stored.message().msg_id.clone(),
            message: KeptMessage::of(&stored),
            placed: false,
        };
        self.local_put(&note_key, &note).await
    }

    /// Where `stored` goes for this app: a request into the payer's request record; a
    /// payer's reply into the payee's record of the request it names, under its label; a
    /// receipt into the payer's record; a list held over its record, in the same step.
    /// `None` for a kind no record of this app's holds, or one its scopes don't write.
    async fn place_of(&mut self, stored: &StoredMessage) -> Result<Option<Place>> {
        let own = self.identity();
        let sender = stored.sender();
        let scopes = self.scopes();
        let pay = self.holds("pay");
        let first = stored.message().refs.first().copied();
        Ok(match stored.body() {
            Body::PaymentRequest(_) if pay => Some(Place::Record(
                ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash())),
                Role::Request,
            )),
            Body::PaymentRequestAcceptance(_)
            | Body::PaymentRequestRejection(_)
            | Body::PaymentRequestCancellation(_)
            | Body::PaymentProof(_) => {
                let label = stored.body().label().cloned();
                match first {
                    Some(request)
                        if crate::protocol::writes_payee_label(&scopes, label.as_ref()) =>
                    {
                        let role = Role::of(stored.body()).unwrap_or(Role::Response);
                        Some(Place::Record(
                            ActivityTarget::payee(
                                natural_key::request_record(&own, &request),
                                label.as_ref(),
                            ),
                            role,
                        ))
                    }
                    _ => None,
                }
            }
            Body::PaymentReceipt(receipt) if pay => {
                let target = match stored.message().refs.get(1) {
                    Some(request) => natural_key::request_record(&sender, request),
                    None => natural_key::payer_payment_record(&own, &receipt.attempt_id),
                };
                Some(Place::Record(ActivityTarget::payer(target), Role::Receipt))
            }
            Body::PrivatePaymentList(_) if pay => {
                let target = list_target(&sender, &stored.message().from.app_id);
                let held = key::held_list(&self.app.record_key(&target)?);
                if self.local_get::<KeptMessage>(&held).await?.is_none() {
                    self.local_put(&held, &KeptMessage::of(stored)).await?;
                }
                Some(Place::Held)
            }
            _ => None,
        })
    }

    /// Write every kept message whose record can take it now, each as a later write of the
    /// app's own, built on the version held; then forget the stored entry of each one in
    /// place. A write that fails now is tried again at the next receive. A receipt goes in
    /// only beside the proof it names (Paykit wire 5.3), and a payer's reply only into a
    /// record this app holds.
    async fn place_stored(&mut self) -> Result<()> {
        let own = self.identity();
        for (rest, mut note) in self.local_scan::<StoredNote>(key::STORED_NOTES).await? {
            let note_key = format!("{}{rest}", key::STORED_NOTES);
            if !note.placed {
                let stored = note.message.stored(&own)?;
                note.placed = match self.place_of(&stored).await? {
                    Some(Place::Held) => true,
                    Some(Place::Record(target, role)) => self
                        .place_note(&target, role, stored)
                        .await
                        .unwrap_or(false),
                    None => false,
                };
                if !note.placed {
                    continue;
                }
                self.local_put(&note_key, &note).await?;
            }
            let summary = self
                .app
                .parked_entries()
                .await?
                .into_iter()
                .find(|entry| entry.stored_id == note.stored_id);
            let gone = match summary {
                None => true,
                Some(entry) => self
                    .app
                    .forget_parked(entry.stored_id, &entry.hash)
                    .await
                    .is_ok(),
            };
            if gone {
                self.local_delete(&note_key).await?;
            }
        }
        Ok(())
    }

    /// Place `stored` in its `role` of the record at `target`, as a write of the app's own.
    /// Whether it is in place: written now, or reflected already.
    async fn place_note(
        &mut self,
        target: &ActivityTarget,
        role: Role,
        stored: StoredMessage,
    ) -> Result<bool> {
        let Some(record) = self.held_activity(target).await? else {
            // A request opens its record; a reply or a receipt waits for one.
            if role != Role::Request {
                return Ok(false);
            }
            return self.write_placed(target, stored).await;
        };
        if role == Role::Receipt
            && !record
                .proof
                .as_ref()
                .is_some_and(|proof| stored.message().refs.first() == Some(&proof.hash()))
        {
            return Ok(false);
        }
        self.write_placed(target, stored).await
    }

    async fn write_placed(
        &mut self,
        target: &ActivityTarget,
        stored: StoredMessage,
    ) -> Result<bool> {
        let role = Role::of(stored.body()).unwrap_or(Role::Response);
        let Some(entry) = self
            .placed_write(target, stored.sender(), role, stored, Vec::new())
            .await?
        else {
            return Ok(true);
        };
        if self
            .app
            .submit_batch(OpBatch::new().entry(entry))
            .await
            .is_err()
        {
            return Ok(false);
        }
        Ok(true)
    }
}
