//! The block floor (Paykit wire 5.6; Paykit spec 8.1, "Blocked counterparties").
//!
//! A message dropped because the user blocked its sender is asked for again once the sender is
//! unblocked (`marks.rs`). A request that comes back that way is routed to every current payer app
//! of the identity, not only to the app whose mark asked: a newly added app, or one that holds no
//! mark of the sender, can receive it first. So the time before which a one-time request is paid
//! only by hand is identity-shared: the `block_floor` record, one per identity, under `paykit_pay`,
//! which every app that pays holds (Paykit spec 3.1: `paykit_allowance_execute` requires
//! `paykit_pay`). Its floor only rises: a writer raises it, a reader refuses a later write of a
//! lower one, and it never expires. A request created now is dated above every floor, so nothing
//! fresh is held by it.
//!
//! A payer raises the shared floor before it removes a sender from the deny list.
//! When its list read shows another app's unblock, it records the change with the
//! shared raise. A restore that widens the blocked window applies its marks with the floor.
//!
//! - **Applied by every payer app at record creation:** a one-time request whose signed
//!   `created_at` is earlier than the floor plus the clock tolerance opens its record with
//!   `manual_only`, in the receive step and in a later write. The floor an app holds at an
//!   entry may lag a raise made after the one before that entry: a write superseded since is
//!   shown as its stub (Switch spec 8.3), and its value comes with the later one.
//! - **Applied before every automatic attempt:** no app pays a one-time request dated before
//!   the floor plus the tolerance automatically, whatever its record holds. Read after the
//!   app's read of the feed, the floor is at least every raise applied before an entry the
//!   app took, so this one holds whatever the order of the apps' reads. And immediately
//!   before each automatic dispatch the record is read again from the switch, with the deny
//!   list and the request record, twice, and the dispatch holds unless the second reads show
//!   the very writes the first did (`Paykit::automatic_dispatch_held`): a raise the app's
//!   copy doesn't hold yet, applied before the attempt's commit, after it, or between those
//!   reads, still holds the dispatch. A first dispatch journals `Paying` after one such
//!   check, and makes it once more after that commit, with no await between the second
//!   reads and the payment method's call (`Paykit::dispatch`). A raise after the second
//!   read of the floor is too late for that payment.
//! - **Unknown holds:** while the floor can't be read (the payer's records not listed yet, a
//!   value sealed under a key not held, a value that doesn't read or an unauthenticated delete
//!   with none seen before), every one-time request counts as dated before it.
//! - **Permanent:** the record sits under the permanent policy (Paykit wire 5.1), so no app
//!   deletes it and the service refuses a delete in every generation. A delete the app can
//!   authenticate is the client's refusal or rollback alarm (Switch spec 10.1, 10.3.1); one it
//!   can't is the service's word, ignored, keeping the floor seen.
//! - **Divergence holds for good:** once a view the app reads shows the record divergent, or
//!   the client reports it so (`AppEvent::RecordDivergent`), the app keeps a durable hold
//!   ([`FloorKept`], `fk`): the hidden upload may hold a higher floor no current Switch API
//!   shows. Under it every one-time request counts as dated before the floor, no automatic
//!   one-time dispatch or repeat goes, and the app writes no `block_floor` create or
//!   successor (Paykit wire 5.6, Divergent floor). Nothing lifts it, the user's confirmation
//!   included, and it is kept across restart, restore, scope changes and later losses. While
//!   the shared post-loss hold (`crate::loss`) stands too, an unblock or a widening goes
//!   without the raise it would need (Paykit wire 5.6, Unblocking under the divergence hold).
//! - **The highest floor seen** is kept in the same record, so an in-place restore from an
//!   earlier backup, or a later lower write, never lowers it.

use paykit_lib::derive::natural_key;
use paykit_lib::records::{BlockFloorRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{
    AppEvent, KeptReason, OpBatch, RecordContent, RecordRead, RecordTarget, SharedRecord,
};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;

use crate::activity;
use crate::error::{Refusal, Result};
use crate::hold::RecordSide;
use crate::paykit::Paykit;
use crate::state::{key, Tx};
use paykit_lib::collections::Collection;

/// Senders whose unblock this app observed, held until the shared raise applies.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingUnblocks {
    pub(crate) to_ms: u64,
    pub(crate) senders: Vec<[u8; 32]>,
}

/// The client's target of the identity's `block_floor` record.
pub(crate) fn floor_target(own: &[u8; 32]) -> RecordTarget {
    RecordTarget::new(
        activity::switch_collection(Collection::BlockFloor, None),
        &natural_key::block_floor(own),
    )
}

/// The floor raises this app owes and hasn't written (Paykit wire 5.6, Raising): an unblock's
/// (`bf`), a restore's widening that isn't clean (`rh`) and a `missed` item's widening (`bw/`),
/// from the commit that writes it until a read of the lists shows it holds no blocked sender
/// or its raise applies. Every reader takes them from here: the
/// time owed applies as a floor meanwhile, the asks wait, and the raise is tried again at the
/// next receive. Only an app holding `paykit_pay` owes a raise (Paykit wire 5.1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwedRaises {
    /// The senders an unblock's raise is owed for, in increasing order: their one-time
    /// requests wait and they are asked nothing.
    senders: Vec<[u8; 32]>,
    /// Whether a widening's raise is owed: it names no sender, so no one is asked.
    widening: bool,
    /// Whether a `missed` item's widening waits for its read of the lists, which is due at once.
    unread: bool,
    /// The highest time a raise is owed to, applied as a floor meanwhile.
    to_ms: Option<u64>,
}

impl OwedRaises {
    /// The raises `unblocks`, `restore` and `missed` owe.
    pub(crate) fn of(
        unblocks: Option<PendingUnblocks>,
        restore: Option<&crate::hold::RestoreHold>,
        missed: impl IntoIterator<Item = crate::marks::MissedWidening>,
    ) -> Self {
        let restoring = restore
            .into_iter()
            .flat_map(|hold| {
                hold.earlier_widening
                    .clone()
                    .into_iter()
                    .chain(hold.widening_owed())
            })
            .map(|owed| owed.read.at_ms);
        // A widening whose read holds no blocked sender raises nothing; one not read yet may.
        let missed: Vec<_> = missed
            .into_iter()
            .filter(|widening| widening.deny.as_ref().is_none_or(|deny| !deny.is_empty()))
            .collect();
        let unread = missed.iter().any(|widening| widening.deny.is_none());
        let missing = missed
            .into_iter()
            .flat_map(|widening| widening.windows)
            .map(|window| window.floor_ms);
        let widenings: Vec<u64> = restoring.chain(missing).collect();
        let (unblock_ms, senders) = match unblocks {
            Some(pending) => (Some(pending.to_ms), pending.senders),
            None => (None, Vec::new()),
        };
        Self {
            senders,
            widening: !widenings.is_empty(),
            unread,
            to_ms: unblock_ms.into_iter().chain(widenings).max(),
        }
    }

    /// Whether any raise is owed.
    pub(crate) fn any(&self) -> bool {
        self.to_ms.is_some()
    }

