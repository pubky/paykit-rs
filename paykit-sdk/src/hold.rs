//! The hold after a restore (Paykit spec 7.2, "What a restore loses"), one-time branch, and
//! the same hold after a `missed` item.
//!
//! A backup can predate a payee's cancellation of a request it holds open: the restored app
//! would see the request payable. So every restore from a backup holds every open request,
//! as the restore itself sets it: [`crate::protocol::PaykitProtocol`]'s restore step adds the
//! hold to the writes it installs, in the restore's one commit, so no restore can leave it
//! out; it is no part of what a backup carries, and a confirmation an older backup holds is
//! dropped by the restore. A `missed` item (Switch spec 8.2) shows the service deleted
//! entries before the app read them, a payee's cancellation possibly among them: the
//! protocol's `missed` step sets the same hold (`mh`) and drops every confirmation of an
//! earlier one, in the commit that passes the item. While a request is held:
//!
//! - no automatic attempt is made on it, and a dispatch made before is not repeated at a
//!   resume;
//! - a manual one only once the user has seen that the payee may have cancelled it
//!   ([`crate::PayOptions::possibly_cancelled_confirmed`]), which ends the hold for that
//!   request;
//! - a cancellation the record shows, or the app keeps, stops it for good, and no
//!   confirmation overrides it (Paykit spec 8.1).
//!
//! After the restore, at the reconciliation it requires, once the records are read again,
//! the app asks every counterparty with an open request to re-send what its inbox accepted
//! from the acceptance time of the last entry the backup's cursor had passed, less the
//! clock tolerance (Paykit spec 9). A cancellation that comes back is kept
//! and placed as any other.

use paykit_lib::collections::Collection;
use paykit_lib::records::ActivityRecord;
use pubky_switch_client::app::Protocol as _;
use pubky_switch_client::app::{
    AppError, Collection as SwitchCollection, PolicyFamily, RecordContent,
    Refusal as ClientRefusal, Unopened,
};
use pubky_switch_core::http::ErrorCode;
use pubky_switch_core::records::{AppCert, CLOCK_TOLERANCE_MS};
use pubky_switch_core::scopes::Permission;
use pubky_switch_core::sig::SignedDoc;
use serde::{Deserialize, Serialize};

use crate::error::{PaykitError, Refusal, Result};
use crate::paykit::Paykit;
use crate::requests::{cancelled, rejected, RequestStatus};
use crate::state::{key, Tx};

/// The hold a restore set (`p/rh`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestoreHold {
    /// From when the counterparties are asked to re-send: the acceptance time of the last
    /// entry the backup's cursor passed, less the clock tolerance.
    pub(crate) resend_from_ms: u64,
    /// The re-sends are all queued.
    pub(crate) asks_queued: bool,
    /// The counterparties asked already while a record of the copy was sealed, in
    /// increasing order ([`Paykit::queue_restore_asks`]). A hold written before this member
    /// existed, a valid run's form, reads as having asked none.
    #[serde(default)]
    pub(crate) asked: Vec<[u8; 32]>,
}

impl RestoreHold {
    /// The hold of a restore whose backup's cursor had passed entries accepted up to
    /// `cursor_time_ms`.
    pub(crate) fn of(cursor_time_ms: u64) -> Self {
        Self {
            resend_from_ms: cursor_time_ms.saturating_sub(CLOCK_TOLERANCE_MS),
            asks_queued: false,
            asked: Vec::new(),
        }
    }
}

/// The hold a `missed` item set (`p/mh`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MissedHold {
    /// A time no earlier than the last acceptance time of the entries the item tells of:
    /// the item's own last acceptance time (`MissedEntries::accepted_until_ms`), or, for an
    /// item kept owed by a build that didn't keep it, the highest service time verified once
    /// the read that took the item returned ([`Paykit::bound_missed_hold`]). `None` until
    /// then, when every open request is held.
    pub(crate) bound_ms: Option<u64>,
}

impl MissedHold {
    /// The hold of an item whose last acceptance time is `accepted_until_ms`: bounded by it,
    /// or unbounded until the read returns when the client knows no end (`u64::MAX`).
    pub(crate) fn of_item(accepted_until_ms: u64) -> Self {
        Self {
            bound_ms: (accepted_until_ms != u64::MAX).then_some(accepted_until_ms),
        }
    }

    /// Whether the open request `request` is held (Paykit spec 7.2, 9). One the app held at
    /// the item, its inbox having accepted it no later than the bound, is held. One first
    /// seen after the item is held only when its signed `created_at` is earlier than the
    /// bound plus the clock tolerance: a request created later can't have had its
    /// cancellation among the entries missed. A request with no acceptance time is held.
    pub(crate) fn holds(&self, request: &paykit_lib::records::StoredMessage) -> bool {
        let Some(bound) = self.bound_ms else {
            return true;
        };
        let held_at_item = request.accepted_at().is_none_or(|at| at <= bound);
        held_at_item || request.message().created_at_ms < bound.saturating_add(CLOCK_TOLERANCE_MS)
    }
}

/// Which side waits for a wider certificate's scope keys ([`Paykit::scope_keys_awaited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeysAwaited {
    /// The payer's records, claims or `pay` key: a resume writes nothing to the switch.
    PayerSide,
    /// Only records the payer's side doesn't read: a resume goes on.
    PayeeSideOnly,
}

/// A side of the shared records, by the collections it reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordSide {
    /// `payer_activity`, `payment_lists` and `block_floor`.
    Payer,
    /// `payee_activity`, every label's.
    Payee,
    /// `allowances`: the Allowance records, on their own side (Paykit spec 8.1, "Records
    /// not listed yet"; Paykit spec 8.3).
    Allowances,
    /// Every collection of Paykit's.
    All,
}

impl RecordSide {
    /// The collections of the side.
    pub(crate) fn collections(self) -> Vec<SwitchCollection> {
        let payer = crate::activity::family(Collection::PayerActivity);
        let payee = crate::activity::family(Collection::PayeeActivity);
        let lists = crate::activity::family(Collection::PaymentLists);
        let floor = crate::activity::family(Collection::BlockFloor);
        let allowances = crate::activity::family(Collection::Allowances);
        match self {
            Self::Payer => vec![payer, lists, floor],
            Self::Payee => vec![payee],
            Self::Allowances => vec![allowances],
            Self::All => vec![payer, payee, lists, floor, allowances],
        }
    }
}

/// Whether `scope` reads `collection`, or a member of it: its read scope, or, in a sub-scope
/// family, one of its sub-scopes (Switch spec 3.3, 10.3).
fn read_by_scope(collection: &SwitchCollection, scope: &str) -> bool {
    scope == collection.policy.read_scope
        || (collection.family == PolicyFamily::SubScopes
            && pubky_switch_core::scopes::split(scope)
                .is_some_and(|(name, sub)| sub.is_some() && name == collection.policy.read_scope))
}

