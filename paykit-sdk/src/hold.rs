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
    AppError, Collection as SwitchCollection, PolicyFamily, RecordContent, Unopened,
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
    /// the highest service time verified once the read that took the item returned, as
    /// the client at this pin tells only the item's first time. `None` until then, when
    /// every open request is held.
    pub(crate) bound_ms: Option<u64>,
}

impl MissedHold {
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
    /// `payer_activity` and `payment_lists`.
    Payer,
    /// `payee_activity`, every label's.
    Payee,
    /// Every collection of Paykit's.
    All,
}

impl RecordSide {
    /// The collections of the side.
    pub(crate) fn collections(self) -> Vec<SwitchCollection> {
        let payer = crate::activity::family(Collection::PayerActivity);
        let payee = crate::activity::family(Collection::PayeeActivity);
        let lists = crate::activity::family(Collection::PaymentLists);
        match self {
            Self::Payer => vec![payer, lists],
            Self::Payee => vec![payee],
            Self::All => vec![payer, payee, lists],
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

    /// Bound a `missed` item's hold set by the read that just returned
    /// ([`MissedHold::bound_ms`]) with the highest service time verified: the item came no
    /// later. Without a verified time, it stays unbounded.
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
    /// time, the two times of each mark of dropped messages (`pd/`, `ud/`), and the
    /// acceptance time of each stop (`kc/`) and kept reply (`pr/`). A cursor time ahead of
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
        ] {
            for (rest, mut mark) in self
                .local_scan::<crate::marks::DropMark>(family.prefix())
                .await?
            {
                if mark.latest_ms > bound {
                    mark.latest_ms = bound;
                    mark.earliest_ms = mark.earliest_ms.min(bound);
                    tx.put(&format!("{}{rest}", family.prefix()), &mark)?;
                }
            }
        }
        for prefix in [key::KEPT_CANCELLATIONS, key::KEPT_REPLIES] {
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

    /// Whether a payer's or payee's activity record in the copy holds a value sealed under
    /// a scope key the app doesn't hold yet ([`awaits_key`]): its counterparty and status
    /// can't be read, so the re-sends a restore or a `missed` item asks for wait for it.
    pub(crate) async fn copy_awaits_key(&self) -> Result<bool> {
        for collection in [Collection::PayerActivity, Collection::PayeeActivity] {
            for held in self.app.records_of(collection.name()).await? {
                if awaits_key(&held.content) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Queue, once after a restore, a `resend_request` to every counterparty with an open
    /// request, from the restore's re-send time; [`Paykit::receive`] sends them. The start is
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
fn local_failure(error: &PaykitError) -> bool {
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