    /// The highest time a raise is owed to, applied as a floor meanwhile.
    pub(crate) fn to_ms(&self) -> Option<u64> {
        self.to_ms
    }

    /// Whether a `missed` item's widening waits for its read of the lists.
    pub(crate) fn unread(&self) -> bool {
        self.unread
    }

    /// Whether `sender`'s one-time requests wait for an unblock's raise.
    pub(crate) fn holds_one_time_requests_of(&self, sender: &[u8; 32]) -> bool {
        self.senders.binary_search(sender).is_ok()
    }

    /// Whether an ask to `to` waits: while its unblock's raise or any widening's is owed.
    pub(crate) fn holds_asks_to(&self, to: &[u8; 32]) -> bool {
        self.widening || self.holds_one_time_requests_of(to)
    }
}

/// The raises owed ([`OwedRaises`]) as the receive step reads them.
pub(crate) fn owed_raises_at(
    cx: &pubky_switch_client::app::ReceiveContext<'_>,
) -> std::result::Result<OwedRaises, crate::receive::Fail> {
    use crate::state::{cx_get, cx_scan};
    if !cx
        .cert()
        .scopes()
        .iter()
        .any(|s| s == paykit_lib::scopes::PAY)
    {
        return Ok(OwedRaises::default());
    }
    Ok(OwedRaises::of(
        cx_get(cx, key::BLOCK_FLOOR)?,
        cx_get::<crate::hold::RestoreHold>(cx, key::RESTORE_HOLD)?.as_ref(),
        cx_scan::<crate::marks::MissedWidening>(cx, key::BLOCKED_WIDENINGS)?
            .into_iter()
            .map(|(_, widening)| widening),
    ))
}

/// Whether an ask, rechecked as it is delivered, waits for a raise owed ([`OwedRaises`]) to
/// its recipient (Paykit wire 5.6, Raising).
pub(crate) fn delivery_raise_holds(
    delivery: &pubky_switch_client::app::DeliveryView<'_>,
) -> Result<bool> {
    use crate::state::{bound_view_scan, delivery_get, Binder};
    if !delivery
        .cert()
        .scopes()
        .iter()
        .any(|s| s == paykit_lib::scopes::PAY)
    {
        return Ok(false);
    }
    let blind = |input: &[u8]| delivery.blind(input);
    let binder = Binder::new(delivery.local().identity(), delivery.cert(), &blind);
    let owed = OwedRaises::of(
        delivery_get(delivery, key::BLOCK_FLOOR)?,
        delivery_get::<crate::hold::RestoreHold>(delivery, key::RESTORE_HOLD)?.as_ref(),
        bound_view_scan::<crate::marks::MissedWidening>(
            delivery.local(),
            key::BLOCKED_WIDENINGS,
            &binder,
        )?
        .into_iter()
        .map(|(_, widening)| widening),
    );
    Ok(owed.holds_asks_to(&delivery.message().to))
}

/// What an app knows of the shared floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedFloor {
    /// The floor, `None` with no record.
    Known(Option<u64>),
    /// It can't be read now (module docs, "Unknown holds").
    Unknown,
    /// The record was seen divergent, now or before: held for good (module docs,
    /// "Divergence holds for good").
    Divergent,
}

/// What this app keeps of the block floor in its own state (`fk`, Paykit wire 5.6, Divergent
/// floor and Seen floor and permanence): the highest floor it has seen, the newest generation
/// it saw the record live in, and whether it has seen the record divergent. All only grow; a
/// backup carries them, and an in-place restore keeps the higher of the backup's and the
/// installation's ([`FloorKept::with`]). Every member is present in what is stored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FloorKept {
    /// The highest floor seen, in Unix milliseconds.
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) seen_ms: Option<u64>,
    /// The newest generation the app followed while it saw the record live: an absence in
    /// that generation, or a later one no provider loss covers, is no lawful absence (Paykit
    /// wire 5.6, Seen floor and permanence).
    #[serde(deserialize_with = "Option::deserialize")]
    pub(crate) live_in: Option<u64>,
    /// The record was seen divergent: the durable divergence hold.
    pub(crate) divergent: bool,
}

impl FloorKept {
    /// Both kept: the higher of each, and divergent if either saw it so.
    pub(crate) fn with(self, other: FloorKept) -> FloorKept {
        FloorKept {
            seen_ms: self.seen_ms.max(other.seen_ms),
            live_in: self.live_in.max(other.live_in),
            divergent: self.divergent || other.divergent,
        }
    }

    /// What `held`, a view of the floor record the app read while it followed `generation`,
    /// adds: its floor when it reads, the generation when it holds a value, and its
    /// divergence. `None` for a generation the caller can't tell, as for an installation's
    /// copy at a restore.
    pub(crate) fn of_view(held: &SharedRecord, generation: Option<u64>) -> FloorKept {
        let value = held.content.value();
        FloorKept {
            seen_ms: value
                .and_then(|value| BlockFloorRecord::read(value).ok())
                .map(|record| record.floor_ms),
            live_in: value.and(generation),
            divergent: held.divergent,
        }
    }

    /// The divergence alone, as the client's event reports it.
    pub(crate) fn divergent() -> FloorKept {
        FloorKept {
            divergent: true,
            ..FloorKept::default()
        }
    }

    /// Whether the record is absent with no provider loss to explain it: it was seen live in
    /// a generation that `lost`, the generation the newest own `identity.json` names lost,
    /// doesn't cover (Paykit wire 5.6, Seen floor and permanence). A caller that can't tell
    /// the loss passes `None`, which takes every absence after a live floor as unlawful.
    pub(crate) fn absence_unlawful(&self, lost: Option<u64>) -> bool {
        self.live_in
            .is_some_and(|live| lost.is_none_or(|lost| lost < live))
    }

    /// Whether nothing is kept: no writer stores this.
    pub(crate) fn is_empty(&self) -> bool {
        *self == FloorKept::default()
    }
}

/// The floor of the record `held`, with `seen`, the mark of the newest value the app took that
/// reads (`Protocol::record_seen`), and `kept`, what the app keeps of the floor: the highest of
/// them. A malformed value and an unauthenticated delete keep the floor seen; with none seen,
/// they can't be read. No record, after the app saw one live in a generation `lost` doesn't
/// cover, can't be read either (Paykit wire 5.6, Seen floor and permanence). An authenticated
/// delete, which the permanent policy forbids, and an unopened value hold every automatic
/// attempt; a divergent view or a kept divergence holds it for good (module docs).
pub(crate) fn shared_floor_of(
    held: Option<&SharedRecord>,
    seen: Option<&[u8]>,
    kept: FloorKept,
    lost: Option<u64>,
) -> SharedFloor {
    if kept.divergent || held.is_some_and(|held| held.divergent) {
        return SharedFloor::Divergent;
    }
    let seen = seen
        .and_then(|value| BlockFloorRecord::read(value).ok())
        .map(|record| record.floor_ms)
        .max(kept.seen_ms);
    let seen_or_unknown = || seen.map_or(SharedFloor::Unknown, |s| SharedFloor::Known(Some(s)));
    let Some(held) = held else {
        if kept.absence_unlawful(lost) {
            return SharedFloor::Unknown;
        }
        return SharedFloor::Known(seen);
    };
    match &held.content {
        RecordContent::Value(value) => match BlockFloorRecord::read(value) {
            Ok(record) => SharedFloor::Known(Some(record.floor_ms.max(seen.unwrap_or(0)))),
            Err(_) => seen_or_unknown(),
        },
        RecordContent::Deleted if !held.authenticated => seen_or_unknown(),
        RecordContent::Deleted | RecordContent::Unreadable(_) => SharedFloor::Unknown,
    }
}