/// Whether the app's copy of `side`'s records may lack records, so a record's absence there
/// decides nothing yet (Paykit spec 8.1, "Records not listed yet"): a restore's or a `missed`
/// item's mark (`marked`, `p/nl` or `p/ml`), no certificate kept as listed under (`p/lc`),
/// or `held`, the certificate held now, another than `listed` while it reads a collection
/// of the side, or a member of one. The one rule for every caller: [`Paykit::unlisted`],
/// [`Paykit::payer_waits`], and the receive step, which reads the same state through its
/// context.
///
/// Any other certificate counts, not only one that reads more than `listed`: one held
/// between the two may have read less, and the copy then lacks what was written meanwhile
/// though `held` reads no more than `listed`. Nothing of Paykit's can say which certificates
/// were held between: the client takes each in a commit of its own, and one read can take
/// several, at its start and at each `identity.json` that lists a renewal it keeps (Switch
/// spec 3.2). So only a listing ends it, which keeps the certificate it ran under
/// ([`Paykit::list_records`]); a side `held` reads none of lacks nothing it reads.
pub(crate) fn side_unlisted(
    side: RecordSide,
    marked: bool,
    listed: Option<&AppCert>,
    held: &AppCert,
) -> bool {
    marked
        || listed.is_none_or(|listed| {
            listed.hash() != held.hash()
                && side
                    .collections()
                    .iter()
                    .any(|collection| collection.readable_by(held))
        })
}

/// Why the shared records aren't listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unlisted {
    /// A restore set `p/nl`: the client owes a listing too, which its next read makes
    /// before the feed (Switch spec 12.4 step 2.1).
    Restore,
    /// A `missed` item set `p/ml` (Switch spec 8.2): the client lists again at the start of
    /// its next read, so entries later in the read that passed the item, and calls before
    /// the next one, would decide from the copy as it was. A listing Paykit makes
    /// ([`Paykit::list_records`]) ends this, at the end of that receive.
    Missed,
    /// The certificate the app holds is another than the one the records were last listed
    /// under (`p/lc`), or none is kept ([`side_unlisted`]). A client owes a listing too for
    /// one that reads collections, or members of one, that the certificate before it didn't
    /// (Switch spec 8.3), made at the start of a later read once it holds the scope keys,
    /// but tells no caller when it made it: a listing Paykit makes ([`Paykit::list_records`])
    /// ends this.
    Certificate,
}

impl Paykit {
    /// The certificate the shared records were last listed under (`p/lc`), if one is kept.
    async fn listed_cert(&self) -> Result<Option<AppCert>> {
        let Some(signed) = self.local_get::<SignedDoc>(key::LISTED_CERT).await? else {
            return Ok(None);
        };
        AppCert::verify(&signed, &self.identity())
            .map(Some)
            .map_err(|_| PaykitError::corrupt())
    }

    /// Why the shared records aren't listed, if they aren't: a restore's mark, a `missed`
    /// item's, or the certificate held now being another than the one last listed under.
    /// The last is read from what the store holds, the client's certificate and `p/lc`,
    /// which only a listing writes, so a read that takes a certificate and fails, a failed
    /// write, a crash or a restart before the next listing loses nothing.
    pub(crate) async fn unlisted(&self) -> Result<Option<Unlisted>> {
        if self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
        {
            return Ok(Some(Unlisted::Restore));
        }
        if self
            .local_get::<bool>(key::MISSED_UNLISTED)
            .await?
            .is_some()
        {
            return Ok(Some(Unlisted::Missed));
        }
        Ok(self
            .side_unlisted(RecordSide::All)
            .await?
            .then_some(Unlisted::Certificate))
    }

    /// [`side_unlisted`] for `side`, from what the store holds. Under a certificate Paykit
    /// refuses too: with none kept as listed under, as after a join, the copy may be empty,
    /// and a call that only reads is refused rather than answer from it.
    pub(crate) async fn side_unlisted(&self, side: RecordSide) -> Result<bool> {
        let marked = self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
            || self
                .local_get::<bool>(key::MISSED_UNLISTED)
                .await?
                .is_some();
        let listed = self.listed_cert().await?;
        Ok(side_unlisted(
            side,
            marked,
            listed.as_ref(),
            self.app.cert(),
        ))
    }

    /// The certificate the records were last listed under, when the one held now leaves
    /// them unlisted ([`side_unlisted`]). With none kept, as after a join or a restore, a
    /// certificate that passes now leaves every side unlisted: its records are listed once.
    async fn unlisted_since(&self) -> Result<Option<Option<AppCert>>> {
        match self.listed_cert().await? {
            None if self.certificate_refused().is_none() => Ok(Some(None)),
            None => Ok(None),
            Some(listed) => {
                Ok(
                    side_unlisted(RecordSide::All, false, Some(&listed), self.app.cert())
                        .then_some(Some(listed)),
                )
            }
        }
    }

    /// Whether the app waits for a certificate's scope keys, which the signer stores when it
    /// is next online (Switch spec 3.5), and on which side: the records aren't listed under
    /// it ([`side_unlisted`]), and can't be until the keys come. The payer's side waits when
    /// the app lacks the current key of `pay`, the certificate reads claims the one listed
    /// under didn't, or the payer's side isn't listed under it; otherwise only the payee's
    /// side does, which a payment's resume doesn't touch.
    pub(crate) async fn scope_keys_awaited(&self) -> Result<Option<KeysAwaited>> {
        if self.holds_scope_keys() {
            return Ok(None);
        }
        let Some(since) = self.unlisted_since().await? else {
            return Ok(None);
        };
        let held = self.app.cert();
        // Nothing listed yet: every side waits.
        let Some(listed) = since else {
            return Ok(Some(KeysAwaited::PayerSide));
        };
        // Without the current `pay` key the payer's records can't be written, as after a data
        // epoch rotation.
        let pay_key = self.app.scope_key(self.app.data_epoch(), "pay").is_some();
        // Claims read now and not under the certificate listed under. Today this follows from
        // the payer collections' term or from `pay` not held, where a resume reads nothing:
        // `claims.read` comes with `pay`, which reads `payer_activity`, or with `allowance`
        // (paykit-lib `scopes.rs`), which doesn't hold `pay`. It is kept so the rule doesn't
        // lean on that table.
        let claims = !listed.has_permission(Permission::ClaimsRead)
            && held.has_permission(Permission::ClaimsRead);
        let payer =
            !pay_key || claims || side_unlisted(RecordSide::Payer, false, Some(&listed), held);
        Ok(Some(if payer {
            KeysAwaited::PayerSide
        } else {
            KeysAwaited::PayeeSideOnly
        }))
    }

    /// Whether the payer's side waits ([`KeysAwaited`], `keys` as
    /// [`Paykit::scope_keys_awaited`] gave it): the payer's records, claims or `pay` key
    /// wait, or keys wait while the payer's side of the copy isn't listed yet
    /// ([`side_unlisted`]), as a restore's isn't. Nothing built on the payer's records is
    /// written then, at a resume or a receive.
    pub(crate) async fn payer_waits(&self, keys: Option<KeysAwaited>) -> Result<bool> {
        Ok(keys == Some(KeysAwaited::PayerSide)
            || (keys.is_some() && self.side_unlisted(RecordSide::Payer).await?))
    }

    /// Whether the app holds, at the current data epoch, the key of every scope of its
    /// certificate that reads a collection of Paykit's (Switch spec 3.5): a certificate
    /// just taken that reads more gets them with the key bundle the next read fetches, and
    /// a listing before then keeps values it can't open, so it waits for them. The client
    /// skips its own listing exactly while this doesn't hold, so it also tells whether a
    /// read listed ([`Paykit::records_listed`]).
    pub(crate) fn holds_scope_keys(&self) -> bool {
        let epoch = self.app.data_epoch();
        let collections = RecordSide::All.collections();
        self.app
            .cert()
            .scopes()
            .iter()
            .filter(|scope| self.protocol.has_keys(scope))
            .filter(|scope| collections.iter().any(|c| read_by_scope(c, scope)))
            .all(|scope| self.app.scope_key(epoch, scope).is_some())
    }

