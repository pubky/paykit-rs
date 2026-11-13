//! The block floor (Paykit wire 5.6; Paykit spec 8.1, "Blocked counterparties").
//!
//! A message dropped because the user blocked its sender is asked for again once the sender
//! is unblocked (`marks.rs`). A request that comes back that way is routed to every current
//! payer app of the identity, not only to the app whose mark asked: a newly added app, or one
//! that holds no mark of the sender, can receive it first. So the time before which a
//! one-time request is paid only by hand is identity-shared: the `block_floor` record, one per
//! identity, under `pay`, which every app that pays holds (Paykit spec 3.1:
//! `allowance_execute` requires `pay`). Its floor only rises: a writer raises it, a reader
//! refuses a later write of a lower one, and it never expires. A request created now is dated
//! above every floor, so nothing fresh is held by it.
//!
//! - **Raised before the ask.** Every `resend_request` this app sends goes through
//!   [`Paykit::queue_resend_request`]: when its counterparty holds a live block mark here, or
//!   the app owes a raise (`p/bf`, below), the raise to the mark's latest time, and to the raise owed, goes
//!   in the same batch as the queued ask, so the ask goes only once the raise is applied, and
//!   the raise is in the identity's chain before any entry the ask brings back (Switch spec
//!   8.3: one order per generation). While the app can't read the floor, the ask waits.
//! - **The raise owed** (`p/bf`): a mark that goes for the bound or its expiry, and a restore's
//!   first read of the lists (`marks.rs`), fold their time into it, in the commit that makes
//!   them; it is written at the next ask, or before the block marks ask, and deleted in the
//!   batch that writes it. This app applies it as a floor meanwhile.
//! - **Applied by every payer app at record creation:** a one-time request whose signed
//!   `created_at` is earlier than the floor plus the clock tolerance opens its record with
//!   `manual_only`, in the receive step and in a later write. The floor an app holds at an
//!   entry may lag a raise made after the one before that entry: a write superseded since is
//!   shown as its stub (Switch spec 8.3), and its value comes with the later one.
//! - **Applied before every automatic attempt:** no app pays a one-time request dated before
//!   the floor plus the tolerance automatically, whatever its record holds. Read after the
//!   app's read of the feed, the floor is at least every raise applied before an entry the
//!   app took, so this one holds whatever the order of the apps' reads. And immediately
//!   before each automatic dispatch the record is read again from the switch
//!   ([`Paykit::floor_holds_fresh`]), as the deny list is: a raise the app's copy doesn't
//!   hold yet, applied before the attempt's commit or after it, still holds the dispatch. A
//!   raise after the dispatch began is too late for that payment.
//! - **Unknown holds:** while the floor can't be read (the payer's records not listed yet, its
//!   value sealed under a key not held, or a value that doesn't read with none seen before),
//!   every one-time request counts as dated before it.

use paykit_lib::derive::natural_key;
use paykit_lib::records::{BlockFloorRecord, StoredMessage};
use paykit_lib::Body;
use pubky_switch_client::app::{OpBatch, RecordContent, RecordTarget, SharedRecord, SignedMessage};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::CLOCK_TOLERANCE_MS;

use crate::activity;
use crate::error::{Refusal, Result};
use crate::hold::RecordSide;
use crate::marks::{DropMark, MarkFamily};
use crate::paykit::Paykit;
use crate::state::{key, Tx};
use paykit_lib::collections::Collection;

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
}

/// The floor of the record `held`, with `seen`, the mark of the newest value the app took that
/// reads (`Protocol::record_seen`): the higher of the two. A delete, or a value that doesn't
/// read, keeps the floor seen; with none seen, the floor is unknown. A value sealed under a
/// key the app doesn't hold may be higher than any seen: unknown.
pub(crate) fn shared_floor_of(held: Option<&SharedRecord>, seen: Option<&[u8]>) -> SharedFloor {
    let seen = seen
        .and_then(|value| BlockFloorRecord::read(value).ok())
        .map(|record| record.floor_ms);
    let Some(held) = held else {
        return SharedFloor::Known(seen);
    };
    match &held.content {
        RecordContent::Value(value) => match BlockFloorRecord::read(value) {
            Ok(record) => SharedFloor::Known(Some(
                seen.map_or(record.floor_ms, |s| s.max(record.floor_ms)),
            )),
            Err(_) => seen.map_or(SharedFloor::Unknown, |s| SharedFloor::Known(Some(s))),
        },
        RecordContent::Deleted => {
            seen.map_or(SharedFloor::Unknown, |s| SharedFloor::Known(Some(s)))
        }
        RecordContent::Unreadable(_) => SharedFloor::Unknown,
    }
}