/// The record a raise is built on, so every member the writer doesn't know is written back
/// as read (Paykit wire 5). For a readable floor use its current value, falling back to the
/// mark for malformed content.
fn write_base(held: Option<&[u8]>, seen: Option<&[u8]>, floor: Option<u64>) -> BlockFloorRecord {
    let read = |value: &[u8]| BlockFloorRecord::read(value).ok();
    held.and_then(read)
        .or_else(|| seen.and_then(read))
        .unwrap_or_else(|| BlockFloorRecord::new(floor.unwrap_or(0)))
}

/// What a raise to `to_ms` does, from the shared floor and the floor the switch holds now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Raise {
    /// The switch's floor already covers it: nothing is written.
    Spared,
    /// Write `to`, the higher of the floor held and `to_ms`, over `floor`.
    Write { floor: Option<u64>, to: u64 },
    /// The divergence hold stands: nothing can be written.
    Held,
    /// The floor can't be read now.
    Unreadable,
}

/// The raise to `to_ms` (Paykit wire 5.6, Raising): only a floor applied at the switch
/// (`at_switch`) at least as high spares it, since another own app reads the switch, never a
/// floor this app saw alone; otherwise it writes the higher of the floor held and `to_ms`.
fn raise_of(shared: SharedFloor, at_switch: Option<u64>, to_ms: u64) -> Raise {
    if at_switch.is_some_and(|floor| floor >= to_ms) {
        return Raise::Spared;
    }
    match shared {
        SharedFloor::Known(floor) => Raise::Write {
            floor,
            to: floor.map_or(to_ms, |floor| floor.max(to_ms)),
        },
        SharedFloor::Divergent => Raise::Held,
        SharedFloor::Unknown => Raise::Unreadable,
    }
}

/// Whether the floor applies to `stored`: a one-time request (module docs).
pub(crate) fn floor_applies(stored: &StoredMessage) -> bool {
    matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring())
}

/// Whether `stored`, a one-time request, is dated before the floor (module docs): its signed
/// `created_at` is earlier than the higher of `shared` and `owed`, the raise this app owes,
/// plus the clock tolerance. An unknown floor holds every one.
pub(crate) fn dated_before(stored: &StoredMessage, shared: SharedFloor, owed: Option<u64>) -> bool {
    if !floor_applies(stored) {
        return false;
    }
    let SharedFloor::Known(floor) = shared else {
        // Unknown and divergent floors hold every one-time request.
        return true;
    };
    let created = stored.message().created_at_ms;
    [floor, owed]
        .into_iter()
        .flatten()
        .any(|time| created < time.saturating_add(CLOCK_TOLERANCE_MS))
}

impl Paykit {
    /// The shared floor as this app holds it now (module docs).
    pub(crate) async fn shared_floor(&self) -> Result<SharedFloor> {
        if self.side_unlisted(RecordSide::Payer).await? {
            return Ok(SharedFloor::Unknown);
        }
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        let held = self.app.shared_record(&key).await?;
        self.floor_of_read(&key, held.as_ref()).await
    }

    /// The floor `held` shows, with the floor seen and what this app keeps of it.
    async fn floor_of_read(
        &self,
        key: &[u8; 32],
        held: Option<&SharedRecord>,
    ) -> Result<SharedFloor> {
        let kept = self.floor_kept().await?;
        let seen = self.app.record_seen(key).await?;
        let lost = self.app.own_identity().body().recovery_from;
        Ok(shared_floor_of(held, seen.as_deref(), kept, lost))
    }

    /// What this app keeps of the floor ([`FloorKept`]).
    pub(crate) async fn floor_kept(&self) -> Result<FloorKept> {
        Ok(self
            .local_get::<FloorKept>(key::FLOOR_KEPT)
            .await?
            .unwrap_or_default())
    }

    /// Keep what `held`, a view of the floor record this app read, adds to what it keeps:
    /// a higher floor, or the divergence it shows, durably, before the view is acted on
    /// (module docs). Writes nothing when it adds nothing. An authenticated copy shows the
    /// record live in its write's own generation; an unauthenticated one, such as a recovery
    /// upload, shows a generation only when `answered_now`, a read the current service answered
    /// now, and then the generation the app follows (Paykit wire 5.6, Seen floor and
    /// permanence). So a floor seen only through a listing or the feed as an upload raises no
    /// absence alarm after a fresh-device restore, while its highest floor seen still holds.
    pub(crate) async fn keep_floor_view(
        &mut self,
        held: &SharedRecord,
        answered_now: bool,
    ) -> Result<()> {
        if held.key != self.app.record_key(&floor_target(&self.identity()))? {
            return Ok(());
        }
        let generation = if held.authenticated {
            Some(held.version.generation)
        } else {
            answered_now.then(|| self.app.service_cursor().generation)
        };
        self.keep_floor(FloorKept::of_view(held, generation)).await
    }

    /// Keep `more` with what this app keeps of the floor. The divergence, once kept for the
    /// first time, is told to the host ([`crate::NoticeKind::BlockFloorHeld`]).
    pub(crate) async fn keep_floor(&mut self, more: FloorKept) -> Result<()> {
        let kept = self.floor_kept().await?;
        let after = kept.with(more);
        if after == kept {
            return Ok(());
        }
        self.local_put(key::FLOOR_KEPT, &after).await?;
        if after.divergent && !kept.divergent {
            self.tell_identity_notice(crate::NoticeKind::BlockFloorHeld)
                .await?;
        }
        Ok(())
    }

    /// Keep what the app holds of the floor now: the record as the client holds it, and a
    /// divergence the client reported that Paykit hasn't taken yet (a pending
    /// `AppEvent::RecordDivergent`), so a view a listing, the feed or a `409` showed is kept
    /// before any later read replaces it (module docs). Run before every floor decision that
    /// reads the switch again, before every write, at a load, at each receive's end and
    /// before a backup.
    pub(crate) async fn keep_held_floor(&mut self) -> Result<()> {
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        if let Some(held) = self.app.shared_record(&key).await? {
            self.keep_floor_view(&held, false).await?;
        } else if self.holds("paykit_pay")
            && self.app.alarm().is_none()
            && !self.side_unlisted(RecordSide::Payer).await?
        {
            // Listed and absent, at an app whose certificate reads the floor, after a floor
            // seen live with no provider loss to explain it: the rollback alarm (Paykit wire
            // 5.6, Seen floor and permanence, an authorized reader). The alarm then holds what
            // it holds; this keep goes on.
            let lost = self.app.own_identity().body().recovery_from;
            if self.floor_kept().await?.absence_unlawful(lost) {
                self.app
                    .raise_rollback(crate::protocol::BACKUP_NAME, ABSENT_FLOOR)
                    .await?;
            }
        }
        let reported = self.app.pending_duty_events().await?.iter().any(|pending| {
            matches!(&pending.event, AppEvent::RecordDivergent { key: at, .. } if *at == key)
        });
        if reported {
            self.keep_floor(FloorKept::divergent()).await?;
        }
        Ok(())
    }