    /// Whether the request record at `record_key`, `record`, is held after a restore or a
    /// `missed` item: one of them set the hold, the user hasn't confirmed this request
    /// since, and it is open.
    /// After a restore every open request is held, one first seen after the restore too: no
    /// time the app can see proves a request new, since its creation time is the
    /// counterparty's and a request from before the backup can come again with a later
    /// acceptance time (Paykit spec 7.2, "What a restore loses": nothing releases the hold
    /// automatically). After a `missed` item, a request first seen after it is held only
    /// when created before the item's last acceptance time plus the clock tolerance
    /// ([`MissedHold::holds`]).
    pub(crate) async fn held(
        &self,
        record_key: &[u8; 32],
        record: &ActivityRecord,
    ) -> Result<bool> {
        let restored = self
            .local_get::<RestoreHold>(key::RESTORE_HOLD)
            .await?
            .is_some();
        if !restored {
            let Some(missed) = self.local_get::<MissedHold>(key::MISSED_HOLD).await? else {
                return Ok(false);
            };
            let Some((request, _)) = crate::requests::request_of(record) else {
                return Ok(false);
            };
            if !missed.holds(request) {
                return Ok(false);
            }
        }
        if record.shows_paid() || cancelled(record) || rejected(record) {
            return Ok(false);
        }
        Ok(self
            .local_get::<bool>(&key::hold_confirmed(record_key))
            .await?
            .is_none())
    }

    /// Bound a `missed` item's hold set by the read that just returned with no last
    /// acceptance time ([`MissedHold::bound_ms`]) by the highest service time verified: the
    /// item came no later. Without a verified time, it stays unbounded.
    pub(crate) async fn bound_missed_hold(&mut self) -> Result<()> {
        let Some(hold) = self.local_get::<MissedHold>(key::MISSED_HOLD).await? else {
            return Ok(());
        };
        if hold.bound_ms.is_some() {
            return Ok(());
        }
        let Some(bound) = self.service_time() else {
            return Ok(());
        };
        let hold = MissedHold {
            bound_ms: Some(bound),
        };
        self.local_put(key::MISSED_HOLD, &hold).await
    }

    /// The user has seen that the payee may have cancelled `request` since the backup the
    /// app was restored from, or a `missed` item, and lets its payment go on: the request's
    /// hold ends, as a payment confirmed with
    /// [`crate::PayOptions::possibly_cancelled_confirmed`] ends it, so a dispatch made before
    /// the hold is repeated at a later resume (Paykit spec 7.2). It never overrides a
    /// cancellation the record shows or the app keeps. A later `missed` item holds the
    /// request again.
    ///
    /// # Errors
    ///
    /// [`Refusal::NotNow`] while the app can't tell whether it holds the request: its record
    /// is sealed under a scope key the app doesn't hold yet, or a value that doesn't read, or
    /// it is missing from a copy that may lack records; [`Refusal::NotFound`] when the copy
    /// holds no record of it. A later restore holds the request again all the same. A store
    /// failure.
    pub async fn confirm_possibly_cancelled(
        &mut self,
        request: &crate::requests::RequestRef,
        _confirmed: pubky_switch_client::app::UserConfirmed,
    ) -> Result<()> {
        self.refresh_own()?;
        let target = crate::activity::ActivityTarget::payer(
            paykit_lib::derive::natural_key::request_record(&request.payee, &request.request_hash),
        );
        if self.held_activity(&target).await?.is_none() {
            return Err(self.request_record_refusal(&target).await?);
        }
        let record_key = self.activity_key(&target)?;
        self.local_put(&key::hold_confirmed(&record_key), &true)
            .await
    }

    /// A restored time above the backup's highest verified service time plus the clock
    /// tolerance is clamped to that bound: the cursor time (`ct`), with the hold's re-send
    /// time, the two times of each mark of dropped messages (`pd/`, `ud/`, `kw/`), and the
    /// acceptance time of each stop (`kc/`, an Allowance's `ae/`) and kept reply (`pr/`). A
    /// cursor time ahead of
    /// any the backup verified would ask counterparties to re-send from the future, so
    /// nothing would come back (Paykit spec 7.2, "What a restore loses"); a mark or a kept
    /// message dated there would never expire. Clamped, not refused: the times decide only
    /// where the re-sends start and when the items go, and a restore must not fail on them.
    /// Without a verified time the device clock is the bound. Each value is read bound to its
    /// key first, as a restore reads it.
    pub(crate) async fn bound_restored_cursor_time(&mut self) -> Result<()> {
        let bound = self
            .service_time()
            .unwrap_or_else(|| self.now())
            .saturating_add(CLOCK_TOLERANCE_MS);
        let mut tx = Tx::default();
        if let Some(cursor) = self.local_get::<u64>(key::CURSOR_TIME).await? {
            if cursor > bound {
                tx.put(key::CURSOR_TIME, &bound)?;
                if let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? {
                    hold.resend_from_ms = RestoreHold::of(bound).resend_from_ms;
                    tx.put(key::RESTORE_HOLD, &hold)?;
                }
            }
        }
        for family in [
            crate::marks::MarkFamily::Replies,
            crate::marks::MarkFamily::Unverified,
            crate::marks::MarkFamily::Blocked,
        ] {
            for (rest, mut mark) in self
                .local_scan::<crate::marks::DropMark>(family.prefix())
                .await?
            {
                if mark.clamp(bound) {
                    tx.put(&format!("{}{rest}", family.prefix()), &mark)?;
                }
            }
        }
        let family = crate::marks::MarkFamily::KeyWaits;
        for (rest, mut held) in self
            .local_scan::<crate::marks::KeyWaitMark>(family.prefix())
            .await?
        {
            if held.mark.clamp(bound) {
                tx.put(&format!("{}{rest}", family.prefix()), &held)?;
            }
        }
        // So is the raise of the block floor owed (`crate::floor`).
        if let Some(floor) = self.local_get::<u64>(key::BLOCK_FLOOR).await? {
            if floor > bound {
                tx.put(key::BLOCK_FLOOR, &bound)?;
            }
        }
        // So is a widening of the block marks owed: one from the backup's cursor time, dated
        // past the bound, starts at the bound, at its own key.
        for (_, window) in self
            .local_scan::<crate::marks::BlockedWindow>(key::BLOCKED_WIDENINGS)
            .await?
        {
            if window.from_ms > bound {
                tx.delete(&key::blocked_widening(window.from_ms));
                // Its end was at or past its start, so past the bound too.
                let until = if window.until_ms == crate::marks::BlockedWindow::TO_NOW {
                    window.until_ms
                } else {
                    bound
                };
                tx.put(
                    &key::blocked_widening(bound),
                    &crate::marks::BlockedWindow {
                        from_ms: bound,
                        until_ms: until,
                    },
                )?;
            }
        }
        for prefix in [
            key::KEPT_CANCELLATIONS,
            key::KEPT_REPLIES,
            key::ALLOWANCE_ENDS,
        ] {
            for (rest, mut kept) in self
                .local_scan::<crate::records::KeptMessage>(prefix)
                .await?
            {
                if kept.bound_acceptance(bound) {
                    tx.put(&format!("{prefix}{rest}"), &kept)?;
                }
            }
        }
        self.local_commit(tx).await
    }

