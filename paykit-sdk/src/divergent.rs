//! Divergent records (Paykit spec 6, "Divergent records"; 8.1; Switch spec 12.6, Switch wire
//! 7.4.9, 7.4.14).
//!
//! After a provider loss, an upload that conflicts with a write made at the new service is
//! kept at the switch, and the record's view shows `divergent` until a writer writes a newer
//! version there. The client holds that flag for the latest view it read
//! ([`SharedRecord::divergent`]) and raises `AppEvent::RecordDivergent` once per change; the
//! app takes the current write as any other, but the upload may hold what it lacks, such as
//! a cancellation, a receipt or a newer list. So:
//!
//! - **A pause mark per record** (`p/dv/<record key>`): set the first time a view the app
//!   reads shows the record divergent, and kept until the user confirms
//!   ([`Paykit::confirm_divergent`]), also once a newer write has cleared the view. A
//!   confirmation keeps the version of the write it was given over: the same divergent view
//!   asks nothing again, a later divergent view of another write pauses the record again,
//!   and the confirmed mark goes at the end of the first receive whose view of the record
//!   shows it no longer divergent. Marks are of any Paykit collection, so an Allowance's
//!   `allowances` and `allowance_ledgers` records take them the same way.
//! - **No automatic payment or acceptance** while a record is paused or its view divergent
//!   ([`Pause::blocks_automatic`]): today a resume's repeat of a dispatched payment, against
//!   its request record or the `payment_lists` record of its destination.
//! - **By hand only after the user's confirmation** ([`Pause::blocks_by_hand`]): paying a
//!   request, paying to an endpoint of a paused list, single-use or reusable, a user's
//!   `done`, and a payee's credit of a proof ([`crate::Refusal::Divergent`]). A payee app
//!   also credits no proof while another record of the same counterparty is paused: it can't
//!   tell what that record's upload holds (Paykit spec 8.1 step 4).
//! - **Writes over a divergent view** ([`Pause::write_waits`]): a stop (a cancellation or a
//!   rejection) is written; any other receive-step write is kept here (`p/dw/<record
//!   key>/<role>`) and written by a later write of the app's own once the user has
//!   confirmed the record, or given up 30 days after its acceptance with the host's notice,
//!   as a stored entry's message is; an own write of another kind, such as a payment's
//!   outcome or a receipt, waits for the confirmation.
//!
//! **Seen at every read.** A view is taken wherever the SDK reads a record to act on it, in
//! the receive step with the cursor move and in its calls; at every receive, each record the
//! client reported divergent (`RecordDivergent`) is marked, as the client saw it so even if
//! a later write in the same read cleared it; and at a load, every record held is looked at
//! once, so a flag a crash left before the receive's events were taken still pauses.
//!
//! **Restore.** A backup carries the marks and the kept writes. A restore installs every
//! mark paused, a confirmation given before it included: the restored app pauses each record
//! it had seen divergent until its user confirms again. A `missed` item changes no mark: it
//! loses inbox entries, not the records' views, which the listing after it reads again.
//!
//! **Two own apps** keep their own marks: one app's confirmation releases nothing in another,
//! which pauses until it has seen the record itself and its own user has confirmed.

use paykit_lib::collections::Collection;
use paykit_lib::derive::natural_key;
use paykit_lib::records::StoredMessage;
use paykit_lib::Body;
use pubky_switch_client::app::{
    AppEvent, LocalWrite, ReceiveContext, RecordTarget, SharedRecord, UserConfirmed,
};
use pubky_switch_core::enc::serde_b64;
use serde::{Deserialize, Serialize};

use crate::activity::{ActivityTarget, Role};
use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::receive::{Fail, Settlement};
use crate::records::{list_target, KeptMessage};
use crate::state::{self, key, StateKey, Tx};

/// A record the app has seen divergent, as the host shows it (Paykit spec 6): from
/// [`Paykit::divergent_records`], a receive's summary, or a refusal
/// ([`crate::Refusal::Divergent`]). [`Paykit::confirm_divergent`] takes its `record`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DivergentRecord {
    /// The record key.
    pub record: [u8; 32],
    /// Its collection: what the upload may hold a newer copy of, such as a cancellation in a
    /// request record or a newer list in a `payment_lists` record.
    pub collection: Collection,
    /// Paused: nothing is paid, accepted or credited on it automatically, and by hand only
    /// once the user confirms ([`Paykit::confirm_divergent`]).
    pub paused: bool,
    /// The latest view the app read shows it divergent now.
    pub divergent: bool,
}

/// The version of a record's write, as a mark keeps it: by generation, then `seq`, the
/// order a record's versions only grow in (Switch wire 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct SeenVersion {
    generation: u64,
    version: u64,
}

/// The pause mark of one record (`p/dv/<record key>`). The record and its collection repeat
/// in it, so a reader binds the value to its key, and the version of the write whose view
/// showed it divergent is the proof of what it points to: a record held at that key is of
/// that collection, at that version or a later one ([`check_points_to`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DivergentMark {
    #[serde(with = "serde_b64")]
    record: [u8; 32],
    collection: String,
    /// The write whose view showed the record divergent when it was paused.
    seen: SeenVersion,
    /// The write the user confirmed over, at or after `seen`; `None` while paused.
    confirmed: Option<SeenVersion>,
}

impl DivergentMark {
    fn paused(view: &View) -> Self {
        Self {
            record: view.record,
            collection: view.collection.name().to_owned(),
            seen: view.version,
            confirmed: None,
        }
    }

    /// Whether the record is paused: seen divergent and not confirmed since.
    pub(crate) fn is_paused(&self) -> bool {
        self.confirmed.is_none()
    }

    /// The mark's collection; a mark is read only bound, so it names one of Paykit's.
    fn collection(&self) -> Result<Collection> {
        Collection::from_name(&self.collection).ok_or_else(PaykitError::corrupt)
    }