    /// The raises this app owes and hasn't written ([`OwedRaises`]).
    pub(crate) async fn owed_raises(&self) -> Result<OwedRaises> {
        if !self.holds("paykit_pay") {
            return Ok(OwedRaises::default());
        }
        Ok(OwedRaises::of(
            self.local_get(key::BLOCK_FLOOR).await?,
            self.local_get::<crate::hold::RestoreHold>(key::RESTORE_HOLD)
                .await?
                .as_ref(),
            self.local_scan::<crate::marks::MissedWidening>(key::BLOCKED_WIDENINGS)
                .await?
                .into_iter()
                .map(|(_, widening)| widening),
        ))
    }

    /// The highest time a raise this app owes is to, applied as a floor meanwhile (module docs).
    async fn owed_floor(&self) -> Result<Option<u64>> {
        Ok(self.owed_raises().await?.to_ms())
    }

    /// Whether an ask to `to` waits for a raise this app owes: its unblock's, or any widening's,
    /// until it is written (Paykit wire 5.6, Raising).
    pub(crate) async fn raise_holds_asks_to(&self, to: &[u8; 32]) -> Result<bool> {
        Ok(self.owed_raises().await?.holds_asks_to(to))
    }

    /// Refuse an ask to `to` while a raise this app owes isn't written ([`OwedRaises`]): it is
    /// asked nothing until then (Paykit wire 5.6, Raising).
    pub(crate) async fn refuse_while_raise_owed_to(&self, to: &[u8; 32]) -> Result<()> {
        if self.raise_holds_asks_to(to).await? {
            return Err(Refusal::NotNow(
                "Paykit wire 5.6: no one is asked while a floor raise this app owes isn't \
                 written"
                    .into(),
            )
            .into());
        }
        Ok(())
    }

    /// Whether the lists are due for a raise this app owes: while a `missed` item's widening
    /// waits for its read, the divergence hold or not, and while a raise is owed that can go,
    /// written with no divergence hold, or without the raise under both holds. It goes with that
    /// read, at the next receive (Paykit wire 5.6, Raising; Unblocking under the divergence
    /// hold).
    pub(crate) async fn raise_owed(&self) -> Result<bool> {
        let owed = self.owed_raises().await?;
        if owed.unread() {
            return Ok(true);
        }
        Ok(owed.any()
            && (!self.floor_kept().await?.divergent || self.shared_loss_hold().await?))
    }

    /// At a receive's end: while a raise this app owes is tried and left unwritten, with no
    /// divergence hold, its payer records listed and no kept batch waiting for its answer, tell
    /// the host ([`crate::NoticeKind::FloorRaiseWaiting`]). Once none is owed, forget the try;
    /// once none is owed or the divergence hold stands, which its own notice tells, forget the
    /// notice, which a later raise left unwritten tells again.
    pub(crate) async fn settle_raise_waiting(&mut self) -> Result<()> {
        let kind = crate::NoticeKind::FloorRaiseWaiting;
        let owed = self.owed_raises().await?.any();
        if !owed || self.floor_kept().await?.divergent {
            if !owed {
                self.raise_unwritten = false;
            }
            self.identity_told.retain(|told| *told != kind);
            let key = key::identity_notice(kind);
            if self.kept_notice(&key).await?.is_some() {
                self.local_delete(&key).await?;
            }
            return Ok(());
        }
        if self.raise_unwritten
            && !self.side_unlisted(RecordSide::Payer).await?
            && !self.floor_write_is_kept().await?
        {
            self.tell_identity_notice(kind).await?;
        }
        Ok(())
    }

    /// Whether `stored` is a one-time request dated before the floor, shared or owed, as this
    /// app holds them now ([`dated_before`]): it is paid only by hand.
    pub(crate) async fn floor_holds(&self, stored: &StoredMessage) -> Result<bool> {
        self.floor_holds_with(&mut None, stored).await
    }