    /// The shared records known, not an empty copy: after a restore the client's copy is
    /// empty until they are listed again (Switch spec 12.4 step 2.1), and under another
    /// certificate than the one they were listed under it may lack what was written since,
    /// so while either holds they are listed first. A listing the network or the switch couldn't
    /// answer now refuses the call as not now: a record's absence then decides nothing,
    /// never "gone". Any other failure (a store failure, an alarm, the switch's refusal for
    /// good) is that error: the call is refused all the same.
    pub(crate) async fn records_known(&mut self) -> Result<()> {
        self.refuse_while_scope_keys_missing().await?;
        if self.unlisted().await?.is_none() {
            return Ok(());
        }
        self.list_records().await.map_err(|error| {
            if unanswered(&error) {
                unlisted()
            } else {
                error
            }
        })
    }

    /// Refused as not now while a wider certificate's scope keys are still missing: the
    /// records it adds can't be listed yet, and a call that acts on what they hold waits,
    /// with a reason of its own, so the host can tell the user the signer must come online.
    pub(crate) async fn refuse_while_scope_keys_missing(&self) -> Result<()> {
        if self.scope_keys_awaited().await?.is_some() {
            return Err(keys_awaited());
        }
        Ok(())
    }

    /// List every collection the app's certificate reads, then clear the restore's and a
    /// `missed` item's marks and keep that certificate as the one listed under, in one
    /// commit: a crash before it leaves the records to be listed again. Refused while a wider certificate's scope
    /// keys are missing ([`Paykit::refuse_while_scope_keys_missing`]): a listing before them
    /// keeps values the app can't open, and keeping the certificate then would mark them
    /// listed for good. Every listing of Paykit's comes here. Only a receive takes a
    /// certificate, so none is taken during the listing.
    pub(crate) async fn list_records(&mut self) -> Result<()> {
        self.refuse_while_scope_keys_missing().await?;
        let signed = self.app.cert().signed().clone();
        self.app.list_collections().await?;
        let mut tx = Tx::default();
        tx.delete(key::RECORDS_UNLISTED);
        tx.delete(key::MISSED_UNLISTED);
        tx.put(key::LISTED_CERT, &signed)?;
        self.local_commit(tx).await
    }

    /// A read that wasn't stopped got past the client's listing, a restore's included, which
    /// the client makes only while it holds the keys of the scopes that read the records
    /// ([`Paykit::holds_scope_keys`]): so the restore's mark goes when those keys were held
    /// at the read's start, `keys_held_before`, and are under the certificate held after it,
    /// which a restore's first read may have widened before its listing. Otherwise the
    /// listing was skipped, and the mark stays until a read that lists. The certificate
    /// listed under stays as it was, since the read may have taken another one after its
    /// listing ([`Unlisted::Certificate`]), and so does a `missed` item's mark: the client
    /// listed before the feed, and the item came after ([`Unlisted::Missed`]).
    pub(crate) async fn records_listed(&mut self, keys_held_before: bool) -> Result<()> {
        if !keys_held_before || !self.holds_scope_keys() {
            return Ok(());
        }
        if self
            .local_get::<bool>(key::RECORDS_UNLISTED)
            .await?
            .is_some()
        {
            self.local_delete(key::RECORDS_UNLISTED).await?;
        }
        Ok(())
    }

    /// At a receive, under another certificate than the one the records were listed under,
    /// or after a `missed` item: they are listed now, so the calls that only read see them,
    /// and the item's deleted entries left out nothing. A listing that fails leaves the
    /// records unlisted, for the next receive or write, and its error's text is returned for
    /// the host ([`crate::ReceiveSummary::records_unlisted`]): the read went through, so its
    /// report isn't lost to it. A store failure or an alarm is the receive's error.
    pub(crate) async fn list_under_new_cert(&mut self) -> Result<Option<String>> {
        let owed = matches!(
            self.unlisted().await?,
            Some(Unlisted::Certificate | Unlisted::Missed)
        );
        if !owed || !self.holds_scope_keys() {
            return Ok(None);
        }
        match self.list_records().await {
            Ok(()) => Ok(None),
            Err(error) if local_failure(&error) => Err(error),
            Err(error) => Ok(Some(error.to_string())),
        }
    }

    /// For a call that only reads: refused as not now while the records aren't listed since
    /// a restore, or under the certificate held now, rather than showing a copy that may
    /// lack them as the records.
    pub(crate) async fn refuse_while_unlisted(&self) -> Result<()> {
        self.refuse_while_scope_keys_missing().await?;
        if self.unlisted().await?.is_some() {
            return Err(unlisted());
        }
        Ok(())
    }

    /// Whether a reconcile still owes the payer pass since a restore: the restore sets the
    /// mark in its own commit, and only a [`Paykit::reconcile`] that ran the payer pass,
    /// under `pay`, clears it. The backup doesn't hold the attempts made after it, and only
    /// that pass finds them on the payer's obligations (Paykit spec 7.3), so until then no
    /// new attempt is made and nothing is dispatched, whatever the host does meanwhile, such
    /// as [`Paykit::end_reconcile`] after a reconcile without `pay` skipped the pass.
    pub(crate) async fn payer_pass_owed(&self) -> Result<bool> {
        Ok(self
            .local_get::<bool>(key::PAYER_PASS_OWED)
            .await?
            .is_some())
    }

    /// Refused as not now while [`Paykit::payer_pass_owed`].
    pub(crate) async fn refuse_while_payer_pass_owed(&self) -> Result<()> {
        if self.payer_pass_owed().await? {
            return Err(payer_pass_refusal().into());
        }
        Ok(())
    }

    /// A reconcile ran the payer pass: the restore's mark goes.
    pub(crate) async fn payer_pass_ran(&mut self) -> Result<()> {
        if self.payer_pass_owed().await? {
            self.local_delete(key::PAYER_PASS_OWED).await?;
        }
        Ok(())
    }

