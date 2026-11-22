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
//!   included, and it is kept across restart, restore, scope changes and later losses.
//! - **The highest floor seen** is kept in the same record, so an in-place restore from an
//!   earlier backup, or a later lower write, never lowers it.

use paykit_lib::derive::natural_key;
use paykit_lib::records::{BlockFloorRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{
    KeptReason, OpBatch, RecordContent, RecordRead, RecordTarget, SharedRecord,
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
/// floor and Seen floor and permanence): the highest floor it has seen and whether it has seen
/// the record divergent. Both only grow; a backup carries them, and an in-place restore keeps
/// the higher of the backup's and the installation's ([`FloorKept::with`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FloorKept {
    /// The highest floor seen, in Unix milliseconds.
    pub(crate) seen_ms: Option<u64>,
    /// The record was seen divergent: the durable divergence hold.
    pub(crate) divergent: bool,
}

impl FloorKept {
    /// Both kept: the higher floor, and divergent if either saw it so.
    pub(crate) fn with(self, other: FloorKept) -> FloorKept {
        FloorKept {
            seen_ms: self.seen_ms.max(other.seen_ms),
            divergent: self.divergent || other.divergent,
        }
    }

    /// What `held`, a view of the floor record the app read, adds: its floor when it reads,
    /// and its divergence.
    pub(crate) fn of_view(held: &SharedRecord) -> FloorKept {
        FloorKept {
            seen_ms: held
                .content
                .value()
                .and_then(|value| BlockFloorRecord::read(value).ok())
                .map(|record| record.floor_ms),
            divergent: held.divergent,
        }
    }

    /// Whether nothing is kept: no writer stores this.
    pub(crate) fn is_empty(&self) -> bool {
        *self == FloorKept::default()
    }
}

/// The floor of the record `held`, with `seen`, the mark of the newest value the app took that
/// reads (`Protocol::record_seen`), and `kept`, what the app keeps of the floor: the highest of
/// them. A malformed value, an unauthenticated delete and an empty answer keep the floor seen;
/// with none seen, a malformed value or an unauthenticated delete can't be read. An
/// authenticated delete, which the permanent policy forbids, and an unopened value hold every
/// automatic attempt; a divergent view or a kept divergence holds it for good (module docs).
pub(crate) fn shared_floor_of(
    held: Option<&SharedRecord>,
    seen: Option<&[u8]>,
    kept: FloorKept,
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
/// mark for malformed content. The caller has already excluded an unresolved deletion.
fn write_base(held: Option<&[u8]>, seen: Option<&[u8]>, floor: Option<u64>) -> BlockFloorRecord {
    let read = |value: &[u8]| BlockFloorRecord::read(value).ok();
    held.and_then(read)
        .or_else(|| seen.and_then(read))
        .unwrap_or_else(|| BlockFloorRecord::new(floor.unwrap_or(0)))
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
        Ok(shared_floor_of(held, seen.as_deref(), kept))
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
    /// (module docs). Writes nothing when it adds nothing.
    pub(crate) async fn keep_floor_view(&mut self, held: &SharedRecord) -> Result<()> {
        if held.key != self.app.record_key(&floor_target(&self.identity()))? {
            return Ok(());
        }
        self.keep_floor(FloorKept::of_view(held)).await
    }

    /// Keep `more` with what this app keeps of the floor.
    pub(crate) async fn keep_floor(&mut self, more: FloorKept) -> Result<()> {
        let kept = self.floor_kept().await?;
        let after = kept.with(more);
        if after == kept {
            return Ok(());
        }
        self.local_put(key::FLOOR_KEPT, &after).await
    }

    /// Keep the floor record as the app holds it now, at a load and at each receive's end, so
    /// a view the listing or the feed showed divergent, or a higher floor it took, is kept
    /// (module docs). The divergence event keeps it as well; this keeps it even when the
    /// event was taken before.
    pub(crate) async fn keep_held_floor(&mut self) -> Result<()> {
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        if let Some(held) = self.app.shared_record(&key).await? {
            self.keep_floor(FloorKept::of_view(&held)).await?;
        }
        Ok(())
    }

    /// Whether `stored` is a one-time request dated before the floor, shared or owed, as this
    /// app holds them now ([`dated_before`]): it is paid only by hand.
    pub(crate) async fn floor_holds(&self, stored: &StoredMessage) -> Result<bool> {
        if !matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring()) {
            return Ok(false);
        }
        let owed = self
            .local_get::<PendingUnblocks>(key::BLOCK_FLOOR)
            .await?
            .map(|pending| pending.to_ms);
        Ok(dated_before(stored, self.shared_floor().await?, owed))
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
    /// A store failure keeping what the read shows.
    pub(crate) async fn floor_read_now(&mut self) -> Result<Option<Option<SharedRecord>>> {
        let Ok(read) = self
            .app
            .read_record_fresh(&floor_target(&self.identity()))
            .await
        else {
            return Ok(None);
        };
        Ok(match read {
            RecordRead::Fresh(record) => {
                self.keep_floor_view(&record).await?;
                Some(Some(record))
            }
            RecordRead::Kept {
                record: None,
                reason: KeptReason::NotFound,
            } => Some(None),
            RecordRead::Kept {
                record: Some(record),
                ..
            } => {
                // Not the switch's write, but a divergence it shows is kept all the same.
                self.keep_floor_view(&record).await?;
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
        let owed = self
            .local_get::<PendingUnblocks>(key::BLOCK_FLOOR)
            .await?
            .map(|pending| pending.to_ms);
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        Ok(dated_before(
            stored,
            self.floor_of_read(&key, read).await?,
            owed,
        ))
    }

    /// The shared floor for a write: the copy's, or, while the payer's records aren't listed
    /// or the copy can't open it, the record as the switch holds it now, read alone. A read
    /// that fails leaves it unknown, as does a scope key the app waits for.
    async fn floor_for_write(&mut self, to_ms: u64) -> Result<SharedFloor> {
        // Sealed under `paykit_pay` at the current data epoch: without that key nothing is written,
        // and nothing read while the copy waits for a wider certificate's keys.
        if !self.holds_scope_keys() {
            return Ok(SharedFloor::Unknown);
        }
        let held = self.shared_floor().await?;
        if matches!(held, SharedFloor::Known(floor) if floor.is_none_or(|value| value < to_ms)) {
            // The conditional raise will check this version at the service.
            return Ok(held);
        }
        // Skipping a raise makes no conditional write. Verify the sufficient floor is
        // still current before letting the unblock or widening proceed.
        let Some(read) = self.floor_read_now().await? else {
            return Ok(SharedFloor::Unknown);
        };
        let key = self.app.record_key(&floor_target(&self.identity()))?;
        self.floor_of_read(&key, read.as_ref()).await
    }

    /// Prepare a conditional raise to `to_ms`. An applied floor at least as high
    /// already covers this window. No create or successor is written over a divergent
    /// floor or while the divergence hold stands (Paykit wire 5.6, Divergent floor).
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the floor can't be read, or is held divergent; the client's
    /// errors.
    async fn floor_batch(&mut self, to_ms: u64) -> Result<Option<OpBatch>> {
        let SharedFloor::Known(floor) = self.floor_for_write(to_ms).await? else {
            return Err(unreadable_floor());
        };
        let target = floor_target(&self.identity());
        let key = self.app.record_key(&target)?;
        let held = self.app.shared_record(&key).await?;
        if floor.is_some_and(|floor| floor >= to_ms) {
            return Ok(None);
        }
        let seen = self.app.record_seen(&key).await?;
        let base = write_base(
            held.as_ref().and_then(|held| held.content.value()),
            seen.as_deref(),
            floor,
        );
        let raised = base
            .raised(to_ms)
            .unwrap_or_else(|| BlockFloorRecord::new(to_ms));
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
                        self.keep_floor_view(&read).await?;
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

    /// Apply local effects with the shared raise, or directly if a sufficient floor
    /// already applied. An unanswered batch keeps its local effects with it.
    pub(crate) async fn commit_with_floor(&mut self, to_ms: u64, tx: Tx) -> Result<()> {
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
        let owed = self
            .local_get::<PendingUnblocks>(key::BLOCK_FLOOR)
            .await?
            .map(|pending| pending.to_ms);
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
        assert_eq!(shared_floor_of(None, None, none), SharedFloor::Known(None));
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            shared_floor_of(None, Some(&seen), none),
            SharedFloor::Known(Some(70))
        );
        let malformed = RecordContent::Value(br#"{"v":1}"#.to_vec());
        assert_eq!(
            shared_floor_of(Some(&held(malformed, true, false)), None, none),
            SharedFloor::Unknown
        );
    }

    #[test]
    fn test_floor_applies_the_highest_of_the_value_the_mark_and_the_kept_floor() {
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        let kept = FloorKept {
            seen_ms: Some(90),
            divergent: false,
        };
        // A later lower write, an in-place restore or a lost mark never lowers the floor.
        let current = held(value(50), true, false);
        assert_eq!(
            shared_floor_of(Some(&current), Some(&seen), kept),
            SharedFloor::Known(Some(90))
        );
        assert_eq!(
            shared_floor_of(None, None, kept),
            SharedFloor::Known(Some(90))
        );
        let higher = held(value(120), true, false);
        assert_eq!(
            shared_floor_of(Some(&higher), Some(&seen), kept),
            SharedFloor::Known(Some(120))
        );
    }

    #[test]
    fn test_floor_unauthenticated_delete_keeps_the_floor_seen() {
        // Paykit wire 5.6: the service's word, never absence. With a floor seen it keeps it;
        // with none it can't be read.
        let delete = held(RecordContent::Deleted, false, false);
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            shared_floor_of(Some(&delete), Some(&seen), FloorKept::default()),
            SharedFloor::Known(Some(70))
        );
        assert_eq!(
            shared_floor_of(Some(&delete), None, FloorKept::default()),
            SharedFloor::Unknown
        );
        // An authenticated one, which the permanent policy forbids, holds whatever was seen.
        let authenticated = held(RecordContent::Deleted, true, false);
        assert_eq!(
            shared_floor_of(Some(&authenticated), Some(&seen), FloorKept::default()),
            SharedFloor::Unknown
        );
    }

    #[test]
    fn test_floor_divergence_holds_over_every_value() {
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        // A divergent view holds, however high the floor it shows.
        let divergent = held(value(u64::MAX / 2), true, true);
        assert_eq!(
            shared_floor_of(Some(&divergent), Some(&seen), FloorKept::default()),
            SharedFloor::Divergent
        );
        // A kept divergence holds over a cleared view, a missing record and any later value.
        let kept = FloorKept {
            seen_ms: Some(70),
            divergent: true,
        };
        for current in [None, Some(held(value(10_000), true, false))] {
            assert_eq!(
                shared_floor_of(current.as_ref(), Some(&seen), kept),
                SharedFloor::Divergent
            );
        }
    }

    #[test]
    fn test_floor_kept_only_grows() {
        let a = FloorKept {
            seen_ms: Some(70),
            divergent: true,
        };
        let b = FloorKept {
            seen_ms: Some(90),
            divergent: false,
        };
        let both = FloorKept {
            seen_ms: Some(90),
            divergent: true,
        };
        assert_eq!(a.with(b), both);
        assert_eq!(b.with(a), both);
        assert_eq!(FloorKept::default().with(a), a);
        assert!(FloorKept::default().is_empty());
        let view = held(value(80), true, true);
        assert_eq!(
            FloorKept::of_view(&view),
            FloorKept {
                seen_ms: Some(80),
                divergent: true
            }
        );
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