    /// [`Paykit::floor_holds`], with the floor and the raise owed taken from `floor`, read
    /// into it once when first needed: a list read shares it across its records.
    pub(crate) async fn floor_holds_with(
        &self,
        floor: &mut Option<(SharedFloor, Option<u64>)>,
        stored: &StoredMessage,
    ) -> Result<bool> {
        if !matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring()) {
            return Ok(false);
        }
        if floor.is_none() {
            let owed = self.owed_floor().await?;
            *floor = Some((self.shared_floor().await?, owed));
        }
        let Some((shared, owed)) = *floor else {
            return Ok(true);
        };
        Ok(dated_before(stored, shared, owed))
    }

    /// The floor record as the switch holds it now (`GET /records/{key}`, Switch wire 7.4.9):
    /// the switch's current write, as the app holds it once taken, or `None` inside when the
    /// switch holds no record (`404`) and the app holds none either. `None` when nothing read
    /// now shows the switch's write: a read that fails or that the switch refuses, a write the
    /// app refuses or that is no newer than the one it holds, or a `404` for a record the app
    /// holds; none of them counts as a floor that doesn't hold a request, and a copy this app
    /// keeps is never taken for the switch's. Read immediately before an automatic dispatch
    /// (module docs, "Applied before every automatic attempt"), as the deny list is read
    /// again there (Paykit spec 8.1, How fresh the deny list is). What the read shows is kept
    /// first ([`Paykit::keep_floor_view`]), its divergence included.
    ///
    /// # Errors
    ///
    /// A store failure, the client's alarm, or the rollback alarm for a floor seen live that
    /// reads absent with no provider loss.
    pub(crate) async fn floor_read_now(&mut self) -> Result<Option<Option<SharedRecord>>> {
        self.keep_held_floor().await?;
        let read = match self
            .app
            .read_record_fresh(&floor_target(&self.identity()))
            .await
        {
            Ok(read) => read,
            // An alarm and a store failure are the app's own state, never a floor to wait for.
            Err(
                error @ (pubky_switch_client::app::AppError::Alarm(_)
                | pubky_switch_client::app::AppError::Store(_)),
            ) => return Err(error.into()),
            Err(_) => return Ok(None),
        };
        Ok(match read {
            RecordRead::Fresh(record) => {
                self.keep_floor_view(&record, true).await?;
                Some(Some(record))
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } => {
                // A floor seen live that the switch now says it holds no record of, with no
                // provider loss to explain it, is the rollback alarm (Paykit wire 5.6, Seen
                // floor and permanence), as the client raises it for a copy it still holds.
                let lost = self.app.own_identity().body().recovery_from;
                if self.floor_kept().await?.absence_unlawful(lost) {
                    let alarm = self
                        .app
                        .raise_rollback(crate::protocol::BACKUP_NAME, ABSENT_FLOOR)
                        .await?;
                    return Err(pubky_switch_client::app::AppError::Alarm(alarm).into());
                }
                Some(None)
            }
            RecordRead::Kept {
                record: Some(record),
                ..
            } => {
                // Not the switch's write, but a divergence it shows is kept all the same.
                self.keep_floor_view(&record, false).await?;
                None
            }
            _ => None,
        })
    }

    /// Whether `stored` is a one-time request dated before the floor as `read`, the record
    /// as [`Paykit::floor_read_now`] read it, shows it, with the floor this app has seen and
    /// a raise it owes ([`dated_before`]). It waits for no raise. A value that doesn't read
    /// with none seen holds the request.
    pub(crate) async fn floor_holds_read(
        &self,
        stored: &StoredMessage,
        read: Option<&SharedRecord>,
    ) -> Result<bool> {
        if !floor_applies(stored) {
            return Ok(false);
        }
        let owed = self.owed_floor().await?;
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        Ok(dated_before(
            stored,
            self.floor_of_read(&key, read).await?,
            owed,
        ))
    }

    /// The shared floor for a write of a raise to `to_ms`, with the floor the switch holds
    /// now: the record as the switch holds it, read alone, with what this app holds and keeps
    /// of it, the copy's divergence kept first ([`Paykit::floor_read_now`]), so no write goes
    /// over a divergence the app saw or the switch shows now. When nothing read now shows the
    /// switch's write, the copy decides a raise that is needed, whose conditional write checks
    /// its version at the service; a raise skipped needs the switch's write. A read that fails
    /// leaves it unknown, as does a scope key the app waits for.
    async fn floor_for_write(&mut self, to_ms: u64) -> Result<(SharedFloor, Option<u64>)> {
        let value = |record: Option<&SharedRecord>| {
            record
                .and_then(|record| record.content.value())
                .and_then(|value| BlockFloorRecord::read(value).ok())
                .map(|record| record.floor_ms)
        };
        // Sealed under `paykit_pay` at the current data epoch: without that key nothing is written,
        // and nothing read while the copy waits for a wider certificate's keys.
        if !self.holds_scope_keys() {
            return Ok((SharedFloor::Unknown, None));
        }
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        if let Some(read) = self.floor_read_now().await? {
            let at_switch = value(read.as_ref());
            return Ok((self.floor_of_read(&key, read.as_ref()).await?, at_switch));
        }
        let copy = self.app.shared_record(&key).await?;
        let at_switch = value(copy.as_ref());
        if at_switch.is_some_and(|floor| floor >= to_ms) {
            return Ok((SharedFloor::Unknown, None));
        }
        let held = self.shared_floor().await?;
        Ok(match held {
            SharedFloor::Known(_) | SharedFloor::Divergent => (held, at_switch),
            SharedFloor::Unknown => (SharedFloor::Unknown, None),
        })
    }

    /// Prepare a conditional raise to `to_ms`: of the higher of the floor held and `to_ms`
    /// (Paykit wire 5.6, Raising). Only a floor applied at the switch at least as high spares
    /// it; a floor this app saw alone doesn't, since another own app reads the switch. No
    /// create or successor is written over a divergent floor or while the divergence hold
    /// stands (Paykit wire 5.6, Divergent floor). A base taken from a recovery upload names
    /// its exact application (`record_current`), which the client adds from the write's base.
    ///
    /// # Errors
    ///
    /// [`Refusal::FloorHeld`] while the divergence hold stands without the shared post-loss
    /// hold; [`Refusal::NotNow`] while the floor can't be read; the client's errors.
    async fn floor_batch(&mut self, to_ms: u64) -> Result<Option<OpBatch>> {
        let (shared, at_switch) = self.floor_for_write(to_ms).await?;
        let (floor, to) = match raise_of(shared, at_switch, to_ms) {
            Raise::Spared => return Ok(None),
            // Paykit wire 5.6, Unblocking under the divergence hold: under both holds no own
            // app pays a one-time request automatically and this app holds every one as dated
            // before the floor, so the unblock or widening goes without the raise. Under the
            // divergence hold alone it waits.
            Raise::Held if self.shared_loss_hold().await? => return Ok(None),
            Raise::Held => return Err(Refusal::FloorHeld.into()),
            Raise::Unreadable => return Err(unreadable_floor()),
            Raise::Write { floor, to } => (floor, to),
        };
        let target = floor_target(&self.identity());
        let key = self.app.record_key(&target)?;
        let held = self.app.shared_record(&key).await?;
        let seen = self.app.record_seen(&key).await?;
        let base = write_base(
            held.as_ref().and_then(|held| held.content.value()),
            seen.as_deref(),
            floor,
        );
        // A base already at `to`, seen but not at the switch, is written back as it is.
        let raised = base.raised(to).unwrap_or(base);
        let entry = self
            .app
            .record_op(&target, Some(&raised.to_json()?), Vec::new())
            .await?;
        Ok(Some(
            OpBatch::for_protocol(crate::protocol::BACKUP_NAME).entry(entry),
        ))
    }

    /// Submit `batch`; after a `409 conflict` the floor is read again before the error
    /// returns, so the next try builds on what the switch holds.
    pub(crate) async fn submit_floor(&mut self, batch: OpBatch) -> Result<()> {
        match self.submit(batch).await {
            Ok(_) => Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    let target = floor_target(&self.identity());
                    if let Some(read) = self.app.read_record(&target).await? {
                        self.keep_floor_view(&read, false).await?;
                    }
                }
                Err(error.into())
            }
        }
    }

    /// A payer uses the latest verified service time for a closed block window.
    pub(crate) fn unblock_floor(&self) -> Result<u64> {
        self.service_time()
            .map(|at| at.saturating_add(CLOCK_TOLERANCE_MS))
            .ok_or_else(unreadable_floor)
    }

    /// An unanswered raise still owns its applied-only local writes. A new list read
    /// waits for the client to settle them before preparing another snapshot.
    pub(crate) async fn floor_write_is_kept(&mut self) -> Result<bool> {
        let Some(id) = self.local_get::<[u8; 16]>(key::FLOOR_BATCH).await? else {
            return Ok(false);
        };
        if matches!(
            self.app.kept_batch(id).await?,
            Some(pubky_switch_client::app::KeptBatchStatus::Kept { .. })
        ) {
            return Ok(true);
        }
        self.local_delete(key::FLOOR_BATCH).await?;
        Ok(false)
    }

    /// [`Paykit::write_with_floor`], noting a raise it leaves unwritten for the host
    /// ([`Paykit::settle_raise_waiting`]).
    pub(crate) async fn commit_with_floor(&mut self, to_ms: u64, tx: Tx) -> Result<()> {
        let result = self.write_with_floor(to_ms, tx).await;
        if result.as_ref().is_err_and(is_raise_unwritten) {
            self.raise_unwritten = true;
        }
        result
    }

    /// Apply local effects with the shared raise, or directly if a sufficient floor
    /// already applied. An unanswered batch keeps its local effects with it.
    async fn write_with_floor(&mut self, to_ms: u64, tx: Tx) -> Result<()> {
        if !self.holds("paykit_pay") {
            return self.local_commit(tx).await;
        }
        let Some(batch) = self.floor_batch(to_ms).await? else {
            return self.local_commit(tx).await;
        };
        let writes = tx.into_writes();
        let with_writes = |batch: OpBatch| {
            let marker = key::FLOOR_BATCH.as_bytes().to_vec();
            batch
                .keep_id_at(marker.clone())
                .locals(writes.iter().cloned())
                .local(pubky_switch_client::app::LocalWrite::Delete { key: marker })
        };
        match self.submit_floor(with_writes(batch)).await {
            Err(error) if error.code() == Some(ErrorCode::Conflict) => {
                // A competing payer can close the same window first. submit_floor refreshed
                // the record, so retry once on that version, keeping the original cutoff.
                if self.floor_write_is_kept().await? {
                    return Err(error);
                }
                if let Some(batch) = self.floor_batch(to_ms).await? {
                    self.submit_floor(with_writes(batch))
                        .await
                        .map_err(floor_write_error)
                } else {
                    self.app
                        .protocol_commit(crate::protocol::BACKUP_NAME, &writes)
                        .await?;
                    Ok(())
                }
            }
            result => result.map_err(floor_write_error),
        }
    }

    /// A user's unblock returns an unanswered or conflicting write directly. The sender
    /// stays denied, and the next user call starts from the current list.
    pub(crate) async fn apply_floor_before_unblock(&mut self) -> Result<()> {
        if let Some(batch) = self.floor_batch(self.unblock_floor()?).await? {
            self.submit_floor(batch).await.map_err(floor_write_error)?;
        }
        Ok(())
    }
}