    /// Whether an activity or Allowance record in the copy holds a value sealed under
    /// a scope key the app doesn't hold yet ([`awaits_key`]): its counterparty and status
    /// can't be read, so the re-sends a restore or a `missed` item asks for wait for it.
    pub(crate) async fn copy_awaits_key(&self) -> Result<bool> {
        for collection in [
            Collection::PayerActivity,
            Collection::PayeeActivity,
            Collection::Allowances,
        ] {
            for held in self.app.records_of(collection.name()).await? {
                if awaits_key(&held.content) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Queue the messages this app signed that its activity records store to be sent and no
    /// outbox holds, with the signed plaintext each record stores (Switch spec 11, "Queued
    /// again by its signer"), at every receive while any is owed:
    ///
    /// - once after a restore, every such message ([`own_to_send_again`]): a batch the backup
    ///   kept unanswered queued its message only with an answer the restored app never
    ///   gets, so no outbox holds it;
    /// - each rejection or payer's cancellation recorded while sends to its payee were held
    ///   ([`HeldAnswer`]), which was never queued;
    /// - once after a restore, the messages its `allowances` records store to be sent
    ///   ([`crate::allowances::own_allowance_to_send_again`]), from each record bound to its
    ///   key. A crossing end recorded while sends were held goes from its own owed mark
    ///   ([`Paykit::send_owed_ends`]).
    ///
    /// Called once the copy of the records is listed and no scope key is awaited. A message
    /// the outbox holds, or that was sent, is queued once; one the client refuses
    /// ([`pubky_switch_client::app::WontDeliver`]: past its replay window of 30 days from its
    /// `created_at`, given up already, too large) is not sent again and never asked for
    /// again; one the client refuses as not this app's own is not sent either. A held
    /// answer refused that way never reached its payee: the host is told, with a
    /// [`crate::NoticeKind::MessageGivenUp`] notice, and the record still holds it, so no own
    /// app pays or accepts the request. A restore's re-send of any other message tells
    /// nothing. A held answer no record shows to send any more goes with nothing sent when a
    /// stop in its record covers it, or the record is gone; one whose record is held but
    /// can't be read now, or is paused as divergent and not confirmed since, stays, since
    /// that record may still hold it (Paykit spec 6). One whose record reads, isn't paused,
    /// and holds neither it nor any stop was lost with a write: it is given up with a
    /// [`crate::NoticeKind::MessageGivenUp`] notice, never silently, so the user may answer
    /// again ([`AnswerRecord`]). The pass waits while a record of the copy is sealed under a
    /// scope key the app doesn't hold yet ([`Paykit::copy_awaits_key`]), so what such a
    /// record stores is sent once it opens. For the restore's re-send, a record held under a
    /// Paykit policy, an activity or an Allowance record, whose value doesn't open for good,
    /// read or bind shows none of its messages: the re-send stays owed and judges every
    /// record again at each receive, until each such record reads, its messages then going
    /// under the same rules, or 30 days have passed since the restore
    /// ([`OwnResendOwed::restored_at_ms`]), after which none of them could still go, so one
    /// found unreadable later than that is not waited for; a delete holds no message, and a
    /// record under another policy nothing of Paykit's. Messages to a counterparty whose
    /// sends are held wait, the restore's mark or the held answer naming it, for a later
    /// receive: nothing reaches it meanwhile. Period details go again only while no later
    /// stop covers them: none once their period is credited or their request is rejected or
    /// cancelled; while their request record isn't held, or doesn't read or bind, they wait
    /// the same way, neither sent nor dropped, and are judged again at each later receive,
    /// until the record shows it or every one of them is past the replay window the client
    /// would refuse it by ([`Paykit::details_stop`]). The other messages of their record go
    /// meanwhile.
    pub(crate) async fn queue_own_again(&mut self) -> Result<()> {
        let owed = self
            .local_get::<OwnResendOwed>(key::OWN_RESEND_OWED)
            .await?;
        let held: std::collections::BTreeMap<String, HeldAnswer> = self
            .local_scan::<HeldAnswer>(key::HELD_ANSWERS)
            .await?
            .into_iter()
            .map(|(_, answer)| (answer.msg_id.clone(), answer))
            .collect();
        if owed.is_none() && held.is_empty() {
            return Ok(());
        }
        if self.copy_awaits_key().await? {
            return Ok(());
        }
        let own = self.identity();
        let app_id = self.app.cert().app_id();
        let mut waiting = std::collections::BTreeSet::new();
        let mut still_held = std::collections::BTreeSet::new();
        let mut done = std::collections::BTreeSet::new();
        let mut given_up = Vec::new();
        let mut unread = false;
        for (collection, side) in [
            (
                Collection::PayerActivity,
                paykit_lib::records::ActivitySide::Payer,
            ),
            (
                Collection::PayeeActivity,
                paykit_lib::records::ActivitySide::Payee,
            ),
        ] {
            for record_held in self.app.records_of(collection.name()).await? {
                // A record under a policy that isn't Paykit's holds nothing of Paykit's, and a
                // delete holds no message: neither is a record the pass can't read.
                let Some(label) = collection.policy_label(&record_held.policy) else {
                    continue;
                };
                if record_held.content == RecordContent::Deleted {
                    continue;
                }
                let record = record_held.content.value().and_then(|value| {
                    crate::records::bind_activity(
                        value,
                        &own,
                        side,
                        label.as_ref(),
                        &record_held.key,
                        |t| self.app.record_key(t).ok(),
                        &|p| self.rail.settlement_id(p),
                    )
                });
                // A record that doesn't open, read or bind shows nothing to send now: the
                // restore's re-send judges every record again at the next receive, until 30
                // days after the restore (`restored_at_ms`), and a held answer it may hold
                // stays (`answer_record`).
                let Some(record) = record else {
                    unread = true;
                    continue;
                };
                let counterparty = record.counterparty;
                let restore_owes = owed.as_ref().is_some_and(|owed| {
                    owed.waiting
                        .as_ref()
                        .is_none_or(|waiting| waiting.binary_search(&counterparty).is_ok())
                });
                // Period details go again only while no later stop covers them: one their
                // request record can't show yet holds them, unless the client would refuse
                // them by now anyway.
                let details = if restore_owes {
                    own_details(&record, &own, &app_id)
                } else {
                    Vec::new()
                };
                let stop = if details.is_empty() {
                    DetailsStop::Goes
                } else {
                    Box::pin(self.details_stop(&record, label.as_ref())).await?
                };
                let details_wait = stop == DetailsStop::Unknown
                    && details
                        .iter()
                        .any(|stored| !self.past_replay_window(stored));
                let details_go = stop == DetailsStop::Goes;
                let messages: Vec<_> = own_to_send_again(&record, &own, &app_id, details_go)
                    .into_iter()
                    .filter(|stored| restore_owes || held.contains_key(&stored.message().msg_id))
                    .collect();
                if messages.is_empty() && !details_wait {
                    continue;
                }
                if self.sends_held(&counterparty).await? {
                    if restore_owes {
                        waiting.insert(counterparty);
                    }
                    still_held.extend(
                        messages
                            .iter()
                            .map(|stored| stored.message().msg_id.clone())
                            .filter(|msg_id| held.contains_key(msg_id)),
                    );
                    continue;
                }
                if details_wait {
                    waiting.insert(counterparty);
                }
                for stored in messages {
                    if held.contains_key(&stored.message().msg_id) {
                        done.insert(stored.message().msg_id.clone());
                    }
                    match self.app.queue_stored(stored.plaintext()).await {
                        Ok(msg_id) => {
                            let _ = self.app.flush_message(&msg_id).await;
                        }
                        // Refused for good: nothing is queued; the pass doesn't ask again. A
                        // held answer never went, so the host is told.
                        Err(AppError::Refused(ClientRefusal::WontDeliver(_))) => {
                            if let Some(answer) = held.get(&stored.message().msg_id) {
                                given_up.push(answer.clone());
                            }
                        }
                        // Not this app's own: nothing is queued and nothing is told.
                        Err(AppError::Refused(ClientRefusal::Usage(_))) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        // A restore's re-send of the messages an `allowances` record stores, from the record
        // bound to its key (Paykit spec 7.2, 8.3; `allowances::own_allowance_to_send_again`).
        if let Some(restore) = owed.as_ref() {
            for record_held in Box::pin(self.app.records_of(Collection::Allowances.name())).await? {
                // As for an activity record: one under another policy holds nothing of
                // Paykit's and a delete holds no message, while one that doesn't open, read or
                // bind keeps the re-send owed, judged again at the next receive.
                if record_held.policy != Collection::Allowances.fixed_policy()
                    || record_held.content == RecordContent::Deleted
                {
                    continue;
                }
                let Some(record) = Box::pin(self.held_allowance(&record_held.key)).await? else {
                    unread = true;
                    continue;
                };
                let counterparty = record.counterparty;
                if restore
                    .waiting
                    .as_ref()
                    .is_some_and(|waiting| waiting.binary_search(&counterparty).is_err())
                {
                    continue;
                }
                // Nothing is sent for a conflicted Allowance, nor under a counterparty's end
                // kept as the stop, which the record doesn't show yet.
                if Box::pin(self.conflict_kept(&record_held.key)).await?
                    || Box::pin(self.allowance_stopped(&record_held.key)).await?
                {
                    continue;
                }
                let messages =
                    crate::allowances::own_allowance_to_send_again(&record, &own, &app_id);
                if messages.is_empty() {
                    continue;
                }
                if Box::pin(self.sends_held(&counterparty)).await? {
                    waiting.insert(counterparty);
                    continue;
                }
                for stored in messages {
                    Box::pin(self.queue_stored_again(stored)).await?;
                }
            }
        }
        // Each held answer is queued, given up with its notice, or shown to send by no record
        // any more, unless its payee's sends are still held. Nothing is decided on a record
        // that can't be read now, or that is paused as divergent and not confirmed since: such
        // a record may still hold the answer, and the answer stays. A record the user has
        // confirmed, or never saw divergent, that lacks the answer and holds no stop gives it
        // up with the host told, never silently: the user may answer again.
        let mut gone = Vec::new();
        for answer in held.values() {
            if still_held.contains(&answer.msg_id) || done.contains(&answer.msg_id) {
                if done.contains(&answer.msg_id) {
                    gone.push(answer.clone());
                }
                continue;
            }
            match Box::pin(self.answer_record(answer)).await? {
                AnswerRecord::Undecided => {}
                AnswerRecord::Gone | AnswerRecord::Covered => gone.push(answer.clone()),
                AnswerRecord::Lacks => {
                    given_up.push(answer.clone());
                    gone.push(answer.clone());
                }
            }
        }
        let mut tx = Tx::default();
        let at = self.service_time().unwrap_or(0);
        for answer in given_up {
            let notice = key::message_notice(&answer.payee, &answer.msg_id);
            if self.kept_notice(&notice).await?.is_none() {
                tx.put(
                    &notice,
                    &crate::notices::KeptNotice::given_up(
                        answer.payee,
                        &answer.msg_id,
                        Some(answer.request_hash),
                        at,
                        crate::notices::GivenUpFrom::Origin(
                            crate::notices::GivenUpOrigin::HeldAnswer,
                        ),
                    ),
                )?;
            }
        }
        for answer in gone {
            tx.delete(&key::held_answer(&answer.payee, &answer.msg_id));
        }
        if let Some(owed) = &owed {
            // The restore's time, kept for the life of the mark. A mark without it, written
            // before it was kept or by a restore that stopped before writing it, takes the
            // time of the first pass that finds a record it can't read, which is later: the
            // wait never ends early.
            let restored_at_ms = owed
                .restored_at_ms
                .or_else(|| unread.then(|| self.window_now()));
            let unread_waits =
                unread && restored_at_ms.is_some_and(|at| !self.past_restore_window(at));
            if unread_waits {
                // A record that can't be read names no counterparty: the next pass judges
                // every record again, whoever its counterparty.
                tx.put(
                    key::OWN_RESEND_OWED,
                    &OwnResendOwed {
                        waiting: None,
                        restored_at_ms,
                    },
                )?;
            } else if waiting.is_empty() {
                tx.delete(key::OWN_RESEND_OWED);
            } else {
                tx.put(
                    key::OWN_RESEND_OWED,
                    &OwnResendOwed {
                        waiting: Some(waiting.into_iter().collect()),
                        restored_at_ms,
                    },
                )?;
            }
        }
        self.local_commit(tx).await
    }

    /// What the request record a held answer was recorded in, which shows no message of it
    /// to send now, says of the answer ([`AnswerRecord`]).
    async fn answer_record(&mut self, answer: &HeldAnswer) -> Result<AnswerRecord> {
        let target = crate::activity::ActivityTarget::payer(
            paykit_lib::derive::natural_key::request_record(&answer.payee, &answer.request_hash),
        );
        let key = self.activity_key(&target)?;
        let Some(held) = self.app.shared_record(&key).await? else {
            return Ok(AnswerRecord::Gone);
        };
        if let RecordContent::Unreadable(_) = held.content {
            return Ok(AnswerRecord::Undecided);
        }
        // A record seen divergent whose user hasn't confirmed it since, its current write a
        // delete or not: the upload may hold the answer, so nothing is decided on the write
        // held (Paykit spec 6).
        if Box::pin(self.pause_of(&held)).await?.paused {
            return Ok(AnswerRecord::Undecided);
        }
        if let RecordContent::Deleted = held.content {
            return Ok(AnswerRecord::Gone);
        }
        let Some(record) = self.held_activity(&target).await? else {
            return Ok(AnswerRecord::Undecided);
        };
        Ok(
            if record.reflects(&answer.msg_id) || rejected(&record) || cancelled(&record) {
                AnswerRecord::Covered
            } else {
                AnswerRecord::Lacks
            },
        )
    }

    /// Queue `stored`, a message this app signed that a record stores, again with its signed
    /// plaintext, and try to deliver it now ([`Paykit::queue_own_again`]). One the client
    /// refuses for good, or as not this app's own, is neither queued nor told, and isn't
    /// asked for again; a store failure is the call's error.
    pub(crate) async fn queue_stored_again(
        &mut self,
        stored: &paykit_lib::records::StoredMessage,
    ) -> Result<()> {
        match self.app.queue_stored(stored.plaintext()).await {
            Ok(msg_id) => {
                let _ = self.app.flush_message(&msg_id).await;
                Ok(())
            }
            // Refused for good, or not this app's own: nothing is queued and nothing is
            // told; the pass doesn't ask again.
            Err(AppError::Refused(ClientRefusal::WontDeliver(_) | ClientRefusal::Usage(_))) => {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The time the client judges a message's replay window by: the later of the device
    /// clock and the highest service time verified (`App::queue_stored`). No counterparty
    /// sets either.
    fn window_now(&self) -> u64 {
        self.now().max(self.service_time().unwrap_or(0))
    }

    /// Whether 30 days have passed, on [`Paykit::window_now`], since `since_ms`, the restore's
    /// time or a later one: every message an activity record stores to be sent again was
    /// signed before the backup, so none could still go by then.
    fn past_restore_window(&self, since_ms: u64) -> bool {
        self.window_now() > since_ms.saturating_add(pubky_switch_client::app::SENT_RETENTION_MS)
    }

    /// Write the restore's time into the restore's re-send mark, right after the client's
    /// restore commit ([`OwnResendOwed::restored_at_ms`]): the restore's own step reads no
    /// clock. A mark that has one keeps it.
    pub(crate) async fn stamp_restore_time(&mut self) -> Result<()> {
        let Some(mut owed) = self
            .local_get::<OwnResendOwed>(key::OWN_RESEND_OWED)
            .await?
        else {
            return Ok(());
        };
        if owed.restored_at_ms.is_some() {
            return Ok(());
        }
        owed.restored_at_ms = Some(self.window_now());
        self.local_put(key::OWN_RESEND_OWED, &owed).await
    }

    /// Whether a later stop covers the period details the payee period record `record`,
    /// under `label`, keeps (Paykit spec 7.2, "What a restore loses"): its period credited,
    /// the record holding the receipt every credit leaves in it, or its request record
    /// holding a rejection or a cancellation from either side. A request record the app
    /// doesn't hold, or holds as a value that doesn't read or bind, can't show it:
    /// [`DetailsStop::Unknown`].
    async fn details_stop(
        &self,
        record: &ActivityRecord,
        label: Option<&paykit_lib::Label>,
    ) -> Result<DetailsStop> {
        if record.receipt.is_some() {
            return Ok(DetailsStop::Stopped);
        }
        let Ok(paykit_lib::records::RecordBinding::Period { request_hash, .. }) = record.binding()
        else {
            return Ok(DetailsStop::Unknown);
        };
        let target = crate::activity::ActivityTarget::payee(
            paykit_lib::derive::natural_key::request_record(&self.identity(), &request_hash),
            label,
        );
        Ok(match self.held_activity(&target).await? {
            None => DetailsStop::Unknown,
            Some(request) if rejected(&request) || cancelled(&request) => DetailsStop::Stopped,
            Some(_) => DetailsStop::Goes,
        })
    }

    /// Whether the client refuses to queue `stored` again by now: past 30 days from its
    /// `created_at`, less the clock tolerance, by the later of the device clock and the
    /// service time verified (Switch spec 11, `App::queue_stored`).
    fn past_replay_window(&self, stored: &paykit_lib::records::StoredMessage) -> bool {
        let until = stored
            .message()
            .created_at_ms
            .saturating_sub(CLOCK_TOLERANCE_MS)
            .saturating_add(pubky_switch_client::app::SENT_RETENTION_MS);
        self.window_now() > until
    }

    /// Queue, once after a restore, a `resend_request` to every counterparty with an open
    /// request or Allowance, from the restore's re-send time; [`Paykit::receive`] sends them. The start is
    /// bounded where it is used as well as at the restore and the load: no ordering of a
    /// load, a renewal and a crash leaves the re-sends starting in the future. A request
    /// record sealed under a scope key the app doesn't hold yet shows no counterparty: the
    /// counterparties the other records show are asked now, each once, and the hold names
    /// them, so the one a sealed record shows is asked once it opens; the asks are all
    /// queued once no record is sealed.
    pub(crate) async fn queue_restore_asks(&mut self) -> Result<()> {
        let Some(mut hold) = self.local_get::<RestoreHold>(key::RESTORE_HOLD).await? else {
            return Ok(());
        };
        let bound = self
            .service_time()
            .unwrap_or_else(|| self.now())
            .saturating_add(CLOCK_TOLERANCE_MS);
        hold.resend_from_ms = hold
            .resend_from_ms
            .min(RestoreHold::of(bound).resend_from_ms);
        if hold.asks_queued {
            return Ok(());
        }
        let mut counterparties = std::collections::BTreeSet::new();
        for view in self.requests().await? {
            let open = !matches!(
                view.status,
                RequestStatus::Paid
                    | RequestStatus::Cancelled
                    | RequestStatus::Rejected
                    | RequestStatus::Expired
            );
            if open {
                counterparties.insert(view.request.payee);
            }
        }
        // And every counterparty of an Allowance not ended or rejected: the backup may
        // predate its end, which comes back with the re-sends (Paykit spec 8.3).
        counterparties.extend(self.open_allowance_counterparties().await?);
        let mut asked: std::collections::BTreeSet<[u8; 32]> = hold.asked.iter().copied().collect();
        let mut tx = Tx::default();
        for counterparty in counterparties {
            if asked.insert(counterparty) {
                self.queue_ask(&mut tx, counterparty, hold.resend_from_ms)
                    .await?;
            }
        }
        if self.copy_awaits_key().await? {
            hold.asked = asked.into_iter().collect();
        } else {
            hold.asks_queued = true;
            hold.asked = Vec::new();
        }
        tx.put(key::RESTORE_HOLD, &hold)?;
        self.local_commit(tx).await
    }
}

/// The restore's re-send of this app's own messages, owed (`p/ro`): set by a restore, never
/// restored, gone once every message it covers was queued again or refused
/// ([`Paykit::queue_own_again`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OwnResendOwed {
    /// `None` while no pass ran, or after a pass that found an activity record it couldn't
    /// read (a value that doesn't open for good, read or bind), which names no counterparty,
    /// within 30 days of the restore: every counterparty's messages are owed. After any other
    /// pass, the counterparties some of whose messages wait, ascending, each once: only
    /// their messages are owed. Those whose sends were held then, a blocked one or every one
    /// while the copy of the deny list isn't known, and those with period details whose
    /// request record couldn't show yet whether a later stop covers them
    /// ([`DetailsStop::Unknown`]).
    pub(crate) waiting: Option<Vec<[u8; 32]>>,
    /// The restore's time, on [`Paykit::window_now`], written by [`Paykit::restore`] right
    /// after the client's restore commit and kept for the life of the mark: 30 days after it
    /// no message a record stores to be sent again could still go, so no record that can't
    /// be read keeps the re-send owed past it. `None`, in a mark written before this field or
    /// by a restore that stopped before writing it, fails closed: the first pass that finds
    /// a record it can't read sets it to its own time, which is later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) restored_at_ms: Option<u64>,
}

/// Whether a later stop covers the period details a payee period record keeps, for the
/// restore's re-send ([`Paykit::queue_own_again`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DetailsStop {
    /// None does: they are queued again.
    Goes,
    /// The period is credited, or its request is rejected or cancelled: they aren't.
    Stopped,
    /// The request record can't show it yet: they wait, neither sent nor dropped, until it
    /// can or the client would refuse them by now anyway.
    Unknown,
}

/// The period details `record` keeps that this app signed, `app_id` of identity `own`, that
/// go again after a restore while no later stop covers them: every one, the earlier ones
/// (`earlier_details`) and then the current one, while the record holds no response and no
/// cancellation.
pub(crate) fn own_details<'a>(
    record: &'a ActivityRecord,
    own: &[u8; 32],
    app_id: &[u8; 16],
) -> Vec<&'a paykit_lib::records::StoredMessage> {
    if record.response.is_some() || record.cancellation.is_some() {
        return Vec::new();
    }
    record
        .earlier_details
        .iter()
        .chain(record.period_details.iter())
        .filter(|stored| own_signed(stored, own, app_id))
        .collect()
}

/// What a request record says of a held answer it shows no message of to send now
/// ([`Paykit::queue_own_again`]).
enum AnswerRecord {
    /// It can't be read now, or is paused as divergent and not confirmed since, a delete
    /// included: it may still hold the answer, which stays.
    Undecided,
    /// It is absent, or deleted and not paused: the request is gone, and the answer goes
    /// with nothing sent.
    Gone,
    /// It holds the answer, or a stop that covers it: the answer goes with nothing sent.
    Covered,
    /// It reads, isn't paused, and holds neither the answer nor any stop: the answer was lost
    /// with a write, and is given up with the host told, so the user may answer again.
    Lacks,
}

/// A rejection or payer's cancellation this app recorded in its request record while sends
/// to the payee were held (`p/ha/<payee>/<msg_id>`), written in the batch that writes the
/// record, which queues nothing (Paykit spec 8.1, "Blocked counterparties"): the record holds
/// the signed message, so no own app pays or accepts the request, and
/// [`Paykit::queue_own_again`] queues it from there once sends to the payee aren't held. Kept
/// through a restart and carried in backups; after a restore the record is the source, as
/// for the restore's re-send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HeldAnswer {
    /// The payee it goes to.
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub(crate) payee: [u8; 32],
    /// Its `msg_id`.
    pub(crate) msg_id: String,
    /// The request it answers.
    #[serde(with = "pubky_switch_core::enc::serde_b64")]
    pub(crate) request_hash: [u8; 32],
}

/// The messages of `record` this app signed, `app_id` of identity `own`, that it queues again
/// after a restore (Paykit spec 7.2, "What a restore loses"): each a record stores to be
/// sent, since every message this app stores in a record it also queues
/// (`records::own_message_tests`), while the counterparty hasn't answered it and no later
/// stop covers it. A request goes while the record holds no response and no cancellation;
/// a Billing Period's details too ([`own_details`]), and only while `details_go`: no stop
/// the period's request record or the period's credit shows covers them
/// ([`DetailsStop`]); an acceptance or a rejection while it holds no cancellation and no
/// receipt; a proof while it holds no receipt; a cancellation and a receipt always. A
/// receipt the app stored without sending it is never sent this way: this app stores its
/// own receipt only in the batch that queues it ([`Paykit::confirm_payment_with`]), so its
/// receipt in a record is one it sent.
pub(crate) fn own_to_send_again<'a>(
    record: &'a ActivityRecord,
    own: &[u8; 32],
    app_id: &[u8; 16],
    details_go: bool,
) -> Vec<&'a paykit_lib::records::StoredMessage> {
    let answered = record.response.is_some() || record.cancellation.is_some();
    let request = record.request.iter().filter(|_| !answered);
    let details = if details_go {
        own_details(record, own, app_id)
    } else {
        Vec::new()
    };
    let later = [
        (
            &record.response,
            record.cancellation.is_none() && record.receipt.is_none(),
        ),
        (&record.proof, record.receipt.is_none()),
        (&record.cancellation, true),
        (&record.receipt, true),
    ]
    .into_iter()
    .filter_map(|(stored, send)| stored.as_ref().filter(|_| send));
    let mut all: Vec<_> = request
        .filter(|stored| own_signed(stored, own, app_id))
        .collect();
    all.extend(details);
    all.extend(later.filter(|stored| own_signed(stored, own, app_id)));
    all
}

/// Whether `stored` is a message this app, `app_id` of identity `own`, signed.
fn own_signed(
    stored: &paykit_lib::records::StoredMessage,
    own: &[u8; 32],
    app_id: &[u8; 16],
) -> bool {
    stored.sender() == *own && stored.cert().app_id() == *app_id
}

/// The refusal while the shared records aren't listed since a restore.
/// The refusal while a reconcile owes the payer pass since a restore.
pub(crate) fn payer_pass_refusal() -> Refusal {
    Refusal::NotNow(
        "Paykit spec 7.3: no reconcile under pay has read the payer's obligations since the \
         restore"
            .into(),
    )
}

pub(crate) fn unlisted() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 8.2, 8.3, 12.4 step 2.1: the shared records aren't listed since the \
         restore, the missed item or under the certificate held now"
            .into(),
    )
    .into()
}

/// The refusal while a wider certificate's scope keys are missing.
fn keys_awaited() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 3.5: waiting for the scope keys of the certificate taken, which the \
         signer stores when it is next online"
            .into(),
    )
    .into()
}