    /// The mark at `key`, read whole and bound to it: the record its key names, of one of
    /// Paykit's collections. The one check for a restore and for every read at runtime; a
    /// value that fails it is a corrupt store, never an absent mark.
    pub(crate) fn read(key: &[u8], value: &[u8]) -> Result<Self> {
        let Some(StateKey::Divergent(record)) = StateKey::parse(key) else {
            return Err(PaykitError::corrupt());
        };
        let mark = state::decode::<Self>(value)?;
        // A confirmation before the write it pauses is a state no run leaves.
        if mark.record != record || mark.confirmed.is_some_and(|at| at < mark.seen) {
            return Err(PaykitError::corrupt());
        }
        mark.collection()?;
        Ok(mark)
    }

    /// The mark checked against the record held at its key, `view`, before it decides
    /// anything ([`check_points_to`]).
    fn checked(self, view: Option<&View>) -> Result<Self> {
        check_points_to(view, self.collection()?, self.seen)?;
        Ok(self)
    }
}

/// Whether a value that points to the record held as `view`, of `collection`, seen at the
/// write `seen`, points to it: the record held there is of that collection, at that write or
/// a later one, a record's versions only growing. A record not held proves nothing either
/// way: a pause stays. Anything else is a corrupt store, never a record to act on.
pub(crate) fn check_points_to(
    view: Option<&View>,
    collection: Collection,
    seen: SeenVersion,
) -> Result<()> {
    match view {
        Some(view) if view.collection != collection || view.version < seen => {
            Err(PaykitError::corrupt())
        }
        _ => Ok(()),
    }
}

/// A record's view, as the pause rules read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct View {
    record: [u8; 32],
    collection: Collection,
    divergent: bool,
    version: SeenVersion,
}

impl View {
    /// The view of `held`; `None` for a record of a collection that isn't Paykit's.
    pub(crate) fn of(held: &SharedRecord) -> Option<Self> {
        Some(Self {
            record: held.key,
            collection: Collection::from_name(&held.collection)?,
            divergent: held.divergent,
            version: SeenVersion {
                generation: held.version.generation,
                version: held.version.version,
            },
        })
    }
}

/// The mark after `view`, when it changes: a divergent view pauses a record not paused,
/// unless the user confirmed that same write. A view that isn't divergent changes nothing:
/// the pause stays until the user confirms (Paykit spec 6), and a confirmed mark goes only
/// at a receive's end ([`confirmed_mark_ends`]).
pub(crate) fn after_view(mark: Option<&DivergentMark>, view: &View) -> Option<DivergentMark> {
    if !view.divergent {
        return None;
    }
    match mark {
        Some(mark) if mark.is_paused() || mark.confirmed == Some(view.version) => None,
        _ => Some(DivergentMark::paused(view)),
    }
}

/// The mark after the client reported the record divergent (`RecordDivergent`), whatever
/// its view shows now: a view showed it so, a later write in the same read may have cleared
/// it since. A record with a mark already is judged by its view.
pub(crate) fn after_event(mark: Option<&DivergentMark>, view: &View) -> Option<DivergentMark> {
    match mark {
        None => Some(DivergentMark::paused(view)),
        Some(_) => after_view(mark, view),
    }
}

/// Whether a confirmed mark goes at a receive's end: the record isn't held, or its view
/// isn't divergent any more. A paused mark never goes but by the user's confirmation.
pub(crate) fn confirmed_mark_ends(mark: &DivergentMark, view: Option<&View>) -> bool {
    !mark.is_paused() && view.is_none_or(|view| !view.divergent)
}

/// The mark after the user's confirmation of the record held as `view`, `mark` before it:
/// confirmed at the version held, or none for a record no longer held.
pub(crate) fn after_confirmation(
    mark: Option<&DivergentMark>,
    view: Option<&View>,
) -> Option<DivergentMark> {
    let view = view?;
    Some(DivergentMark {
        record: view.record,
        collection: view.collection.name().to_owned(),
        seen: mark.map_or(view.version, |mark| mark.seen.min(view.version)),
        confirmed: Some(view.version),
    })
}

/// What a record's mark and view allow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pause {
    /// Seen divergent and not confirmed since.
    pub(crate) paused: bool,
    /// The latest view shows it divergent.
    pub(crate) divergent: bool,
}

impl Pause {
    /// The pause of a record with `mark` and `view`, `mark` as `view` leaves it.
    pub(crate) fn of(mark: Option<&DivergentMark>, view: Option<&View>) -> Self {
        let after = view.and_then(|view| after_view(mark, view));
        Self {
            paused: after
                .as_ref()
                .or(mark)
                .is_some_and(DivergentMark::is_paused),
            divergent: view.is_some_and(|view| view.divergent),
        }
    }

    /// No automatic payment or acceptance: while the view shows it divergent, the user's
    /// confirmation included, and while it is paused (Paykit spec 6).
    pub(crate) fn blocks_automatic(self) -> bool {
        self.paused || self.divergent
    }

    /// A payment, acceptance or credit by hand waits for the user's confirmation.
    pub(crate) fn blocks_by_hand(self) -> bool {
        self.paused
    }

    /// A write over it that adds no stop waits for the user's confirmation: while the view
    /// shows it divergent and the user hasn't confirmed it (Paykit spec 6).
    pub(crate) fn write_waits(self) -> bool {
        self.paused && self.divergent
    }
}

/// Whether `stored` adds a stop to the record it goes into: a cancellation or a rejection,
/// which go over a divergent view (Paykit spec 6).
pub(crate) fn adds_stop(stored: &StoredMessage) -> bool {
    matches!(
        stored.body(),
        Body::PaymentRequestCancellation(_) | Body::PaymentRequestRejection(_)
    )
}

/// The role a message kept over a divergent record holds there (`p/dw/<record>/<role>`):
/// one kept per record and role, as a record holds one message per role (Paykit spec 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WriteRole {
    /// A request, in the payer's request record.
    Request,
    /// A payer's acceptance, in the payee's request record.
    Response,
    /// A payer's proof, in the payee's record of the request or of the settlement.
    Proof,
    /// A payee's receipt, in the payer's record.
    Receipt,
    /// A Private Payment List, in its `payment_lists` record.
    List,
}