impl Paykit {
    /// Check the request records the reads opened without `manual_only` (`fo/`) against the
    /// floor this app holds now that they are done (module docs, "Applied by every payer app
    /// at record creation"), and mark `manual_only` on each still open whose one-time request
    /// is dated before it: a raise the read took before the entry wasn't committed when the
    /// entry's step opened the record. The mark is never lifted, and an executor's batch built
    /// on the version before it fails (Paykit spec 8.3, Manual only). Each record checked
    /// drops its note, but one whose write meets a conflict, kept for the next receive.
    pub(crate) async fn sweep_floor(&mut self) -> Result<()> {
        self.require_first_read()?;
        let notes = self.local_scan::<bool>(key::FLOOR_OPENED).await?;
        if notes.is_empty() {
            return Ok(());
        }
        // A divergent floor marks every open one-time request; an unknown one waits.
        let floor = self.shared_floor().await?;
        if floor == SharedFloor::Unknown {
            return Ok(());
        }
        let owed = self.owed_floor().await?;
        let own = self.identity();
        for (rest, _) in notes {
            let note = format!("{}{rest}", key::FLOOR_OPENED);
            let record_key = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| crate::error::PaykitError::corrupt())?;
            let held = self.app.shared_record(&record_key).await?;
            if crate::records::activity_base_changed(held.as_ref()) {
                continue;
            }
            let record = held.as_ref().and_then(|held| {
                crate::records::bind_activity(
                    held.content.value()?,
                    &own,
                    paykit_lib::records::ActivitySide::Payer,
                    None,
                    &held.key,
                    |t| self.app.record_key(t).ok(),
                    &|p| self.rail.settlement_id(p),
                )
            });
            let marks = record.as_ref().and_then(|record| {
                let (stored, _) = crate::requests::request_of(record)?;
                let open = !(record.manual_only
                    || record.shows_paid()
                    || crate::requests::cancelled(record)
                    || crate::requests::rejected(record));
                (open && dated_before(stored, floor, owed)).then(|| stored.hash())
            });
            if let (Some(mut record), Some(hash)) = (record, marks) {
                let target = crate::activity::ActivityTarget::payer(natural_key::request_record(
                    &record.counterparty,
                    &hash,
                ));
                record.manual_only = true;
                match self
                    .app
                    .put_record(&target.record_target(), &record.to_json()?, Vec::new())
                    .await
                {
                    Ok(_) => {}
                    Err(error) if error.code() == Some(ErrorCode::Conflict) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            self.local_delete(&note).await?;
        }
        Ok(())
    }
}

const UNREADABLE_FLOOR: &str = "Paykit wire 5.6: the block floor can't be read now";

/// The rollback alarm's reason for a floor seen live that reads absent with no provider loss.
const ABSENT_FLOOR: &str =
    "Paykit wire 5.6: a permanent block floor seen live reads absent with no provider loss";

/// The refusal while the floor a raise builds on can't be read.
pub(crate) fn unreadable_floor() -> crate::error::PaykitError {
    Refusal::NotNow(UNREADABLE_FLOOR.into()).into()
}

/// Report an unavailable floor for retryable service failures. Preserve definite refusals,
/// conflicts, kept batch identities and errors that require action by the host.
pub(crate) fn floor_write_error(error: crate::error::PaykitError) -> crate::error::PaykitError {
    use pubky_switch_client::app::{AppError, Refusal as ClientRefusal};
    use pubky_switch_core::http::Retry;
    if error.kept_batch().is_some() || error.code() == Some(ErrorCode::Conflict) {
        return error;
    }
    match &error {
        crate::error::PaykitError::Client(AppError::Switch(answer))
            if answer.retry() == Some(Retry::No) =>
        {
            error
        }
        crate::error::PaykitError::Client(
            AppError::Switch(_)
            | AppError::Transport(_)
            | AppError::NotAnsweringAsAccount { .. }
            | AppError::Refused(ClientRefusal::NotNow(_)),
        ) => unreadable_floor(),
        _ => error,
    }
}

/// Whether the floor is temporarily unavailable.
pub(crate) fn is_unreadable_floor(error: &crate::error::PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotNow(why)) if why == UNREADABLE_FLOOR)
}