/// Whether a record's value isn't open for now only: sealed under a scope key the app
/// doesn't hold yet (Switch spec 3.5). The client opens it once the key comes, so a value
/// that is only waiting is never treated as one that doesn't read: nothing offers to write
/// over it, which would drop what it holds unseen, such as single-use marks.
pub(crate) fn awaits_key(content: &RecordContent) -> bool {
    matches!(
        content,
        RecordContent::Unreadable(Unopened::KeyNotHeld { .. })
    )
}

/// The refusal of a write over a value that is only waiting for its scope key
/// ([`awaits_key`]).
pub(crate) fn value_awaits_key() -> crate::error::PaykitError {
    Refusal::NotNow(
        "Switch spec 3.5: waiting for the scope keys the record's value is sealed under, which \
         the signer stores when it is next online"
            .into(),
    )
    .into()
}

/// A listing the network or the switch couldn't answer now, as a payment's switch read
/// counts it ([`crate::pay::switch_unavailable`]), or a `429`: a record's absence then
/// decides nothing.
fn unanswered(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(client) if crate::pay::switch_unavailable(client))
        || error.code() == Some(ErrorCode::RateLimited)
}

/// A failure of this app's own: its store or an alarm.
pub(crate) fn local_failure(error: &PaykitError) -> bool {
    matches!(error, PaykitError::Client(client) if matches!(client.unkept(), AppError::Store(_) | AppError::Alarm(_)))
}