impl WriteRole {
    /// The role's name in a key.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Proof => "proof",
            Self::Receipt => "receipt",
            Self::List => "list",
        }
    }

    /// The role a key names.
    pub(crate) fn from_tag(tag: &str) -> Option<Self> {
        Some(match tag {
            "request" => Self::Request,
            "response" => Self::Response,
            "proof" => Self::Proof,
            "receipt" => Self::Receipt,
            "list" => Self::List,
            _ => return None,
        })
    }

    /// The role of a kept message; `None` for a stop, which is written over a divergent
    /// view and never kept, and for a kind no record of the SDK holds.
    pub(crate) fn of(stored: &StoredMessage) -> Option<Self> {
        Some(match stored.body() {
            Body::PaymentRequest(_) => Self::Request,
            Body::PaymentRequestAcceptance(_) => Self::Response,
            Body::PaymentProof(_) => Self::Proof,
            Body::PaymentReceipt(_) => Self::Receipt,
            Body::PrivatePaymentList(_) => Self::List,
            _ => return None,
        })
    }
}

/// A message kept to write over a divergent record after the user's confirmation
/// (`p/dw/<record key>/<role>`). The record and role repeat in it, so a reader binds the
/// value to its key; the record is the one its message names, derived again at every read;
/// and the version of the divergent write it was kept over is the proof that the record
/// held there later is that record, of its collection, at that write or a later one
/// ([`check_points_to`]).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DivergentWrite {
    #[serde(with = "serde_b64")]
    record: [u8; 32],
    role: WriteRole,
    seen: SeenVersion,
    message: KeptMessage,
}

/// A kept write read bound ([`kept_write_at`]): its message, where it goes, and the write
/// it was kept over.
pub(crate) struct KeptWrite {
    pub(crate) message: StoredMessage,
    pub(crate) target: WriteTarget,
    pub(crate) seen: SeenVersion,
}

/// Where a kept message goes, by its kind, as at its first read.
pub(crate) enum WriteTarget {
    /// An activity record, in `role`; `creates` when the message may open the record.
    Activity {
        target: ActivityTarget,
        role: Role,
        creates: bool,
    },
    /// A `payment_lists` record.
    List(RecordTarget),
}

impl WriteTarget {
    fn record_target(&self) -> RecordTarget {
        match self {
            Self::Activity { target, .. } => target.record_target(),
            Self::List(target) => target.clone(),
        }
    }

    /// The collection of the record it goes into.
    fn collection(&self) -> Option<Collection> {
        Collection::from_name(&self.record_target().collection.name)
    }
}