/// Whether `stored`, a one-time request, is dated before the floor (module docs): its signed
/// `created_at` is earlier than the higher of `shared` and `owed`, the raise this app owes,
/// plus the clock tolerance. An unknown floor holds every one.
pub(crate) fn dated_before(stored: &StoredMessage, shared: SharedFloor, owed: Option<u64>) -> bool {
    if !matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring()) {
        return false;
    }
    let SharedFloor::Known(floor) = shared else {
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
        let seen = self.app.record_seen(&key).await?;
        Ok(shared_floor_of(held.as_ref(), seen.as_deref()))
    }

    /// Whether `stored` is a one-time request dated before the floor, shared or owed, as this
    /// app holds them now ([`dated_before`]): it is paid only by hand.
    pub(crate) async fn floor_holds(&self, stored: &StoredMessage) -> Result<bool> {
        if !matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring()) {
            return Ok(false);
        }
        let owed = self.local_get::<u64>(key::BLOCK_FLOOR).await?;
        Ok(dated_before(stored, self.shared_floor().await?, owed))
    }

    /// Whether `stored` is a one-time request dated before the floor as a read of the record
    /// made now, from the switch, shows it, with the floor this app holds and a raise it owes
    /// (module docs, "Applied before every automatic attempt"): the check immediately before
    /// an automatic dispatch, as the deny list is read again there (Paykit spec 8.1, How fresh
    /// the deny list is). It waits for no raise. A read that fails, or a value that doesn't
    /// read with none seen, holds the request.
    pub(crate) async fn floor_holds_fresh(&mut self, stored: &StoredMessage) -> Result<bool> {
        if !matches!(stored.body(), Body::PaymentRequest(r) if !r.is_recurring()) {
            return Ok(false);
        }
        let owed = self.local_get::<u64>(key::BLOCK_FLOOR).await?;
        let target = floor_target(&self.identity());
        let Ok(read) = self.app.read_record(&target).await else {
            return Ok(true);
        };
        let key = self.app.record_key(&target)?;
        let seen = self.app.record_seen(&key).await?;
        Ok(dated_before(
            stored,
            shared_floor_of(read.as_ref(), seen.as_deref()),
            owed,
        ))
    }

    /// The time the floor must reach before an ask to `to` goes: its live block mark's latest
    /// time and the raise owed, the higher; `None` when neither is kept, or the app doesn't
    /// hold `pay`: it opens no payer record, so its marks raise nothing (`crate::floor`).
    async fn floor_owed_for(&self, to: Option<&[u8; 32]>) -> Result<Option<u64>> {
        if !self.holds("pay") {
            return Ok(None);
        }
        let owed = self.local_get::<u64>(key::BLOCK_FLOOR).await?;
        let mark = match to {
            Some(to) => self
                .local_get::<DropMark>(&MarkFamily::Blocked.key(to))
                .await?
                .map(|mark| mark.latest_ms),
            None => None,
        };
        Ok(owed.max(mark))
    }

    /// The shared floor for a write: the copy's, or, while the payer's records aren't listed
    /// or the copy can't open it, the record as the switch holds it now, read alone. A read
    /// that fails leaves it unknown, as does a scope key the app waits for.
    async fn floor_for_write(&mut self) -> Result<SharedFloor> {
        // Sealed under `pay` at the current data epoch: without that key nothing is written,
        // and nothing read while the copy waits for a wider certificate's keys.
        if !self.holds_scope_keys() {
            return Ok(SharedFloor::Unknown);
        }
        let held = self.shared_floor().await?;
        if held != SharedFloor::Unknown {
            return Ok(held);
        }
        let target = floor_target(&self.identity());
        let Ok(read) = self.app.read_record(&target).await else {
            return Ok(SharedFloor::Unknown);
        };
        let key = self.app.record_key(&target)?;
        let seen = self.app.record_seen(&key).await?;
        Ok(shared_floor_of(read.as_ref(), seen.as_deref()))
    }

    /// The batch that raises the floor to `to_ms` and deletes the raise owed, with nothing
    /// else yet; `None` when the floor is at `to_ms` already, after the raise owed is
    /// deleted if one is kept.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the floor can't be read; the client's errors.
    async fn floor_batch(&mut self, to_ms: u64) -> Result<Option<OpBatch>> {
        let SharedFloor::Known(floor) = self.floor_for_write().await? else {
            return Err(unreadable_floor());
        };
        let target = floor_target(&self.identity());
        let key = self.app.record_key(&target)?;
        let held = self.app.shared_record(&key).await?;
        let mut owed = Tx::default();
        if self
            .local_get::<u64>(key::BLOCK_FLOOR)
            .await?
            .is_some_and(|o| o <= to_ms)
        {
            owed.delete(key::BLOCK_FLOOR);
        }
        if floor.is_some_and(|floor| floor >= to_ms) {
            let writes = owed.into_writes();
            if !writes.is_empty() {
                self.app.protocol_commit(&writes).await?;
            }
            return Ok(None);
        }
        // Built on the value held, whose unknown members it keeps; on the floor seen when the
        // value held doesn't read.
        let base = held
            .as_ref()
            .and_then(|held| held.content.value())
            .and_then(|value| BlockFloorRecord::read(value).ok())
            .unwrap_or_else(|| BlockFloorRecord::new(floor.unwrap_or(0)));
        let raised = base
            .raised(to_ms)
            .unwrap_or_else(|| BlockFloorRecord::new(to_ms));
        let entry = self
            .app
            .record_op(&target, Some(&raised.to_json()?), Vec::new())
            .await?;
        Ok(Some(OpBatch::new().entry(entry).locals(owed.into_writes())))
    }

    /// Submit `batch`; after a `409 conflict` the floor is read again before the error
    /// returns, so the next try builds on what the switch holds.
    async fn submit_floor(&mut self, batch: OpBatch) -> Result<()> {
        match self.app.submit_batch(batch).await {
            Ok(_) => Ok(()),
            Err(error) => {
                if error.code() == Some(ErrorCode::Conflict) {
                    let target = floor_target(&self.identity());
                    self.app.read_record(&target).await?;
                }
                Err(error.into())
            }
        }
    }

    /// Queue `signed`, a `resend_request` to `to`, the one way this app queues one (module
    /// docs, "Raised before the ask"): with the raise of the floor that its live block mark
    /// of `to` and the raise owed need, in one batch, so the ask is queued only if the raise
    /// is applied. Returns the `msg_id`. Nothing is sent: the caller flushes.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while a raise is needed and the floor can't be read, or its write
    /// fails other than by a conflict, with nothing queued and the raise still owed; a `409
    /// conflict` after the floor is read again; the client's errors queuing an ask that needs
    /// no raise.
    pub(crate) async fn queue_resend_request(
        &mut self,
        signed: SignedMessage,
        to: &[u8; 32],
    ) -> Result<String> {
        let msg_id = signed.msg_id().to_owned();
        let batch = match self.floor_owed_for(Some(to)).await? {
            Some(to_ms) => self.floor_batch(to_ms).await?,
            None => None,
        };
        match batch {
            Some(batch) => match self.submit_floor(batch.queue(signed)).await {
                Ok(()) => {}
                Err(error) if error.code() == Some(ErrorCode::Conflict) => return Err(error),
                // Any other failure of the raise queues nothing: the ask waits as for a floor
                // it can't read, and the raise stays owed.
                Err(_) => return Err(unreadable_floor()),
            },
            None => {
                self.app.queue_signed(signed).await?;
            }
        }
        Ok(msg_id)
    }

    /// Write the raise owed, when one is kept and the floor can be read, before the block
    /// marks ask (module docs, "The raise owed"). One that can't be written now, for any
    /// reason, stays owed, for the next pass or the next ask, which waits for it: this pass
    /// marks nothing done.
    pub(crate) async fn write_floor_owed(&mut self) -> Result<()> {
        let Some(owed) = self.floor_owed_for(None).await? else {
            return Ok(());
        };
        // A raise that can't be built or written now stays owed: every ask waits for it
        // (`Paykit::queue_resend_request`), so nothing the floor must cover goes first.
        let Ok(Some(batch)) = self.floor_batch(owed).await else {
            return Ok(());
        };
        let _ = self.submit_floor(batch).await;
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
        let notes = self.local_scan::<bool>(key::FLOOR_OPENED).await?;
        if notes.is_empty() {
            return Ok(());
        }
        let floor = self.shared_floor().await?;
        if floor == SharedFloor::Unknown {
            return Ok(());
        }
        let owed = self.local_get::<u64>(key::BLOCK_FLOOR).await?;
        let own = self.identity();
        for (rest, _) in notes {
            let note = format!("{}{rest}", key::FLOOR_OPENED);
            let record_key = pubky_switch_core::enc::unb64_array::<32>(&rest)
                .map_err(|_| crate::error::PaykitError::corrupt())?;
            let held = self.app.shared_record(&record_key).await?;
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
fn unreadable_floor() -> crate::error::PaykitError {
    Refusal::NotNow(UNREADABLE_FLOOR.into()).into()
}

/// Whether `error` is [`unreadable_floor`]'s: the ask that needs the raise waits.
pub(crate) fn is_unreadable_floor(error: &crate::error::PaykitError) -> bool {
    matches!(error.refusal(), Some(Refusal::NotNow(why)) if why == UNREADABLE_FLOOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_floor_unknown_without_a_value_or_a_seen_floor() {
        assert_eq!(shared_floor_of(None, None), SharedFloor::Known(None));
        let seen = BlockFloorRecord::new(70).to_json().unwrap();
        assert_eq!(
            shared_floor_of(None, Some(&seen)),
            SharedFloor::Known(Some(70))
        );
    }
}