#[cfg(test)]
mod tests {
    use pubky_switch_client::error::{ClientError, StoreError, TransportError};

    use super::*;

    fn switch(code: ErrorCode) -> PaykitError {
        PaykitError::Client(AppError::Switch(ClientError::Switch {
            status: code.status(),
            code,
            message: "scripted".into(),
            current: serde_json::Value::Null,
            known_version: None,
            retry_after: None,
        }))
    }

    /// Alice's wallet's certificate for `scopes`, issued `after_ms` after the fixtures' time.
    fn cert(scopes: &[&str], after_ms: u64) -> AppCert {
        use pubky_switch_core::records::{AppCertBody, DEFAULT_CERT_LIFETIME_MS};
        let party = crate::testkit::Party::new("alice", "wallet", scopes);
        let owned: Vec<String> = scopes.iter().map(|s| (*s).to_owned()).collect();
        let issued_at = crate::testkit::NOW + after_ms;
        AppCert::issue(
            &party.key,
            AppCertBody::new(
                party.key.public(),
                party.keys.signing_public(),
                party.keys.bundle_public(),
                owned.clone(),
                &paykit_lib::scopes::permissions_for(&owned),
                issued_at,
                issued_at + DEFAULT_CERT_LIFETIME_MS,
            ),
        )
        .unwrap()
    }