/// The record `stored`, a kept message, goes into, as the receive step places it: a request
/// into the payer's request record, an acceptance or a proof into the payee's record of the
/// request it names, a proof of a payment without a request into the payee's record of its
/// settlement, a receipt into the payer's record, a list into its `payment_lists` record.
pub(crate) fn write_target(
    stored: &StoredMessage,
    own: &[u8; 32],
    settlement: Settlement<'_>,
) -> Option<WriteTarget> {
    let sender = stored.sender();
    let first = stored.message().refs.first().copied();
    let activity = |target, role, creates| {
        Some(WriteTarget::Activity {
            target,
            role,
            creates,
        })
    };
    match stored.body() {
        Body::PaymentRequest(_) => activity(
            ActivityTarget::payer(natural_key::request_record(&sender, &stored.hash())),
            Role::Request,
            true,
        ),
        Body::PaymentProof(proof) if first.is_none() => {
            let settlement = settlement(&proof.proof)?;
            activity(
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
        Body::PaymentRequestAcceptance(_) | Body::PaymentProof(_) => {
            let role = Role::of(stored.body())?;
            activity(
                ActivityTarget::payee(
                    natural_key::request_record(own, &first?),
                    stored.body().label(),
                ),
                role,
                false,
            )
        }
        Body::PaymentReceipt(receipt) => activity(
            crate::receive::receipt_target(stored, own, &receipt.attempt_id),
            Role::Receipt,
            false,
        ),
        Body::PrivatePaymentList(_) => Some(WriteTarget::List(list_target(
            &sender,
            &stored.message().from.app_id,
        ))),
        _ => None,
    }
}

/// The message kept at `key` (`p/dw/<record>/<role>`), read whole and bound to it: a message
/// that verifies for `own`, accepted by its inbox, of the role its key names, whose record,
/// as `record_key` derives it, is the one its key names. The one check for a restore and for
/// every read at runtime; one that fails it is a corrupt store.
pub(crate) fn kept_write_at(
    key: &[u8],
    value: &[u8],
    own: &[u8; 32],
    record_key: &impl Fn(&RecordTarget) -> std::result::Result<[u8; 32], String>,
    settlement: Settlement<'_>,
) -> std::result::Result<KeptWrite, String> {
    let bad =
        || String::from("Paykit spec 6: a message kept over a divergent record doesn't verify");
    let Some(StateKey::DivergentWrite { record, role }) = StateKey::parse(key) else {
        return Err(bad());
    };
    let kept = state::decode::<DivergentWrite>(value).map_err(|_| bad())?;
    let stored = kept.message.stored(own).map_err(|_| bad())?;
    if kept.record != record
        || kept.role != role
        || WriteRole::of(&stored) != Some(role)
        || stored.accepted_at().is_none()
    {
        return Err(bad());
    }
    let target = write_target(&stored, own, settlement).ok_or_else(bad)?;
    if target.collection().is_none() || record_key(&target.record_target())? != record {
        return Err(bad());
    }
    Ok(KeptWrite {
        message: stored,
        target,
        seen: kept.seen,
    })
}

/// The mark a restore installs for the mark at `key`: read bound as at runtime, and paused,
/// a confirmation given before the restore included (module docs, "Restore").
pub(crate) fn restored_mark(key: &[u8], value: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let bad =
        || String::from("Paykit spec 6: a divergent record's mark in the backup doesn't read");
    let mut mark = DivergentMark::read(key, value).map_err(|_| bad())?;
    mark.confirmed = None;
    state::encode(&mark).map_err(|_| bad())
}

/// Whether a kept list replaces `held`, the one kept for its record: only a newer version,
/// a list being latest state (Paykit spec 8.4). A message of any other role kept already
/// stays: the record holds one per role, and a later one would only be reflected.
fn replaces(held: &StoredMessage, new: &StoredMessage) -> bool {
    matches!(WriteRole::of(new), Some(WriteRole::List))
        && crate::receive::list_version(new) > crate::receive::list_version(held)
}

// ---------------------------------------------------------------------------------
// The receive step

/// The mark of the record held as `view` in the receive step, read bound and checked
/// against it ([`DivergentMark::read`], [`check_points_to`]).
fn step_mark(
    cx: &ReceiveContext<'_>,
    view: &View,
) -> std::result::Result<Option<DivergentMark>, Fail> {
    let mark_key = key::divergent(&view.record);
    match cx.local_get(mark_key.as_bytes())? {
        None => Ok(None),
        Some(value) => DivergentMark::read(mark_key.as_bytes(), &value)
            .and_then(|mark| mark.checked(Some(view)))
            .map(Some)
            .map_err(|_| Fail::corrupt()),
    }
}

/// In the receive step: take the view of `held`, keeping the mark it sets with the cursor
/// move, and tell what it allows.
pub(crate) fn step_pause(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
) -> std::result::Result<Pause, Fail> {
    let Some(view) = View::of(held) else {
        return Ok(Pause::default());
    };
    let mark = step_mark(cx, &view)?;
    if let Some(after) = after_view(mark.as_ref(), &view) {
        let write =
            state::local_put(&key::divergent(&held.key), &after).map_err(|_| Fail::corrupt())?;
        crate::receive::keep_local(cx, write);
    }
    Ok(Pause::of(mark.as_ref(), Some(&view)))
}

/// In the receive step: keep `stored`, a message whose write over `held`, a divergent
/// record, waits for the user's confirmation, with the cursor move (Paykit spec 6, 8.1
/// "Stored entries"). One kept already for the record and role stays, but for a newer list.
pub(crate) fn step_keep(
    cx: &mut ReceiveContext<'_>,
    held: &SharedRecord,
    stored: &StoredMessage,
) -> std::result::Result<(), Fail> {
    let (Some(role), Some(view)) = (WriteRole::of(stored), View::of(held)) else {
        return Ok(());
    };
    let record = held.key;
    let kept_key = key::divergent_write(&record, role);
    if let Some(value) = cx.local_get(kept_key.as_bytes())? {
        // Only a list is compared, and a list's record needs no settlement.
        if role != WriteRole::List {
            return Ok(());
        }
        let held = kept_write_at(
            kept_key.as_bytes(),
            &value,
            &cx.identity(),
            &|target| cx.record_key(target).map_err(|error| error.to_string()),
            &|_| None,
        )
        .map_err(|_| Fail::corrupt())?;
        check_points_to(
            Some(&view),
            held.target.collection().ok_or_else(Fail::corrupt)?,
            held.seen,
        )
        .map_err(|_| Fail::corrupt())?;
        if !replaces(&held.message, stored) {
            return Ok(());
        }
    }
    let kept = DivergentWrite {
        record,
        role,
        seen: view.version,
        message: KeptMessage::of(stored),
    };
    let write = state::local_put(&kept_key, &kept).map_err(|_| Fail::corrupt())?;
    crate::receive::keep_local(cx, write);
    Ok(())
}

impl Paykit {
    /// The mark at `record`, read bound ([`DivergentMark::read`]) and checked against the
    /// record held there, `view` ([`check_points_to`]), before anything is decided on it.
    async fn divergent_mark(
        &self,
        record: &[u8; 32],
        view: Option<&View>,
    ) -> Result<Option<DivergentMark>> {
        let mark_key = key::divergent(record);
        match self.app.protocol_get(mark_key.as_bytes()).await? {
            None => Ok(None),
            Some(value) => DivergentMark::read(mark_key.as_bytes(), &value)?
                .checked(view)
                .map(Some),
        }
    }

    /// The record held at `record`, as a view, and its mark checked against it.
    async fn mark_and_view(
        &self,
        record: &[u8; 32],
    ) -> Result<(Option<DivergentMark>, Option<View>)> {
        let held = self.app.shared_record(record).await?;
        let view = held.as_ref().and_then(View::of);
        let mark = self.divergent_mark(record, view.as_ref()).await?;
        Ok((mark, view))
    }

    /// Every mark, each read bound to its key.
    async fn divergent_marks(&self) -> Result<Vec<DivergentMark>> {
        let mut out = Vec::new();
        for (mark_key, value) in self.app.protocol_scan(key::DIVERGENT.as_bytes()).await? {
            out.push(DivergentMark::read(&mark_key, &value)?);
        }
        Ok(out)
    }

    /// Put or keep the mark `after` of `record`.
    async fn put_mark(&mut self, record: &[u8; 32], after: &DivergentMark) -> Result<()> {
        self.local_put(&key::divergent(record), after).await
    }

    /// Take the view of `held`, keeping the mark it sets, and tell what it allows.
    pub(crate) async fn pause_of(&mut self, held: &SharedRecord) -> Result<Pause> {
        let Some(view) = View::of(held) else {
            return Ok(Pause::default());
        };
        let mark = self.divergent_mark(&held.key, Some(&view)).await?;
        if let Some(after) = after_view(mark.as_ref(), &view) {
            self.put_mark(&held.key, &after).await?;
        }
        Ok(Pause::of(mark.as_ref(), Some(&view)))
    }

    /// [`Paykit::pause_of`] for the record at `record`: a mark alone for one not held.
    pub(crate) async fn pause_at(&mut self, record: &[u8; 32]) -> Result<Pause> {
        match self.app.shared_record(record).await? {
            Some(held) => self.pause_of(&held).await,
            None => Ok(Pause::of(
                self.divergent_mark(record, None).await?.as_ref(),
                None,
            )),
        }
    }

    /// What the record at `record` allows, as its mark and view stand, writing nothing: for
    /// the views.
    pub(crate) async fn pause_seen(&self, record: &[u8; 32]) -> Result<Pause> {
        let (mark, view) = self.mark_and_view(record).await?;
        Ok(Pause::of(mark.as_ref(), view.as_ref()))
    }

    /// The host's view of the record at `record`, `pause` as it allows.
    async fn shown(&self, record: [u8; 32], pause: Pause) -> Result<DivergentRecord> {
        let (mark, view) = self.mark_and_view(&record).await?;
        let collection = match (view, mark) {
            (Some(view), _) => view.collection,
            (None, Some(mark)) => mark.collection()?,
            (None, None) => return Err(PaykitError::corrupt()),
        };
        Ok(DivergentRecord {
            record,
            collection,
            paused: pause.paused,
            divergent: pause.divergent,
        })
    }

    /// Refused for an action by hand while any of `records` is paused, naming those, for
    /// the user to confirm ([`Refusal::Divergent`]). Each record's view is taken first.
    pub(crate) async fn refuse_while_paused(&mut self, records: &[[u8; 32]]) -> Result<()> {
        let mut paused = Vec::new();
        for record in records {
            let pause = self.pause_at(record).await?;
            if pause.blocks_by_hand() {
                paused.push(self.shown(*record, pause).await?);
            }
        }
        if paused.is_empty() {
            return Ok(());
        }
        Err(Refusal::Divergent { records: paused }.into())
    }

    /// The refusal of an own write over the divergent record `record`, naming it.
    pub(crate) async fn divergent_refusal(&mut self, record: [u8; 32]) -> Result<PaykitError> {
        let pause = self.pause_at(&record).await?;
        let shown = self.shown(record, pause).await?;
        Ok(Refusal::Divergent {
            records: vec![shown],
        }
        .into())
    }

    /// Refused while an own write of another kind than a stop over the record at `record`
    /// waits for the user's confirmation ([`Pause::write_waits`]).
    pub(crate) async fn refuse_while_write_waits(&mut self, record: &[u8; 32]) -> Result<()> {
        let pause = self.pause_at(record).await?;
        if pause.write_waits() {
            let shown = self.shown(*record, pause).await?;
            return Err(Refusal::Divergent {
                records: vec![shown],
            }
            .into());
        }
        Ok(())
    }

    /// Whether a write of `stored` over the record at `record` waits for the user's
    /// confirmation: it adds no stop and the record's view is divergent and not confirmed.
    pub(crate) async fn divergent_write_waits(
        &mut self,
        record: &[u8; 32],
        stored: &StoredMessage,
    ) -> Result<bool> {
        if adds_stop(stored) {
            return Ok(false);
        }
        Ok(self.pause_at(record).await?.write_waits())
    }

    /// A payee app's credit of a proof in `record`, from `counterparty`, waits for the user
    /// while `record` or any other record of the same counterparty it holds is paused
    /// (Paykit spec 8.1 step 4): the upload may hold a receipt of the same settlement, so
    /// the app can't tell it isn't credited already. Refused naming them.
    pub(crate) async fn refuse_credit_while_paused(
        &mut self,
        record: &[u8; 32],
        counterparty: &[u8; 32],
    ) -> Result<()> {
        let mut records = vec![*record];
        for held in self
            .app
            .records_of(Collection::PayeeActivity.name())
            .await?
        {
            if held.key == *record {
                continue;
            }
            // A record that doesn't bind isn't acted on (Paykit wire 5.3); a failure to read
            // one is the call's error.
            match self.payee_record(&held.key).await {
                Ok((_, other)) if other.counterparty == *counterparty => records.push(held.key),
                Ok(_) | Err(PaykitError::Refused(Refusal::NotFound) | PaykitError::Lib(_)) => {}
                Err(error) => return Err(error),
            }
        }
        self.refuse_while_paused(&records).await
    }

    /// The marks every record the client reported divergent (`RecordDivergent` in `events`)
    /// sets: a view showed it so, whatever its view shows now.
    pub(crate) async fn take_divergent_events(&mut self, events: &[AppEvent]) -> Result<()> {
        for event in events {
            let AppEvent::RecordDivergent { key: record, .. } = event else {
                continue;
            };
            let Some(held) = self.app.shared_record(record).await? else {
                continue;
            };
            let Some(view) = View::of(&held) else {
                continue;
            };
            let mark = self.divergent_mark(record, Some(&view)).await?;
            if let Some(after) = after_event(mark.as_ref(), &view) {
                self.put_mark(record, &after).await?;
            }
        }
        Ok(())
    }

    /// Take the view of every record of Paykit's collections the app holds, once at a load:
    /// a flag a crash left before a receive's events were taken still pauses its record.
    pub(crate) async fn sweep_divergent(&mut self) -> Result<()> {
        for collection in crate::hold::RecordSide::All.collections() {
            for held in self.app.records_of(&collection.name).await? {
                if held.divergent {
                    self.pause_of(&held).await?;
                }
            }
        }
        Ok(())
    }

    /// At a receive's end: the confirmed marks whose record's view isn't divergent any more,
    /// or that the app holds no more, go ([`confirmed_mark_ends`]).
    pub(crate) async fn end_confirmed_marks(&mut self) -> Result<()> {
        let mut tx = Tx::default();
        for mark in self.divergent_marks().await? {
            let held = self.app.shared_record(&mark.record).await?;
            let view = held.as_ref().and_then(View::of);
            let mark = mark.checked(view.as_ref())?;
            if confirmed_mark_ends(&mark, view.as_ref()) {
                tx.delete(&key::divergent(&mark.record));
            }
        }
        self.local_commit(tx).await
    }

    /// The records paused now, by key.
    pub(crate) async fn paused_records(&self) -> Result<std::collections::BTreeSet<[u8; 32]>> {
        Ok(self
            .divergent_marks()
            .await?
            .into_iter()
            .filter(DivergentMark::is_paused)
            .map(|mark| mark.record)
            .collect())
    }

    /// The records paused now that weren't in `before`, for a receive's summary.
    pub(crate) async fn newly_paused(
        &self,
        before: &std::collections::BTreeSet<[u8; 32]>,
    ) -> Result<Vec<DivergentRecord>> {
        let mut out = Vec::new();
        for record in self.paused_records().await? {
            if !before.contains(&record) {
                let pause = self.pause_seen(&record).await?;
                out.push(self.shown(record, pause).await?);
            }
        }
        Ok(out)
    }

    /// Write every message kept over a divergent record whose record the user has
    /// confirmed, each as a later write of the app's own with its kept copy dropped in the
    /// batch, through the stored entries' later write ([`Paykit::place_note`]) and its one
    /// rule ([`crate::stored::later_write`]); a list goes to its held list (`lh/`), which
    /// the receive places next. One whose record is still paused waits, and is given up
    /// [`crate::KEPT_MESSAGE_MS`] after its acceptance, on the highest service time
    /// verified, with the host's notice ([`crate::NoticeKind::MessageGivenUp`]) (Paykit spec
    /// 8.1, "Stored entries"). An error is returned, and the message stays kept.
    pub(crate) async fn place_divergent_writes(&mut self) -> Result<()> {
        let own = self.identity();
        for (kept_key, value) in self
            .app
            .protocol_scan(key::DIVERGENT_WRITES.as_bytes())
            .await?
        {
            let kept = kept_write_at(
                &kept_key,
                &value,
                &own,
                &|target| {
                    self.app
                        .record_key(target)
                        .map_err(|error| error.to_string())
                },
                &|proof| self.rail.settlement_id(proof),
            )
            .map_err(|_| PaykitError::corrupt())?;
            let Some(StateKey::DivergentWrite { record, .. }) = StateKey::parse(&kept_key) else {
                return Err(PaykitError::corrupt());
            };
            // What it points to is checked before anything is written: the record held at its
            // key is of its target's collection, at the write it was kept over or a later one.
            let (_, view) = self.mark_and_view(&record).await?;
            let collection = kept.target.collection().ok_or_else(PaykitError::corrupt)?;
            check_points_to(view.as_ref(), collection, kept.seen)?;
            let kept_key = String::from_utf8(kept_key).map_err(|_| PaykitError::corrupt())?;
            let stored = kept.message;
            let accepted_at = stored.accepted_at().ok_or_else(PaykitError::corrupt)?;
            let placed = if self.pause_at(&record).await?.blocks_by_hand() {
                crate::stored::Placed::Not
            } else {
                let mut tx = Tx::default();
                tx.delete(&kept_key);
                match kept.target {
                    WriteTarget::List(_) => {
                        self.hold_kept_list(record, &stored, tx).await?;
                        crate::stored::Placed::Done
                    }
                    WriteTarget::Activity {
                        target,
                        role,
                        creates,
                    } => {
                        self.place_note(&target, role, creates, stored.clone(), tx)
                            .await?
                    }
                }
            };
            if crate::stored::later_write(placed, accepted_at, self.service_time())
                == crate::stored::LaterWrite::GiveUp
            {
                self.give_up_divergent(&kept_key, &stored).await?;
            }
        }
        Ok(())
    }

    /// Hold `list`, kept over its divergent record `record` and confirmed since, as a list
    /// held over its record (`lh/`), unless a list as new is held, with `tx`, in one commit:
    /// [`Paykit::place_held_lists`] writes it, on the record as it reads then.
    async fn hold_kept_list(
        &mut self,
        record: [u8; 32],
        list: &StoredMessage,
        mut tx: Tx,
    ) -> Result<()> {
        let held_key = key::held_list(&record);
        // A list held already is read bound to its key: a list of the payee app whose
        // `payment_lists` record the key names.
        let newer = match self.app.protocol_get(held_key.as_bytes()).await? {
            None => true,
            Some(value) => {
                let held = state::decode::<KeptMessage>(&value)?.stored(&self.identity())?;
                let at = self
                    .app
                    .record_key(&list_target(&held.sender(), &held.message().from.app_id))?;
                if !matches!(held.body(), Body::PrivatePaymentList(_)) || at != record {
                    return Err(PaykitError::corrupt());
                }
                crate::receive::list_version(&held) < crate::receive::list_version(list)
            }
        };
        if newer {
            tx.put(&held_key, &KeptMessage::of(list))?;
        }
        self.local_commit(tx).await
    }

    /// Give up `stored`, kept over a divergent record at `kept_key`: the kept copy goes and
    /// the host is told, in one commit; the notice is raised once.
    async fn give_up_divergent(&mut self, kept_key: &str, stored: &StoredMessage) -> Result<()> {
        let mut tx = Tx::default();
        tx.delete(kept_key);
        let notice = key::message_notice(&stored.sender(), &stored.message().msg_id);
        if self.kept_notice(&notice).await?.is_none() {
            let at = self.service_time().unwrap_or(0);
            tx.put(
                &notice,
                &crate::stored::given_up_notice(
                    stored,
                    at,
                    crate::notices::GivenUpOrigin::Divergent,
                ),
            )?;
        }
        self.local_commit(tx).await
    }

    /// The records the app has seen divergent and its user hasn't confirmed, and those whose
    /// latest view shows them divergent (Paykit spec 6), by record key: for the host to show,
    /// each with what the user is asked to see, such as a request the payee may have
    /// cancelled or a list that may be stale, and to confirm
    /// ([`Paykit::confirm_divergent`]). While one is paused, nothing is paid, accepted or
    /// credited on it automatically, and by hand only once the user confirms it; a write over
    /// it that adds no stop waits too.
    ///
    /// # Errors
    ///
    /// A store failure, or a mark that doesn't read.
    pub async fn divergent_records(&self) -> Result<Vec<DivergentRecord>> {
        let mut keys = std::collections::BTreeSet::new();
        for mark in self.divergent_marks().await? {
            keys.insert(mark.record);
        }
        for collection in crate::hold::RecordSide::All.collections() {
            for held in self.app.records_of(&collection.name).await? {
                if held.divergent {
                    keys.insert(held.key);
                }
            }
        }
        let mut out = Vec::new();
        for record in keys {
            let pause = self.pause_seen(&record).await?;
            if pause.paused || pause.divergent {
                out.push(self.shown(record, pause).await?);
            }
        }
        Ok(out)
    }

    /// The user has seen what the upload of the divergent record `record` may hold, such as
    /// a cancellation of a request, a receipt, a newer list or entries of a ledger, and lets
    /// its payments and writes go on (Paykit spec 6): the record's pause ends in this app.
    /// Given while its view shows it divergent, the confirmation holds for that write: a
    /// later divergent view of another write pauses it again. The messages kept over it are
    /// written at the next receive. No automatic payment or acceptance is made on it while
    /// its view still shows it divergent. Another own app keeps its own pause until its own
    /// user confirms. A restore from a backup pauses the record again.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotFound`] for a record the app neither has seen divergent nor holds
    /// divergent now; [`Refusal::CertificateRefused`] while the app's certificate fails
    /// Paykit's checks; a store failure.
    pub async fn confirm_divergent(
        &mut self,
        record: &[u8; 32],
        _confirmed: UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        let (mark, view) = self.mark_and_view(record).await?;
        if mark.is_none() && !view.is_some_and(|view| view.divergent) {
            return Err(Refusal::NotFound.into());
        }
        match after_confirmation(mark.as_ref(), view.as_ref()) {
            Some(after) => self.put_mark(record, &after).await,
            None => self.local_delete(&key::divergent(record)).await,
        }
    }
}

/// The local write of a mark, for a restore's layer.
pub(crate) fn restored_mark_write(
    key: Vec<u8>,
    value: &[u8],
) -> std::result::Result<LocalWrite, String> {
    Ok(LocalWrite::Put {
        value: restored_mark(&key, value)?,
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: [u8; 32] = [7; 32];

    fn view(divergent: bool, version: u64) -> View {
        View {
            record: RECORD,
            collection: Collection::PayerActivity,
            divergent,
            version: SeenVersion {
                generation: 1,
                version,
            },
        }
    }

    fn confirmed(version: u64) -> DivergentMark {
        after_confirmation(None, Some(&view(true, version))).unwrap()
    }

    #[test]
    fn test_divergent_view_pauses_a_record_not_paused() {
        let after = after_view(None, &view(true, 3)).unwrap();
        assert!(after.is_paused());
        let pause = Pause::of(None, Some(&view(true, 3)));
        assert!(pause.blocks_automatic() && pause.blocks_by_hand() && pause.write_waits());
    }

    #[test]
    fn test_divergent_clean_view_neither_pauses_nor_ends_a_pause() {
        assert!(after_view(None, &view(false, 3)).is_none());
        let paused = DivergentMark::paused(&view(true, 3));
        assert!(after_view(Some(&paused), &view(false, 4)).is_none());
        let pause = Pause::of(Some(&paused), Some(&view(false, 4)));
        // Kept after a newer write cleared the view: by hand waits, a write doesn't.
        assert!(pause.blocks_by_hand() && pause.blocks_automatic() && !pause.write_waits());
        assert!(!confirmed_mark_ends(&paused, Some(&view(false, 4))));
        assert!(!confirmed_mark_ends(&paused, None));
    }

    #[test]
    fn test_divergent_confirmation_holds_for_its_write_only() {
        let mark = confirmed(3);
        assert!(after_view(Some(&mark), &view(true, 3)).is_none());
        let pause = Pause::of(Some(&mark), Some(&view(true, 3)));
        assert!(!pause.blocks_by_hand() && !pause.write_waits());
        // Still no automatic payment while the view shows it divergent.
        assert!(pause.blocks_automatic());
        // Another write seen divergent pauses it again.
        let again = after_view(Some(&mark), &view(true, 5)).unwrap();
        assert!(again.is_paused());
        assert!(Pause::of(Some(&mark), Some(&view(true, 5))).blocks_by_hand());
    }

    #[test]
    fn test_divergent_confirmed_mark_ends_once_the_view_clears() {
        let mark = confirmed(3);
        assert!(!confirmed_mark_ends(&mark, Some(&view(true, 3))));
        assert!(confirmed_mark_ends(&mark, Some(&view(false, 4))));
        assert!(confirmed_mark_ends(&mark, None));
        assert!(!Pause::of(Some(&mark), Some(&view(false, 4))).blocks_automatic());
    }

    #[test]
    fn test_divergent_event_pauses_a_record_without_a_mark_whatever_its_view() {
        assert!(after_event(None, &view(false, 4)).unwrap().is_paused());
        // A confirmed record's earlier event asks nothing again.
        assert!(after_event(Some(&confirmed(3)), &view(true, 3)).is_none());
        assert!(after_event(Some(&confirmed(3)), &view(false, 4)).is_none());
    }

    #[test]
    fn test_divergent_confirmation_of_a_record_not_held_drops_the_mark() {
        assert!(after_confirmation(Some(&DivergentMark::paused(&view(true, 3))), None).is_none());
        // Confirmed at the write held, keeping the write it was first seen divergent at.
        let mark = after_confirmation(
            Some(&DivergentMark::paused(&view(true, 3))),
            Some(&view(false, 4)),
        )
        .unwrap();
        assert_eq!(mark.seen, view(true, 3).version);
        assert_eq!(mark.confirmed, Some(view(false, 4).version));
    }

    #[test]
    fn test_divergent_mark_read_bound_to_its_key() {
        let mark = DivergentMark::paused(&view(true, 3));
        let value = state::encode(&mark).unwrap();
        let at = key::divergent(&RECORD);
        assert_eq!(DivergentMark::read(at.as_bytes(), &value).unwrap(), mark);
        // At another record's key.
        let other = key::divergent(&[8; 32]);
        assert!(DivergentMark::read(other.as_bytes(), &value).is_err());
        // Of a collection that isn't Paykit's.
        let foreign = DivergentMark {
            collection: "notes".into(),
            ..mark.clone()
        };
        let value = state::encode(&foreign).unwrap();
        assert!(DivergentMark::read(at.as_bytes(), &value).is_err());
        // At a key of another family.
        assert!(DivergentMark::read(key::peer(&RECORD).as_bytes(), &value).is_err());
        assert!(DivergentMark::read(at.as_bytes(), b"not json").is_err());
    }

    #[test]
    fn test_divergent_restore_pauses_a_confirmed_mark() {
        let at = key::divergent(&RECORD);
        let value = state::encode(&confirmed(3)).unwrap();
        let restored = restored_mark(at.as_bytes(), &value).unwrap();
        let mark = DivergentMark::read(at.as_bytes(), &restored).unwrap();
        assert!(mark.is_paused());
        let paused = state::encode(&DivergentMark::paused(&view(true, 3))).unwrap();
        let restored = restored_mark(at.as_bytes(), &paused).unwrap();
        assert!(DivergentMark::read(at.as_bytes(), &restored)
            .unwrap()
            .is_paused());
    }

    #[test]
    fn test_divergent_restore_refuses_a_mark_at_another_key() {
        let value = state::encode(&DivergentMark::paused(&view(true, 3))).unwrap();
        assert!(restored_mark(key::divergent(&[9; 32]).as_bytes(), &value).is_err());
        let foreign = DivergentMark {
            collection: "notes".into(),
            ..DivergentMark::paused(&view(true, 3))
        };
        let value = state::encode(&foreign).unwrap();
        assert!(restored_mark(key::divergent(&RECORD).as_bytes(), &value).is_err());
    }

    #[test]
    fn test_divergent_mark_points_to_its_record_by_collection_and_version() {
        let mark = DivergentMark::paused(&view(true, 3));
        assert!(mark.clone().checked(Some(&view(true, 3))).is_ok());
        assert!(mark.clone().checked(Some(&view(false, 9))).is_ok());
        // Not held proves nothing: the pause stays.
        assert!(mark.clone().checked(None).is_ok());
        // A record held at an earlier write than the one seen divergent isn't it.
        assert!(mark.clone().checked(Some(&view(true, 2))).is_err());
        // Nor is one of another collection.
        let lists = View {
            collection: Collection::PaymentLists,
            ..view(true, 3)
        };
        assert!(mark.checked(Some(&lists)).is_err());
    }

    #[test]
    fn test_divergent_mark_confirmed_before_its_write_is_corrupt() {
        let mark = DivergentMark {
            confirmed: Some(view(true, 2).version),
            ..DivergentMark::paused(&view(true, 3))
        };
        let value = state::encode(&mark).unwrap();
        let at = key::divergent(&RECORD);
        assert!(DivergentMark::read(at.as_bytes(), &value).is_err());
        assert!(restored_mark(at.as_bytes(), &value).is_err());
    }

    #[test]
    fn test_divergent_write_role_never_keeps_a_stop() {
        for (kind, role) in [
            ("paykit.payment_request_cancellation", None),
            ("paykit.payment_request_rejection", None),
        ] {
            let alice = crate::testkit::Party::new("alice", "wallet", &["pay", "contacts"]);
            let bob = crate::testkit::Party::new("bob", "shop", &["request"]);
            let stop = alice.message(
                &bob,
                kind,
                vec![[1; 32]],
                crate::testkit::object(serde_json::json!({ "reason": null })),
            );
            assert_eq!(WriteRole::of(&stop), role, "{kind}");
            assert!(adds_stop(&stop), "{kind}");
        }
        for role in [
            WriteRole::Request,
            WriteRole::Response,
            WriteRole::Proof,
            WriteRole::Receipt,
            WriteRole::List,
        ] {
            assert_eq!(WriteRole::from_tag(role.tag()), Some(role));
        }
        assert_eq!(WriteRole::from_tag("cancellation"), None);
    }
}

#[cfg(test)]
mod reader_tests {
    /// Every Rust source file of the crate, by its path under `src/`, with its text, cut at
    /// its first test module.
    fn sources() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in std::fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let name = path.strip_prefix(dir).unwrap().display().to_string();
                    let mut text = std::fs::read_to_string(&path).unwrap();
                    if let Some(cut) = text.find("#[cfg(test)]") {
                        text.truncate(cut);
                    }
                    out.push((name, text));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn test_divergent_state_read_only_through_its_readers() {
        // The marks and the kept writes are read only through `DivergentMark::read` and
        // `kept_write_at`, which bind each value to its key: a raw read of their keys
        // anywhere else, or a raw typed read of their values, fails here.
        for (name, text) in sources() {
            for needle in [
                "key::divergent(",
                "key::DIVERGENT",
                "key::divergent_write(",
                "local_get::<DivergentMark",
                "local_get::<DivergentWrite",
                "local_scan::<DivergentMark",
                "local_scan::<DivergentWrite",
                "decode::<DivergentMark",
                "decode::<DivergentWrite",
            ] {
                let uses = text
                    .lines()
                    .filter(|line| !line.trim_start().starts_with("//"))
                    .filter(|line| line.contains(needle))
                    .count();
                assert!(
                    uses == 0 || name == "divergent.rs",
                    "{name} reads divergent state outside its readers: {needle}"
                );
            }
        }
    }
}