/// Whether a raise couldn't be written: the floor can't be read now, or the divergence hold
/// stands. Neither fails a read of the lists: the raise stays owed (module docs).
pub(crate) fn is_raise_unwritten(error: &crate::error::PaykitError) -> bool {
    is_unreadable_floor(error) || matches!(error.refusal(), Some(Refusal::FloorHeld))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The floor record as an app holds it: `content`, authenticated or not, divergent or not.
    fn held(content: RecordContent, authenticated: bool, divergent: bool) -> SharedRecord {
        SharedRecord {
            key: [4; 32],
            collection: Collection::BlockFloor.name().into(),
            policy: Collection::BlockFloor.fixed_policy(),
            version: pubky_switch_core::ops::RecordVersion::new(1, 2),
            authenticated,
            writer: [1; 16],
            written_at_ms: Some(10),
            content,
            doubtful: false,
            flagged: false,
            not_taken: false,
            different_write_at_held_version: false,
            divergent,
        }
    }

    fn value(floor_ms: u64) -> RecordContent {
        RecordContent::Value(BlockFloorRecord::new(floor_ms).to_json().unwrap())
    }

    #[test]
    fn test_floor_unknown_without_a_value_or_a_seen_floor() {
        let none = FloorKept::default();
        assert_eq!(
            shared_floor_of(None, None, none, None),
            SharedFloor::Known(None)
        );
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            shared_floor_of(None, Some(&seen), none, None),
            SharedFloor::Known(Some(70))
        );
        let malformed = RecordContent::Value(br#"{"v":1}"#.to_vec());
        assert_eq!(
            shared_floor_of(Some(&held(malformed, true, false)), None, none, None),
            SharedFloor::Unknown
        );
    }

    #[test]
    fn test_floor_applies_the_highest_of_the_value_the_mark_and_the_kept_floor() {
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        let kept = FloorKept {
            seen_ms: Some(90),
            live_in: None,
            divergent: false,
        };
        // A later lower write, an in-place restore or a lost mark never lowers the floor.
        let current = held(value(50), true, false);
        assert_eq!(
            shared_floor_of(Some(&current), Some(&seen), kept, None),
            SharedFloor::Known(Some(90))
        );
        assert_eq!(
            shared_floor_of(None, None, kept, None),
            SharedFloor::Known(Some(90))
        );
        let higher = held(value(120), true, false);
        assert_eq!(
            shared_floor_of(Some(&higher), Some(&seen), kept, None),
            SharedFloor::Known(Some(120))
        );
    }

    #[test]
    fn test_floor_absence_after_a_live_floor_holds_unless_a_loss_covers_it() {
        // Paykit wire 5.6, Seen floor and permanence: seen live in generation 3.
        let kept = FloorKept {
            seen_ms: Some(70),
            live_in: Some(3),
            divergent: false,
        };
        // No loss, or one before the floor was seen live: the absence is unlawful.
        for lost in [None, Some(2)] {
            assert_eq!(
                shared_floor_of(None, None, kept, lost),
                SharedFloor::Unknown
            );
        }
        // A loss of that generation or a later one covers it; the floor seen stays.
        for lost in [Some(3), Some(4)] {
            assert_eq!(
                shared_floor_of(None, None, kept, lost),
                SharedFloor::Known(Some(70))
            );
        }
        // A record held decides as before.
        assert_eq!(
            shared_floor_of(Some(&held(value(80), true, false)), None, kept, None),
            SharedFloor::Known(Some(80))
        );
    }

    #[test]
    fn test_floor_unauthenticated_delete_keeps_the_floor_seen() {
        // Paykit wire 5.6: the service's word, never absence. With a floor seen it keeps it;
        // with none it can't be read.
        let delete = held(RecordContent::Deleted, false, false);
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            shared_floor_of(Some(&delete), Some(&seen), FloorKept::default(), None),
            SharedFloor::Known(Some(70))
        );
        assert_eq!(
            shared_floor_of(Some(&delete), None, FloorKept::default(), None),
            SharedFloor::Unknown
        );
        // An authenticated one, which the permanent policy forbids, holds whatever was seen.
        let authenticated = held(RecordContent::Deleted, true, false);
        assert_eq!(
            shared_floor_of(
                Some(&authenticated),
                Some(&seen),
                FloorKept::default(),
                None
            ),
            SharedFloor::Unknown
        );
    }

    #[test]
    fn test_floor_divergence_holds_over_every_value() {
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        // A divergent view holds, however high the floor it shows.
        let divergent = held(value(u64::MAX / 2), true, true);
        assert_eq!(
            shared_floor_of(Some(&divergent), Some(&seen), FloorKept::default(), None),
            SharedFloor::Divergent
        );
        // A kept divergence holds over a cleared view, a missing record and any later value.
        let kept = FloorKept {
            seen_ms: Some(70),
            live_in: Some(1),
            divergent: true,
        };
        for current in [None, Some(held(value(10_000), true, false))] {
            assert_eq!(
                shared_floor_of(current.as_ref(), Some(&seen), kept, None),
                SharedFloor::Divergent
            );
        }
    }

    #[test]
    fn test_floor_raise_spared_only_by_the_switchs_floor() {
        // The switch's floor covers the raise: nothing to write, whatever is held.
        for shared in [
            SharedFloor::Known(Some(10)),
            SharedFloor::Divergent,
            SharedFloor::Unknown,
        ] {
            assert_eq!(raise_of(shared, Some(100), 100), Raise::Spared);
        }
        // A floor this app saw higher than the switch's doesn't spare it: the higher is written.
        assert_eq!(
            raise_of(SharedFloor::Known(Some(500)), None, 100),
            Raise::Write {
                floor: Some(500),
                to: 500
            }
        );
        assert_eq!(
            raise_of(SharedFloor::Known(Some(500)), Some(50), 100),
            Raise::Write {
                floor: Some(500),
                to: 500
            }
        );
        assert_eq!(
            raise_of(SharedFloor::Known(None), None, 100),
            Raise::Write {
                floor: None,
                to: 100
            }
        );
        // Under the hold, or unreadable, nothing is written.
        assert_eq!(raise_of(SharedFloor::Divergent, Some(50), 100), Raise::Held);
        assert_eq!(raise_of(SharedFloor::Unknown, None, 100), Raise::Unreadable);
    }

    fn missed(floor_ms: u64, deny: Option<Vec<[u8; 32]>>) -> crate::marks::MissedWidening {
        crate::marks::MissedWidening {
            id: crate::hold::HoldEpoch::new().unwrap(),
            windows: vec![crate::marks::BlockedWindow {
                from_ms: 1,
                until_ms: floor_ms,
                floor_ms,
            }],
            deny,
        }
    }

    #[test]
    fn test_floor_owed_raises_cover_unblocks_restores_and_missed_items() {
        // Nothing owed: no floor, no ask held.
        let none = OwedRaises::of(None, None, Vec::new());
        assert!(!none.any() && !none.holds_asks_to(&[1; 32]));
        // An unblock's raise holds its senders' one-time requests and asks, no one else's.
        let unblock = OwedRaises::of(
            Some(PendingUnblocks {
                to_ms: 50,
                senders: vec![[1; 32]],
            }),
            None,
            Vec::new(),
        );
        assert_eq!(unblock.to_ms(), Some(50));
        assert!(unblock.holds_one_time_requests_of(&[1; 32]) && unblock.holds_asks_to(&[1; 32]));
        assert!(!unblock.holds_one_time_requests_of(&[2; 32]) && !unblock.holds_asks_to(&[2; 32]));
        // A restore's owed widening names no sender: every ask waits, no entry does.
        let mut hold = crate::hold::RestoreHold::of(10, None);
        hold.verdict = crate::hold::RestoreVerdict::NotClean;
        hold.widening_read = Some(crate::marks::WideningRead {
            at_ms: 70,
            deny: Vec::new(),
        });
        let restore = OwedRaises::of(None, Some(&hold), Vec::new());
        assert_eq!(restore.to_ms(), Some(70));
        assert!(restore.holds_asks_to(&[2; 32]) && !restore.holds_one_time_requests_of(&[2; 32]));
        // A missed item's widening owes a raise from its write: before its read of the lists,
        // which is then due, and once that read holds a blocked sender. A read that holds none
        // owes none.
        let read = OwedRaises::of(None, None, vec![missed(90, Some(vec![[3; 32]]))]);
        assert_eq!(read.to_ms(), Some(90));
        assert!(read.holds_asks_to(&[2; 32]) && !read.unread());
        let unread = OwedRaises::of(None, None, vec![missed(80, None)]);
        assert_eq!(unread.to_ms(), Some(80));
        assert!(unread.holds_asks_to(&[2; 32]) && unread.unread());
        assert!(!OwedRaises::of(None, None, vec![missed(90, Some(Vec::new()))]).any());
        // The floor applied meanwhile is the highest owed.
        let all = OwedRaises::of(
            Some(PendingUnblocks {
                to_ms: 50,
                senders: vec![[1; 32]],
            }),
            Some(&hold),
            vec![missed(90, Some(vec![[3; 32]]))],
        );
        assert_eq!(all.to_ms(), Some(90));
    }

    #[test]
    fn test_floor_kept_only_grows() {
        let a = FloorKept {
            seen_ms: Some(70),
            live_in: Some(3),
            divergent: true,
        };
        let b = FloorKept {
            seen_ms: Some(90),
            live_in: Some(2),
            divergent: false,
        };
        let both = FloorKept {
            seen_ms: Some(90),
            live_in: Some(3),
            divergent: true,
        };
        assert_eq!(a.with(b), both);
        assert_eq!(b.with(a), both);
        assert_eq!(FloorKept::default().with(a), a);
        assert!(FloorKept::default().is_empty());
        let view = held(value(80), true, true);
        assert_eq!(
            FloorKept::of_view(&view, Some(4)),
            FloorKept {
                seen_ms: Some(80),
                live_in: Some(4),
                divergent: true
            }
        );
        // A delete is no live floor; an installation's copy tells no generation.
        let deleted = held(RecordContent::Deleted, false, false);
        assert_eq!(FloorKept::of_view(&deleted, Some(4)).live_in, None);
        assert_eq!(FloorKept::of_view(&view, None).live_in, None);
    }

    /// A floor value of `floor_ms` with a member no writer here knows.
    fn with_unknown_member(floor_ms: u64) -> Vec<u8> {
        let json = BlockFloorRecord::new(floor_ms).to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&json).unwrap();
        value["later_member"] = serde_json::json!({"kinds": ["a", {"b": null}]});
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn test_floor_write_base_keeps_the_seen_unknown_members_over_a_value_that_does_not_read() {
        let seen = with_unknown_member(70);
        let expected = BlockFloorRecord::read(&seen).unwrap();
        assert!(!expected.unknown.is_empty());
        // The value held doesn't read: the value seen is the base.
        let malformed = br#"{"v":1}"#;
        assert_eq!(write_base(Some(malformed), Some(&seen), Some(70)), expected);
        assert_eq!(write_base(None, Some(&seen), Some(70)), expected);
        // Raised, it writes those members back as read.
        let raised = expected.raised(90).unwrap().to_json().unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&seen).unwrap();
        value["floor"] =
            serde_json::from_slice::<serde_json::Value>(&raised).unwrap()["floor"].clone();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&raised).unwrap(),
            value
        );
    }

    fn client(error: pubky_switch_client::app::AppError) -> crate::error::PaykitError {
        crate::error::PaykitError::Client(error)
    }

    fn answered(code: ErrorCode) -> pubky_switch_client::app::AppError {
        pubky_switch_client::app::AppError::Switch(
            pubky_switch_client::error::ClientError::Switch {
                status: 400,
                code,
                message: String::new(),
                current: serde_json::Value::Null,
                known_version: None,
                retry_after: None,
            },
        )
    }

    fn lost() -> pubky_switch_client::app::AppError {
        pubky_switch_client::app::AppError::Transport(
            pubky_switch_client::error::TransportError::Connect("down".into()),
        )
    }

    #[test]
    fn test_floor_write_error_keeps_a_failure_of_the_app_itself() {
        use pubky_switch_client::app::{Alarm, AppError, Refusal as ClientRefusal, StoreFailure};
        use pubky_switch_client::error::StoreError;
        // A store failure, the protocol's included, an alarm and an app stopped are returned
        // as they are: none is a floor to wait for.
        let failures = [
            AppError::Store(StoreError::Unavailable),
            AppError::Protocol(StoreFailure::unavailable(StoreError::Unavailable)),
            AppError::Alarm(Alarm::Equivocation { version: 3 }),
            AppError::Refused(ClientRefusal::Stopped),
        ];
        for failure in failures {
            let text = failure.to_string();
            let error = floor_write_error(client(failure));
            assert!(!is_unreadable_floor(&error), "{text}");
            let crate::error::PaykitError::Client(kept) = &error else {
                panic!("{text}");
            };
            assert_eq!(kept.to_string(), text);
        }
    }

    #[test]
    fn test_floor_write_error_keeps_a_kept_batch_and_a_conflict() {
        use pubky_switch_client::app::AppError;
        // A batch the client kept goes at the next write or read: never "nothing queued",
        // whatever failed, an answer lost included.
        for inner in [lost(), answered(ErrorCode::Unavailable)] {
            let error = floor_write_error(client(AppError::Kept {
                batch_id: [7; 16],
                error: Box::new(inner),
            }));
            assert_eq!(error.kept_batch(), Some([7; 16]));
            assert!(!is_unreadable_floor(&error));
        }
        let conflict = floor_write_error(client(answered(ErrorCode::Conflict)));
        assert_eq!(conflict.code(), Some(ErrorCode::Conflict));
    }

    #[test]
    fn test_floor_write_error_waits_for_a_switch_that_may_answer_otherwise_or_does_not_answer() {
        // With nothing kept, an answer the switch may change, no answer, or a batch the
        // client refuses for now queues nothing: the ask waits as for a floor that can't be
        // read.
        let not_now = pubky_switch_client::app::AppError::Refused(
            pubky_switch_client::app::Refusal::NotNow("kept batches at their bound".into()),
        );
        for error in [
            answered(ErrorCode::Unavailable),
            answered(ErrorCode::RateLimited),
            lost(),
            not_now,
        ] {
            assert!(is_unreadable_floor(&floor_write_error(client(error))));
        }
    }

    #[test]
    fn test_floor_write_error_keeps_a_refusal_the_same_request_does_not_change() {
        use pubky_switch_core::http::Retry;
        // An answer the Retry column says "No" to is the switch's refusal, as it is: never a
        // floor to wait for.
        for code in [ErrorCode::PermissionMissing, ErrorCode::CertExpired] {
            assert_eq!(code.retry(), Retry::No);
            let error = floor_write_error(client(answered(code)));
            assert!(!is_unreadable_floor(&error), "{error}");
            assert_eq!(error.code(), Some(code));
        }
    }

    #[test]
    fn test_floor_write_base_is_the_value_held_when_it_reads() {
        let held = with_unknown_member(40);
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            write_base(Some(&held), Some(&seen), Some(70)),
            BlockFloorRecord::read(&held).unwrap()
        );
        // With nothing that reads, a record of the floor alone.
        assert_eq!(
            write_base(Some(br#"{"v":1}"#), None, None),
            BlockFloorRecord::new(0)
        );
    }
}