    #[test]
    fn test_side_unlisted_under_any_other_certificate_that_reads_the_side() {
        let listed = cert(&["pay", "contacts", "request"], 0);
        for held in [
            // Narrower: one held between may have read less again.
            cert(&["pay", "contacts"], 1),
            // The same scopes, renewed: one held between may have read less.
            cert(&["pay", "contacts", "request"], 2),
            // Wider.
            cert(&["pay", "contacts", "request", "allowance"], 3),
        ] {
            for side in [RecordSide::Payer, RecordSide::All] {
                assert!(side_unlisted(side, false, Some(&listed), &held));
            }
        }
        assert!(side_unlisted(RecordSide::Payee, false, None, &listed));
        assert!(side_unlisted(
            RecordSide::Payee,
            true,
            Some(&listed),
            &listed
        ));
    }

    #[test]
    fn test_side_unlisted_not_under_the_certificate_listed_under_or_for_a_side_unread() {
        let listed = cert(&["pay", "contacts", "request"], 0);
        for side in [RecordSide::Payer, RecordSide::Payee, RecordSide::All] {
            assert!(!side_unlisted(side, false, Some(&listed), &listed));
        }
        let payer_only = cert(&["pay", "contacts"], 1);
        assert!(!side_unlisted(
            RecordSide::Payee,
            false,
            Some(&listed),
            &payer_only
        ));
    }

    #[test]
    fn test_listing_unanswered_as_a_switch_read_counts_it() {
        let lost = PaykitError::Client(AppError::Transport(TransportError::Connect("down".into())));
        assert!(unanswered(&lost));
        assert!(unanswered(&PaykitError::Client(
            AppError::NotAnsweringAsAccount { since_ms: 1 }
        )));
        assert!(unanswered(&PaykitError::Client(AppError::Switch(
            ClientError::BadServiceSignature { status: 502 }
        ))));
        assert!(unanswered(&switch(ErrorCode::RateLimited)));
        assert!(unanswered(&switch(ErrorCode::Unavailable)));
        assert!(!unanswered(&switch(ErrorCode::PermissionMissing)));
    }

    #[test]
    fn test_listing_failure_of_the_app_own_is_the_store_or_an_alarm() {
        assert!(local_failure(&PaykitError::Client(AppError::Store(
            StoreError::Unavailable
        ))));
        assert!(!local_failure(&switch(ErrorCode::PermissionMissing)));
        assert!(!local_failure(&PaykitError::Client(AppError::InvalidData(
            "bad".into()
        ))));
    }
}
